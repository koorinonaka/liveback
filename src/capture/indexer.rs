//! The single writer of a recording's index (task430).
//!
//! Before this, a session had two writers: the capture worker wrote the segment
//! mp4s and their thumbnails, and the **UI thread** wrote `manifest.json` --
//! `diagnostics()` called `sync_ring`, which took `sessions` and ran
//! `add`/`merge_markers`/`prune` (each of which persists) inside that lock while
//! still holding `active`. That split is what let "the segment is on disk but
//! not in the manifest" exist at all, and a container appended in one file
//! cannot have two writers at all (計画書 R4 / §3.3).
//!
//! So the writes move here: one thread per recording, fed straight from the
//! capture worker, owning every index mutation for the session it was spawned
//! for. The UI keeps reading `sessions`; it no longer writes through it.
//!
//! Nor does it wait on this thread's disk I/O: a pass holds `sessions` only
//! while it touches the ring, and appends, prunes and checkpoints the
//! container outside that lock (t260925-dea6; see `SegmentIndexer::write`).

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::{encoder, ring_buffer};

use crate::encoder::exclusive_segment_end;

/// How long the loop waits for a segment before looking at the marker queue
/// anyway. A marker pressed between two segments used to be merged by the next
/// `diagnostics()` poll (~1s); waiting for the next segment instead would hold
/// it for up to the segment length (2s), which the timeline would show as a
/// marker that lands late.
const MARKER_POLL: Duration = Duration::from_millis(500);

#[cfg(test)]
thread_local! {
    /// Runs at each container I/O point of a pass (t260925-dea6), so a test can
    /// look at who holds `sessions` while the bytes are being written.
    static DURING_IO: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn during_io() {
    DURING_IO.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn during_io() {}

/// How many finalized segments of each recording [`RecentSegments`] keeps.
const RECENT_SEGMENTS: usize = 3;

/// The newest finalized segments of each recording still running, kept in
/// memory so live follow plays them without reading the container back
/// (t261012-5b97). On an HDD buffer that read took 1.8 s at p99, which is what
/// the playback reserve and the two-segment resume lead were sized around;
/// with the bytes here, neither is needed.
///
/// Per session, because two recordings can run at once (task2030). The index
/// writer puts a segment here *before* appending it, so by the time the
/// timeline grows to show it -- that waits for the append -- it is here.
/// The bytes are the very ones the container gets, so a reader cannot tell
/// which of the two it was served from. About 9MB a session at the default
/// 12Mbps and 2 s segments.
#[derive(Default)]
pub(super) struct RecentSegments(Mutex<HashMap<String, RecentOfOne>>);

/// One recording's newest segments, oldest first: `(index, bytes)`.
type RecentOfOne = VecDeque<(u64, Arc<Vec<u8>>)>;

impl RecentSegments {
    pub(super) fn put(&self, session_id: &str, index: u64, bytes: Arc<Vec<u8>>) {
        let Ok(mut sessions) = self.0.lock() else {
            return;
        };
        let recent = sessions.entry(session_id.to_owned()).or_default();
        recent.push_back((index, bytes));
        while recent.len() > RECENT_SEGMENTS {
            recent.pop_front();
        }
    }

    pub(super) fn get(&self, session_id: &str, index: u64) -> Option<Arc<Vec<u8>>> {
        let sessions = self.0.lock().ok()?;
        sessions
            .get(session_id)?
            .iter()
            .find(|(cached, _)| *cached == index)
            .map(|(_, bytes)| bytes.clone())
    }

    /// The recording ended: its segments are read from the container again.
    pub(super) fn forget(&self, session_id: &str) {
        if let Ok(mut sessions) = self.0.lock() {
            sessions.remove(session_id);
        }
    }
}

/// What reaches the writer thread.
///
/// Two senders now (task450): the capture worker, which finalizes segments, and
/// the thumbnail writer, which used to publish its own JPEG into the session
/// directory. A container cannot have two writers, so the thumbnail bytes
/// come here instead and the one thread that owns the `ContainerWriter` appends
/// both.
pub enum IndexEvent {
    /// The capture worker always fills `bytes`; one without is not indexed.
    Segment(Box<encoder::VideoSegmentMetadata>),
    /// An encoded segment thumbnail.
    Thumbnail(u64, Vec<u8>),
}

/// Everything the writer thread needs. Held by value in the thread, so the
/// session id can never drift from the ring it writes to -- unlike `sync_ring`,
/// which re-read `active_session` on every call.
pub(super) struct SegmentIndexer {
    pub(super) session_id: String,
    pub(super) sessions: Arc<Mutex<HashMap<String, ring_buffer::RingBuffer>>>,
    pub(super) pending_markers: Arc<Mutex<Vec<ring_buffer::MarkerRecord>>>,
    /// Note/protection edits made on the session being recorded (task720).
    /// They arrive here rather than being written where they were made,
    /// because this thread holds the only writer allowed near that `.lvb`.
    pub(super) pending_meta: Arc<Mutex<Vec<ring_buffer::container::MetaEvent>>>,
    pub(super) last_timeline: Arc<Mutex<Option<ring_buffer::SessionManifest>>>,
    /// This thread is the **only** thing in the process that writes to the
    /// recording's `.lvb`.
    pub(super) writer: ring_buffer::container::ContainerWriter,
    /// Thumbnails whose segment has not finalized yet. See `write`.
    pub(super) pending_thumbnails: HashMap<u64, Vec<u8>>,
    /// Shared with `CaptureController`, which serves review reads from it.
    pub(super) recent: Arc<RecentSegments>,
}

impl SegmentIndexer {
    /// Runs the writer until `segments` closes, which happens when the capture
    /// worker (the only sender) ends. The join handle is the caller's proof
    /// that every finalized segment is in the manifest -- `close` must not run
    /// before it.
    pub(super) fn spawn(
        self,
        segments: Receiver<IndexEvent>,
    ) -> std::io::Result<thread::JoinHandle<()>> {
        thread::Builder::new()
            .name("livia-index".into())
            .spawn(move || self.run(segments))
    }

    fn run(mut self, segments: Receiver<IndexEvent>) {
        loop {
            match segments.recv_timeout(MARKER_POLL) {
                Ok(event) => {
                    // Whatever else arrived while the last write was in flight
                    // goes in the same pass: one lock, one persist.
                    let mut batch = vec![event];
                    batch.extend(segments.try_iter());
                    self.write(batch);
                }
                Err(RecvTimeoutError::Timeout) => self.write(Vec::new()),
                // The worker is gone. Anything it managed to send before that
                // is still queued, and a marker pressed in the last instant is
                // still in `pending_markers`; both belong in the manifest.
                Err(RecvTimeoutError::Disconnected) => {
                    let rest = segments.try_iter().collect();
                    self.write(rest);
                    self.finish();
                    return;
                }
            }
        }
    }

    /// Appends one finalized segment (and its thumbnail, if it arrived) to the
    /// container, and describes it the way the index view wants it.
    ///
    /// `path`/`thumbnail_path` stay empty on purpose: inside a container the
    /// *index* is the address, which is the same shape `RingBuffer::open_container`
    /// builds when it reads a closed one back.
    fn append_to_container(
        writer: &mut ring_buffer::container::ContainerWriter,
        segment: &encoder::VideoSegmentMetadata,
        thumbnail: Option<Vec<u8>>,
    ) -> std::io::Result<(ring_buffer::SegmentRecord, ring_buffer::ContainerSpans)> {
        during_io();
        let body = segment.bytes.as_deref().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a container session finalized a segment without its bytes",
            )
        })?;
        let data = writer.append_segment(
            segment.index,
            segment.start_timestamp_100ns,
            exclusive_segment_end(segment.end_timestamp_100ns, segment.fps),
            &segment.audio_offsets_100ns,
            body,
        )?;
        let mut thumbnail_path = None;
        let mut thumbnail_span = None;
        if let Some(jpeg) = thumbnail {
            match writer.append_thumbnail(segment.index, &jpeg) {
                Ok(Some(located)) => {
                    thumbnail_path = Some(std::path::PathBuf::new());
                    thumbnail_span = Some(located);
                }
                // The segment was just appended, so `None` here would mean the
                // index disagrees with itself; log rather than pretend.
                Ok(None) => tracing::warn!(
                    event = "container_thumbnail_orphaned",
                    index = segment.index,
                    "the thumbnail found no segment to attach to"
                ),
                Err(error) => tracing::warn!(
                    event = "container_thumbnail_failed",
                    index = segment.index,
                    %error,
                ),
            }
        }
        Ok((
            ring_buffer::SegmentRecord {
                index: segment.index,
                path: std::path::PathBuf::new(),
                start_100ns: segment.start_timestamp_100ns,
                end_100ns: exclusive_segment_end(segment.end_timestamp_100ns, segment.fps),
                bytes: body.len() as u64,
                thumbnail_path,
                audio_offsets_100ns: segment.audio_offsets_100ns.clone(),
            },
            ring_buffer::ContainerSpans {
                data,
                thumbnail: thumbnail_span,
            },
        ))
    }

    /// The clean end of a container recording: `Closed` plus a final
    /// checkpoint, written here because this thread owns the writer. Runs after
    /// the last batch, so everything the worker sent is already appended.
    fn finish(self) {
        self.recent.forget(&self.session_id);
        if let Err(error) = self.writer.close() {
            tracing::error!(
                event = "container_close_failed",
                %error,
                "the container could not be closed; it will be recovered on the next launch"
            );
        }
    }

    /// Appends note, protection and marker edits to the container, answering
    /// whether any of them landed.
    fn append_meta_events(
        writer: &mut ring_buffer::container::ContainerWriter,
        events: &[ring_buffer::container::MetaEvent],
        failure_event: &'static str,
        failure_message: &'static str,
    ) -> bool {
        let mut written = false;
        for event in events {
            match writer.append_meta(event) {
                Ok(()) => written = true,
                Err(error) => tracing::error!(event = failure_event, %error, "{failure_message}"),
            }
        }
        written
    }

    /// Drops from the ring what retention no longer keeps, and answers the
    /// first index kept when anything went. Runs under `sessions`; the
    /// container's hole punch for that answer is `prune_container`, outside it.
    fn prune_ring(ring: &mut ring_buffer::RingBuffer) -> Option<u64> {
        ring.prune().unwrap_or_else(|error| {
            tracing::error!(event = "ring_prune_failed", %error, "ring buffer prune failed");
            None
        })
    }

    /// A container gives its space back by punching a hole rather than by
    /// unlinking files, and only this thread holds the writer that can do it.
    /// `ContainerWriter::prune` may write a checkpoint of its own (task3460),
    /// on the same interval as the pass's (t261012-fe57), so a pass that
    /// prunes writes at most one between them.
    ///
    /// It runs after the ring already dropped these segments, so a reader can
    /// no longer newly resolve a span into the hole.
    fn prune_container(writer: &mut ring_buffer::container::ContainerWriter, keep_from_index: u64) {
        during_io();
        if let Err(error) = writer.prune(keep_from_index) {
            tracing::error!(event = "container_prune_failed", keep_from_index, %error);
        }
    }

    /// One pass: take what has queued, put the new segments in the container
    /// and the ring, merge the markers, prune and checkpoint, then publish the
    /// manifest.
    ///
    /// `sessions` is held only for the ring's own state: which segments are
    /// new, adding them, merging markers, deciding what to prune, copying the
    /// manifest. Every container write -- segment bytes, meta records, hole
    /// punch, checkpoint -- runs outside it (t260925-dea6), because the UI
    /// thread and playback take `sessions` for every timeline read, and a heavy
    /// recording's append held it for 9.6s (2026-09-24). Two locks per pass at
    /// most. Nothing but this thread adds to this ring, and the stop path joins
    /// it before `close`, so what the first lock saw still holds at the second.
    fn write(&mut self, batch: Vec<IndexEvent>) {
        let markers = take_queued(&self.pending_markers);
        let meta = take_queued(&self.pending_meta);
        if batch.is_empty() && markers.is_empty() && meta.is_empty() {
            return;
        }
        // Meta edits go straight to the writer, not through the ring: the
        // ring's copy was already updated where the edit was made
        // (`RingBuffer::stage_comment`), and this thread is the only one allowed
        // to touch the file. Done before the `sessions` lock so an edit is not
        // lost if the ring has since gone.
        let mut meta_written = Self::append_meta_events(
            &mut self.writer,
            &meta,
            "container_meta_append_failed",
            "a note or protection edit could not be appended to the container",
        );
        let mut segments = self.split_batch(batch);
        let segment_count = segments.len();
        let mut timing = PassTiming::default();
        let Some(appended) = self.append_new_segments(&mut segments, &mut timing) else {
            return;
        };
        let Some(pass) = self.update_ring(appended, markers, meta_written, &mut timing) else {
            return;
        };
        // The container's copy of a marker is this append and nothing else
        // (task1630): the ring writes no file, and this thread is the only
        // one allowed to touch the `.lvb`. Same
        // rail as the note/protection edits above -- they just had one and
        // markers did not, so every hotkey marker died with the process.
        meta_written |= Self::append_meta_events(
            &mut self.writer,
            &pass.marker_events,
            "container_marker_append_failed",
            "a marker could not be appended to the container",
        );
        let checkpoints = self.prune_and_checkpoint(
            pass.keep_from_index,
            pass.changed || meta_written,
            &mut timing,
        );
        timing.report(segment_count, checkpoints, pass.keep_from_index.is_some());
        if let Some(manifest) = pass.manifest {
            if let Ok(mut timeline) = self.last_timeline.lock() {
                *timeline = Some(manifest);
            }
        }
    }

    /// Splits a batch into its segments and parks its thumbnails. Before the
    /// lock: appending a thumbnail needs its segment to already be in the
    /// container's index (`append_thumbnail` silently drops one for an unknown
    /// index), and thumbnails are captured when a segment *opens* -- well
    /// before it finalizes. So they wait here and go in right after the
    /// segment they belong to.
    fn split_batch(&mut self, batch: Vec<IndexEvent>) -> Vec<encoder::VideoSegmentMetadata> {
        let mut segments = Vec::new();
        for event in batch {
            match event {
                IndexEvent::Segment(segment) => segments.push(*segment),
                IndexEvent::Thumbnail(index, jpeg) => {
                    self.pending_thumbnails.insert(index, jpeg);
                }
            }
        }
        segments
    }

    /// Picks the new segments under a short lock and appends them without it.
    /// `None` when the ring has gone (the pass ends there).
    fn append_new_segments(
        &mut self,
        segments: &mut Vec<encoder::VideoSegmentMetadata>,
        timing: &mut PassTiming,
    ) -> Option<
        Vec<(
            u64,
            (ring_buffer::SegmentRecord, ring_buffer::ContainerSpans),
        )>,
    > {
        let mut appended = Vec::new();
        if !segments.is_empty() {
            let waiting = Instant::now();
            let sessions = self.sessions.lock().ok()?;
            timing.lock_wait_ms += waiting.elapsed().as_millis() as u64;
            let locked = Instant::now();
            let ring = sessions.get(&self.session_id)?;
            segments.retain(|segment| !ring.contains(segment.index));
            drop(sessions);
            timing.locked_ms += locked.elapsed().as_millis() as u64;
        }
        // Before the append, which is what the timeline's growth waits on
        // (t261012-5b97): a segment the UI can see is always here.
        for segment in segments.iter() {
            if let Some(bytes) = &segment.bytes {
                self.recent
                    .put(&self.session_id, segment.index, Arc::new(bytes.clone()));
            }
        }
        let appending = Instant::now();
        for segment in segments.iter() {
            match Self::append_to_container(
                &mut self.writer,
                segment,
                self.pending_thumbnails.remove(&segment.index),
            ) {
                Ok(located) => appended.push((segment.index, located)),
                Err(error) => tracing::error!(
                    event = "container_append_failed",
                    index = segment.index,
                    %error,
                    "the finalized segment could not be appended to the container"
                ),
            }
        }
        timing.append_ms += appending.elapsed().as_millis() as u64;
        Some(appended)
    }

    /// The pass's one stretch under the `sessions` lock: add the segments to
    /// the ring, merge the markers, decide what to prune and copy the
    /// manifest. `None` when the ring has gone.
    fn update_ring(
        &mut self,
        appended: Vec<(
            u64,
            (ring_buffer::SegmentRecord, ring_buffer::ContainerSpans),
        )>,
        markers: Vec<ring_buffer::MarkerRecord>,
        meta_written: bool,
        timing: &mut PassTiming,
    ) -> Option<RingPass> {
        let waiting = Instant::now();
        let mut sessions = self.sessions.lock().ok()?;
        timing.lock_wait_ms += waiting.elapsed().as_millis() as u64;
        let locked = Instant::now();
        let ring = sessions.get_mut(&self.session_id)?;
        let mut changed = false;
        for (index, (record, spans)) in appended {
            match ring.add_container_segment(record, spans) {
                Ok(()) => changed = true,
                Err(error) => tracing::error!(
                    event = "segment_index_failed",
                    index,
                    %error,
                    "the finalized segment could not be written to the index"
                ),
            }
        }
        let markers_merged = !markers.is_empty();
        let marker_events = ring.merge_markers(markers).unwrap_or_else(|error| {
            tracing::error!(event = "marker_merge_failed", %error, "marker merge failed");
            Vec::new()
        });
        let keep_from_index = if changed {
            Self::prune_ring(ring)
        } else {
            None
        };
        // A marker alone changes the manifest even when no segment finalized
        // this pass, and the frontend converges on `get_timeline`.
        let manifest = (changed || markers_merged || meta_written).then(|| ring.manifest().clone());
        drop(sessions);
        timing.locked_ms += locked.elapsed().as_millis() as u64;
        Some(RingPass {
            changed,
            marker_events,
            keep_from_index,
            manifest,
        })
    }

    /// After the lock: the container prune and the pass's checkpoint. Returns
    /// how many checkpoints were written.
    fn prune_and_checkpoint(
        &mut self,
        keep_from_index: Option<u64>,
        wrote: bool,
        timing: &mut PassTiming,
    ) -> u64 {
        // `prune_ms` covers the `Pruned` meta record, the hole punch and the
        // checkpoint `ContainerWriter::prune` writes for itself when one is due.
        let generation = self.writer.checkpoint_generation();
        let pruning = Instant::now();
        if let Some(keep_from_index) = keep_from_index {
            Self::prune_container(&mut self.writer, keep_from_index);
        }
        timing.prune_ms = pruning.elapsed().as_millis() as u64;
        // The pass's checkpoint, after everything this batch appended: it is
        // what a crash recovers to, so it goes last and covers the whole pass.
        // Only once per `CHECKPOINT_INTERVAL_100NS` of media (t261012-fe57): a
        // checkpoint is the whole snapshot, and one per segment kept the
        // ring's square allocated in the window. After a `prune` nothing new
        // was appended, so this writes only if the prune did not.
        let checkpointing = Instant::now();
        if wrote {
            during_io();
            if let Err(error) = self.writer.checkpoint_if_due() {
                tracing::error!(event = "container_checkpoint_failed", %error);
            }
        }
        timing.checkpoint_ms = checkpointing.elapsed().as_millis() as u64;
        // Written, not attempted: Verification reads this column for the
        // cadence.
        self.writer
            .checkpoint_generation()
            .saturating_sub(generation)
    }
}

/// Empties one of the controller's queues for this pass.
fn take_queued<T>(queue: &Mutex<Vec<T>>) -> Vec<T> {
    queue
        .lock()
        .map(|mut queue| std::mem::take(&mut *queue))
        .unwrap_or_default()
}

/// What the locked stretch of a pass decided.
struct RingPass {
    changed: bool,
    marker_events: Vec<ring_buffer::container::MetaEvent>,
    keep_from_index: Option<u64>,
    manifest: Option<ring_buffer::SessionManifest>,
}

/// The `task3460_index_pass` columns: `lock_wait_ms` / `locked_ms` are the
/// sums over both locks; `append_ms` / `prune_ms` / `checkpoint_ms` are the
/// container I/O, outside the lock (a sidecar session's `ring.add` persist is
/// its own and is still counted in `append_ms`, under the lock). They are
/// timed separately because they are different fixes: segment bytes, a hole
/// punch, a snapshot that grows with the ring.
#[derive(Default)]
struct PassTiming {
    lock_wait_ms: u64,
    locked_ms: u64,
    append_ms: u64,
    prune_ms: u64,
    checkpoint_ms: u64,
}

impl PassTiming {
    /// A pass with nothing to report is about half of all passes (one a
    /// second against a segment every two): ~12k identical all-zero lines a
    /// day on 2026-10-15. Silence is what an empty pass looks like.
    fn report(&self, segments: usize, checkpoints: u64, pruned: bool) {
        let idle = segments == 0
            && checkpoints == 0
            && !pruned
            && self.lock_wait_ms
                + self.locked_ms
                + self.append_ms
                + self.prune_ms
                + self.checkpoint_ms
                == 0;
        if idle {
            return;
        }
        tracing::info!(
            target: "task3460_index_pass",
            lock_wait_ms = self.lock_wait_ms,
            locked_ms = self.locked_ms,
            append_ms = self.append_ms,
            prune_ms = self.prune_ms,
            checkpoint_ms = self.checkpoint_ms,
            checkpoints,
            pruned,
            segments,
            "index pass"
        );
    }
}

#[cfg(test)]
mod tests;
