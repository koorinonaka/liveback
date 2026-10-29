//! Releasing finished readers off the playback thread (task246), split out of
//! `segment_reader.rs` (t261025-d24d).

use super::segment_reader::SegmentReader;
use super::worker::{deserves_a_line, SLOW_STEP};

/// Releasing an `IMFSourceReader` tears down the decoder MFT and its worker
/// threads, and that costs a measured **29-34ms every time** -- not a spread,
/// a fixed toll. It used to be paid wherever the old reader was dropped, which
/// is three places on the hot path: `ensure_segment`'s replacement, `tick`'s
/// prefetched crossing (on the playback thread), and `do_seek`'s discard of an
/// unused prefetch. None of those are waiting on the object for anything; the
/// user is simply waiting for its funeral.
// The field exists to be dropped -- that *is* the job -- which rustc counts
// as never being read.
struct Retiring(#[allow(dead_code)] SegmentReader);

/// `IMFSourceReader` is not `Send` because windows-rs is conservative about COM
/// apartments. This app initialises MTA (`COINIT_MULTITHREADED`), so the reader
/// is an MTA object and any other MTA thread may release it -- which is what
/// `retire_thread` below is: it calls `CoInitializeEx(COINIT_MULTITHREADED)`
/// before touching anything. The same reasoning already carries
/// `Mp4FinalizeCallback` (`encoder/segment_writer.rs`) and the export writer
/// across threads.
unsafe impl Send for Retiring {}

/// How many readers may be waiting to be released before the sender gives up
/// and releases inline. One crossing produces one, so depth is generous; a
/// burst of scrubbing is the case it exists for. Each holds its segment's
/// bytes (~4.5MB), so this is also the cap on what the queue can pin.
const RETIRE_QUEUE_DEPTH: usize = 2;

/// How long the retire thread waits between checks while playback is busy.
const RETIRE_IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// Hands finished readers to a thread that releases them. Cloneable, cheap,
/// and safe to drop: the thread ends when the last sender goes.
pub(super) struct ReaderRetirer {
    tx: crossbeam_channel::Sender<Retiring>,
    /// True while the playback thread is decoding frames on a deadline.
    ///
    /// Moving the release off the hot path is not enough on its own: tearing a
    /// decoder MFT down *at an arbitrary moment* contends with the decode that
    /// is running right then, and the first measurement of this change showed
    /// it -- seeks improved by 20-25ms while 30s of playback went from 0 late
    /// frames to 4-7. The teardown is not expensive because of where it runs,
    /// it is expensive because of *when*. So the thread holds what it is given
    /// until playback is idle. The queue filling up during long continuous
    /// playback is the intended outcome: the sender then falls back to inline
    /// release at a crossing, which is exactly what the code did before this
    /// mechanism existed, and that profile measured 0 late frames.
    busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl ReaderRetirer {
    pub(super) fn spawn() -> Self {
        let (tx, rx) = crossbeam_channel::bounded::<Retiring>(RETIRE_QUEUE_DEPTH);
        let busy = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_busy = busy.clone();
        // Deliberately not joined at shutdown. The only thing a join buys is
        // releasing the last reader or two before the process exits, which the
        // OS does anyway; blocking exit for 30ms of COM teardown per queued
        // reader would be a worse trade. The thread dies with its channel.
        let _ = std::thread::Builder::new()
            .name("livia-reader-retire".into())
            .spawn(move || {
                // MTA, so the readers arriving here are released in the same
                // apartment they were created in.
                crate::com::init_mta();
                retire_loop(&rx, &thread_busy);
            });
        Self { tx, busy }
    }

    /// Tells the retire thread whether the playback thread is decoding right
    /// now. Called from `write_status`, so it tracks the transport for free.
    pub(super) fn set_busy(&self, busy: bool) {
        self.busy.store(busy, std::sync::atomic::Ordering::Relaxed);
    }

    /// Takes the reader if there is room, and releases it inline if there is
    /// not. A full queue means seeks are arriving faster than COM can tear
    /// readers down; paying the 30ms occasionally beats blocking playback on
    /// a queue that is behind.
    ///
    /// Returns how long the inline release took, `None` when the reader was
    /// queued (t260924-327f). `caller` names the site in the line a slow
    /// release writes to `liveback.log`: the inline branch had no scope at all,
    /// and a hardware decoder's teardown is the one step here that can wait on
    /// the GPU.
    pub(super) fn retire(
        &self,
        reader: Option<SegmentReader>,
        caller: &'static str,
    ) -> Option<std::time::Duration> {
        let reader = reader?;
        // Dropped here, on this thread, at the old cost, if the queue is full.
        let rejected = queue_or_inline(&self.tx, Retiring(reader))?;
        let started = std::time::Instant::now();
        {
            crate::insight_scope!("playback_retire_inline");
            drop(rejected);
        }
        let took = started.elapsed();
        if deserves_a_line(took, SLOW_STEP) {
            tracing::info!(
                target: "task327f_stall",
                stage = "retire_inline",
                caller,
                ms = took.as_millis() as u64,
                "a reader released inline on the playback thread"
            );
        }
        Some(took)
    }
}

/// The retire thread's body: drain and drop, but only while playback is idle.
///
/// Generic over the item so the gating can be tested without Media Foundation.
/// Ends when every sender is gone; while `busy` it only polls, so the bounded
/// channel stays full and senders keep falling back to inline release.
fn retire_loop<T>(rx: &crossbeam_channel::Receiver<T>, busy: &std::sync::atomic::AtomicBool) {
    loop {
        if busy.load(std::sync::atomic::Ordering::Relaxed) {
            // Nothing is taken out of the queue here. `try_recv` on an empty
            // channel is only how a disconnect is noticed without consuming
            // anything -- otherwise this thread would outlive its worker.
            if rx.is_empty()
                && matches!(
                    rx.try_recv(),
                    Err(crossbeam_channel::TryRecvError::Disconnected)
                )
            {
                return;
            }
            std::thread::sleep(RETIRE_IDLE_POLL);
            continue;
        }
        // Each `recv` yields an item that is dropped at the end of the
        // iteration -- the release itself, off the hot path and off the beat.
        match rx.recv_timeout(RETIRE_IDLE_POLL) {
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Queues `item`, or hands it back when there is no room. Split out of
/// `retire` so the full-queue fallback has a test that needs neither Media
/// Foundation nor a real recording.
fn queue_or_inline<T>(tx: &crossbeam_channel::Sender<T>, item: T) -> Option<T> {
    match tx.try_send(item) {
        Err(crossbeam_channel::TrySendError::Full(rejected)) => Some(rejected),
        // Disconnected means the retire thread is gone; releasing inline is the
        // only thing left, and it is also what the old code did.
        Err(crossbeam_channel::TrySendError::Disconnected(rejected)) => Some(rejected),
        Ok(()) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The gating that task246's first measurement forced: a busy transport
    /// keeps its readers queued (so the queue fills and senders fall back to
    /// inline release), and going idle releases everything that piled up.
    #[test]
    fn a_busy_transport_keeps_the_retire_queue_full_until_it_goes_idle() {
        use std::sync::{atomic::AtomicBool, Arc};
        let (tx, rx) = crossbeam_channel::bounded::<u32>(2);
        let busy = Arc::new(AtomicBool::new(true));
        let thread_busy = busy.clone();
        let thread = std::thread::spawn(move || retire_loop(&rx, &thread_busy));
        assert_eq!(queue_or_inline(&tx, 1), None);
        assert_eq!(queue_or_inline(&tx, 2), None);
        // Still busy after a few poll intervals, so nothing was drained and the
        // next reader has to be released by the caller -- the pre-task246 cost,
        // paid at a crossing instead of mid-frame.
        std::thread::sleep(RETIRE_IDLE_POLL * 5);
        assert_eq!(queue_or_inline(&tx, 3), Some(3));
        busy.store(false, std::sync::atomic::Ordering::Relaxed);
        // Idle: the queue drains, and there is room again.
        let mut waited = std::time::Duration::ZERO;
        while !tx.is_empty() && waited < std::time::Duration::from_secs(5) {
            std::thread::sleep(RETIRE_IDLE_POLL);
            waited += RETIRE_IDLE_POLL;
        }
        assert!(tx.is_empty(), "an idle transport releases what piled up");
        assert_eq!(queue_or_inline(&tx, 4), None);
        drop(tx);
        thread
            .join()
            .expect("the retire thread ends with its channel");
    }

    #[test]
    fn a_dead_retire_thread_falls_back_to_inline_too() {
        let (tx, rx) = crossbeam_channel::bounded::<u32>(2);
        drop(rx);
        assert_eq!(queue_or_inline(&tx, 1), Some(1));
    }
}
