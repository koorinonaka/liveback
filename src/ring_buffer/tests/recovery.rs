//! Crash-recovery and segment-path resolution tests.
use super::*;

// --- Minimal fMP4 fixture builders for task082's duration tests, mirroring
// encoder::mp4_boxes's own private test builders (not reusable across the
// module boundary) at the reduced scope this module actually needs: one
// video trak plus one moof/traf/trun carrying a single sample duration.
fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}
fn make_video_trak(track_id: u32, timescale: u32) -> Vec<u8> {
    let mut tkhd_payload = vec![0u8; 16];
    tkhd_payload[12..16].copy_from_slice(&track_id.to_be_bytes());
    let mut mdhd_payload = vec![0u8; 16];
    mdhd_payload[12..16].copy_from_slice(&timescale.to_be_bytes());
    let mut hdlr_payload = vec![0u8; 12];
    hdlr_payload[8..12].copy_from_slice(b"vide");
    let mdia_payload = [
        make_box(b"mdhd", &mdhd_payload),
        make_box(b"hdlr", &hdlr_payload),
    ]
    .concat();
    let trak_payload = [
        make_box(b"tkhd", &tkhd_payload),
        make_box(b"mdia", &mdia_payload),
    ]
    .concat();
    make_box(b"trak", &trak_payload)
}
fn make_video_moof(track_id: u32, sample_duration_ticks: u32) -> Vec<u8> {
    let mut tfhd_payload = vec![0u8; 8];
    tfhd_payload[4..8].copy_from_slice(&track_id.to_be_bytes());
    let mut trun_payload = Vec::new();
    trun_payload.extend_from_slice(&0x0000_0100u32.to_be_bytes()); // sample-duration-present
    trun_payload.extend_from_slice(&1u32.to_be_bytes()); // sample_count
    trun_payload.extend_from_slice(&sample_duration_ticks.to_be_bytes());
    let traf_payload = [
        make_box(b"tfhd", &tfhd_payload),
        make_box(b"trun", &trun_payload),
    ]
    .concat();
    make_box(b"moof", &make_box(b"traf", &traf_payload))
}
/// A single-video-track fMP4 segment carrying one sample of
/// `duration_ticks` at `timescale`, structurally minimal but with real
/// `trun`/`mdhd` box fields -- exactly what
/// `recovered_segment_duration_100ns` (via
/// `encoder::dump_segment_track_summary`) reads.
fn synthetic_segment_bytes(timescale: u32, duration_ticks: u32) -> Vec<u8> {
    let track_id = 1;
    let moov_payload = make_video_trak(track_id, timescale);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&make_box(b"ftyp", b"isomiso2mp41"));
    bytes.extend_from_slice(&make_box(b"moov", &moov_payload));
    bytes.extend_from_slice(&make_video_moof(track_id, duration_ticks));
    bytes.extend_from_slice(&make_box(b"mdat", &[0u8; 8]));
    bytes
}

#[test]
fn recovered_segment_duration_100ns_converts_track_timescale_to_100ns() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("segment.mp4");
    // 30_000 ticks at a 60_000 timescale is 0.5s = 5_000_000 (100ns units).
    fs::write(&path, synthetic_segment_bytes(60_000, 30_000)).unwrap();
    assert_eq!(recovered_segment_duration_100ns(&path), Some(5_000_000));
}

#[test]
fn recovered_segment_duration_100ns_is_none_for_unparseable_data() {
    let dir = tmp();
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("segment.mp4");
    fs::write(&path, b"not an mp4 at all").unwrap();
    assert_eq!(recovered_segment_duration_100ns(&path), None);
}

/// Task082's actual goal, exercised end-to-end: two segments whose real
/// durations both differ from each other and from the fixed
/// `SEGMENT_DURATION_100NS` (2s) estimate `recover_session` used before
/// this task. The recovered timeline must reflect the real, unequal
/// durations, not `i * SEGMENT_DURATION_100NS`.
#[test]
fn recover_session_uses_each_segments_real_duration_not_the_fixed_estimate() {
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    fs::create_dir_all(&session_dir).unwrap();
    // 0.5s (segment 0) and 1.3s (segment 1) -- both far from the fixed 2s
    // estimate, and unequal to each other so a bug that reused segment
    // 0's duration for segment 1 (or vice versa) would also be caught.
    fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        synthetic_segment_bytes(60_000, 30_000),
    )
    .unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000001.mp4"),
        synthetic_segment_bytes(60_000, 78_000),
    )
    .unwrap();

    let manifest = recover_session(&root, "capture-recovered").unwrap();
    assert_eq!(manifest.segments.len(), 2);
    let first = &manifest.segments[0];
    let second = &manifest.segments[1];
    assert_eq!(first.start_100ns, 0);
    assert_eq!(
        first.end_100ns, 5_000_000,
        "0.5s, not the fixed 2s estimate"
    );
    assert_eq!(
        second.start_100ns, first.end_100ns,
        "second segment must start exactly where the first's real duration ended"
    );
    assert_eq!(
        second.end_100ns,
        first.end_100ns + 13_000_000,
        "1.3s, not the fixed 2s estimate"
    );
}

/// A segment this app can't parse (crash-truncated mid-write, or any
/// other malformed fMP4) must not fail the whole recovery: it falls back
/// to the fixed `SEGMENT_DURATION_100NS` estimate and the surrounding
/// segments' real durations are still used.
#[test]
fn recover_session_falls_back_to_fixed_estimate_for_an_unparseable_segment() {
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        synthetic_segment_bytes(60_000, 30_000), // 0.5s
    )
    .unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000001.mp4"),
        b"truncated mid-write, not valid fMP4",
    )
    .unwrap();

    let manifest = recover_session(&root, "capture-recovered").unwrap();
    assert_eq!(manifest.segments.len(), 2);
    let first = &manifest.segments[0];
    let second = &manifest.segments[1];
    assert_eq!(first.end_100ns, 5_000_000);
    assert_eq!(
        second.end_100ns - second.start_100ns,
        encoder::SEGMENT_DURATION_100NS,
        "unparseable segment must fall back to the fixed estimate, not abort recovery"
    );
}

#[test]
fn recover_session_persists_manifest_json_so_load_session_can_open_it() {
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        b"first",
    )
    .unwrap();

    let manifest = recover_session(&root, "capture-recovered").unwrap();
    assert!(
        session_dir.join("manifest.json").exists(),
        "recover_session must write manifest.json so load_session can find it"
    );
    assert!(!session_dir.join("manifest.tmp").exists());

    let ring = RingBuffer::open(session_dir.clone()).expect(
            "load_session (RingBuffer::open) must succeed once recover_session has persisted a manifest",
        );
    assert_eq!(ring.manifest(), &manifest);
    assert!(!ring.manifest().closed);
}

#[test]
fn recover_session_is_idempotent_once_manifest_json_exists() {
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        b"first",
    )
    .unwrap();

    let first = recover_session(&root, "capture-recovered").unwrap();
    // A second "この候補を復旧" click on an already-recovered session must
    // take the RingBuffer::open branch (manifest.json already exists)
    // rather than rebuilding from directory contents, and return the
    // same manifest.
    let second = recover_session(&root, "capture-recovered").unwrap();
    assert_eq!(first, second);
}

#[test]
fn recover_session_removes_the_recording_marker_so_it_stops_being_a_candidate() {
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("recording.marker"), b"recording").unwrap();
    fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        b"first",
    )
    .unwrap();

    assert!(session_dir.join("recording.marker").exists());
    recover_session(&root, "capture-recovered").unwrap();
    assert!(
        !session_dir.join("recording.marker").exists(),
        "recover_session must remove recording.marker: load_session's closed:false \
             exception and the recoverable-candidate listing both key off its absence"
    );
}

#[test]
fn recover_session_removes_the_recording_marker_even_when_manifest_json_already_exists() {
    // The common real-world crash: at least one periodic `persist()` ran
    // before the crash, so manifest.json (closed: false) already exists
    // and recover_session takes the RingBuffer::open early-return branch
    // rather than reconstructing from directory contents. That branch
    // must remove recording.marker too, or load_session's closed:false
    // exception never actually fires for most crashed sessions.
    let root = tmp();
    let session_dir = root.join("capture-recovered");
    let ring =
        RingBuffer::create(session_dir.clone(), "capture-recovered".into(), 15, None).unwrap();
    drop(ring);
    assert!(session_dir.join("manifest.json").exists());
    assert!(session_dir.join("recording.marker").exists());

    let manifest = recover_session(&root, "capture-recovered").unwrap();
    assert!(!manifest.closed);
    assert!(
        !session_dir.join("recording.marker").exists(),
        "recover_session must remove recording.marker even when it only had to \
             open an already-persisted manifest.json"
    );
}

/// Backward compatibility: a manifest written before Task067 (or by any
/// process that recorded an absolute path) must keep resolving correctly
/// with no migration and no `MANIFEST_VERSION` bump.
#[test]
fn resolve_segment_path_passes_an_existing_absolute_path_through_unchanged() {
    let root = tmp();
    let ring = RingBuffer::create(root.to_path_buf(), "x".into(), 15, None).unwrap();
    let elsewhere = std::env::temp_dir()
        .join("livia-elsewhere")
        .join("segment.mp4");
    let resolved = ring.resolve_segment_path(&elsewhere);
    assert_eq!(resolved, elsewhere);
}

/// A relative `SegmentRecord.path` can never legitimately contain `..`
/// (this app only ever writes a bare `file_name()`), but `resolve_segment
/// _path` itself is a plain, non-validating `Path::join` — the guard
/// against a tampered/corrupted manifest escaping the session root lives
/// in the callers (`read_review_segment`, `validate_segment_paths`), which
/// must reject `..` explicitly since `starts_with` alone is lexical and
/// would not catch it. This documents what `resolve_segment_path` itself
/// does with such an input (no rejection) so that contract stays visible.
#[test]
fn resolve_segment_path_does_not_itself_reject_parent_dir_components() {
    let root = tmp();
    let ring = RingBuffer::create(root.to_path_buf(), "x".into(), 15, None).unwrap();
    let traversal = PathBuf::from("../../outside.mp4");
    let resolved = ring.resolve_segment_path(&traversal);
    assert!(
        resolved
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "resolve_segment_path does not normalize `..`; callers must reject it themselves"
    );
}

// ---- startup repair sweep (task164) ----

/// A closed, current-version session as `summarize_session_dir` would report it.
fn healthy() -> SessionSummary {
    SessionSummary {
        session_id: "capture-00000000000000000000000000000001".into(),
        start_100ns: Some(0),
        end_100ns: Some(20_000_000),
        segment_count: 2,
        total_bytes: 4096,
        closed: true,
        has_partial: false,
        recoverable: false,
        manifest_version: MANIFEST_VERSION,
        readable: true,
        target_title: None,
        thumbnail_index: None,
        note: None,
        protected: false,
    }
}

#[test]
fn a_closed_current_session_is_left_alone() {
    assert_eq!(classify_session(&healthy()), SessionRepair::Intact);
}

/// The three shapes a crash leaves behind, all of which have real segments and
/// so become ordinary recordings rather than questions for the user.
#[test]
fn every_crash_shape_with_segments_is_repaired() {
    // The recording marker survived the crash.
    let mut marker_left = healthy();
    marker_left.closed = false;
    marker_left.recoverable = true;
    assert_eq!(classify_session(&marker_left), SessionRepair::Recoverable);

    // A manifest was persisted mid-recording and never closed.
    let mut never_closed = healthy();
    never_closed.closed = false;
    assert_eq!(classify_session(&never_closed), SessionRepair::Recoverable);

    // No manifest at all, but the segments are on disk -- `summarize_session_dir`
    // reports version 0 / unreadable for exactly this case.
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

    // Only `.partial.mp4` left, which `summarize_session_dir` never counts.
    let mut debris = healthy();
    debris.closed = false;
    debris.readable = false;
    debris.manifest_version = 0;
    debris.segment_count = 0;
    debris.has_partial = true;
    assert_eq!(classify_session(&debris), SessionRepair::Unsalvageable);
}

/// End to end over a real directory: one crash-interrupted session with a
/// segment comes back as a normal closed recording, and a directory holding
/// nothing but debris is gone afterwards.
#[test]
fn the_sweep_repairs_one_session_and_removes_the_other() {
    let root = std::env::temp_dir().join(format!("livia-sweep-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let crashed = root.join("capture-00000000000000000000000000000001");
    fs::create_dir_all(&crashed).unwrap();
    fs::write(crashed.join("segment-00000000000000000000.mp4"), b"data").unwrap();
    fs::write(crashed.join("recording.marker"), b"").unwrap();

    let debris = root.join("capture-00000000000000000000000000000002");
    fs::create_dir_all(&debris).unwrap();
    fs::write(
        debris.join("segment-00000000000000000000.partial.mp4"),
        b"x",
    )
    .unwrap();

    assert_eq!(repair_sessions(&root).unwrap(), 2);

    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 1, "the debris directory must be gone");
    assert!(listed[0].closed, "a repaired session lists as closed");
    assert!(listed[0].readable);
    assert_eq!(listed[0].segment_count, 1);
    assert!(
        !crashed.join("recording.marker").exists(),
        "recovery clears the still-open signal"
    );

    // Idempotent: the second pass has nothing left to do.
    assert_eq!(repair_sessions(&root).unwrap(), 0);
    let _ = fs::remove_dir_all(&root);
}

/// Task3100: the startup sweep leaves `<buffer_root>\Clips` alone, in both of
/// the shapes that used to destroy it, while still doing its job to the
/// `capture-*` directories beside it.
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
    // must still be recovered, and one debris-only directory that must still be
    // discarded.
    let crashed = root.join("capture-00000000000000000000000000000001");
    fs::create_dir_all(&crashed).unwrap();
    fs::write(crashed.join("segment-00000000000000000000.mp4"), b"data").unwrap();
    fs::write(crashed.join("recording.marker"), b"").unwrap();
    let debris = root.join("capture-00000000000000000000000000000002");
    fs::create_dir_all(&debris).unwrap();
    fs::write(
        debris.join("segment-00000000000000000000.partial.mp4"),
        b"x",
    )
    .unwrap();

    let clips_are_intact = |what: &str| {
        assert!(clips.is_dir(), "the clip folder survives {what}");
        for n in 0..4 {
            assert!(
                clips.join(format!("verify-{n}.mp4")).is_file(),
                "clip {n} must still be there after {what}"
            );
        }
    };

    // Pass 1 is the launch that used to write `Clips\manifest.json`; pass 2 is
    // the launch that then deleted the folder for having one.
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
    assert!(
        !clips.join("manifest.json").exists(),
        "still no manifest after the second pass"
    );

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

    assert!(
        !debris.exists(),
        "a real debris directory is still discarded"
    );
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

/// Task470: recovery deletes the `.partial.mp4` a crash left behind, and
/// touches nothing else.
///
/// Both branches of `recover_session` are exercised -- the one that finds a
/// manifest already on disk and the one that rebuilds it -- because only the
/// second had any file-handling code at all, and the first is where the debris
/// actually accumulated.
#[test]
fn recovery_deletes_partial_debris_and_keeps_everything_else() {
    let root = std::env::temp_dir().join(format!("livia-task470-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    // (a) manifest reconstructed from the files: no manifest.json present.
    let rebuilt = root.join("capture-00000000000000000000000000000001");
    fs::create_dir_all(&rebuilt).unwrap();
    fs::write(
        rebuilt.join("segment-00000000000000000000.mp4"),
        b"finished",
    )
    .unwrap();
    fs::write(rebuilt.join("segment-00000000000000000000.jpg"), b"thumb").unwrap();
    fs::write(
        rebuilt.join("segment-00000000000000000001.partial.mp4"),
        b"debris",
    )
    .unwrap();
    fs::write(rebuilt.join("recording.marker"), b"").unwrap();

    recover_session(&root, "capture-00000000000000000000000000000001").unwrap();

    assert!(
        !rebuilt
            .join("segment-00000000000000000001.partial.mp4")
            .exists(),
        "the partial must be gone"
    );
    // The important half: nothing else moved.
    assert_eq!(
        fs::read(rebuilt.join("segment-00000000000000000000.mp4")).unwrap(),
        b"finished",
        "the finished segment must survive untouched"
    );
    assert_eq!(
        fs::read(rebuilt.join("segment-00000000000000000000.jpg")).unwrap(),
        b"thumb",
        "the thumbnail must survive"
    );
    assert!(
        rebuilt.join("manifest.json").is_file(),
        "recovery still wrote the manifest it rebuilt"
    );

    // (b) manifest already on disk -- the branch that used to return early.
    let existing = root.join("capture-00000000000000000000000000000002");
    fs::create_dir_all(&existing).unwrap();
    fs::write(
        existing.join("segment-00000000000000000000.mp4"),
        b"finished",
    )
    .unwrap();
    fs::write(
        existing.join("segment-00000000000000000007.partial.mp4"),
        b"debris",
    )
    .unwrap();
    fs::write(existing.join("recording.marker"), b"").unwrap();
    // Build a real manifest by recovering once, then plant fresh debris and
    // recover again: the second pass takes the has-manifest branch.
    recover_session(&root, "capture-00000000000000000000000000000002").unwrap();
    assert!(existing.join("manifest.json").is_file());
    fs::write(
        existing.join("segment-00000000000000000009.partial.mp4"),
        b"more",
    )
    .unwrap();

    recover_session(&root, "capture-00000000000000000000000000000002").unwrap();

    assert!(
        !existing
            .join("segment-00000000000000000009.partial.mp4")
            .exists(),
        "the has-manifest branch must drop debris too"
    );
    assert_eq!(
        fs::read(existing.join("segment-00000000000000000000.mp4")).unwrap(),
        b"finished"
    );
    assert!(existing.join("manifest.json").is_file());

    let _ = fs::remove_dir_all(&root);
}

/// Task470 Verification, taken headlessly: the count of partials across a
/// buffer root drops when the sweep runs, and the count of everything else
/// does not move.
///
/// A fixture root rather than the real one, deliberately. The five partials
/// sitting in `Videos\Liveback` belong to sessions that are `closed` and
/// `readable`, so `classify_session` calls them `Intact` and the sweep never
/// reaches them -- this fix is forward-looking, and "watch the real number
/// drop" is not an observation anyone can make.
#[test]
fn the_sweep_reduces_the_partial_count_without_touching_finished_files() {
    let root = std::env::temp_dir().join(format!("livia-task470-sweep-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let count = |suffix: &str| -> usize {
        let mut total = 0;
        for session in fs::read_dir(&root).unwrap().flatten() {
            for entry in fs::read_dir(session.path()).unwrap().flatten() {
                if entry.file_name().to_string_lossy().ends_with(suffix) {
                    total += 1;
                }
            }
        }
        total
    };

    for index in 1..=3u32 {
        let session = root.join(format!("capture-{index:032}"));
        fs::create_dir_all(&session).unwrap();
        fs::write(
            session.join("segment-00000000000000000000.mp4"),
            b"finished",
        )
        .unwrap();
        fs::write(session.join("segment-00000000000000000000.jpg"), b"thumb").unwrap();
        fs::write(
            session.join("segment-00000000000000000001.partial.mp4"),
            b"debris",
        )
        .unwrap();
        fs::write(session.join("recording.marker"), b"").unwrap();
    }

    let partials_before = count(".partial.mp4");
    let finished_before = count(".mp4") - partials_before;
    let thumbs_before = count(".jpg");
    assert_eq!(partials_before, 3);
    assert_eq!(finished_before, 3);

    repair_sessions(&root).unwrap();

    let partials_after = count(".partial.mp4");
    let finished_after = count(".mp4") - partials_after;
    println!(
        "partials {partials_before} -> {partials_after}, finished {finished_before} -> \
         {finished_after}, thumbnails {thumbs_before} -> {}",
        count(".jpg")
    );
    assert_eq!(partials_after, 0, "the sweep must clear the debris");
    assert_eq!(
        finished_after, finished_before,
        "the sweep must not touch finished segments"
    );
    assert_eq!(count(".jpg"), thumbs_before, "nor thumbnails");
    assert_eq!(
        list_sessions(&root).unwrap().len(),
        3,
        "and no session may be removed"
    );

    let _ = fs::remove_dir_all(&root);
}
