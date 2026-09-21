//! Lifecycle status and load_session catalog tests.
use super::*;

#[test]
fn lifecycle_status_uses_anonymous_safe_stop_codes() {
    let source_closed = CaptureLifecycleStatus::stopped(CaptureStopReason::SourceClosed);
    assert_eq!(source_closed.state, "stopped");
    assert_eq!(
        source_closed.diagnostic_code.as_deref(),
        Some("CAP-TGT-001")
    );
    assert!(!source_closed.message.unwrap().contains("0x"));
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
    assert!(!audio_death.message.unwrap().contains("0x"));
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
    assert!(!mid_recording.message.unwrap().contains("0x"));
    let requested = CaptureLifecycleStatus::stopped(CaptureStopReason::Requested);
    assert_eq!(requested.diagnostic_code.as_deref(), Some("CAP-EXIT-001"));
    assert!(!requested.message.unwrap().contains("0x"));
    // Task1420: running out of room is a safe stop, so it carries no failure
    // stage -- that field is what turns the toast into 「録画に失敗しました」.
    let disk_full = CaptureLifecycleStatus::stopped(CaptureStopReason::DiskFull);
    assert_eq!(disk_full.diagnostic_code.as_deref(), Some("CAP-DSK-001"));
    assert_eq!(disk_full.failure_stage, None);
    let message = disk_full.message.unwrap();
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
    let root = std::env::temp_dir();
    for id in ["../outside", "..\\outside", "a/b", "a\\b", ".."] {
        assert_eq!(
            controller.load_session_at(&root, id),
            Err("invalid session id".to_owned()),
            "{id} should be rejected"
        );
    }
}

#[test]
fn load_session_rejects_an_incompatible_manifest_version() {
    let root = std::env::temp_dir().join(format!("livia-load-old-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_id = "capture-old-version";
    let session_dir = root.join(session_id);
    let mut ring =
        ring_buffer::RingBuffer::create(session_dir.clone(), session_id.into(), 15, None).unwrap();
    ring.close().unwrap();
    let mut manifest = ring.manifest().clone();
    manifest.version = ring_buffer::MANIFEST_VERSION - 1;
    std::fs::write(
        session_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let controller = CaptureController::new();
    // CaptureController::new() loads any real closed sessions already on this
    // machine's %LOCALAPPDATA%\Liveback\buffer, so timeline() may be non-None
    // before this call -- what matters is that a failed load doesn't change it.
    let timeline_before = controller.timeline();
    let error = controller.load_session_at(&root, session_id).unwrap_err();
    assert!(
        error.starts_with("session読込失敗"),
        "expected an explicit version-incompatibility error, got: {error}"
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
    let root = std::env::temp_dir().join(format!("livia-load-open-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_id = "capture-still-recording";
    let session_dir = root.join(session_id);
    let _ring =
        ring_buffer::RingBuffer::create(session_dir.clone(), session_id.into(), 15, None).unwrap();

    let controller = CaptureController::new();
    let error = controller.load_session_at(&root, session_id).unwrap_err();
    assert_eq!(error, "指定sessionは読込できません");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_session_accepts_a_recovered_session_once_its_recording_marker_is_gone() {
    let root = std::env::temp_dir().join(format!("livia-load-recovered-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_id = "capture-recovered";
    let session_dir = root.join(session_id);
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(
        session_dir.join("segment-00000000000000000000.mp4"),
        b"first",
    )
    .unwrap();
    // Mirrors the real recovery UI flow: recover_session synthesizes and
    // persists manifest.json (closed: false) and removes recording.marker.
    let recovered = ring_buffer::recover_session(&root, session_id).unwrap();
    assert!(!recovered.closed);
    assert!(!session_dir.join("recording.marker").exists());

    let controller = CaptureController::new();
    let manifest = controller
        .load_session_at(&root, session_id)
        .expect("a recovered session with no recording.marker must be loadable");
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
    let missing_root = std::env::temp_dir().join("livia-load-should-not-be-read");
    let error = controller
        .load_session_at(&missing_root, "recording-session")
        .unwrap_err();
    assert_eq!(error, "録画中のsessionはLIVEの行から開いてください");

    // Task570: a `.lvb` named by path -- a double-click on the recording
    // session -- meets the same refusal, and the reason is what the toast will
    // say. The stem is the id, so this names the same session as above.
    let error = controller
        .load_session_file(&missing_root.join("recording-session.lvb"))
        .unwrap_err();
    assert_eq!(error, "録画中のsessionはLIVEの行から開いてください");

    // The other half: a different id is no longer turned away by the mere fact
    // that something is recording. It passes the gate and reaches the disk
    // lookup, which is what the missing root then fails on -- a different
    // error, which is precisely the proof that the gate let it through.
    let error = controller
        .load_session_at(&missing_root, "some-other-session")
        .unwrap_err();
    assert!(
        error.starts_with("session読込失敗"),
        "a non-recording id must pass the gate and reach disk, got {error:?}"
    );
}

/// LiveReview's door (task161): the session being recorded loads from memory,
/// and it is the one session `load_session_at` will not open (task3700).
#[test]
fn load_active_session_serves_the_recording_session_that_load_refuses() {
    let root = std::env::temp_dir().join(format!("livia-load-active-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_id = "active-session";
    let session_dir = root.join(session_id);
    let mut ring =
        ring_buffer::RingBuffer::create(session_dir.clone(), session_id.into(), 15, None).unwrap();
    let segment = session_dir.join("segment.mp4");
    std::fs::write(&segment, b"data").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: segment,
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 4,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    // Left open on purpose: this is what a session mid-recording looks like.
    assert!(!ring.manifest().closed);

    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.into(), ring);
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
        Err("録画中のsessionはLIVEの行から開いてください".to_owned())
    );

    controller.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_active_session_refuses_when_nothing_is_recording() {
    let controller = CaptureController::new();
    assert_eq!(
        controller.load_active_session(),
        Err("録画中のsessionがありません".to_owned())
    );
}

/// Task3700: a *closed* session opens while a different capture is running.
/// Its on-disk manifest is final -- the staleness that justified the old
/// process-wide refusal belongs to the recording session alone -- so all that
/// stood between the history's past rows and the review screen was a gate
/// asking the wrong question.
#[test]
fn load_session_opens_a_closed_session_while_another_capture_records() {
    let root =
        std::env::temp_dir().join(format!("livia-load-while-recording-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);

    // The past session: written, closed, and left on disk only, so this covers
    // the disk-fallback branch running mid-recording as well as the gate.
    let closed_id = "closed-session";
    let closed_dir = root.join(closed_id);
    let mut closed_ring =
        ring_buffer::RingBuffer::create(closed_dir.clone(), closed_id.into(), 15, None).unwrap();
    let segment = closed_dir.join("segment.mp4");
    std::fs::write(&segment, b"data").unwrap();
    closed_ring
        .add(ring_buffer::SegmentRecord {
            index: 0,
            path: segment,
            start_100ns: 0,
            end_100ns: 20_000_000,
            bytes: 4,
            thumbnail_path: None,
            audio_offsets_100ns: vec![0],
        })
        .unwrap();
    closed_ring.close().unwrap();
    drop(closed_ring);

    // The capture in flight: its ring stays open and resident, as a real one is.
    let recording_id = "recording-session";
    let recording_ring =
        ring_buffer::RingBuffer::create(root.join(recording_id), recording_id.into(), 15, None)
            .unwrap();
    let recording_manifest = recording_ring.manifest().clone();

    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert(recording_id.into(), recording_ring);
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
    *controller.last_timeline.lock().unwrap() = Some(recording_manifest);
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

    controller.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

/// Task3700 over task570's route: the Explorer double-click and
/// `liveback.exe <path>` land on `load_session_file`, and that door opens
/// mid-recording too -- as long as the `.lvb` is not the recording itself
/// (which the sibling test above pins).
#[test]
fn a_container_opens_by_path_while_another_capture_records() {
    let root = std::env::temp_dir().join(format!(
        "livia-open-by-path-recording-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
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

    controller.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn load_session_swaps_timeline_for_an_already_resident_session_and_enables_review_lease() {
    let root = std::env::temp_dir().join(format!("livia-load-resident-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_id = "resident-session";
    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), session_id.into(), 15, None).unwrap();
    let segment = root.join("segment.mp4");
    std::fs::write(&segment, b"data").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: segment,
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 4,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    ring.close().unwrap();

    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert(session_id.into(), ring);

    let manifest = controller
        .load_session_at(&std::env::temp_dir(), session_id)
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
    let root = std::env::temp_dir().join(format!("livia-load-disk-only-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_id = "disk-only-session";
    let session_dir = root.join(session_id);
    let mut ring =
        ring_buffer::RingBuffer::create(session_dir.clone(), session_id.into(), 15, None).unwrap();
    let segment = session_dir.join("segment.mp4");
    std::fs::write(&segment, b"data").unwrap();
    ring.add(ring_buffer::SegmentRecord {
        index: 0,
        path: segment,
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 4,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .unwrap();
    ring.close().unwrap();
    drop(ring);

    let controller = CaptureController::new();
    assert!(
        !controller.sessions.lock().unwrap().contains_key(session_id),
        "session must not already be resident for this test to be meaningful"
    );

    let manifest = controller.load_session_at(&root, session_id).unwrap();
    assert_eq!(manifest.session_id, session_id);
    assert!(controller.sessions.lock().unwrap().contains_key(session_id));

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
    let root = std::env::temp_dir().join(format!("livia-sweep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");

    // Two shapes of legacy session: one with a manifest, and one crash-shaped
    // directory that never got as far as writing one.
    let with_manifest = root.join("capture-000000000000000000000000000000aa");
    std::fs::create_dir_all(&with_manifest).expect("session dir");
    ring_buffer::RingBuffer::create(
        with_manifest.clone(),
        "capture-000000000000000000000000000000aa".to_owned(),
        ring_buffer::DEFAULT_RETENTION_MINUTES,
        None,
    )
    .expect("legacy session");
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
    let root = std::env::temp_dir().join(format!("livia-discard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
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
    let root = std::env::temp_dir().join(format!("livia-open-by-path-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
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
