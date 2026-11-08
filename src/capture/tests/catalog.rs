//! Lifecycle status and load_session catalog tests.
use super::*;

/// What the screen says for this stop (t261101-5ff3: the status carries the
/// code, `ui_state::lifecycle` the words).
fn stop_text(status: &CaptureLifecycleStatus) -> &'static str {
    crate::ui_state::lifecycle::stop_message(
        crate::ui_state::locale::Locale::Ja,
        status.diagnostic_code.as_deref().unwrap(),
    )
    .unwrap()
}

#[test]
fn lifecycle_status_uses_anonymous_safe_stop_codes() {
    let source_closed = CaptureLifecycleStatus::stopped(CaptureStopReason::SourceClosed);
    assert_eq!(source_closed.state, "stopped");
    assert_eq!(
        source_closed.diagnostic_code.as_deref(),
        Some("CAP-TGT-001")
    );
    assert!(!stop_text(&source_closed).contains("0x"));
    let device_lost = CaptureLifecycleStatus::stopped(CaptureStopReason::InitializationFailed {
        stage: VIDEO_ENCODER_STAGE,
    });
    assert_eq!(device_lost.diagnostic_code.as_deref(), Some("CAP-DEV-001"));
    // The stage rides along so the UI can name the failure (task193).
    assert_eq!(
        device_lost.failure_stage.as_deref(),
        Some(VIDEO_ENCODER_STAGE)
    );
    // Task2150: the audio session dying mid-recording is the same class of stop
    // -- CAP-DEV-001 with a stage the UI can read -- and it has to arrive with
    // its own stage, not the generic `start_capture` the worker was still
    // holding, or the toast goes back to blaming the capture target.
    let audio_death = CaptureLifecycleStatus::stopped(CaptureStopReason::InitializationFailed {
        stage: AUDIO_SESSION_STAGE,
    });
    assert_eq!(audio_death.diagnostic_code.as_deref(), Some("CAP-DEV-001"));
    assert_eq!(
        audio_death.failure_stage.as_deref(),
        Some(AUDIO_SESSION_STAGE)
    );
    assert!(!stop_text(&audio_death).contains("0x"));
    // t260911-77c3: every other mid-recording death arrives the same way. The
    // worker used to hand the UI the `start_capture` it was still holding, which
    // is why the 2026-09-11 incident's toast blamed the capture target for a
    // sink that refused a write.
    let mid_recording = CaptureLifecycleStatus::stopped(CaptureStopReason::InitializationFailed {
        stage: MID_RECORDING_STAGE,
    });
    assert_eq!(
        mid_recording.diagnostic_code.as_deref(),
        Some("CAP-DEV-001")
    );
    assert_eq!(
        mid_recording.failure_stage.as_deref(),
        Some(MID_RECORDING_STAGE)
    );
    assert!(!stop_text(&mid_recording).contains("0x"));
    let requested = CaptureLifecycleStatus::stopped(CaptureStopReason::Requested);
    assert_eq!(requested.diagnostic_code.as_deref(), Some("CAP-EXIT-001"));
    assert!(!stop_text(&requested).contains("0x"));
    // Task1420: running out of room is a safe stop, so it carries no failure
    // stage -- that field is what turns the toast into 「録画に失敗しました」.
    let disk_full = CaptureLifecycleStatus::stopped(CaptureStopReason::DiskFull);
    assert_eq!(disk_full.diagnostic_code.as_deref(), Some("CAP-DSK-001"));
    assert_eq!(disk_full.failure_stage, None);
    let message = stop_text(&disk_full);
    assert!(message.contains("容量"), "the body has to name the cause");
    assert!(!message.contains("0x"));
}

/// What this used to be: `matches!` on a unit variant, asserting that a value
/// equals itself. Neither half of its own name was checked -- and the flag the
/// event sets had no reader at all, so a recording that lost its sound finished
/// with nothing but a `warn!` line to say so (task2140). It now drives the event
/// through a running capture and reads what the UI reads.
#[test]
fn audio_unavailable_event_is_anonymous_and_does_not_stop_capture() {
    let controller = CaptureController::new();
    let (session, worker) = session_with_encoder_events();
    let _stopped = install_active(&controller, "mute-session", session);

    // Before the trial fails, the capture is running and has its sound.
    assert_eq!(
        controller.audio_status(),
        vec![(
            "mute-session".to_owned(),
            Some("mute-session".to_owned()),
            false
        )]
    );

    // Anonymous: the event carries no payload at all -- no session id, no
    // HRESULT, no MFT string. Which capture it belongs to is the channel it
    // arrived on, which is why the reader has to supply the name. (The one
    // event that did carry a payload, `AudioStopped`, went to a diagnostics
    // field nothing read; task2150 replaced it with a `warn!` and a stage.)
    worker
        .send(encoder::EncoderEvent::AudioUnavailable)
        .unwrap();

    assert_eq!(
        controller.audio_status(),
        vec![(
            "mute-session".to_owned(),
            Some("mute-session".to_owned()),
            true
        )],
        "the flag has to reach a reader, with the name of the capture it is about"
    );
    // And does not stop the capture: an unavailable audio session still permits
    // isolated video capture (task198 / task1430), so this recording keeps its
    // video. The half of the name that was never checked.
    assert!(controller.is_active());
    assert_eq!(
        controller.active_session_ids(),
        vec!["mute-session".to_owned()]
    );
}

#[test]
fn load_session_rejects_path_traversal_ids() {
    let controller = CaptureController::new();
    let root = TmpDir::new("livia-load-invalid-id");
    for id in ["../outside", "..\\outside", "a/b", "a\\b", ".."] {
        assert_eq!(
            controller.load_session_at(&root, id),
            Err("invalid session id".to_owned()),
            "{id} should be rejected"
        );
    }
}

#[test]
fn load_session_rejects_a_container_it_cannot_read() {
    let root = TmpDir::new("livia-load-old");
    let session_id = "capture-unreadable";
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join(format!("{session_id}.lvb")),
        b"not a container this build can read",
    )
    .unwrap();

    let controller = CaptureController::new();
    // CaptureController::new() loads any real closed sessions already on this
    // machine's %LOCALAPPDATA%\Liveback\buffer, so timeline() may be non-None
    // before this call -- what matters is that a failed load doesn't change it.
    let timeline_before = controller.timeline();
    let error = controller.load_session_at(&root, session_id).unwrap_err();
    assert!(
        error.starts_with("could not load the session"),
        "expected an explicit load error, got: {error}"
    );
    assert_eq!(
        controller.timeline(),
        timeline_before,
        "last_timeline must not be swapped on failure"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_session_rejects_a_session_still_open_on_disk() {
    let root = TmpDir::new("livia-load-open");
    let session_id = "capture-still-recording";
    session_file(&root, session_id, 0, b"data", false);

    let controller = CaptureController::new();
    let error = controller.load_session_at(&root, session_id).unwrap_err();
    assert_eq!(error, "that session cannot be loaded");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_session_accepts_a_recovered_session() {
    let root = TmpDir::new("livia-load-recovered");
    let session_id = "capture-recovered";
    session_file(&root, session_id, 0, b"first", false);
    // The startup sweep's repair: `recover` ends with the `Closed` record.
    let recovered = ring_buffer::recover_session(&root, session_id).unwrap();
    assert!(recovered.closed);

    let controller = CaptureController::new();
    let manifest = controller
        .load_session_at(&root, session_id)
        .expect("a recovered session must be loadable");
    assert_eq!(manifest, recovered);
    let _ = std::fs::remove_dir_all(&root);
}

/// Task3700 narrowed this from "every load while capturing" to "the session
/// being recorded, by id". Both halves are here: the recording id is refused
/// without a disk read, and another id is not refused at all.
#[test]
fn load_session_rejects_only_the_recording_session_and_without_touching_disk() {
    let controller = CaptureController::new();
    install_active(&controller, "recording-session", idle_session());

    // A nonexistent root proves this errors out before ever touching disk --
    // if it tried the disk-fallback branch it would fail with a different error.
    let missing_root = TmpDir::new("livia-load-should-not-be-read");
    let error = controller
        .load_session_at(&missing_root, "recording-session")
        .unwrap_err();
    assert_eq!(error, "open a recording session from its LIVE row");

    // Task570: a `.lvb` named by path -- a double-click on the recording
    // session -- meets the same refusal, and the reason is what the toast will
    // say. The stem is the id, so this names the same session as above.
    let error = controller
        .load_session_file(&missing_root.join("recording-session.lvb"))
        .unwrap_err();
    assert_eq!(error, "open a recording session from its LIVE row");

    // The other half: a different id is no longer turned away by the mere fact
    // that something is recording. It passes the gate and reaches the disk
    // lookup, which is what the missing root then fails on -- a different
    // error, which is precisely the proof that the gate let it through.
    let error = controller
        .load_session_at(&missing_root, "some-other-session")
        .unwrap_err();
    assert!(
        error.starts_with("could not load the session"),
        "a non-recording id must pass the gate and reach disk, got {error:?}"
    );
}

/// LiveReview's door (task161): the session being recorded loads from memory,
/// and it is the one session `load_session_at` will not open (task3700).
#[test]
fn load_active_session_serves_the_recording_session_that_load_refuses() {
    let root = TmpDir::new("livia-load-active");
    let session_id = "active-session";
    let session_dir = root.join(session_id);
    let ring = ring_with_segment(&session_dir, session_id, 0, b"data");
    // Left open on purpose: this is what a session mid-recording looks like.
    assert!(!ring.manifest().closed);

    let controller = CaptureController::new();
    controller.insert_session(session_id.into(), ring);
    install_active(&controller, session_id, idle_session());

    let manifest = controller
        .load_active_session()
        .expect("the recording session must be loadable");
    assert_eq!(manifest.session_id, session_id);
    assert_eq!(
        controller.timeline().map(|timeline| timeline.session_id),
        Some(session_id.to_owned()),
        "loading swaps last_timeline, like load_session does"
    );
    // Reading its segments is the point: the lease has to resolve mid-recording.
    let lease = controller.acquire_review_lease(None, vec![0]).unwrap();
    assert!(lease.starts_with(session_id));
    controller.release_review_lease(&lease);

    // The other half of the contract: this session opens *here* and nowhere
    // else. `Load` refusing it is what keeps a LIVE row from arriving as
    // `live: false` and parking the review screen at the head.
    assert_eq!(
        controller.load_session_at(&root, session_id),
        Err("open a recording session from its LIVE row".to_owned())
    );

    controller.inner.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_active_session_refuses_when_nothing_is_recording() {
    let controller = CaptureController::new();
    assert_eq!(
        controller.load_active_session(),
        Err("no session is recording".to_owned())
    );
}

/// Task3700: a *closed* session opens while a different capture is running.
/// Its on-disk manifest is final -- the staleness that justified the old
/// process-wide refusal belongs to the recording session alone -- so all that
/// stood between the history's past rows and the review screen was a gate
/// asking the wrong question.
#[test]
fn load_session_opens_a_closed_session_while_another_capture_records() {
    let root = TmpDir::new("livia-load-while-recording");

    // The past session: written, closed, and left on disk only, so this covers
    // the disk-fallback branch running mid-recording as well as the gate.
    let closed_id = "closed-session";
    session_file(&root, closed_id, 0, b"data", true);

    // The capture in flight: its ring stays open and resident, as a real one is.
    let recording_id = "recording-session";
    let recording_ring = live_ring(&root, recording_id);
    let recording_manifest = recording_ring.manifest().clone();

    let controller = CaptureController::new();
    controller.insert_session(recording_id.into(), recording_ring);
    install_active(&controller, recording_id, idle_session());

    let manifest = controller
        .load_session_at(&root, closed_id)
        .expect("a closed session must open while another capture records");
    assert_eq!(manifest.session_id, closed_id);
    assert!(manifest.closed);
    assert_eq!(
        controller.timeline().map(|timeline| timeline.session_id),
        Some(closed_id.to_owned()),
        "loading swaps last_timeline the same way it does when nothing records"
    );
    // Reading it is the point of loading it: the lease has to resolve the
    // session that was just inserted, mid-recording.
    let lease = controller
        .acquire_review_lease(Some(closed_id.to_owned()), vec![0])
        .unwrap();
    assert_eq!(controller.read_review_segment(&lease, 0).unwrap(), b"data");
    controller.release_review_lease(&lease);

    // The acceptance criterion the review pane rests on, without a screen: the
    // recording's index writer overwrites `last_timeline` with its own manifest
    // every pass (indexer.rs). `timeline()` follows it away; the id
    // `refresh_review_pane` now asks with does not.
    *controller.inner.last_timeline.lock().unwrap() = Some(recording_manifest);
    assert_eq!(
        controller.timeline().map(|timeline| timeline.session_id),
        Some(recording_id.to_owned()),
        "the simulated index pass really did move last_timeline"
    );
    assert_eq!(
        controller
            .timeline_for(closed_id)
            .map(|timeline| timeline.session_id),
        Some(closed_id.to_owned()),
        "timeline_for keeps answering with what was loaded"
    );

    controller.inner.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

/// Task3700 over task570's route: the Explorer double-click and
/// `liveback.exe <path>` land on `load_session_file`, and that door opens
/// mid-recording too -- as long as the `.lvb` is not the recording itself
/// (which the sibling test above pins).
#[test]
fn a_container_opens_by_path_while_another_capture_records() {
    let root = TmpDir::new("livia-open-by-path-recording");
    std::fs::create_dir_all(&root).expect("root");

    let session_id = "capture-000000000000000000000000000000e2";
    let path = root.join(format!("{session_id}.lvb"));
    ring_buffer::container::ContainerWriter::create(&path, session_id, 15)
        .expect("container")
        .close()
        .expect("close");

    let controller = CaptureController::new();
    install_active(&controller, "recording-session", idle_session());

    let manifest = controller
        .load_session_file(&path)
        .expect("a closed .lvb must open by path while a capture records");
    assert_eq!(manifest.session_id, session_id);
    assert_eq!(
        controller
            .timeline_for(session_id)
            .map(|timeline| timeline.session_id),
        Some(session_id.to_owned()),
        "the container joined the catalog, so the review pane can ask for it by id"
    );

    controller.inner.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_session_swaps_timeline_for_an_already_resident_session_and_enables_review_lease() {
    let root = TmpDir::new("livia-load-resident");
    let session_id = "resident-session";
    let mut ring = ring_with_segment(&root, session_id, 0, b"data");
    ring.close().unwrap();

    let controller = CaptureController::new();
    controller.insert_session(session_id.into(), ring);

    let manifest = controller
        .load_session_at(&TmpDir::new("livia-load-resident-elsewhere"), session_id)
        .unwrap();
    assert_eq!(manifest.session_id, session_id);
    assert_eq!(
        controller.timeline().map(|timeline| timeline.session_id),
        Some(session_id.to_owned())
    );

    let lease = controller.acquire_review_lease(None, vec![0]).unwrap();
    assert!(lease.starts_with(session_id));
    controller.release_review_lease(&lease);
    let _ = std::fs::remove_dir_all(&root);
}

// AC5's specific gap flagged during planning: a session that exists only on disk
// (never loaded into `self.sessions`, e.g. a closed session from a previous app
// run this process never opened at startup) must still be leasable/fetchable
// after `load_session`, not just readable via `get_timeline`.
#[test]
fn load_session_inserts_a_disk_only_session_so_review_lease_can_resolve_it() {
    let root = TmpDir::new("livia-load-disk-only");
    let session_id = "disk-only-session";
    session_file(&root, session_id, 0, b"data", true);

    let controller = CaptureController::new();
    assert!(
        !controller
            .inner
            .sessions
            .lock()
            .unwrap()
            .contains_key(session_id),
        "session must not already be resident for this test to be meaningful"
    );

    let manifest = controller.load_session_at(&root, session_id).unwrap();
    assert_eq!(manifest.session_id, session_id);
    assert!(controller
        .inner
        .sessions
        .lock()
        .unwrap()
        .contains_key(session_id));

    let lease = controller.acquire_review_lease(None, vec![0]).unwrap();
    assert!(lease.starts_with(session_id));
    let bytes = controller.read_review_segment(&lease, 0).unwrap();
    assert_eq!(bytes, b"data");
    controller.release_review_lease(&lease);
    let _ = std::fs::remove_dir_all(&root);
}

/// The sweep is the one irreversible thing task450 does, and the buffer root it
/// runs over is *not* only sessions: it holds every exported mp4 (the export
/// default output directory is the same folder) and, from now on, every `.lvb`.
/// So this pins what it is allowed to touch -- directories that are sessions --
/// rather than just that it deletes something.
#[test]
fn the_legacy_sweep_takes_session_directories_and_nothing_else() {
    let root = TmpDir::new("livia-sweep");
    std::fs::create_dir_all(&root).expect("root");

    // Two shapes of legacy session: one with a manifest, and one crash-shaped
    // directory that never got as far as writing one.
    let with_manifest = root.join("capture-000000000000000000000000000000aa");
    std::fs::create_dir_all(&with_manifest).expect("session dir");
    std::fs::write(with_manifest.join("manifest.json"), b"{}").expect("legacy manifest");
    std::fs::write(with_manifest.join("recording.marker"), b"recording").expect("marker");
    let no_manifest = root.join("capture-000000000000000000000000000000bb");
    std::fs::create_dir_all(&no_manifest).expect("bare session dir");
    let quarantine = root.join("quarantine");
    std::fs::create_dir_all(&quarantine).expect("quarantine");

    // Everything below must survive.
    let container = root.join("capture-000000000000000000000000000000cc.lvb");
    std::fs::write(&container, b"not really a container, but a file").expect("container");
    let exported = root.join("Some Game_1787053055.mp4");
    std::fs::write(&exported, b"an export the user asked for").expect("export");
    let unrelated = root.join("notes");
    std::fs::create_dir_all(&unrelated).expect("unrelated dir");
    std::fs::write(unrelated.join("keep.txt"), b"not a session").expect("unrelated file");
    // Task3100: the clip output folder lives in the buffer root, and on a
    // machine that already hit the defect it carries a `manifest.json` the
    // listing scan wrote into it. That manifest used to be enough to qualify it
    // for `remove_dir_all`, which is how four of the user's clips were lost.
    let clips = root.join("Clips");
    std::fs::create_dir_all(&clips).expect("clips dir");
    std::fs::write(clips.join("manifest.json"), b"{}").expect("leftover manifest");
    std::fs::write(clips.join("verify-0.mp4"), b"a clip the user kept").expect("clip");

    CaptureController::sweep_legacy_sessions(&root);

    assert!(
        !with_manifest.exists(),
        "a legacy session with a manifest is removed"
    );
    assert!(
        !no_manifest.exists(),
        "a capture-* directory is removed even without a manifest"
    );
    assert!(
        !quarantine.exists(),
        "an old recovery's quarantine is removed"
    );
    assert!(
        container.is_file(),
        "a .lvb is a file and must survive the sweep"
    );
    assert!(
        exported.is_file(),
        "an exported mp4 in the same folder must survive"
    );
    assert!(
        unrelated.is_dir(),
        "a directory that is not a session is left alone"
    );
    assert!(
        clips.is_dir() && clips.join("verify-0.mp4").is_file(),
        "the clip folder survives even carrying a leftover manifest.json (task3100)"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// The bug that ate a user's exports (task450).
///
/// A container session is one file *inside* the buffer folder, so its ring's
/// `root()` is the buffer folder itself. Discard used to hand that root to
/// `remove_dir_all`, which deleted every other session and every exported clip
/// sitting beside it. Asserting "the .lvb is gone" is not enough -- that passed
/// while the folder was being destroyed. What this pins is the blast radius.
#[test]
fn discarding_a_container_session_removes_only_its_own_file() {
    let root = TmpDir::new("livia-discard");
    std::fs::create_dir_all(&root).expect("root");

    let victim_id = "capture-000000000000000000000000000000d1";
    let victim = root.join(format!("{victim_id}.lvb"));
    ring_buffer::container::ContainerWriter::create(&victim, victim_id, 15)
        .expect("victim")
        .close()
        .expect("close victim");

    // The bystanders: another session, and something the user exported.
    let bystander_id = "capture-000000000000000000000000000000d2";
    let bystander = root.join(format!("{bystander_id}.lvb"));
    ring_buffer::container::ContainerWriter::create(&bystander, bystander_id, 15)
        .expect("bystander")
        .close()
        .expect("close bystander");
    let exported = root.join("Some Game_1787056175.mp4");
    std::fs::write(&exported, b"the clip the user kept").expect("export");

    let controller = CaptureController::new();
    controller.reload_catalog_at(&root).expect("catalog");
    controller
        .discard_session(victim_id, ring_buffer::Disposal::Permanent)
        .expect("discard");

    assert!(!victim.exists(), "the discarded session's file is gone");
    assert!(root.is_dir(), "the buffer folder itself survives");
    assert!(
        bystander.is_file(),
        "another session in the same folder survives"
    );
    assert!(
        exported.is_file(),
        "an exported clip in the same folder survives"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// Task570: a `.lvb` named by path -- a double-click in Explorer -- opens even
/// when it sits nowhere near the buffer root. The id is the file stem and the
/// root is the folder it happens to be in, which is the whole trick.
#[test]
fn a_container_opens_by_path_from_outside_the_buffer_root() {
    let root = TmpDir::new("livia-open-by-path");
    std::fs::create_dir_all(&root).expect("root");

    let session_id = "capture-000000000000000000000000000000e1";
    let path = root.join(format!("{session_id}.lvb"));
    ring_buffer::container::ContainerWriter::create(&path, session_id, 15)
        .expect("container")
        .close()
        .expect("close");

    let controller = CaptureController::new();
    let manifest = controller.load_session_file(&path).expect("opens by path");
    assert_eq!(manifest.session_id, session_id);
    assert_eq!(
        controller.timeline().map(|timeline| timeline.session_id),
        Some(session_id.to_owned()),
        "the loaded session becomes the one the review screen reads"
    );

    // The two ways a path can be wrong, both of which reach this from a
    // command line or a drop and must not panic.
    let exported = root.join("Some Game_1787056175.mp4");
    std::fs::write(&exported, b"not a session").expect("export");
    assert!(
        controller.load_session_file(&exported).is_err(),
        "a file that is not a .lvb is refused before anything opens it"
    );
    assert!(
        controller
            .load_session_file(&root.join("nothing-here.lvb"))
            .is_err(),
        "a .lvb that does not exist fails rather than panicking"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// t261025-d59e: the containers `load_closed_sessions_at` opens at startup seed
/// the listing cache, so the history's first listings open nothing -- and the
/// rows they return are the ones a fresh open builds.
#[test]
fn startup_catalog_seeds_the_listing_with_the_rows_a_fresh_open_builds() {
    use crate::ring_buffer::container::{open_for_reading_calls, ContainerBuilder, MetaEvent};
    let root = tmp_dir("listing-seed");
    let mut paths = vec![];
    for (id, closed) in [
        ("capture-000000000000000000000000000000d1", true),
        ("capture-000000000000000000000000000000d2", false),
    ] {
        let path = root.join(format!("{id}.lvb"));
        let builder = ContainerBuilder::new(id)
            .segment(0, 0, 20_000_000, &[0xA0; 256])
            .thumbnail(0, &[0x4A; 32])
            .segment(1, 20_000_000, 40_000_000, &[0xB1; 256])
            .meta(MetaEvent::TitleSet {
                title: Some("seeded".into()),
            })
            .meta(MetaEvent::CommentSet {
                comment: Some("a comment".into()),
            })
            .meta(MetaEvent::ProtectedSet { protected: true })
            .meta(MetaEvent::MarkerAdded {
                time_100ns: 5,
                label: "boss".into(),
                palette_index: None,
                comment: None,
            })
            .checkpoint();
        let builder = if closed { builder.closed() } else { builder };
        builder.build(&path).expect("fixture builds");
        paths.push(path);
    }

    let _ = CaptureController::load_closed_sessions_at(&root);
    let opens = open_for_reading_calls();
    let seeded = ring_buffer::list_sessions(&root).expect("list");
    assert_eq!(
        open_for_reading_calls() - opens,
        0,
        "the startup catalog already opened both"
    );

    // A new modification time with the same bytes: the key no longer matches,
    // so the next listing opens both afresh.
    for path in &paths {
        std::fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
            .expect("touch");
    }
    let opens = open_for_reading_calls();
    let fresh = ring_buffer::list_sessions(&root).expect("list");
    assert_eq!(open_for_reading_calls() - opens, 2, "a changed key reopens");
    assert_eq!(seeded.len(), 2);
    assert_eq!(seeded, fresh, "seeded rows match freshly opened ones");

    let _ = std::fs::remove_dir_all(&root);
}

/// t261101-27d6: the history's listing takes every recording session's row
/// from its in-memory manifest -- and only the recording ones: a loaded closed
/// session in the same map is left to the listing's own open.
#[test]
fn live_session_rows_come_from_the_recording_manifests_only() {
    use crate::ring_buffer::container::open_for_reading_calls;
    let root = tmp_dir("listing-live-rows");
    let live_id = "capture-000000000000000000000000000000f1";
    let closed_id = "capture-000000000000000000000000000000f2";
    let live = ring_with_segment(&root, live_id, 0, b"live");
    let (live_path, live_end) = (
        live.container_path().to_path_buf(),
        live.manifest()
            .segments
            .last()
            .map(|segment| segment.end_100ns),
    );
    let closed =
        ring_buffer::RingBuffer::open_container(session_file(&root, closed_id, 0, b"closed", true))
            .unwrap();

    let controller = CaptureController::new();
    controller.insert_session(live_id.into(), live);
    controller.insert_session(closed_id.into(), closed);
    install_active(&controller, live_id, idle_session());

    let rows = controller.live_session_rows();
    assert_eq!(rows.keys().collect::<Vec<_>>(), [&live_path]);
    assert_eq!(rows[&live_path].segment_count, 1);
    assert_eq!(rows[&live_path].end_100ns, live_end);

    let opens = open_for_reading_calls();
    let listed = ring_buffer::list_sessions_with_live(&root, &rows).expect("list");
    assert_eq!(listed.len(), 2);
    assert_eq!(
        open_for_reading_calls() - opens,
        1,
        "only the closed one opens"
    );

    controller.inner.active.lock().unwrap().clear();
    assert!(
        controller.live_session_rows().is_empty(),
        "a stopped capture has no row"
    );
    let _ = std::fs::remove_dir_all(&root);
}
