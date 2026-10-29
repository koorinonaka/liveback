//! Crash-recovery, startup-sweep and segment-path resolution tests.
use super::*;
use crate::ring_buffer::container::ContainerBuilder;

/// A container a crash left behind: segments and a checkpoint, but no `Closed`
/// record. `segments` of them, each 2 s.
fn crashed_container(root: &Path, session_id: &str, segments: u64) -> PathBuf {
    fs::create_dir_all(root).unwrap();
    let path = root.join(format!("{session_id}.{CONTAINER_EXTENSION}"));
    let mut builder = ContainerBuilder::new(session_id);
    for index in 0..segments {
        let start = index as i64 * 20_000_000;
        builder = builder.segment(index, start, start + 20_000_000, &[0xC0; 64]);
    }
    builder.checkpoint().build(&path).unwrap();
    path
}

/// Backward compatibility: a manifest written before Task067 (or by any
/// process that recorded an absolute path) must keep resolving correctly
/// with no migration and no `MANIFEST_VERSION` bump.
#[test]
fn resolve_segment_path_passes_an_existing_absolute_path_through_unchanged() {
    let root = tmp();
    let ring = ring_at(&root, "x", 15);
    let elsewhere = std::env::temp_dir()
        .join("livia-elsewhere")
        .join("segment.mp4");
    let resolved = ring.resolve_segment_path(&elsewhere);
    assert_eq!(resolved, elsewhere);
}

// ---- startup repair sweep (task164) ----

/// A closed, current-version session as the listing reports it.
fn healthy() -> SessionSummary {
    SessionSummary {
        session_id: "capture-00000000000000000000000000000001".into(),
        start_100ns: Some(0),
        end_100ns: Some(20_000_000),
        segment_count: 2,
        total_bytes: 4096,
        closed: true,
        recoverable: false,
        manifest_version: MANIFEST_VERSION,
        readable: true,
        target_title: None,
        thumbnail_index: None,
        note: None,
        protected: false,
        target_executable: None,
        target_executable_path: None,
        marker_count: 0,
    }
}

/// A closed, current session is left alone; the three shapes a crash leaves
/// behind, all of which have real segments, become ordinary recordings rather
/// than questions for the user.
#[test]
fn classify_session_leaves_intact_ones_and_repairs_every_crash_shape() {
    assert_eq!(classify_session(&healthy()), SessionRepair::Intact);

    // The recording marker survived the crash.
    let mut marker_left = healthy();
    marker_left.closed = false;
    marker_left.recoverable = true;
    assert_eq!(classify_session(&marker_left), SessionRepair::Recoverable);

    // A manifest was persisted mid-recording and never closed.
    let mut never_closed = healthy();
    never_closed.closed = false;
    assert_eq!(classify_session(&never_closed), SessionRepair::Recoverable);

    // Version 0 / unreadable, but with segments: still repaired.
    let mut no_manifest = healthy();
    no_manifest.closed = false;
    no_manifest.readable = false;
    no_manifest.manifest_version = 0;
    assert_eq!(classify_session(&no_manifest), SessionRepair::Recoverable);
}

#[test]
fn a_manifest_this_build_cannot_read_and_segmentless_debris_are_discarded() {
    // An older manifest version: its segment layout is unknown, and rebuilding
    // one from the mp4 files would silently drop its markers and title.
    let mut old_version = healthy();
    old_version.readable = false;
    old_version.manifest_version = MANIFEST_VERSION - 1;
    assert_eq!(classify_session(&old_version), SessionRepair::Unsalvageable);
    // Even a *closed* one: readable is what decides whether it can be listed.
    assert!(old_version.closed);

    // Nothing but partial work: no segment to recover.
    let mut debris = healthy();
    debris.closed = false;
    debris.readable = false;
    debris.manifest_version = 0;
    debris.segment_count = 0;
    assert_eq!(classify_session(&debris), SessionRepair::Unsalvageable);
}

/// End to end over a real buffer root: one crash-interrupted container with a
/// segment comes back as a normal closed recording, and one holding no segment
/// at all is gone afterwards.
#[test]
fn the_sweep_repairs_one_session_and_removes_the_other() {
    let root = std::env::temp_dir().join(format!("livia-sweep-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    crashed_container(&root, "capture-00000000000000000000000000000001", 1);
    let debris = crashed_container(&root, "capture-00000000000000000000000000000002", 0);

    assert_eq!(repair_sessions(&root).unwrap(), 2);

    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 1, "the segmentless container must be gone");
    assert!(!debris.exists());
    assert!(listed[0].closed, "a repaired session lists as closed");
    assert!(listed[0].readable);
    assert_eq!(listed[0].segment_count, 1);

    // Idempotent: the second pass has nothing left to do.
    assert_eq!(repair_sessions(&root).unwrap(), 0);
    let _ = fs::remove_dir_all(&root);
}

/// Task3100: the startup sweep leaves `<buffer_root>\Clips` alone, in both of
/// the shapes that used to destroy it, while still doing its job to the
/// sessions beside it.
///
/// The two paths the defect took, from the same `list_sessions` row:
/// a clip folder with mp4s classified `Recoverable`, so `repair_sessions`
/// wrote a `manifest.json` into it (and `sweep_legacy_sessions` deleted the
/// folder on the *next* launch); an empty one classified `Unsalvageable`, so
/// `repair_sessions` deleted it outright in the same launch. Two passes here,
/// because the first defect needed two.
#[test]
fn the_sweep_leaves_the_clip_folder_alone_while_still_repairing_real_sessions() {
    let root = std::env::temp_dir().join(format!("livia-task3100-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let clips = root.join("Clips");
    fs::create_dir_all(&clips).unwrap();
    for n in 0..4 {
        fs::write(
            clips.join(format!("verify-{n}.mp4")),
            b"a clip the user kept",
        )
        .unwrap();
    }
    // Regression company, in the same root: one crash-interrupted session that
    // must still be recovered, and one segmentless one that must still be
    // discarded.
    crashed_container(&root, "capture-00000000000000000000000000000001", 1);
    let debris = crashed_container(&root, "capture-00000000000000000000000000000002", 0);

    let clips_are_intact = |what: &str| {
        assert!(clips.is_dir(), "the clip folder survives {what}");
        for n in 0..4 {
            assert!(
                clips.join(format!("verify-{n}.mp4")).is_file(),
                "clip {n} must still be there after {what}"
            );
        }
    };

    assert_eq!(
        repair_sessions(&root).unwrap(),
        2,
        "the two real sessions are still swept; Clips is not one of them"
    );
    assert!(
        !clips.join("manifest.json").exists(),
        "the sweep must not write a manifest into the clip folder"
    );
    assert_eq!(repair_sessions(&root).unwrap(), 0);
    clips_are_intact("two passes over a manifest-less clip folder");

    // And the other post-defect state: the folder as a machine that already hit
    // this bug has it, carrying the manifest the old scan left behind. That
    // residue must neither be read nor rewritten, and must not condemn it.
    fs::write(clips.join("manifest.json"), b"{}").unwrap();
    assert_eq!(repair_sessions(&root).unwrap(), 0);
    assert_eq!(repair_sessions(&root).unwrap(), 0);
    clips_are_intact("two more passes with a leftover manifest");
    assert_eq!(
        fs::read(clips.join("manifest.json")).unwrap(),
        b"{}",
        "the leftover manifest is neither read nor rewritten"
    );

    assert!(!debris.exists(), "a real debris session is still discarded");
    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 1, "only the recovered session lists");
    assert_eq!(
        listed[0].session_id,
        "capture-00000000000000000000000000000001"
    );
    assert!(listed[0].closed, "a repaired session lists as closed");

    let _ = fs::remove_dir_all(&root);
}

/// The other half of task3100: an **empty** `Clips` folder -- the state a fresh
/// install is in before the first clip is saved -- used to be classified
/// `Unsalvageable` and deleted inside a single launch (observed three times on
/// 2026-09-06).
#[test]
fn an_empty_clip_folder_survives_the_startup_sweep() {
    let root = std::env::temp_dir().join(format!("livia-task3100-empty-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let clips = root.join("Clips");
    fs::create_dir_all(&clips).unwrap();

    assert_eq!(repair_sessions(&root).unwrap(), 0);
    assert!(clips.is_dir(), "an empty clip folder is not debris");
    assert!(list_sessions(&root).unwrap().is_empty());

    let _ = fs::remove_dir_all(&root);
}
