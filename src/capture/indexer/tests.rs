use std::path::Path;

use super::*;
use crate::capture::CaptureSize;
use crate::test_support::TmpDir;

/// An indexer for `session_id` with nothing pending and no timeline yet.
fn indexer(
    session_id: String,
    sessions: Arc<Mutex<HashMap<String, ring_buffer::RingBuffer>>>,
    writer: ring_buffer::container::ContainerWriter,
    recent: Arc<RecentSegments>,
) -> SegmentIndexer {
    SegmentIndexer {
        session_id,
        sessions,
        pending_markers: Arc::new(Mutex::new(Vec::new())),
        pending_meta: Arc::new(Mutex::new(Vec::new())),
        last_timeline: Arc::new(Mutex::new(None)),
        writer,
        pending_thumbnails: Default::default(),
        recent,
    }
}

fn temp_root(name: &str) -> TmpDir {
    TmpDir::new(&format!("livia-indexer-{name}"))
}

/// A finalized segment as the capture worker hands it over: its bytes in
/// memory, nothing on disk.
fn segment(root: &Path, index: u64) -> encoder::VideoSegmentMetadata {
    let path = root.join(format!("segment-{index}.mp4"));
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
        bytes: Some(vec![0u8; 64]),
    }
}

/// An indexer for a fresh recording at `root/session.lvb`, as a recording
/// start builds one.
fn indexer_for(
    root: &Path,
) -> (
    SegmentIndexer,
    Arc<Mutex<HashMap<String, ring_buffer::RingBuffer>>>,
) {
    std::fs::create_dir_all(root).expect("session dir");
    let path = root.join("session.lvb");
    let writer = ring_buffer::container::ContainerWriter::create(
        &path,
        "session",
        ring_buffer::DEFAULT_RETENTION_MINUTES,
    )
    .expect("writer");
    let ring = ring_buffer::RingBuffer::create_container(
        path,
        "session".to_owned(),
        ring_buffer::DEFAULT_RETENTION_MINUTES,
        None,
        None,
    );
    let sessions = Arc::new(Mutex::new(HashMap::from([("session".to_owned(), ring)])));
    (
        indexer(
            "session".to_owned(),
            sessions.clone(),
            writer,
            Default::default(),
        ),
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
    // container on disk is what it wrote, with nobody else to write it.
    let persisted =
        ring_buffer::RingBuffer::open_container(root.join("session.lvb")).expect("reopen");
    assert_eq!(persisted.manifest().segments.len(), 2);
    drop(sessions);
    let _ = std::fs::remove_dir_all(&root);
}

/// task226's guard, carried over to the container: a segment that arrives
/// without its bytes is a timeline position that can never be played, so it
/// must not reach the manifest.
#[test]
fn a_segment_without_its_bytes_is_never_indexed() {
    let root = temp_root("missing-file");
    let (mut indexer, sessions) = indexer_for(&root);
    let present = segment(&root, 0);
    let mut missing = segment(&root, 1);
    missing.bytes = None;

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
        .push(ring_buffer::MarkerRecord::at(42));

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
    let writer =
        ring_buffer::container::ContainerWriter::create(&path, "capture-test", 15).expect("writer");
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
    let mut indexer = indexer(
        "capture-test".to_owned(),
        sessions.clone(),
        writer,
        Default::default(),
    );

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
    let writer =
        ring_buffer::container::ContainerWriter::create(&path, "capture-span", 15).expect("writer");
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
    let mut indexer = indexer(
        "capture-span".to_owned(),
        sessions.clone(),
        writer,
        Default::default(),
    );

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
        ring_buffer::read_target(&segment_target, false).expect("segment"),
        b"the finalized mp4"
    );
    assert_eq!(
        ring_buffer::read_target(&thumbnail_target, true).expect("thumbnail"),
        b"jpeg"
    );
    assert_eq!(
        ring_buffer::container::open_for_reading_calls() - before,
        0,
        "a live read goes by span and never reopens the container"
    );

    // Control: the same read without the span does reopen, and the counter
    // sees it -- so the zero above is a measurement, not a dead probe.
    let ring_buffer::ReadTarget { path, index, .. } = &segment_target;
    let unresolved = ring_buffer::ReadTarget {
        path: path.clone(),
        index: *index,
        span: None,
    };
    assert_eq!(
        ring_buffer::read_target(&unresolved, false).expect("checkpoint path"),
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
    let writer =
        ring_buffer::container::ContainerWriter::create(&path, "capture-live", 15).expect("writer");
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
        pending_meta: pending_meta.clone(),
        ..indexer(
            "capture-live".to_owned(),
            sessions.clone(),
            writer,
            Default::default(),
        )
    };

    let mut finalized = segment(&root, 0);
    finalized.bytes = Some(b"the finalized mp4".to_vec());
    indexer.write(vec![IndexEvent::Segment(Box::new(finalized))]);

    // The edit arrives between segments, which is the case that has no
    // batch to ride along with -- the 500ms timeout pass has to carry it.
    pending_meta.lock().expect("queue").extend([
        ring_buffer::container::MetaEvent::CommentSet {
            comment: Some("録画中に書いたメモ".to_owned()),
        },
        ring_buffer::container::MetaEvent::ProtectedSet { protected: true },
    ]);
    indexer.write(Vec::new());
    indexer.finish();

    let reopened = ring_buffer::RingBuffer::open_container(path).expect("reopen");
    assert_eq!(
        reopened.manifest().comment.as_deref(),
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
    let writer =
        ring_buffer::container::ContainerWriter::create(&path, "capture-mark", 15).expect("writer");
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
        pending_markers: pending_markers.clone(),
        ..indexer(
            "capture-mark".to_owned(),
            sessions.clone(),
            writer,
            Default::default(),
        )
    };

    let mut finalized = segment(&root, 0);
    finalized.bytes = Some(b"the finalized mp4".to_vec());
    indexer.write(vec![IndexEvent::Segment(Box::new(finalized))]);
    // Pressed between two segments, like the hotkey's own pass.
    pending_markers
        .lock()
        .expect("queue")
        .push(ring_buffer::MarkerRecord::at(12_345));
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

/// t260925-dea6: the container append and the checkpoint are disk I/O
/// (9.6s under a heavy UE recording, 2026-09-24), and the UI thread and
/// playback take `sessions` for every timeline read. So while the pass is
/// writing, another thread has to get `sessions` promptly. The pair of
/// t260924-327f's `the_last_started_answer_does_not_wait_for_the_session_catalog`.
#[test]
fn the_session_catalog_is_free_while_the_pass_writes_the_container() {
    let root = temp_root("container-io-unlocked");
    std::fs::create_dir_all(&root).expect("root");
    let path = root.join("capture-io.lvb");
    let writer =
        ring_buffer::container::ContainerWriter::create(&path, "capture-io", 15).expect("writer");
    let ring = ring_buffer::RingBuffer::create_container(
        path.clone(),
        "capture-io".to_owned(),
        15,
        None,
        None,
    );
    let sessions = Arc::new(Mutex::new(HashMap::from([("capture-io".to_owned(), ring)])));
    let mut indexer = indexer(
        "capture-io".to_owned(),
        sessions.clone(),
        writer,
        Default::default(),
    );

    // Each I/O point asks another thread to take `sessions` and waits up
    // to 2s for it. Waiting on this thread is the "slow disk": it happens
    // exactly where the bytes are written.
    let answers = Arc::new(Mutex::new(Vec::new()));
    {
        let sessions = sessions.clone();
        let answers = answers.clone();
        DURING_IO.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let (tx, rx) = crossbeam_channel::bounded(1);
                let sessions = sessions.clone();
                thread::spawn(move || {
                    let _guard = sessions.lock();
                    let _ = tx.send(());
                });
                let got = rx.recv_timeout(Duration::from_secs(2)).is_ok();
                answers.lock().expect("answers").push(got);
            }));
        });
    }
    let mut finalized = segment(&root, 0);
    finalized.bytes = Some(b"the finalized mp4".to_vec());
    indexer.write(vec![IndexEvent::Segment(Box::new(finalized))]);
    DURING_IO.with(|hook| *hook.borrow_mut() = None);

    let answers = answers.lock().expect("answers").clone();
    assert_eq!(
        answers.len(),
        2,
        "the hook ran at the append and at the checkpoint"
    );
    assert_eq!(
        answers,
        vec![true, true],
        "another thread got `sessions` during the append and the checkpoint"
    );
    // The pass still did its job.
    assert!(sessions
        .lock()
        .expect("sessions")
        .get("capture-io")
        .expect("ring")
        .contains(0));

    indexer.finish();
    let _ = std::fs::remove_dir_all(&root);
}

/// t261012-5b97: the newest three segments of a recording are kept in
/// memory for review reads, put there before the append (whose completion
/// is what grows the timeline), and dropped when the recording ends.
#[test]
fn a_recording_keeps_its_newest_three_segments_in_memory_until_it_ends() {
    let root = temp_root("container-recent");
    std::fs::create_dir_all(&root).expect("root");
    let path = root.join("capture-recent.lvb");
    let writer = ring_buffer::container::ContainerWriter::create(&path, "capture-recent", 15)
        .expect("writer");
    let ring = ring_buffer::RingBuffer::create_container(
        path.clone(),
        "capture-recent".to_owned(),
        15,
        None,
        None,
    );
    let recent = Arc::new(RecentSegments::default());
    let mut indexer = indexer(
        "capture-recent".to_owned(),
        Arc::new(Mutex::new(HashMap::from([(
            "capture-recent".to_owned(),
            ring,
        )]))),
        writer,
        recent.clone(),
    );
    let bytes = |index: u64| format!("segment {index}").into_bytes();

    // Whether the segment was already in memory at its append.
    let seen_at_append = Arc::new(Mutex::new(Vec::new()));
    {
        let recent = recent.clone();
        let seen = seen_at_append.clone();
        DURING_IO.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let found = recent.get("capture-recent", 0).is_some();
                seen.lock().expect("seen").push(found);
            }));
        });
    }
    let mut first = segment(&root, 0);
    first.bytes = Some(bytes(0));
    indexer.write(vec![IndexEvent::Segment(Box::new(first))]);
    DURING_IO.with(|hook| *hook.borrow_mut() = None);
    assert_eq!(
        seen_at_append.lock().expect("seen").first(),
        Some(&true),
        "in memory before the append"
    );

    for index in 1..4 {
        let mut finalized = segment(&root, index);
        finalized.bytes = Some(bytes(index));
        indexer.write(vec![IndexEvent::Segment(Box::new(finalized))]);
    }
    let cached = |index| {
        recent
            .get("capture-recent", index)
            .map(|bytes| bytes.as_ref().clone())
    };
    assert_eq!(cached(0), None, "the fourth pushed the oldest out");
    for index in 1..4 {
        assert_eq!(cached(index), Some(bytes(index)));
    }
    assert_eq!(recent.get("another-session", 3), None);

    indexer.finish();
    assert_eq!(cached(3), None, "the recording ended");
    let _ = std::fs::remove_dir_all(&root);
}
