//! Retention, lease, prune and gap tests.
use super::*;

#[test]
fn retention_and_lease() {
    let p = tmp();
    let mut r = ring_at(&p, "x", 5);
    for i in 0..152 {
        r.add(seg(i)).unwrap()
    }
    r.lease(0);
    r.prune().unwrap();
    assert!(r.manifest().segments.iter().any(|s| s.index == 0));
    r.release(0);
    r.prune().unwrap();
    assert!(!r.manifest().segments.iter().any(|s| s.index == 0));
}
/// task1410: the ring buffer switch sends 0 to mean "never drop the head".
/// The clamp has to leave it alone -- rounded up to 5 it would be the shortest
/// ring in the app instead of no ring at all -- and `prune` has to keep every
/// segment however far past the window it runs. Same fixture as
/// `retention_and_lease` above, whose 5 minutes still drops segment 0.
#[test]
fn a_zero_retention_session_never_drops_its_head() {
    let p = tmp();
    let mut r = ring_at(&p, "x", NO_RETENTION_LIMIT);
    assert_eq!(r.manifest().retention_minutes, NO_RETENTION_LIMIT);
    for i in 0..152 {
        r.add(seg(i)).unwrap()
    }
    assert_eq!(r.prune().unwrap(), None);
    assert_eq!(r.manifest().segments.len(), 152);
}
#[test]
fn gaps_use_a_fixed_one_second_threshold() {
    let segments = vec![
        SegmentRecord {
            end_100ns: 20_000_000,
            ..seg(0)
        },
        SegmentRecord {
            start_100ns: 20_357_300,
            end_100ns: 40_357_300,
            ..seg(1)
        },
        SegmentRecord {
            start_100ns: 50_357_299,
            end_100ns: 70_357_299,
            ..seg(2)
        },
        SegmentRecord {
            start_100ns: 80_357_299,
            end_100ns: 100_357_299,
            ..seg(3)
        },
    ];
    let gaps = derive_gaps(&segments);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].start_100ns, 70_357_299);
    assert_eq!(gaps[0].end_100ns, 80_357_299);
    assert_eq!(gaps[0].reason, GAP_REASON);
}

/// The live ring's gaps follow its segments: `add` re-derives them, which a
/// manifest write used to do on the way to disk (t261025-d118).
#[test]
fn adding_a_segment_after_a_hole_reports_the_gap() {
    let root = tmp();
    let mut ring = ring_at(&root, "x", 15);
    ring.add(seg(0)).unwrap();
    assert!(ring.manifest().gaps.is_empty());
    ring.add(SegmentRecord {
        start_100ns: 35_000_000,
        end_100ns: 55_000_000,
        ..seg(1)
    })
    .unwrap();
    let gaps = &ring.manifest().gaps;
    assert_eq!(gaps.len(), 1);
    assert_eq!(
        (gaps[0].start_100ns, gaps[0].end_100ns),
        (20_000_000, 35_000_000)
    );
}

/// t260929-e171: `add` stamps when it folded, which the review screen's load
/// reads as the live segment's phase: none before any fold, and it advances
/// with each one.
#[test]
fn adding_a_segment_stamps_when_it_was_folded() {
    let root = tmp();
    let mut ring = ring_at(&root, "x", 15);
    assert_eq!(ring.last_fold(), None);
    let before = std::time::Instant::now();
    ring.add(seg(0)).unwrap();
    let first = ring.last_fold().expect("the fold stamps");
    assert!(first >= before);
    std::thread::sleep(std::time::Duration::from_millis(5));
    ring.add(seg(1)).unwrap();
    assert!(ring.last_fold().expect("still stamped") > first);
}

#[test]
fn recovery_session_operations_reject_invalid_ids_without_touching_outside_root() {
    let base = tmp();
    fs::create_dir_all(&base).unwrap();
    let root = base.join("buffer");
    fs::create_dir_all(&root).unwrap();
    let sentinel = base.join("outside.txt");
    fs::write(&sentinel, b"sentinel-data").unwrap();

    for id in ["../outside", "..\\outside", "C:\\outside"] {
        let recover_err = recover_session(&root, id).unwrap_err();
        assert_eq!(recover_err.kind(), io::ErrorKind::InvalidInput);
        let discard_err = discard_session(&root, id, Disposal::Permanent).unwrap_err();
        assert_eq!(discard_err.kind(), io::ErrorKind::InvalidInput);
    }

    assert!(sentinel.exists());
    assert_eq!(fs::read(&sentinel).unwrap(), b"sentinel-data");
}

#[test]
fn free_bytes_fails_for_a_directory_that_does_not_exist_yet() {
    // The premise task156's fix rests on: `GetDiskFreeSpaceExW` answers
    // ERROR_PATH_NOT_FOUND rather than walking up to the nearest existing
    // ancestor, so the buffer root has to be created before it is measured. If
    // this ever starts returning Ok, the create-first ordering in
    // `capture::ensure_buffer_root_has_space` is no longer load-bearing.
    let base = tmp();
    let missing = base.join("definitely-not-created");
    assert!(free_bytes(&missing).is_err());

    // ...and the same path answers once it exists.
    fs::create_dir_all(&missing).unwrap();
    assert!(free_bytes(&missing).unwrap() > 0);
}
