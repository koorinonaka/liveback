//! Retention, lease, prune, gap and manifest-version tests.
use super::*;

#[test]
fn retention_and_lease() {
    let p = tmp();
    let mut r = RingBuffer::create(p.to_path_buf(), "x".into(), 5, None).unwrap();
    for i in 0..152 {
        fs::write(p.join(format!("{i}.mp4")), b"x").unwrap();
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
    let mut r = RingBuffer::create(p.to_path_buf(), "x".into(), NO_RETENTION_LIMIT, None).unwrap();
    assert_eq!(r.manifest().retention_minutes, NO_RETENTION_LIMIT);
    for i in 0..152 {
        fs::write(p.join(format!("{i}.mp4")), b"x").unwrap();
        r.add(seg(i)).unwrap()
    }
    assert_eq!(r.prune().unwrap(), None);
    assert_eq!(r.manifest().segments.len(), 152);
    assert!(p.join("0.mp4").exists());
}
#[test]
fn prune_deletes_thumbnail_alongside_segment() {
    let p = tmp();
    let mut r = RingBuffer::create(p.to_path_buf(), "x".into(), 5, None).unwrap();
    for i in 0..152 {
        fs::write(p.join(format!("{i}.mp4")), b"x").unwrap();
        let mut s = seg(i);
        if i == 0 {
            fs::write(p.join("0.jpg"), b"jpg").unwrap();
            s.thumbnail_path = Some("0.jpg".into());
        }
        r.add(s).unwrap()
    }
    r.prune().unwrap();
    assert!(!r.manifest().segments.iter().any(|s| s.index == 0));
    assert!(!p.join("0.mp4").exists());
    assert!(!p.join("0.jpg").exists());
}
#[test]
fn prune_tolerates_a_thumbnail_path_whose_file_is_already_gone() {
    let p = tmp();
    let mut r = RingBuffer::create(p.to_path_buf(), "x".into(), 5, None).unwrap();
    for i in 0..152 {
        fs::write(p.join(format!("{i}.mp4")), b"x").unwrap();
        let mut s = seg(i);
        if i == 0 {
            // Stale reference to a thumbnail file that never made it to
            // disk (write failure, or a pre-Task066 recovered segment):
            // deletion must not error.
            s.thumbnail_path = Some("0.jpg".into());
        }
        r.add(s).unwrap()
    }
    r.prune().unwrap();
    assert!(!r.manifest().segments.iter().any(|s| s.index == 0));
    assert!(!p.join("0.mp4").exists());
}
#[test]
fn open_drops_records_whose_segment_file_is_gone() {
    let p = tmp();
    let mut r = RingBuffer::create(p.to_path_buf(), "x".into(), 5, None).unwrap();
    for i in 0..3 {
        fs::write(p.join(format!("{i}.mp4")), b"x").unwrap();
        r.add(seg(i)).unwrap()
    }
    // What a crash between `prune`'s delete and its `persist` leaves behind
    // (and what task226's re-add bug wrote wholesale): a manifest advertising
    // a segment that is not on disk.
    fs::remove_file(p.join("1.mp4")).unwrap();
    let reopened = RingBuffer::open(p.to_path_buf()).unwrap();
    assert_eq!(
        reopened
            .manifest()
            .segments
            .iter()
            .map(|s| s.index)
            .collect::<Vec<_>>(),
        vec![0, 2]
    );
    // ...and the hole it leaves is reported as the gap it is.
    assert_eq!(reopened.manifest().gaps.len(), 1);
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

/// t260929-e171: `add` stamps when it folded, which the review screen's load
/// reads as the live segment's phase: none before any fold, and it advances
/// with each one.
#[test]
fn adding_a_segment_stamps_when_it_was_folded() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "x".into(), 15, None).unwrap();
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
fn opening_an_old_manifest_rebuilds_missing_gaps() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "x".into(), 15, None).unwrap();
    // Both segments need their file on disk: `open` drops records whose mp4 is
    // gone, and a dropped pair would leave no gap to rebuild.
    fs::write(root.join("0.mp4"), b"x").unwrap();
    fs::write(root.join("1.mp4"), b"x").unwrap();
    ring.add(seg(0)).unwrap();
    ring.add(SegmentRecord {
        start_100ns: 35_000_000,
        end_100ns: 55_000_000,
        ..seg(1)
    })
    .unwrap();
    let mut manifest = ring.manifest().clone();
    manifest.gaps.clear();
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let reopened = RingBuffer::open(root.to_path_buf()).unwrap();
    assert_eq!(reopened.manifest().gaps.len(), 1);
    assert_eq!(reopened.manifest().gaps[0].start_100ns, 20_000_000);
    assert_eq!(reopened.manifest().gaps[0].end_100ns, 35_000_000);
}

#[test]
fn old_manifest_version_is_incompatible_but_does_not_crash() {
    let root = tmp();
    let mut ring = RingBuffer::create(root.to_path_buf(), "x".into(), 15, None).unwrap();
    ring.add(seg(0)).unwrap();
    let mut manifest = ring.manifest().clone();
    manifest.version = MANIFEST_VERSION - 1;
    fs::write(
        root.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let result = RingBuffer::open(root.to_path_buf());
    assert!(result.is_err(), "old version must not reopen");
    assert_eq!(result.err().unwrap().kind(), io::ErrorKind::InvalidData);

    let recovered = recover_session(
        root.parent().unwrap(),
        root.file_name().unwrap().to_str().unwrap(),
    );
    assert!(
        recovered.is_err(),
        "recover_session must reject an old manifest version instead of crashing"
    );

    assert!(discard_session(
        root.parent().unwrap(),
        root.file_name().unwrap().to_str().unwrap(),
        Disposal::Permanent
    )
    .is_ok());
    assert!(!root.exists());
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

/// Task067 regression: `recover_session`'s manifest-less-directory branch
/// writes `SegmentRecord.path` as a bare `file_name()` (relative). Before
/// Task067, every reader of `SegmentRecord.path` (`read_review_segment`,
/// `export::plan::validate_segment_paths`) required an absolute path,
/// so a recovered session's segments could never actually be read.
/// `resolve_segment_path` is the fix: it joins a relative path onto the
/// session root, and — because `Path::join` treats an absolute argument
/// as replacing the base entirely — passes an already-absolute path
/// (any pre-Task067 manifest) through unchanged, with no format
/// detection needed.
#[test]
fn recover_session_paths_resolve_to_readable_segment_bytes() {
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("recording.marker"), b"recording").unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        b"first",
    )
    .unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000001.mp4"),
        b"second",
    )
    .unwrap();
    // A quarantined/partial fragment must never surface as a segment.
    fs::write(
        session_dir.join("segment-00000000000000000002.partial.mp4"),
        b"unfinished",
    )
    .unwrap();

    let manifest = recover_session(&root, "capture-recovered").unwrap();
    assert_eq!(
        manifest.segments.len(),
        2,
        "partial fragment must be excluded"
    );

    let ring = RingBuffer {
        store: SessionStore::new(session_dir.clone()),
        container: None,
        manifest: manifest.clone(),
        leases: HashMap::new(),
        spans: HashMap::new(),
        last_fold: None,
    };
    for segment in &manifest.segments {
        assert!(
            !segment.path.is_absolute(),
            "recover_session must record a session-relative path: {:?}",
            segment.path
        );
        let resolved = ring.resolve_segment_path(&segment.path);
        assert!(resolved.starts_with(&session_dir));
        assert!(resolved.is_file());
    }
    let bytes: Vec<Vec<u8>> = manifest
        .segments
        .iter()
        .map(|segment| fs::read(ring.resolve_segment_path(&segment.path)).unwrap())
        .collect();
    assert!(bytes.contains(&b"first".to_vec()));
    assert!(bytes.contains(&b"second".to_vec()));
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
