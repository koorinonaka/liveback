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

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, RecvTimeoutError};

use crate::{encoder, ring_buffer};

use super::worker::exclusive_segment_end;

/// How long the loop waits for a segment before looking at the marker queue
/// anyway. A marker pressed between two segments used to be merged by the next
/// `diagnostics()` poll (~1s); waiting for the next segment instead would hold
/// it for up to the segment length (2s), which the timeline would show as a
/// marker that lands late.
const MARKER_POLL: Duration = Duration::from_millis(500);

/// What reaches the writer thread.
///
/// Two senders now (task450): the capture worker, which finalizes segments, and
/// the thumbnail writer, which used to publish its own JPEG through
/// `SessionStore`. A container cannot have two writers, so the thumbnail bytes
/// come here instead and the one thread that owns the `ContainerWriter` appends
/// both.
pub enum IndexEvent {
    /// `bytes` is `Some` exactly when the session is a container.
    Segment(Box<encoder::VideoSegmentMetadata>),
    /// An encoded segment thumbnail, container sessions only.
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
    /// `Some` for a container recording, and then this thread is the **only**
    /// thing in the process that writes to that `.lvb`.
    pub(super) writer: Option<ring_buffer::container::ContainerWriter>,
    /// Thumbnails whose segment has not finalized yet. See `write`.
    pub(super) pending_thumbnails: HashMap<u64, Vec<u8>>,
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
    ///
    /// The directory arm has nothing to do -- `RingBuffer::close` clears its
    /// `recording.marker` from the stop path, as it always has.
    fn finish(&mut self) {
        let Some(writer) = self.writer.take() else {
            return;
        };
        if let Err(error) = writer.close() {
            tracing::error!(
                event = "container_close_failed",
                %error,
                "the container could not be closed; it will be recovered on the next launch"
            );
        }
    }

    /// One pass: fold `batch` and any queued markers into the ring, prune, and
    /// republish the timeline. The `sessions` lock is held across the persist,
    /// exactly as `sync_ring` held it -- the difference is which thread waits.
    /// Appends note, protection and marker edits to the container, answering
    /// whether any of them landed. A session with no container has no writer
    /// and nothing to append to -- its copy of the edit was already made where
    /// the edit was.
    fn append_meta_events(
        writer: Option<&mut ring_buffer::container::ContainerWriter>,
        events: &[ring_buffer::container::MetaEvent],
        failure_event: &'static str,
        failure_message: &'static str,
    ) -> bool {
        let Some(writer) = writer else {
            return false;
        };
        let mut written = false;
        for event in events {
            match writer.append_meta(event) {
                Ok(()) => written = true,
                Err(error) => tracing::error!(event = failure_event, %error, "{failure_message}"),
            }
        }
        written
    }

    /// Drops what retention no longer keeps. A container gives its space back
    /// by punching a hole rather than by unlinking files, and only this thread
    /// holds the writer that can do it.
    /// Answers whether anything was actually dropped, which is also whether
    /// `ContainerWriter::prune` wrote a checkpoint of its own (task3460): once
    /// the ring is full this is true on every pass, and the pass then
    /// checkpoints a second time below.
    fn prune_ring(
        writer: Option<&mut ring_buffer::container::ContainerWriter>,
        ring: &mut ring_buffer::RingBuffer,
    ) -> bool {
        match ring.prune() {
            Ok(Some(keep_from_index)) => {
                if let Some(writer) = writer {
                    if let Err(error) = writer.prune(keep_from_index) {
                        tracing::error!(
                            event = "container_prune_failed",
                            keep_from_index,
                            %error,
                        );
                    }
                }
                true
            }
            Ok(None) => false,
            Err(error) => {
                tracing::error!(event = "ring_prune_failed", %error, "ring buffer prune failed");
                false
            }
        }
    }

    /// Records a segment finalized as its own file rather than into a
    /// container, and answers whether the ring took it.
    ///
    /// No file, no record. A `bytes: 0` entry for a segment that isn't on disk
    /// is a timeline position that can never be played, and the stat failing
    /// here means precisely that -- either the finalize never landed, or
    /// `prune` already took it (task226's re-add bug wrote 1526 such entries
    /// into one session).
    fn add_sidecar_segment(
        ring: &mut ring_buffer::RingBuffer,
        segment: &encoder::VideoSegmentMetadata,
    ) -> bool {
        let Ok(bytes) = std::fs::metadata(&segment.path).map(|metadata| metadata.len()) else {
            return false;
        };
        // Session-directory-relative, not `segment.path` (absolute) itself:
        // keeps the manifest portable and matches what `recover_session` has
        // always written. `RingBuffer::resolve_segment_path` joins it back with
        // the session root wherever a segment is actually read (Task067; see
        // also `read_review_segment`).
        let Some(file_name) = segment.path.file_name().map(Into::into) else {
            return false;
        };
        // The thumbnail (if any) was already written when this segment
        // *opened* (see `drain_pending_segment_thumbnail` in `run_capture`),
        // well before its finalize event reaches here, so its presence on disk
        // can just be checked directly.
        let thumbnail_path = segment.path.parent().and_then(|directory| {
            let (_, final_thumbnail_path) =
                ring_buffer::store::thumbnail_paths(directory, segment.index);
            final_thumbnail_path.is_file().then(|| {
                final_thumbnail_path
                    .file_name()
                    .expect("thumbnail path always has a file name")
                    .into()
            })
        });
        match ring.add(ring_buffer::SegmentRecord {
            index: segment.index,
            path: file_name,
            start_100ns: segment.start_timestamp_100ns,
            end_100ns: exclusive_segment_end(segment.end_timestamp_100ns, segment.fps),
            bytes,
            thumbnail_path,
            audio_offsets_100ns: segment.audio_offsets_100ns.clone(),
        }) {
            Ok(()) => true,
            // Nobody polls this thread for errors, so they go to the log rather
            // than into `CaptureDiagnostics::encoder_errors` the way
            // `sync_ring` reported them (task410's precedent).
            Err(error) => {
                tracing::error!(
                    event = "segment_index_failed",
                    index = segment.index,
                    %error,
                    "the finalized segment could not be written to the index"
                );
                false
            }
        }
    }

    fn write(&mut self, batch: Vec<IndexEvent>) {
        let markers = self
            .pending_markers
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        let meta = self
            .pending_meta
            .lock()
            .map(|mut queue| std::mem::take(&mut *queue))
            .unwrap_or_default();
        if batch.is_empty() && markers.is_empty() && meta.is_empty() {
            return;
        }
        // Meta edits go straight to the writer, not through the ring: the
        // ring's copy was already updated where the edit was made
        // (`RingBuffer::stage_note`), and this thread is the only one allowed
        // to touch the file. Done before the `sessions` lock so an edit is not
        // lost if the ring has since gone.
        let mut meta_written = Self::append_meta_events(
            self.writer.as_mut(),
            &meta,
            "container_meta_append_failed",
            "a note or protection edit could not be appended to the container",
        );
        // Split before taking the lock: appending a thumbnail needs its
        // segment to already be in the container's index (`append_thumbnail`
        // silently drops one for an unknown index), and thumbnails are captured
        // when a segment *opens* -- well before it finalizes. So they wait here
        // and go in right after the segment they belong to.
        let mut segments = Vec::new();
        for event in batch {
            match event {
                IndexEvent::Segment(segment) => segments.push(*segment),
                IndexEvent::Thumbnail(index, jpeg) => {
                    self.pending_thumbnails.insert(index, jpeg);
                }
            }
        }
        // Everything from here to the end of the pass runs with `sessions`
        // held, and every review read waits on it -- `refresh_live`, the
        // lease, the segment's target resolution (task3460). The phases are
        // timed separately because they are different fixes: the append is
        // segment bytes, the prune is a hole punch, the checkpoint is a JSON
        // snapshot that grows with the ring.
        let waiting = Instant::now();
        let Ok(mut sessions) = self.sessions.lock() else {
            return;
        };
        let lock_wait_ms = waiting.elapsed().as_millis() as u64;
        let locked = Instant::now();
        let Some(ring) = sessions.get_mut(&self.session_id) else {
            return;
        };
        let mut changed = false;
        let segment_count = segments.len();
        let appending = Instant::now();
        for segment in segments {
            if ring.contains(segment.index) {
                continue;
            }
            if let Some(writer) = self.writer.as_mut() {
                match Self::append_to_container(
                    writer,
                    &segment,
                    self.pending_thumbnails.remove(&segment.index),
                ) {
                    Ok((record, spans)) => match ring.add_container_segment(record, spans) {
                        Ok(()) => changed = true,
                        Err(error) => tracing::error!(
                            event = "segment_index_failed",
                            index = segment.index,
                            %error,
                            "the finalized segment could not be written to the index"
                        ),
                    },
                    Err(error) => tracing::error!(
                        event = "container_append_failed",
                        index = segment.index,
                        %error,
                        "the finalized segment could not be appended to the container"
                    ),
                }
                continue;
            }
            changed |= Self::add_sidecar_segment(ring, &segment);
        }
        let append_ms = appending.elapsed().as_millis() as u64;
        let markers_merged = !markers.is_empty();
        match ring.merge_markers(markers) {
            // The container's copy of a marker is this append and nothing else
            // (task1630): `RingBuffer::persist` writes no file for a container,
            // and this thread is the only one allowed to touch the `.lvb`. Same
            // rail as the note/protection edits above -- they just had one and
            // markers did not, so every hotkey marker died with the process.
            Ok(events) => {
                meta_written |= Self::append_meta_events(
                    self.writer.as_mut(),
                    &events,
                    "container_marker_append_failed",
                    "a marker could not be appended to the container",
                );
            }
            Err(error) => {
                tracing::error!(event = "marker_merge_failed", %error, "marker merge failed")
            }
        }
        // `prune_ms` covers the `Pruned` meta record, the hole punch and the
        // checkpoint `ContainerWriter::prune` writes for itself.
        let pruning = Instant::now();
        let pruned = changed && Self::prune_ring(self.writer.as_mut(), ring);
        let prune_ms = pruning.elapsed().as_millis() as u64;
        // One checkpoint per pass, after everything this batch appended. It is
        // what a crash recovers to, so it goes last and covers the whole pass
        // rather than each record. `prune` already checkpointed if it ran.
        let checkpointing = Instant::now();
        let mut checkpoints = u32::from(pruned);
        if changed || meta_written {
            if let Some(writer) = self.writer.as_mut() {
                checkpoints += 1;
                if let Err(error) = writer.checkpoint() {
                    tracing::error!(event = "container_checkpoint_failed", %error);
                }
            }
        }
        let checkpoint_ms = checkpointing.elapsed().as_millis() as u64;
        let locked_ms = locked.elapsed().as_millis() as u64;
        tracing::info!(
            target: "task3460_index_pass",
            lock_wait_ms,
            locked_ms,
            append_ms,
            prune_ms,
            checkpoint_ms,
            checkpoints,
            pruned,
            segments = segment_count,
            "index pass"
        );
        // A marker alone changes the manifest even when no segment finalized
        // this pass, and the frontend converges on `get_timeline`.
        if changed || markers_merged || meta_written {
            let manifest = ring.manifest().clone();
            drop(sessions);
            if let Ok(mut timeline) = self.last_timeline.lock() {
                *timeline = Some(manifest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::capture::CaptureSize;

    fn temp_root(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir()
            .join("livia-tests")
            .join(format!("indexer-{name}-{nanos}"))
    }

    /// A finalized segment with a real file behind it, which is what the
    /// task226 guard checks for.
    fn segment(root: &Path, index: u64) -> encoder::VideoSegmentMetadata {
        let path = root.join(format!("segment-{index}.mp4"));
        std::fs::write(&path, [0u8; 64]).expect("segment file");
        encoder::VideoSegmentMetadata {
            index,
            start_timestamp_100ns: index as i64 * 20_000_000,
            end_timestamp_100ns: index as i64 * 20_000_000 + 19_000_000,
            path,
            resolution: CaptureSize {
                width: 0,
                height: 0,
            },
            fps: 60,
            bitrate: 0,
            keyframe_first: true,
            audio_offsets_100ns: vec![0],
            bytes: None,
        }
    }

    fn indexer_for(
        root: &Path,
    ) -> (
        SegmentIndexer,
        Arc<Mutex<HashMap<String, ring_buffer::RingBuffer>>>,
    ) {
        std::fs::create_dir_all(root).expect("session dir");
        let ring = ring_buffer::RingBuffer::create(
            root.to_path_buf(),
            "session".to_owned(),
            ring_buffer::DEFAULT_RETENTION_MINUTES,
            None,
        )
        .expect("ring");
        let sessions = Arc::new(Mutex::new(HashMap::from([("session".to_owned(), ring)])));
        (
            SegmentIndexer {
                session_id: "session".to_owned(),
                sessions: sessions.clone(),
                pending_markers: Arc::new(Mutex::new(Vec::new())),
                pending_meta: Arc::new(Mutex::new(Vec::new())),
                last_timeline: Arc::new(Mutex::new(None)),
                writer: None,
                pending_thumbnails: Default::default(),
            },
            sessions,
        )
    }

    /// The reason the join exists: whatever the capture worker sent before it
    /// ended has to be in the manifest before `close` writes it out. Sending
    /// after the writer is already parked in `recv_timeout` and *then* closing
    /// the channel is the stop sequence in miniature.
    #[test]
    fn the_last_segment_lands_in_the_index_when_the_worker_ends() {
        let root = temp_root("last-segment");
        let (indexer, sessions) = indexer_for(&root);
        let (finalized, segments) = crossbeam_channel::unbounded();
        let handle = indexer.spawn(segments).expect("writer starts");

        finalized
            .send(IndexEvent::Segment(Box::new(segment(&root, 0))))
            .expect("send 0");
        finalized
            .send(IndexEvent::Segment(Box::new(segment(&root, 1))))
            .expect("send 1");
        drop(finalized);
        handle.join().expect("writer ends");

        let sessions = sessions.lock().expect("sessions");
        let ring = sessions.get("session").expect("ring");
        assert_eq!(
            ring.manifest()
                .segments
                .iter()
                .map(|segment| segment.index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        // From disk, not from memory: the point of one writer is that the
        // manifest on disk is what it wrote, with nobody else to write it.
        let persisted = ring_buffer::RingBuffer::open(root.clone()).expect("reopen");
        assert_eq!(persisted.manifest().segments.len(), 2);
        drop(sessions);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// task226's guard, moved with the write: a segment whose file is not on
    /// disk is a timeline position that can never be played, so it must not
    /// reach the manifest.
    #[test]
    fn a_segment_whose_file_is_gone_is_never_indexed() {
        let root = temp_root("missing-file");
        let (mut indexer, sessions) = indexer_for(&root);
        let present = segment(&root, 0);
        let mut missing = segment(&root, 1);
        std::fs::remove_file(&missing.path).expect("remove");
        missing.path = root.join("segment-1.mp4");

        indexer.write(vec![
            IndexEvent::Segment(Box::new(present)),
            IndexEvent::Segment(Box::new(missing)),
        ]);

        let sessions = sessions.lock().expect("sessions");
        let ring = sessions.get("session").expect("ring");
        assert_eq!(
            ring.manifest()
                .segments
                .iter()
                .map(|segment| segment.index)
                .collect::<Vec<_>>(),
            vec![0]
        );
        drop(sessions);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A marker pressed between two segments must not wait for the next one.
    #[test]
    fn a_marker_alone_is_merged_without_a_segment() {
        let root = temp_root("marker-only");
        let (mut indexer, sessions) = indexer_for(&root);
        indexer
            .pending_markers
            .lock()
            .expect("markers")
            .push(ring_buffer::MarkerRecord {
                time_100ns: 42,
                label: String::new(),
                color_index: None,
            });

        indexer.write(Vec::new());

        let sessions = sessions.lock().expect("sessions");
        let ring = sessions.get("session").expect("ring");
        assert_eq!(
            ring.manifest()
                .markers
                .iter()
                .map(|marker| marker.time_100ns)
                .collect::<Vec<_>>(),
            vec![42]
        );
        drop(sessions);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The container arm, and specifically its ordering trap: a thumbnail is
    /// captured when a segment *opens*, so it reaches this thread well before
    /// the segment finalizes -- and `append_thumbnail` silently drops one whose
    /// segment is not in the index yet. Sending them in that order here is what
    /// catches a regression that would otherwise only show up as thumbnails
    /// quietly missing from every recording.
    #[test]
    fn a_container_takes_the_segment_then_the_thumbnail_that_arrived_first() {
        let root = temp_root("container");
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("capture-test.lvb");
        let writer = ring_buffer::container::ContainerWriter::create(&path, "capture-test", 15)
            .expect("writer");
        let ring = ring_buffer::RingBuffer::create_container(
            path.clone(),
            "capture-test".to_owned(),
            15,
            None,
            None,
        );
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            "capture-test".to_owned(),
            ring,
        )])));
        let mut indexer = SegmentIndexer {
            session_id: "capture-test".to_owned(),
            sessions: sessions.clone(),
            pending_markers: Arc::new(Mutex::new(Vec::new())),
            pending_meta: Arc::new(Mutex::new(Vec::new())),
            last_timeline: Arc::new(Mutex::new(None)),
            writer: Some(writer),
            pending_thumbnails: Default::default(),
        };

        let mut finalized = segment(&root, 0);
        finalized.bytes = Some(b"the finalized mp4".to_vec());
        // Thumbnail first, exactly as the capture worker emits them.
        indexer.write(vec![
            IndexEvent::Thumbnail(0, b"jpeg".to_vec()),
            IndexEvent::Segment(Box::new(finalized)),
        ]);
        indexer.finish();

        // Read it back the way the app does, so this asserts on the shape the
        // history and LiveReview actually see rather than on writer internals.
        let reopened = ring_buffer::RingBuffer::open_container(path).expect("reopen");
        assert!(reopened.manifest().closed, "finish() closes the container");
        let segments = &reopened.manifest().segments;
        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].index, 0);
        assert!(
            segments[0].thumbnail_path.is_some(),
            "the thumbnail that arrived before its segment still lands in the index"
        );
        assert!(
            reopened.segment_target(0).is_some(),
            "the segment is addressable for playback"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Task t260913-66a6: a read of a segment that is still being recorded goes
    /// by the span the writer handed back and never reopens the container --
    /// reopening (header + newest checkpoint, parsed) is what cost 26-231ms
    /// per seek while this very writer was appending (task3590). The writer
    /// stays open for the whole test, so this is the live case, not a closed
    /// file read back.
    #[test]
    fn a_live_container_reads_by_span_without_reopening_the_file() {
        let root = temp_root("container-span");
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("capture-span.lvb");
        let writer = ring_buffer::container::ContainerWriter::create(&path, "capture-span", 15)
            .expect("writer");
        let ring = ring_buffer::RingBuffer::create_container(
            path.clone(),
            "capture-span".to_owned(),
            15,
            None,
            None,
        );
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            "capture-span".to_owned(),
            ring,
        )])));
        let mut indexer = SegmentIndexer {
            session_id: "capture-span".to_owned(),
            sessions: sessions.clone(),
            pending_markers: Arc::new(Mutex::new(Vec::new())),
            pending_meta: Arc::new(Mutex::new(Vec::new())),
            last_timeline: Arc::new(Mutex::new(None)),
            writer: Some(writer),
            pending_thumbnails: Default::default(),
        };

        let mut finalized = segment(&root, 0);
        finalized.bytes = Some(b"the finalized mp4".to_vec());
        indexer.write(vec![
            IndexEvent::Thumbnail(0, b"jpeg".to_vec()),
            IndexEvent::Segment(Box::new(finalized)),
        ]);

        // Resolved under the lock and read after it, as `read_review_segment`
        // and `read_review_thumbnail_at` do.
        let (segment_target, thumbnail_target) = {
            let sessions = sessions.lock().expect("sessions");
            let ring = sessions.get("capture-span").expect("ring");
            (
                ring.segment_target(0).expect("segment target"),
                ring.thumbnail_target(0).expect("thumbnail target"),
            )
        };
        let before = ring_buffer::container::open_for_reading_calls();
        assert_eq!(
            ring_buffer::read_target(&segment_target, "mp4", false).expect("segment"),
            b"the finalized mp4"
        );
        assert_eq!(
            ring_buffer::read_target(&thumbnail_target, "jpg", true).expect("thumbnail"),
            b"jpeg"
        );
        assert_eq!(
            ring_buffer::container::open_for_reading_calls() - before,
            0,
            "a live read goes by span and never reopens the container"
        );

        // Control: the same read without the span does reopen, and the counter
        // sees it -- so the zero above is a measurement, not a dead probe.
        let ring_buffer::ReadTarget::Container { path, index, .. } = &segment_target else {
            panic!("a container session resolves a container target");
        };
        let unresolved = ring_buffer::ReadTarget::Container {
            path: path.clone(),
            index: *index,
            span: None,
        };
        assert_eq!(
            ring_buffer::read_target(&unresolved, "mp4", false).expect("checkpoint path"),
            b"the finalized mp4"
        );
        assert_eq!(ring_buffer::container::open_for_reading_calls() - before, 1);

        indexer.finish();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Task720: a note or a protection toggle made *while* the container is
    /// being recorded has to come through this queue. Written where it was made
    /// it would go through `sessions::append_meta`, whose `ContainerWriter::reopen`
    /// does `set_len(valid_end)` -- truncating away every segment this thread
    /// appended since the last checkpoint. So the segment surviving is as much
    /// the point here as the note landing.
    #[test]
    fn a_live_edit_reaches_the_container_without_cutting_the_recording() {
        let root = temp_root("container-live-meta");
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("capture-live.lvb");
        let writer = ring_buffer::container::ContainerWriter::create(&path, "capture-live", 15)
            .expect("writer");
        let ring = ring_buffer::RingBuffer::create_container(
            path.clone(),
            "capture-live".to_owned(),
            15,
            None,
            None,
        );
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            "capture-live".to_owned(),
            ring,
        )])));
        let pending_meta = Arc::new(Mutex::new(Vec::new()));
        let mut indexer = SegmentIndexer {
            session_id: "capture-live".to_owned(),
            sessions: sessions.clone(),
            pending_markers: Arc::new(Mutex::new(Vec::new())),
            pending_meta: pending_meta.clone(),
            last_timeline: Arc::new(Mutex::new(None)),
            writer: Some(writer),
            pending_thumbnails: Default::default(),
        };

        let mut finalized = segment(&root, 0);
        finalized.bytes = Some(b"the finalized mp4".to_vec());
        indexer.write(vec![IndexEvent::Segment(Box::new(finalized))]);

        // The edit arrives between segments, which is the case that has no
        // batch to ride along with -- the 500ms timeout pass has to carry it.
        pending_meta.lock().expect("queue").extend([
            ring_buffer::container::MetaEvent::NoteSet {
                note: Some("録画中に書いたメモ".to_owned()),
            },
            ring_buffer::container::MetaEvent::ProtectedSet { protected: true },
        ]);
        indexer.write(Vec::new());
        indexer.finish();

        let reopened = ring_buffer::RingBuffer::open_container(path).expect("reopen");
        assert_eq!(
            reopened.manifest().note.as_deref(),
            Some("録画中に書いたメモ")
        );
        assert!(reopened.manifest().protected);
        assert_eq!(
            reopened.manifest().segments.len(),
            1,
            "the segment appended before the edit is still there"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Task1630: the marker hotkey's queue is the note queue's twin, and until
    /// this test it had no container arm at all. `merge_markers` put the marker
    /// in the in-memory manifest and called `persist`, which writes nothing for
    /// a container -- so the marker showed for the rest of the run and was gone
    /// the next time the `.lvb` was opened. Reopening from disk is the assert.
    #[test]
    fn a_marker_pressed_while_recording_reaches_the_container() {
        let root = temp_root("container-marker");
        std::fs::create_dir_all(&root).expect("root");
        let path = root.join("capture-mark.lvb");
        let writer = ring_buffer::container::ContainerWriter::create(&path, "capture-mark", 15)
            .expect("writer");
        let ring = ring_buffer::RingBuffer::create_container(
            path.clone(),
            "capture-mark".to_owned(),
            15,
            None,
            None,
        );
        let sessions = Arc::new(Mutex::new(HashMap::from([(
            "capture-mark".to_owned(),
            ring,
        )])));
        let pending_markers = Arc::new(Mutex::new(Vec::new()));
        let mut indexer = SegmentIndexer {
            session_id: "capture-mark".to_owned(),
            sessions: sessions.clone(),
            pending_markers: pending_markers.clone(),
            pending_meta: Arc::new(Mutex::new(Vec::new())),
            last_timeline: Arc::new(Mutex::new(None)),
            writer: Some(writer),
            pending_thumbnails: Default::default(),
        };

        let mut finalized = segment(&root, 0);
        finalized.bytes = Some(b"the finalized mp4".to_vec());
        indexer.write(vec![IndexEvent::Segment(Box::new(finalized))]);
        // Pressed between two segments, like the hotkey's own pass.
        pending_markers
            .lock()
            .expect("queue")
            .push(ring_buffer::MarkerRecord {
                time_100ns: 12_345,
                label: String::new(),
                color_index: None,
            });
        indexer.write(Vec::new());
        indexer.finish();

        let reopened = ring_buffer::RingBuffer::open_container(path).expect("reopen");
        assert_eq!(
            reopened
                .manifest()
                .markers
                .iter()
                .map(|marker| (marker.time_100ns, marker.color_index))
                .collect::<Vec<_>>(),
            vec![(12_345, Some(0))],
            "the marker -- and the palette slot merge assigned it -- survives the file"
        );
        assert_eq!(
            reopened.manifest().segments.len(),
            1,
            "appending the marker did not cut the recording short"
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
