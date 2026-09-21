//! Native playback engine for the slint bin (task128). Replaces the WebView
//! build's hand-written MSE streaming engine (`src/playback/playbackEngine.ts`,
//! 1,347 lines): segments are decoded with a Media Foundation Source Reader
//! per segment (they are self-contained fragmented MP4s, task120), video comes
//! out as BGRA via the reader's auto-inserted Video Processor, audio as float
//! PCM into a WASAPI shared-mode render stream. No HTTP shim, no ports.
//!
//! The engine runs on its own thread. The UI sends `PlaybackCommand`s and
//! reads frames from a latest-wins mailbox (`PlaybackShared::frame`): a slow
//! UI drops frames instead of queueing 8MB buffers. `notify` is called after
//! every publish so the UI thread can pull.
//!
//! Master clock is the wall clock scaled by the playback rate, re-anchored on
//! every seek and segment switch (which is also what carries playback across
//! gaps: the next segment simply re-anchors at its own start). Audio follows
//! the clock and is dropped/refilled when it drifts more than 50ms.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{unbounded, Sender};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::capture::CaptureController;
use crate::encoder::MfRuntime;
use crate::ui_state::timeline::{TimelineSegment, TimelineSnapshot};
use source::{ClipSource, ControllerSource, SegmentSource};

/// Segments the lease window pins ahead of the current one.
const LEASE_WINDOW: usize = 4;
/// How much decoded audio is kept queued in the WASAPI buffer.
const AUDIO_BUFFER: Duration = Duration::from_millis(200);
/// Audio behind the master clock by more than this is dropped to resync.
const DRIFT_LIMIT_100NS: i64 = 50 * 10_000;

pub struct PlaybackFrame {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
    /// The same picture before it was resampled down to the stage (task990),
    /// present only when it was. Screenshots save this, and the stage aspect is
    /// measured from it -- the aspect is what the stage sizes itself by, and
    /// deriving it from dimensions that were themselves derived from the stage
    /// closes a loop that never settles.
    pub full: Option<(u32, u32, FullFrame)>,
}

/// The recording-sized picture behind a stage-sized frame.
///
/// It used to always be RGBA, because the CPU path produced recording-sized
/// RGBA on its way to the stage and keeping it cost nothing. The GPU path
/// (t260913-2527) fuses the conversion and the downscale and reads back only
/// the stage, so there is no recording-sized RGBA to keep -- what it keeps
/// instead is the decoder's own NV12, three eighths the bytes, converted on the
/// one occasion anything asks for the pixels: a screenshot, or the paused-stage
/// redraw. Neither is per frame.
pub enum FullFrame {
    Rgba(Vec<u8>),
    /// Tightly packed NV12 (`width` bytes a row, luma then interleaved chroma),
    /// studio-range BT.709 like everything else this app decodes, plus the
    /// conversion once anything has asked for it.
    ///
    /// The cache is why the paused stage can be redrawn on the UI thread at
    /// all: a resize is a drag, so `paused_stage_image` -- the common body
    /// behind both the review stage and the clip screen (t260914-39c4) --
    /// re-reads *the same* frame many times a second, and the conversion is
    /// ~3ms at 1922x1112 -- exactly the work task990 moved off this thread.
    /// Living inside the frame is what makes it safe: there is no
    /// invalidation rule to get wrong, because replacing `last_frame` drops
    /// the cache with the pixels it was made from.
    Nv12(Vec<u8>, std::sync::OnceLock<Vec<u8>>),
}

impl FullFrame {
    /// The decoder's NV12, with an empty conversion cache.
    pub fn nv12(planes: Vec<u8>) -> Self {
        Self::Nv12(planes, std::sync::OnceLock::new())
    }

    /// The recording-sized RGBA rows, converting the NV12 case the first time
    /// and handing back the same bytes after that.
    ///
    /// `width` / `height` describe this frame and never change for it -- they
    /// are the pair `last_frame` keeps beside it -- so they take part in the
    /// first call only.
    pub fn rgba(&self, width: u32, height: u32) -> &[u8] {
        match self {
            Self::Rgba(rgba) => rgba,
            Self::Nv12(nv12, rgba) => {
                rgba.get_or_init(|| livia_pixels::nv12_to_rgba(nv12, width, height, width))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackStatus {
    pub playing: bool,
    pub position_100ns: i64,
    pub at_live_edge: bool,
    pub frames_shown: u64,
    /// Frames published later than one frame interval past their deadline.
    pub frames_late: u64,
    pub actual_fps: u32,
    pub last_seek_ms: u64,
    /// The target of the last seek the *UI* asked for, echoed back once the
    /// worker has run it (task2910). Until it matches what the UI last sent,
    /// `position_100ns` still describes where the transport was before that
    /// seek, and the UI must keep its own position rather than take this one
    /// (`ui_state::playback::owns_position`). Internal seeks -- the poster
    /// frame, play-from-head -- deliberately leave it alone: they answer no
    /// request the UI is waiting on.
    pub acked_seek_100ns: Option<i64>,
    pub drift_corrections: u64,
    pub audio_enabled: bool,
    /// How much audio the WASAPI endpoint still has queued. Non-zero while
    /// playing means the device is actually consuming what was written.
    pub audio_queued_ms: u32,
    /// Interleaved f32 frames handed to the endpoint since the last flush.
    pub audio_frames_written: u64,
    /// How many audio tracks the segment being played carries (task1270): 1
    /// for every recording made before multi-track audio, 0 for one with no
    /// audio at all. Their names live in `SessionManifest::audio_tracks` --
    /// the engine only ever knows how many there are.
    pub audio_tracks: usize,
}

pub enum PlaybackCommand {
    Play,
    Pause,
    Seek {
        target_100ns: i64,
        precise: bool,
    },
    SetRate(f64),
    SetVolume {
        percent: i64,
        muted: bool,
    },
    /// One audio track's own level, on top of the master `SetVolume`
    /// (task1270). Track 0 is the capture target; the rest are the extra
    /// applications, in the order `SessionManifest::audio_tracks` lists them.
    ///
    /// Session-lived on purpose: nothing persists it, so a recording sounds
    /// the way it was recorded the next time it is opened.
    SetTrackVolume {
        track: usize,
        percent: i64,
        muted: bool,
    },
    /// Appends segments finalized after the engine started, and moves the live
    /// edge with them (task161). Push-only: the UI's 100ms drain sends the
    /// difference, so the worker thread stays the only owner of the playlist
    /// and never has to reach back into the controller for a timeline.
    ExtendPlaylist {
        segments: Vec<TimelineSegment>,
        live_edge_100ns: i64,
    },
    Shutdown,
}

#[derive(Default)]
pub struct PlaybackShared {
    pub frame: Mutex<Option<PlaybackFrame>>,
    pub status: Mutex<PlaybackStatus>,
    /// Physical pixels the stage is drawing the picture into, packed
    /// `width << 32 | height` (task990). Zero means "publish at full
    /// resolution".
    ///
    /// Written first by `PlaybackEngine::start_with`, before the worker thread
    /// exists (t260915-c1ca), so the poster frame `Worker::new` decodes already
    /// sees it; after that by the UI whenever the stage is resized. Still zero
    /// for the first session of a process: no stage has been laid out yet when
    /// that engine is created.
    ///
    /// An atomic rather than another mutex: the worker reads it once per frame
    /// and the UI writes it while dragging a window edge, and neither has
    /// anything to gain from blocking the other over eight bytes.
    display_target: AtomicU64,
    /// How long one display refresh lasts, in nanoseconds (t260913-3842).
    /// Zero -- the value a `Default` engine starts at -- means "no answer",
    /// and the worker then shows every frame exactly as it did before.
    ///
    /// Same reasoning as `display_target` for the atomic: written once per
    /// engine by the UI thread, read once per frame by the worker.
    display_period_ns: AtomicU64,
    /// The last recording-sized buffer the UI finished with, on its way back to
    /// the engine to be written again (task1640). A fresh `Vec` costs the engine
    /// thread page faults it does not pay writing one it has already touched:
    /// 1171us against 428us, measured in-app on a 7.8MB frame (8.3MB at 1080p).
    ///
    /// Ownership, which is the whole safety argument: the UI puts a buffer here
    /// only at the moment it *drops* its own last reference to it, so anything
    /// the engine takes out is a buffer nobody else can read -- not slint (
    /// `image_from_rgba` copies into its own `SharedPixelBuffer`) and not the
    /// screenshot path (it reads `Review::last_frame`, which owns its `Vec`
    /// until the frame after it replaces it).
    ///
    /// RGBA only, which is why there are three slots and not two: the full
    /// frame comes in two variants of different sizes (`FullFrame`), and
    /// `take_fitting` drops what does not fit, so one slot serving both hands
    /// the next taker a misfit whenever the path switches (t260914-862b).
    spare: Mutex<Option<Vec<u8>>>,
    /// The same channel for the NV12 planes the GPU path publishes
    /// (t260914-862b), kept apart from `spare` for the reason written there.
    /// The switch is rare -- a failed blit, or the stage crossing the size
    /// where scaling is worth it -- but each one cost both buffers before.
    nv12_spare: Mutex<Option<Vec<u8>>>,
    /// The same channel for the *stage-sized* buffer the frame was resampled
    /// into (task1690). A separate slot rather than a second tenant of `spare`:
    /// the two sizes differ, and `take_fitting` drops what does not fit, so one
    /// slot serving both would throw away whichever buffer arrived last. Only
    /// one variant reaches this one -- the stage is always RGBA.
    ///
    /// Same ownership argument as `spare`, one step earlier: the UI puts the
    /// resampled buffer here only after `image_from_rgba` has copied it into
    /// slint's own `SharedPixelBuffer`, at the moment it drops its only
    /// reference. Screenshots never read this one at all -- they read
    /// `Review::last_frame`, which is the full-resolution picture.
    scaled_spare: Mutex<Option<Vec<u8>>>,
}

impl PlaybackShared {
    /// Tells the engine how big the picture is actually being drawn, in
    /// physical pixels. `None` puts it back to full resolution.
    pub fn set_display_target(&self, target: Option<(u32, u32)>) {
        let packed = target.map_or(0, |(width, height)| {
            (u64::from(width) << 32) | u64::from(height)
        });
        self.display_target.store(packed, Ordering::Relaxed);
    }

    pub fn display_target(&self) -> Option<(u32, u32)> {
        match self.display_target.load(Ordering::Relaxed) {
            0 => None,
            packed => Some(((packed >> 32) as u32, packed as u32)),
        }
    }

    /// Tells the engine how often the display refreshes, so it can stop
    /// building frames that will never reach a refresh (t260913-3842). An
    /// implausible `hz` -- `display_refresh_hz` answers 0 when Windows will
    /// not say -- leaves the engine converting every frame, because a period
    /// that is wrong the other way freezes the picture.
    ///
    /// The range is `ui_state::playback::REFRESH_HZ_RANGE` itself, which gates
    /// `StageGate` for the same reason -- one definition, not two.
    pub fn set_display_refresh_hz(&self, hz: u32) {
        let period_ns = if crate::ui_state::playback::REFRESH_HZ_RANGE.contains(&hz) {
            1_000_000_000 / u64::from(hz)
        } else {
            0
        };
        self.display_period_ns.store(period_ns, Ordering::Relaxed);
    }

    pub fn display_period(&self) -> Option<Duration> {
        match self.display_period_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Duration::from_nanos(ns)),
        }
    }

    /// Hands a recording-sized *RGBA* buffer the UI is done with back to the
    /// engine (task1640). Latest-wins like the frame slot, and never blocks: a
    /// UI that cannot take the lock just frees the buffer as it did before, and
    /// the engine allocates the next one. NV12 planes go to
    /// [`recycle_nv12`](Self::recycle_nv12) instead, and a `FullFrame` whose
    /// variant is not known to the caller to
    /// [`recycle_full`](Self::recycle_full).
    pub fn recycle(&self, buffer: Vec<u8>) {
        if let Ok(mut slot) = self.spare.try_lock() {
            *slot = Some(buffer);
        }
    }

    /// A returned buffer of exactly `bytes`, or `None` to allocate one. Never
    /// waits: a frame is due, and a spare is an optimization.
    pub fn take_spare(&self, bytes: usize) -> Option<Vec<u8>> {
        let mut slot = self.spare.try_lock().ok()?;
        take_fitting(&mut slot, bytes)
    }

    /// Hands a finished full frame back to the slot its variant belongs in
    /// (t260914-862b). The one door for the UI and for frames the UI never
    /// took: the variant decides, so neither caller has to know that the two
    /// sizes are not interchangeable -- a slot returns a buffer of exactly the
    /// size asked for, so a buffer put in the wrong one is dropped on the next
    /// take rather than reused.
    pub fn recycle_full(&self, full: FullFrame) {
        match full {
            FullFrame::Rgba(buffer) => self.recycle(buffer),
            FullFrame::Nv12(buffer, _) => self.recycle_nv12(buffer),
        }
    }

    /// [`recycle`](Self::recycle) for the NV12 planes (t260914-862b). Taken
    /// directly by the GPU path, which holds the planes as a bare `Vec` before
    /// a frame is ever built around them.
    pub fn recycle_nv12(&self, buffer: Vec<u8>) {
        if let Ok(mut slot) = self.nv12_spare.try_lock() {
            *slot = Some(buffer);
        }
    }

    /// [`take_spare`](Self::take_spare) for the NV12 planes (t260914-862b).
    pub fn take_nv12_spare(&self, bytes: usize) -> Option<Vec<u8>> {
        let mut slot = self.nv12_spare.try_lock().ok()?;
        take_fitting(&mut slot, bytes)
    }

    /// [`recycle`](Self::recycle) for the stage-sized buffer (task1690).
    pub fn recycle_scaled(&self, buffer: Vec<u8>) {
        if let Ok(mut slot) = self.scaled_spare.try_lock() {
            *slot = Some(buffer);
        }
    }

    /// [`take_spare`](Self::take_spare) for the stage-sized buffer (task1690).
    pub fn take_scaled_spare(&self, bytes: usize) -> Option<Vec<u8>> {
        let mut slot = self.scaled_spare.try_lock().ok()?;
        take_fitting(&mut slot, bytes)
    }

    /// Empties all three spare slots and reports how many bytes each one gave
    /// back, in the order `spare`, `nv12_spare`, `scaled_spare`
    /// (t260917-efbe).
    ///
    /// `take_fitting` already drops a misfit -- but only when something
    /// *takes*, and nothing takes while the engine publishes nothing: a paused
    /// 4K session, or one parked at the live edge, sat on ~23-56MB of
    /// recording- and stage-sized buffers for as long as it stayed loaded. The
    /// worker calls this once it has been quiet for `worker::SPARE_IDLE`;
    /// the three slots stay three (t260914-862b, task1690), this only clears
    /// them. Never blocks, like every other door here: a slot whose lock is
    /// busy is being refilled by a publish, which is not idle.
    pub fn release_spares(&self) -> [usize; 3] {
        let release = |slot: &Mutex<Option<Vec<u8>>>| {
            slot.try_lock()
                .ok()
                .and_then(|mut slot| slot.take())
                .map_or(0, |buffer| buffer.len())
        };
        [
            release(&self.spare),
            release(&self.nv12_spare),
            release(&self.scaled_spare),
        ]
    }
}

/// Which returned buffer the engine may write the next frame into: only one of
/// exactly the right size.
///
/// A misfit is dropped rather than left in the slot -- the frame size changes
/// when the recording does, so a spare that no longer fits never will, and
/// keeping it would block every later frame from being recycled.
fn take_fitting(slot: &mut Option<Vec<u8>>, bytes: usize) -> Option<Vec<u8>> {
    slot.take().filter(|spare| spare.len() == bytes)
}

/// How many `PlaybackEngine`s are alive right now (task4280).
///
/// Starting a recording loads the review from every door (`picker.rs`'s
/// `spawn_start_capture` sends `Cmd::LoadStarted`), so one record->stop round
/// stands up one engine and its decoder MFT whether or not anyone opened the
/// review. `load_review`'s `review.engine = Some(..)` is supposed to drop the
/// previous one on the UI thread; this counter is how that gets asserted rather
/// than assumed:
///
/// - **stays at 1 across rounds** -> the replace works; the fixed ~20.5 MB/round
///   4210 measured is not a pile of live engines, so look at the encoder.
/// - **grows with the rounds** -> the old engine is still held and the decoder
///   side owns the leak.
///
/// The decrement is after the worker join, so a log line here means the thread
/// is gone too -- not merely that the handle was dropped.
static ENGINES_ALIVE: AtomicUsize = AtomicUsize::new(0);

/// [`ENGINES_ALIVE`]'s current value, for a log line that has to say whether
/// the engine it describes is the only one (t260917-efbe).
pub fn live_engines() -> usize {
    ENGINES_ALIVE.load(Ordering::Relaxed)
}

pub struct PlaybackEngine {
    tx: Sender<PlaybackCommand>,
    /// Nothing is ever sent on it; only its *disconnect* carries meaning
    /// (task1620). `Drop` releases it before the join, which is what wakes the
    /// worker out of a frame-deadline sleep -- see `worker::nap`.
    stop_tx: Option<Sender<()>>,
    shared: Arc<PlaybackShared>,
    alive: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlaybackEngine {
    pub fn start(
        controller: CaptureController,
        snapshot: TimelineSnapshot,
        initial_position_100ns: i64,
        volume_percent: i64,
        muted: bool,
        display_target: Option<(u32, u32)>,
        notify: Box<dyn Fn() + Send>,
    ) -> Self {
        Self::start_with(
            Arc::new(ControllerSource::new(controller)),
            snapshot,
            initial_position_100ns,
            volume_percent,
            muted,
            display_target,
            notify,
        )
    }

    /// One exported clip mp4, played by the same engine as the review screen
    /// (task3500).
    ///
    /// The clip is a one-segment timeline: nothing prunes it, nothing extends
    /// it, and there is no live edge past its end. `duration_100ns` is what the
    /// shell already knows about the file (`clips::details`); a zero or missing
    /// duration would leave the transport with nowhere to seek, so the caller
    /// passes the real one.
    ///
    /// MF reads the file directly -- the bytes are never copied into the
    /// process, which is the whole reason the seam is an `IMFByteStream`
    /// (task3480 (d): a 3-minute clip of a 100Mbps recording is ~2.2GB).
    pub fn start_file(
        path: std::path::PathBuf,
        duration_100ns: i64,
        initial_position_100ns: i64,
        volume_percent: i64,
        muted: bool,
        display_target: Option<(u32, u32)>,
        notify: Box<dyn Fn() + Send>,
    ) -> Self {
        let snapshot = TimelineSnapshot {
            session_id: format!("clip:{}", path.display()),
            segments: vec![TimelineSegment {
                index: 0,
                start_100ns: 0,
                end_100ns: duration_100ns,
                audio_offsets_100ns: Vec::new(),
            }],
            gaps: Vec::new(),
            live_edge_100ns: duration_100ns,
            target_title: None,
            target_executable: None,
            target_executable_path: None,
            audio_tracks: Vec::new(),
        };
        Self::start_with(
            Arc::new(ClipSource::new(path)),
            snapshot,
            initial_position_100ns,
            volume_percent,
            muted,
            display_target,
            notify,
        )
    }

    fn start_with(
        source: Arc<dyn SegmentSource>,
        snapshot: TimelineSnapshot,
        initial_position_100ns: i64,
        volume_percent: i64,
        muted: bool,
        display_target: Option<(u32, u32)>,
        notify: Box<dyn Fn() + Send>,
    ) -> Self {
        let (tx, rx) = unbounded();
        let (stop_tx, stop_rx) = unbounded();
        let shared = Arc::new(PlaybackShared::default());
        // Before the spawn, not after it (t260915-c1ca). The worker's first
        // act is the poster frame (`Worker::new`'s `do_seek`), and a store the
        // caller made once `spawn` had returned only usually beat it: the
        // atomic is `Relaxed` and there is no handshake. Stored here, the
        // value is simply there before the thread that reads it exists.
        shared.set_display_target(display_target);
        let alive = Arc::new(AtomicBool::new(true));
        let worker_shared = shared.clone();
        let worker_alive = alive.clone();
        let thread = thread::Builder::new()
            .name("playback-engine".into())
            .spawn(move || {
                // Cleared on *every* exit -- normal return, MF startup
                // failure, or a panic anywhere in the worker. `is_alive` is
                // how the UI detects a dead engine, so an early `return` must
                // not leave it stuck true.
                struct AliveGuard(Arc<AtomicBool>);
                impl Drop for AliveGuard {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _alive_guard = AliveGuard(worker_alive);
                unsafe {
                    // MF and WASAPI both live on this thread for the engine's
                    // lifetime; MTA keeps their proxies happy off the UI thread.
                    let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
                }
                let _runtime = match MfRuntime::start() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(?error, "playback: MF startup failed");
                        return;
                    }
                };
                let mut worker = Worker::new(
                    source,
                    snapshot,
                    initial_position_100ns,
                    volume_percent,
                    muted,
                    worker_shared,
                    notify,
                    stop_rx,
                );
                worker.run(rx);
            })
            .expect("playback engine thread");
        let engines_alive = ENGINES_ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            event = "playback_engine_started",
            alive = engines_alive,
            "playback engine started"
        );
        Self {
            tx,
            stop_tx: Some(stop_tx),
            shared,
            alive,
            thread: Some(thread),
        }
    }

    pub fn send(&self, command: PlaybackCommand) {
        let _ = self.tx.send(command);
    }

    /// Grows a live session's playlist while it plays (task161). Segments at or
    /// before the current tail are ignored, so the caller may resend.
    pub fn extend_playlist(&self, segments: Vec<TimelineSegment>, live_edge_100ns: i64) {
        self.send(PlaybackCommand::ExtendPlaylist {
            segments,
            live_edge_100ns,
        });
    }

    pub fn shared(&self) -> Arc<PlaybackShared> {
        self.shared.clone()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        // The join stays synchronous on purpose: the worker releases the review
        // lease on its way out, and three callers depend on that having
        // happened by the time they return -- `unload_review`,
        // `sessions_confirm_accepted`, and through them
        // `CaptureController::discard_session`'s `has_leases` refusal
        // (task1520).
        //
        // What task1620 added is the line above it. `load_review` drops the old
        // engine on the *UI thread*, so anything the worker can wait on
        // unboundedly hangs the whole window -- and `tick` waits on the next
        // frame's deadline, which a stale master-clock anchor can put hours
        // out (2026-08-25: a fresh LiveReview load anchored at media 0 while
        // its segments carried ~9.6h absolute timestamps, and opening the same
        // session from 履歴 froze the app until it was killed). Releasing the
        // sender disconnects the channel `nap` waits on, so every deadline
        // sleep returns at once and the join costs one tick's remaining work.
        self.stop_tx = None;
        let _ = self.tx.send(PlaybackCommand::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // After the join on purpose (task4280): the line means the worker thread
        // is gone, not just that the handle was dropped.
        let engines_alive = ENGINES_ALIVE.fetch_sub(1, Ordering::Relaxed) - 1;
        tracing::info!(
            event = "playback_engine_dropped",
            alive = engines_alive,
            "playback engine dropped"
        );
    }
}

mod audio_out;
mod fetch_gate;
mod gpu;

pub mod scale;

mod mixer;
mod segment_reader;
mod source;

mod stretcher;

mod worker;
use worker::Worker;

#[cfg(test)]
mod tests;
