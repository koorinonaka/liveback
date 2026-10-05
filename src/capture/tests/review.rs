//! Review-lease and thumbnail access tests.
use super::*;

#[test]
fn review_lease_authorizes_only_current_leased_segments() {
    let root = std::env::temp_dir().join(format!("livia-review-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "session".into(), 15, None).unwrap();
    let segment = root.join("segment.mp4");
    std::fs::write(&segment, b"review-video").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 7,
        path: segment,
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 12,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);

    let lease = controller
        .acquire_review_lease(Some("session".into()), vec![7])
        .unwrap();
    assert_eq!(
        controller.read_review_segment(&lease, 7).unwrap(),
        b"review-video"
    );
    assert!(controller.read_review_segment(&lease, 8).is_err());
    assert!(controller.read_review_segment("../outside", 7).is_err());
    controller.release_review_lease(&lease);
    assert!(controller.read_review_segment(&lease, 7).is_err());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn read_review_segment_releases_sessions_lock_before_the_disk_read() {
    let root = std::env::temp_dir().join(format!("livia-review-io-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "session".into(), 15, None).unwrap();
    let segment = root.join("segment.mp4");
    // Large enough that a synchronous read takes measurable time even on fast
    // storage, so a regression that holds `sessions` across the read is observable
    // rather than racing past unnoticed.
    let payload = vec![0u8; 256 * 1024 * 1024];
    std::fs::write(&segment, &payload).unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: segment,
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: payload.len() as u64,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);
    let lease = controller
        .acquire_review_lease(Some("session".into()), vec![0])
        .unwrap();

    let reader = {
        let controller = controller.clone();
        let lease = lease.clone();
        std::thread::spawn(move || controller.read_review_segment(&lease, 0))
    };

    // Give the reader thread a moment to enter read_review_segment and start the
    // disk read, then confirm `sessions` is acquirable well before the read
    // finishes. If the lock were held across `fs::read` (the bug this guards
    // against), this try_lock would only succeed once `reader` has completed.
    std::thread::sleep(Duration::from_millis(5));
    let acquired_while_reading = controller.sessions.try_lock().is_ok();
    let result = reader.join().unwrap();

    assert!(result.is_ok(), "{result:?}");
    assert_eq!(result.unwrap().len(), payload.len());
    assert!(
        acquired_while_reading,
        "sessions lock must be released before the segment disk read runs"
    );
    controller.release_review_lease(&lease);
    let _ = std::fs::remove_dir_all(root);
}

/// Task067: `SegmentRecord.path` is session-relative for a recovered
/// session (and for any live recording going forward). Before Task067,
/// `read_review_segment` unconditionally rejected any non-absolute path,
/// so this case could never actually be read.
#[test]
fn read_review_segment_resolves_a_relative_segment_path_against_session_root() {
    let root = std::env::temp_dir().join(format!("livia-review-relative-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "session".into(), 15, None).unwrap();
    std::fs::write(root.join("segment-0.mp4"), b"relative-payload").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: "segment-0.mp4".into(),
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 17,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);
    let lease = controller
        .acquire_review_lease(Some("session".into()), vec![0])
        .unwrap();

    let bytes = controller.read_review_segment(&lease, 0).unwrap();
    assert_eq!(bytes, b"relative-payload");

    controller.release_review_lease(&lease);
    let _ = std::fs::remove_dir_all(root);
}

/// A `SegmentRecord.path` this app writes is always a bare `file_name()`
/// (never carries `..`), but the manifest is a local JSON file this
/// function should not trust unconditionally. Proves the explicit `..`
/// rejection matters: without it, a resolved path that still lexically
/// `starts_with(root)` would pass `is_file()` too, since a real file
/// exists at the escaped location.
#[test]
fn read_review_segment_rejects_a_traversal_tampered_segment_path() {
    let base = std::env::temp_dir().join(format!("livia-review-traversal-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("session");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(base.join("outside.mp4"), b"secret").unwrap();

    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "session".into(), 15, None).unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: "../outside.mp4".into(),
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 1,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);
    let lease = controller
        .acquire_review_lease(Some("session".into()), vec![0])
        .unwrap();

    let result = controller.read_review_segment(&lease, 0);
    assert!(
        result.is_err(),
        "a `..`-carrying segment path must be rejected even though the \
         target file exists at the escaped location"
    );

    controller.release_review_lease(&lease);
    let _ = std::fs::remove_dir_all(base);
}

/// Same traversal-defense gap as `read_review_segment_rejects_a_traversal_tampered_segment_path`,
/// but for `thumbnail_path`: a lexical `starts_with(root)` check alone
/// would pass for a `..`-carrying path that resolves to a real file
/// outside the session root.
#[test]
fn read_review_thumbnail_rejects_a_traversal_tampered_thumbnail_path() {
    let base = std::env::temp_dir().join(format!(
        "livia-review-thumbnail-traversal-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("session");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(base.join("outside.jpg"), b"secret").unwrap();

    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "session".into(), 15, None).unwrap();
    std::fs::write(root.join("segment-0.mp4"), b"payload").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: "segment-0.mp4".into(),
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 7,
        thumbnail_path: Some("../outside.jpg".into()),
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);

    let result = controller.read_review_thumbnail("session", 0);
    assert!(
        result.is_err(),
        "a `..`-carrying thumbnail path must be rejected even though the \
         target file exists at the escaped location"
    );

    let _ = std::fs::remove_dir_all(base);
}

/// Task069: a session absent from `self.sessions` (unclosed, or
/// crash-recovered — never inserted into the catalog) must still be able
/// to serve a thumbnail by reading its `manifest.json` straight off disk.
#[test]
fn read_review_thumbnail_falls_back_to_disk_for_a_catalog_unregistered_session() {
    let root = std::env::temp_dir().join(format!(
        "livia-review-thumbnail-fallback-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let session_dir = root.join("crash-session");
    let mut ring =
        ring_buffer::RingBuffer::create(session_dir.clone(), "crash-session".into(), 15, None)
            .unwrap();
    std::fs::write(session_dir.join("segment-0.mp4"), b"payload").unwrap();
    std::fs::write(session_dir.join("segment-0.jpg"), b"jpg-bytes").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: "segment-0.mp4".into(),
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 7,
        thumbnail_path: Some("segment-0.jpg".into()),
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    drop(ring);

    // Deliberately never inserted into `controller.sessions`, matching an
    // unclosed or crash-recovered session that `load_closed_sessions`/
    // `recover_session` never add to the in-memory catalog.
    let controller = CaptureController::new();
    let result = controller.read_review_thumbnail_at(&root, "crash-session", 0);

    assert_eq!(result, Ok(b"jpg-bytes".to_vec()));
    // The control: an id in neither the catalog nor the disk is refused.
    assert!(controller
        .read_review_thumbnail_at(&root, "no-such-session", 0)
        .is_err());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_review_thumbnail_rejects_traversal_session_ids_without_touching_outside_root() {
    let base = std::env::temp_dir().join(format!(
        "livia-review-thumbnail-id-traversal-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("buffer");
    std::fs::create_dir_all(&root).unwrap();
    let sentinel = base.join("outside.jpg");
    std::fs::write(&sentinel, b"sentinel-data").unwrap();

    let controller = CaptureController::new();
    for id in ["../outside", "..\\outside", "C:\\outside"] {
        let result = controller.read_review_thumbnail_at(&root, id, 0);
        assert!(result.is_err(), "session id {id:?} must be rejected");
    }

    assert!(sentinel.exists());
    assert_eq!(std::fs::read(&sentinel).unwrap(), b"sentinel-data");
    let _ = std::fs::remove_dir_all(&base);
}
