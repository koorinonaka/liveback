//! Review-lease and thumbnail access tests.
use super::*;

#[test]
fn review_lease_authorizes_only_current_leased_segments() {
    let root = TmpDir::new("livia-review");
    let ring = ring_with_segment(&root, "session", 7, b"review-video");
    let controller = CaptureController::new();
    controller.insert_session("session".into(), ring);

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

/// t261012-5b97: a recording's newest segments are served from memory, behind
/// the same lease check as a disk read. The two hold different bytes here so
/// the answer says which one served it.
#[test]
fn a_review_read_takes_the_newest_segments_from_memory_but_only_under_the_lease() {
    let root = TmpDir::new("livia-review-recent");
    let ring = ring_with_segment(&root, "session", 7, b"from-disk");
    let controller = CaptureController::new();
    controller.insert_session("session".into(), ring);
    let lease = controller
        .acquire_review_lease(Some("session".into()), vec![7])
        .unwrap();
    // The control: nothing in memory yet, so the disk answers.
    assert_eq!(
        controller.read_review_segment(&lease, 7).unwrap(),
        b"from-disk"
    );

    let recent = &controller.inner.recent_segments;
    recent.put("session", 7, Arc::new(b"from-memory".to_vec()));
    recent.put("session", 8, Arc::new(b"not leased".to_vec()));
    assert_eq!(
        controller.read_review_segment(&lease, 7).unwrap(),
        b"from-memory"
    );
    assert_eq!(
        controller
            .read_review_segment_from(&lease, 7, 10_000_000)
            .unwrap(),
        (b"from-memory".to_vec(), 0),
        "a seek's read gets the whole segment, from its start"
    );
    assert!(controller.read_review_segment(&lease, 8).is_err());
    assert!(controller.read_review_segment_from(&lease, 8, 0).is_err());

    recent.forget("session");
    assert_eq!(
        controller.read_review_segment(&lease, 7).unwrap(),
        b"from-disk"
    );
    controller.release_review_lease(&lease);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn read_review_segment_releases_sessions_lock_before_the_disk_read() {
    let root = TmpDir::new("livia-review-io");
    // Large enough that a synchronous read takes measurable time even on fast
    // storage, so a regression that holds `sessions` across the read is observable
    // rather than racing past unnoticed. Just under a container record's
    // 256 MiB cap.
    let payload = vec![0u8; 200 * 1024 * 1024];
    let ring = ring_with_segment(&root, "session", 0, &payload);
    let controller = CaptureController::new();
    controller.insert_session("session".into(), ring);
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
    let acquired_while_reading = controller.inner.sessions.try_lock().is_ok();
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

/// Task069: a session absent from `self.sessions` (unclosed, or
/// crash-recovered — never inserted into the catalog) must still be able
/// to serve a thumbnail by opening its container straight off disk.
#[test]
fn read_review_thumbnail_falls_back_to_disk_for_a_catalog_unregistered_session() {
    let root = TmpDir::new("livia-review-thumbnail-fallback");
    std::fs::create_dir_all(&root).unwrap();
    ring_buffer::container::ContainerBuilder::new("crash-session")
        .segment(0, 0, 20_000_000, b"payload")
        .thumbnail(0, b"jpg-bytes")
        .checkpoint()
        .build(&root.join("crash-session.lvb"))
        .unwrap();

    // Deliberately never inserted into `controller.inner.sessions`, matching an
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
    let base = TmpDir::new("livia-review-thumbnail-id-traversal");
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
