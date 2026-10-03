//! Controller lifecycle, discard, title and marker tests.
use super::*;
use crate::capture::catalog::LiveMarkerEdit;

#[test]
fn tray_tooltip_goes_through_the_event_sink() {
    let sink = Arc::new(crate::events::RecordingSink::default());
    let controller = CaptureController::new();
    controller.set_event_sink(sink.clone());

    // Task2030: the tooltip carries the count, and names the target only when
    // there is exactly one to name.
    controller.update_tray_tooltip(1, None);
    controller.update_tray_tooltip(1, Some("ペイント"));
    controller.update_tray_tooltip(3, Some("ペイント"));
    controller.update_tray_tooltip(0, None);

    let tooltips = sink.tooltips.lock().unwrap();
    assert_eq!(
        tooltips[0],
        crate::ui_state::lifecycle::tray_tooltip_recording(crate::ui_state::locale::Locale::Ja)
    );
    assert!(tooltips[1].contains("ペイント"), "{}", tooltips[1]);
    assert!(tooltips[2].contains('3'), "{}", tooltips[2]);
    assert!(!tooltips[2].contains("ペイント"), "{}", tooltips[2]);
    assert_eq!(
        tooltips[3],
        crate::ui_state::lifecycle::tray_tooltip_idle(crate::ui_state::locale::Locale::Ja)
    );
}

#[test]
fn fps_throttle_drops_duplicates_and_excess() {
    let mut gate = FrameThrottle::new(60).unwrap();
    assert!(gate.accepts(0));
    assert!(!gate.accepts(0));
    assert!(!gate.accepts(166_000));
    assert!(gate.accepts(166_667));
}

/// One second of arrivals at `arrival_100ns`, counted through a throttle set to
/// `frame_rate`.
fn accepted_over_one_second(arrival_100ns: i64, frame_rate: u8) -> i64 {
    let mut gate = FrameThrottle::new(frame_rate).unwrap();
    let mut accepted = 0;
    let mut timestamp = 0;
    while timestamp < 10_000_000 {
        if gate.accepts(timestamp) {
            accepted += 1;
        }
        timestamp += arrival_100ns;
    }
    accepted
}

/// Task t260913-578a. WGC hands frames over at whole refresh intervals, so the
/// arrival rate is never the target -- on the 279.86Hz machine this was measured on
/// it is 139.93/s or 279.86/s against a 120 target. The throttle has to average down
/// to the target from *any* of them.
///
/// The previous gap-from-last implementation returned 70 and 93 for the first two
/// rows and 28 for the third: it can only produce divisors of the arrival rate.
#[test]
fn the_throttle_averages_down_to_the_target_from_any_arrival_rate() {
    // (arrival period, target, what the recording should come out at)
    const CASES: [(i64, u8, i64); 6] = [
        (71_466, 120, 120), // 139.93/s, the rate `MinUpdateInterval = T/2` produces
        (35_733, 120, 120), // 279.86/s, the display's full refresh rate
        (178_665, 30, 30),  // 55.97/s against the slowest setting
        (178_665, 120, 56), // a source slower than the target passes through
        (166_667, 60, 60),  // arrival already at the target
        (83_333, 60, 60),   // 120/s against 60
    ];
    for (arrival_100ns, frame_rate, expected) in CASES {
        let accepted = accepted_over_one_second(arrival_100ns, frame_rate);
        assert!(
            (accepted - expected).abs() <= 2,
            "arrivals every {arrival_100ns} (100ns) at a {frame_rate}fps target gave {accepted},              expected about {expected}"
        );
    }
}

#[test]
fn measured_fps_from_counts_reports_rate_from_encoded_frame_count() {
    for (count, elapsed, expected) in [
        (30, Duration::from_secs(1), 30.0),
        // No time has passed: zero, not a division by zero.
        (30, Duration::ZERO, 0.0),
        // Nothing encoded since the last poll.
        (0, Duration::from_secs(1), 0.0),
    ] {
        assert_eq!(
            measured_fps_from_counts(count, elapsed),
            expected,
            "{count} frames over {elapsed:?}"
        );
    }
}

#[test]
fn stopping_an_idle_controller_is_a_safe_no_op() {
    let controller = CaptureController::new();
    assert!(!controller.is_active());
    assert_eq!(controller.stop_async(), CaptureStopReason::Requested);
    // Give the spawned stop_blocking() thread a moment to run; it must not panic.
    std::thread::sleep(Duration::from_millis(50));
    assert!(!controller.is_active());
}

// Task3200: the process exit calls `shutdown` on its way out of `run()`, so an
// idle controller -- every quit that was not recording -- must not add anything
// the user can feel to it.
#[test]
fn shutting_down_an_idle_controller_returns_at_once() {
    let controller = CaptureController::new();
    let started = Instant::now();

    assert!(controller.shutdown(Duration::from_secs(10)));

    // Generous against a loaded CI box; the point is that it is nowhere near the
    // timeout it was handed.
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "idle shutdown took {:?}",
        started.elapsed()
    );
}

// Task3200: 「キャプチャ停止」 followed straight away by 「終了」 -- the stop has
// already taken the entry out of `active`, so waiting on that map alone would
// return instantly and the process would exit before `Indexer::finish` wrote
// `Closed`. `shutdown` waits on the `stopping` claim as well, and gives up on
// the timeout rather than hanging the quit. Standing in for the in-flight stop
// with a bare claim keeps this off the desktop: no capture, no window.
#[test]
fn shutting_down_waits_for_a_stop_in_flight_and_honours_the_timeout() {
    const TIMEOUT_MS: u64 = 100;

    let controller = CaptureController::new();
    assert!(controller.claim_stopping("task3200-fake-session"));

    let started = Instant::now();
    let landed = controller.shutdown(Duration::from_millis(TIMEOUT_MS));
    let elapsed = started.elapsed();

    assert!(
        !landed,
        "a stop still in flight must not report a clean exit"
    );
    assert!(
        elapsed >= Duration::from_millis(TIMEOUT_MS),
        "gave up after {elapsed:?}, before the timeout"
    );

    // Lets the detached waiter thread finish instead of spinning for the rest of
    // the suite.
    controller.release_stopping("task3200-fake-session");
}

// Task 048 (found while investigating Task 047's UI-hang report): stop_blocking()
// used to hold `self.active`'s lock for the entire `session.stop()` call, which
// joins the capture worker thread -- a join that can take an unbounded amount of
// time to finish flushing/finalizing a long recording. `is_active()`,
// `capture_diagnostics()`, and `recording_position_100ns()` all lock that same
// mutex, so they would block for however long that join took. This test installs
// a fake session whose "worker" is a thread that sleeps for a fixed span standing
// in for an arbitrarily slow real teardown, drives `stop_blocking()` on another
// thread, and asserts the diagnostics-path methods return well before that sleep
// elapses.
#[test]
fn diagnostics_calls_do_not_wait_for_the_worker_join_during_stop() {
    const WORKER_JOIN_MS: u64 = 500;
    const MAX_PROBE_MS: u64 = 200;

    let controller = CaptureController::new();
    let (stop_tx, _stop_rx) = bounded::<()>(1);
    let (_frames_tx, frames_rx) = bounded::<CapturedFrame>(1);
    let (_encoder_tx, encoder_rx) = bounded::<encoder::EncoderEvent>(1);
    let worker = thread::spawn(|| thread::sleep(Duration::from_millis(WORKER_JOIN_MS)));
    let session = CaptureSession {
        stop: stop_tx,
        worker: Some(worker),
        // No real worker: the done channel's sender is already gone, which
        // reads as a worker that ended (t260929-ea5e).
        jobs: bounded(1).0,
        extra_audio: bounded(1).0,
        recording_done: bounded(1).1,
        frames: frames_rx,
        encoder_events: encoder_rx,
        diagnostics: blank_diagnostics(),
        last_sequence: 0,
        recording_position: Arc::new(AtomicI64::new(0)),
        encoded_frame_count: Arc::new(AtomicU64::new(0)),
        last_measured_frame_count: 0,
        last_measured_at: Instant::now(),
    };
    install_active(&controller, "slow-session", session);

    let stopper = controller.clone();
    let stop_thread = thread::spawn(move || {
        stopper.stop_blocking();
    });

    // Give stop_blocking() time to take the session out of `active` and enter
    // the (still-running) worker join before probing.
    thread::sleep(Duration::from_millis(100));

    let probe = std::time::Instant::now();
    let active = controller.is_active();
    assert!(
        probe.elapsed() < Duration::from_millis(MAX_PROBE_MS),
        "is_active() took {:?} while the worker join was still in flight",
        probe.elapsed()
    );
    assert!(
        !active,
        "the session should already read as inactive once taken out of `active`"
    );

    let probe = std::time::Instant::now();
    let _ = controller.diagnostics();
    assert!(
        probe.elapsed() < Duration::from_millis(MAX_PROBE_MS),
        "diagnostics() took {:?} while the worker join was still in flight",
        probe.elapsed()
    );

    let probe = std::time::Instant::now();
    let _ = controller.recording_position_100ns();
    assert!(
        probe.elapsed() < Duration::from_millis(MAX_PROBE_MS),
        "recording_position_100ns() took {:?} while the worker join was still in flight",
        probe.elapsed()
    );

    stop_thread.join().unwrap();
}

#[test]
fn discard_session_still_removes_an_in_memory_session_and_enforces_has_leases() {
    let root = std::env::temp_dir().join(format!("livia-discard-mem-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "mem-session".into(), 15, None).unwrap();
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
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("mem-session".into(), ring);

    let leased = controller
        .acquire_review_lease(Some("mem-session".into()), vec![0])
        .unwrap();
    let blocked = controller.discard_session("mem-session", ring_buffer::Disposal::Permanent);
    assert_eq!(
        blocked,
        Err("a session being reviewed or exported cannot be deleted".to_owned())
    );
    assert!(
        root.exists(),
        "a leased in-memory session must not be deleted"
    );

    controller.release_review_lease(&leased);
    let result = controller.discard_session("mem-session", ring_buffer::Disposal::Permanent);

    assert!(result.is_ok(), "{result:?}");
    assert!(!root.exists());
    let _ = std::fs::remove_dir_all(&root);
}

/// t260920-bb09: a session that is recording is never discarded, whoever asks.
/// Until that task nothing here said so -- the history screen simply refused to
/// select a LIVE row, and when the user's 2026-09-19 ruling opened that
/// selection the last line of defence had to move to where the deletion is.
/// The UI's own guards (the disabled menu entry, the Delete key) sit above
/// this; reaching it at all means both were got past, which is exactly the
/// case CLAUDE.md's recording-loss incident is about.
#[test]
fn discard_session_refuses_a_recording() {
    let root = std::env::temp_dir().join(format!("livia-discard-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let mut ring =
        ring_buffer::RingBuffer::create(root.clone(), "live-session".into(), 15, None).unwrap();
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
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("live-session".into(), ring);

    let (stop_tx, _stop_rx) = bounded::<()>(1);
    let (_frames_tx, frames_rx) = bounded::<CapturedFrame>(1);
    let (_encoder_tx, encoder_rx) = bounded::<encoder::EncoderEvent>(1);
    let session = CaptureSession {
        stop: stop_tx,
        worker: Some(thread::spawn(|| {})),
        // No real worker: the done channel's sender is already gone, which
        // reads as a worker that ended (t260929-ea5e).
        jobs: bounded(1).0,
        extra_audio: bounded(1).0,
        recording_done: bounded(1).1,
        frames: frames_rx,
        encoder_events: encoder_rx,
        diagnostics: blank_diagnostics(),
        last_sequence: 0,
        recording_position: Arc::new(AtomicI64::new(0)),
        encoded_frame_count: Arc::new(AtomicU64::new(0)),
        last_measured_frame_count: 0,
        last_measured_at: Instant::now(),
    };
    let _stopped = install_active(&controller, "live-session", session);
    assert!(controller.is_live_session("live-session"));

    let refused = controller.discard_session("live-session", ring_buffer::Disposal::Permanent);
    assert_eq!(
        refused,
        Err("a recording session cannot be deleted".to_owned())
    );
    assert!(root.exists(), "a recording must not be deleted");

    // Control: the same call on the same controller, for a session that is not
    // recording, still deletes -- so the refusal above is the live check and
    // not the whole path having gone quiet.
    controller.active.lock().unwrap().remove("live-session");
    let result = controller.discard_session("live-session", ring_buffer::Disposal::Permanent);
    assert!(result.is_ok(), "{result:?}");
    assert!(!root.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discard_session_deletes_a_session_whose_manifest_has_no_segment_path_to_derive() {
    // The regression behind task125: deriving the directory from
    // `segments.first().path.parent()` produced the empty path for a
    // manifest that records a bare filename, and nothing at all for a
    // manifest with no segments -- both returned Ok while leaving the
    // directory on disk.
    let root = std::env::temp_dir().join(format!("livia-discard-nopath-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let ring =
        ring_buffer::RingBuffer::create(root.clone(), "empty-session".into(), 15, None).unwrap();
    assert!(ring.manifest().segments.is_empty());
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("empty-session".into(), ring);

    let result = controller.discard_session("empty-session", ring_buffer::Disposal::Permanent);

    assert!(result.is_ok(), "{result:?}");
    assert!(!root.exists(), "the directory must actually be gone");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discard_session_from_disk_deletes_a_crash_recovery_session_absent_from_memory() {
    let root = std::env::temp_dir().join(format!("livia-discard-disk-{}-a", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let session_dir = root.join("crash-session");
    std::fs::create_dir_all(&session_dir).unwrap();
    std::fs::write(session_dir.join("segment-0.mp4"), b"data").unwrap();

    let result = CaptureController::discard_session_from_disk(
        &root,
        "crash-session",
        ring_buffer::Disposal::Permanent,
    );

    assert!(result.is_ok(), "{result:?}");
    assert!(!session_dir.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discard_session_from_disk_rejects_a_session_id_absent_from_disk_and_memory() {
    let root = std::env::temp_dir().join(format!("livia-discard-disk-{}-b", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kept = root.join("kept-session");
    std::fs::create_dir_all(&kept).unwrap();

    let result = CaptureController::discard_session_from_disk(
        &root,
        "no-such-session",
        ring_buffer::Disposal::Permanent,
    );

    assert_eq!(result, Err("no such session".to_owned()));
    assert!(kept.exists(), "unrelated sessions must be left untouched");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn discard_session_from_disk_rejects_traversal_session_ids_without_touching_outside_root() {
    let root = std::env::temp_dir().join(format!("livia-discard-disk-{}-c", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let sentinel = root
        .parent()
        .unwrap()
        .join("livia-discard-disk-outside-sentinel");
    std::fs::create_dir_all(&sentinel).unwrap();
    std::fs::write(sentinel.join("keep.txt"), b"do not delete").unwrap();

    for traversal_id in [
        "../livia-discard-disk-outside-sentinel",
        "..\\outside",
        "a/b",
    ] {
        let result = CaptureController::discard_session_from_disk(
            &root,
            traversal_id,
            ring_buffer::Disposal::Permanent,
        );
        assert!(result.is_err(), "{traversal_id} should be rejected");
    }

    assert!(
        sentinel.join("keep.txt").is_file(),
        "outside-root sentinel must survive"
    );
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&sentinel);
}

// The bug behind this: moving the buffer folder swapped `BUFFER_ROOT` (so the
// history listed the new folder) but left `self.sessions` holding rings opened
// from the old one, and every edit resolves through the cached ring's root. A
// session id present in both folders had its protect flag written to the old
// copy -- and a discard would have deleted the old folder's directory.
/// A recording has to be written where everything else looks for it. Listing,
/// loading, discard and retention all resolve through
/// `CaptureController::buffer_root()`, but the recorder used to rebuild the
/// *default* path from `LOCALAPPDATA` by hand -- so once a custom buffer folder
/// was configured (task164) every recording landed somewhere the history screen
/// never read, invisible and never pruned.
#[test]
fn recordings_are_written_under_the_configured_buffer_root() {
    let _root_lock = BUFFER_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let base = std::env::temp_dir().join(format!("livia-outdir-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let configured = base.join("elsewhere");

    CaptureController::apply_buffer_root(Some(configured.clone())).expect("root applies");
    let output = default_encoder_output_dir();
    // Put it back before asserting: a failure here must not leave every later
    // test in this binary pointed at a temp folder.
    CaptureController::apply_buffer_root(None).expect("root resets");

    assert!(
        output.starts_with(&configured),
        "a recording must go under the configured buffer root: {} is not inside {}",
        output.display(),
        configured.display(),
    );
    assert_eq!(
        output.parent(),
        Some(configured.as_path()),
        "the session folder sits directly in the buffer root"
    );
    assert!(default_encoder_output_dir().starts_with(CaptureController::default_buffer_root()));
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn reloading_the_catalog_moves_session_edits_to_the_new_buffer_root() {
    let base = std::env::temp_dir().join(format!("livia-reload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (old_root, new_root) = (base.join("buffer"), base.join("buffer-moved"));
    // Same session id in both folders, as a copied session has; only a closed
    // manifest is picked up by the catalog scan. The id has to be shaped like a
    // real one (`capture-*`): since task3100 the scan skips a directory that is
    // not named like a session, which is what keeps `Clips` out of it.
    for root in [&old_root, &new_root] {
        ring_buffer::RingBuffer::create(
            root.join("capture-shared-session"),
            "capture-shared-session".into(),
            15,
            None,
        )
        .expect("create ring")
        .close()
        .expect("close ring");
    }
    let controller = CaptureController::new();
    controller.reload_catalog_at(&old_root).unwrap();
    assert_eq!(
        controller.session_root("capture-shared-session").unwrap(),
        old_root.join("capture-shared-session")
    );

    controller.reload_catalog_at(&new_root).unwrap();
    controller
        .set_session_protected("capture-shared-session", true)
        .unwrap();

    assert_eq!(
        controller.session_root("capture-shared-session").unwrap(),
        new_root.join("capture-shared-session")
    );
    let protected = |root: &std::path::Path| {
        ring_buffer::RingBuffer::open(root.join("capture-shared-session"))
            .unwrap()
            .manifest()
            .protected
    };
    assert!(protected(&new_root), "the listed folder must take the edit");
    assert!(
        !protected(&old_root),
        "the folder the app moved away from must be left untouched"
    );
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn finalized_segment_end_is_exclusive_at_30_and_60_fps() {
    assert_eq!(exclusive_segment_end(1_000_000, 30), 1_333_333);
    assert_eq!(exclusive_segment_end(1_000_000, 60), 1_166_666);
}

/// Task202: the capture loop must never block past the moment the frame-hold
/// watchdog is due, or a still target records a hole the length of whatever
/// the block *was* -- a flat 250ms, which is exactly the 250-290ms every
/// segment used to open with.
#[test]
fn the_capture_loop_never_blocks_past_the_frame_hold_deadline() {
    use crate::capture::worker::loop_block_timeout;
    let ms = Duration::from_millis;

    // 60fps: the watchdog is due two frames (33ms) after the last one.
    assert_eq!(loop_block_timeout(ms(0), true, 60), ms(33));
    assert_eq!(loop_block_timeout(ms(20), true, 60), ms(13));
    // Already overdue: run now rather than block, but never spin at zero.
    assert_eq!(loop_block_timeout(ms(33), true, 60), ms(1));
    assert_eq!(loop_block_timeout(ms(5_000), true, 60), ms(1));
    // 30fps holds for longer, and the window check still caps the wait.
    assert_eq!(loop_block_timeout(ms(0), true, 30), ms(66));
    // Nothing captured yet: no frame to hold, so only the window check matters.
    assert_eq!(loop_block_timeout(ms(0), false, 60), ms(250));
    assert_eq!(loop_block_timeout(ms(9_999), false, 60), ms(250));
}

#[test]
fn set_session_title_updates_an_in_memory_session_and_its_timeline_snapshot() {
    let root = super::tmp_dir("rename");
    let ring = ring_buffer::RingBuffer::create(root.to_path_buf(), "session".into(), 15, None)
        .expect("create ring");
    let manifest = ring.manifest().clone();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);
    *controller.last_timeline.lock().unwrap() = Some(manifest);

    controller
        .set_session_title("session", Some("Renamed".into()))
        .unwrap();

    assert_eq!(
        ring_buffer::RingBuffer::open(root.to_path_buf())
            .unwrap()
            .manifest()
            .target_title,
        Some("Renamed".to_owned())
    );
    assert_eq!(
        controller
            .last_timeline
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .target_title,
        Some("Renamed".to_owned())
    );
}

/// Task1990: the predicate is now "does this id have an entry in the map",
/// which is what makes it answer for *each* capture once more than one can run.
/// The old shape -- one `active_session` id plus a recording flag -- could only
/// ever refuse one session, and needed the flag because that id outlived its
/// recording.
#[test]
fn set_session_title_refuses_only_the_sessions_being_recorded() {
    let controller = CaptureController::new();
    assert!(!controller.is_live_session("session"));

    install_active(&controller, "session", idle_session());
    install_active(&controller, "other", idle_session());

    // Recording: refused, because the capture worker owns that manifest and
    // would overwrite anything written here. Both of them, not just one.
    assert!(controller.is_live_session("session"));
    assert!(controller.is_live_session("other"));
    // A session that is not being recorded stays editable while they run.
    assert!(!controller.is_live_session("closed"));

    // Stopped: the entry is gone, so nothing stale can keep refusing.
    controller.active.lock().unwrap().clear();
    assert!(!controller.is_live_session("session"));
}

/// Task3960 flipped the first half of this: a marker edit on the recording
/// session used to be refused unconditionally, and the confirmation page --
/// which never gated its marker rows -- turned every press into
/// `marker_save_error`. Rename and delete are routed now, so this asserts `Ok`
/// where it used to assert that exact error string.
///
/// **Which arm this fixture takes matters**, because it is not the production
/// one: the marker is already in `manifest.markers` (so not the queue arm) and
/// the ring is `RingBuffer::create` -- a directory, `container: None` -- so
/// `is_container()` sends it past the staging arm too, into the ordinary
/// in-memory arm. That combination cannot occur in a shipped build (a
/// recording's ring is always built by `create_container`), which is why the
/// staging arm has a fixture of its own below
/// (`a_live_container_marker_edit_is_staged_for_the_index_writer`) and the
/// queue arm has two (`..._in_the_index_writers_queue...`).
#[test]
fn marker_edits_route_the_recording_session_and_apply_to_a_closed_one() {
    let root = super::tmp_dir("markers");
    let mut ring =
        ring_buffer::RingBuffer::create(root.to_path_buf(), "session".into(), 15, None).unwrap();
    ring.merge_markers(vec![ring_buffer::MarkerRecord {
        time_100ns: 5,
        label: String::new(),
        color_index: None,
    }])
    .unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);

    // Recording it: the edit goes through instead of failing.
    install_active(&controller, "session", idle_session());
    controller
        .update_marker("session", 5, "boss".into())
        .expect("a recording session's marker rename is routed, not refused");
    assert_eq!(
        controller.session_manifest("session").unwrap().markers[0].label,
        "boss"
    );
    // Task1560 opened the add too: it goes onto the index writer's queue
    // instead of failing (was `Err("a recording session's markers cannot be
    // changed")` until then).
    controller
        .add_marker("session", 7)
        .expect("a recording session's marker add is queued, not refused");
    controller.active.lock().unwrap().clear();

    // Not capturing any more: the edit applies.
    controller
        .update_marker("session", 5, "boss".into())
        .unwrap();
    assert_eq!(
        controller.session_manifest("session").unwrap().markers[0].label,
        "boss"
    );
    assert!(controller
        .update_marker("session", 999, "x".into())
        .is_err());
    controller.delete_marker("session", 5).unwrap();
    assert!(controller
        .session_manifest("session")
        .unwrap()
        .markers
        .is_empty());
}

/// Task3960, the window (AC 2): a marker pressed moments ago is in the index
/// writer's queue and has never been in `manifest.markers`, so a rename has to
/// rewrite the queue entry -- staging a `MarkerRenamed` would reach the
/// container *before* the `MarkerAdded` that creates the marker (`write`
/// appends `pending_meta` first, then runs `merge_markers`) and
/// `Snapshot::apply` would drop it on the floor.
///
/// `install_active` builds an `ActiveCapture` with `indexer: None`, so nothing
/// drains the queue: the window stays open for the whole test and there is no
/// 500ms to wait out.
#[test]
fn a_queued_marker_takes_its_rename_in_the_queue_and_stages_nothing() {
    let controller = CaptureController::new();
    install_active(&controller, "session", idle_session());
    // The production path onto the queue, not a hand-built `Vec`.
    controller.push_marker_for("session", 5);

    controller
        .update_marker("session", 5, "boss".into())
        .expect("a queued marker is renamed, not reported missing");

    let queued = {
        let active = controller.active.lock().unwrap();
        let capture = active.get("session").expect("the installed capture");
        assert!(
            capture.pending_meta.lock().unwrap().is_empty(),
            "no MetaEvent may be staged for a marker the container has never seen"
        );
        let queued = capture.pending_markers.lock().unwrap().clone();
        queued
    };
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].label, "boss");

    // The other half of AC 2 is `merge_markers`' contract: the label the queue
    // carries is the one the single `MarkerAdded` writes.
    let root = super::tmp_dir("marker-window-rename");
    let mut ring =
        ring_buffer::RingBuffer::create(root.to_path_buf(), "session".into(), 15, None).unwrap();
    let events = ring.merge_markers(queued).expect("merge");
    assert!(
        matches!(
            &events[..],
            [ring_buffer::container::MetaEvent::MarkerAdded { label, .. }] if label == "boss"
        ),
        "{events:?}"
    );
}

/// Task3960, the window (AC 3): deleting inside it takes the entry out of the
/// queue and writes *nothing*. A staged `MarkerRemoved` would land before the
/// `MarkerAdded`, i.e. be a no-op, and the marker the user deleted would be
/// back at the next load.
#[test]
fn a_queued_marker_is_deleted_from_the_queue_and_stages_nothing() {
    let controller = CaptureController::new();
    install_active(&controller, "session", idle_session());
    controller.push_marker_for("session", 5);

    controller
        .delete_marker("session", 5)
        .expect("a queued marker is removed, not reported missing");

    let active = controller.active.lock().unwrap();
    let capture = active.get("session").expect("the installed capture");
    assert!(
        capture.pending_markers.lock().unwrap().is_empty(),
        "the queue entry is gone, so `merge_markers` never writes a MarkerAdded"
    );
    assert!(
        capture.pending_meta.lock().unwrap().is_empty(),
        "and nothing was staged for the container either"
    );
}

/// Task1560: the review pane's "+" on the session being recorded. It goes onto
/// the index writer's queue -- the same place `push_marker_for` puts a hotkey
/// press -- and stages no `MetaEvent`, for the ordering reason
/// `edit_marker_at`'s Arm 1 documents. What the queue then carries is the one
/// `MarkerAdded` that `merge_markers` writes into the manifest.
#[test]
fn an_add_on_the_recording_session_is_queued_and_stages_nothing() {
    let controller = CaptureController::new();
    install_active(&controller, "session", idle_session());

    controller
        .add_marker("session", 7)
        .expect("a recording session's marker add is queued, not refused");
    // The same instant twice is still one marker.
    controller.add_marker("session", 7).unwrap();

    let queued = {
        let active = controller.active.lock().unwrap();
        let capture = active.get("session").expect("the installed capture");
        assert!(
            capture.pending_meta.lock().unwrap().is_empty(),
            "no MetaEvent may be staged for a marker the container has never seen"
        );
        let queued = capture.pending_markers.lock().unwrap().clone();
        queued
    };
    assert_eq!(
        queued
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![7]
    );

    let root = super::tmp_dir("marker-window-add");
    let mut ring =
        ring_buffer::RingBuffer::create(root.to_path_buf(), "session".into(), 15, None).unwrap();
    let events = ring.merge_markers(queued).expect("merge");
    assert!(
        matches!(
            &events[..],
            [ring_buffer::container::MetaEvent::MarkerAdded { time_100ns: 7, .. }]
        ),
        "{events:?}"
    );
    assert_eq!(ring.manifest().markers[0].time_100ns, 7);
}

/// Task1560's other two routes: a stopped session is written directly, as
/// before, and so is a stopped session while a *different* one records --
/// `is_live_session` is per id, so the recording one's queue stays empty.
#[test]
fn an_add_on_a_stopped_session_is_applied_even_while_another_records() {
    let root = super::tmp_dir("marker-add-closed");
    let ring =
        ring_buffer::RingBuffer::create(root.to_path_buf(), "closed".into(), 15, None).unwrap();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("closed".into(), ring);

    controller
        .add_marker("closed", 5)
        .expect("a stopped session takes the add directly");

    install_active(&controller, "session", idle_session());
    controller
        .add_marker("closed", 9)
        .expect("still direct while another session records");

    assert_eq!(
        controller
            .session_manifest("closed")
            .unwrap()
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![5, 9]
    );
    let active = controller.active.lock().unwrap();
    assert!(
        active["session"].pending_markers.lock().unwrap().is_empty(),
        "the recording session's queue did not take the stopped one's add"
    );
}

/// Task3960, the arm the shipped build actually takes: the marker has been
/// folded into the manifest already, and the session is a live container. The
/// edit rewrites the in-memory manifest, hands the record to the index writer,
/// and updates the `last_timeline` snapshot the confirmation page reads -- all
/// three, and no file write from this thread.
#[test]
fn a_live_container_marker_edit_is_staged_for_the_index_writer() {
    let root = super::tmp_dir("marker-staging");
    let mut ring = ring_buffer::RingBuffer::create_container(
        root.join("session.lvb"),
        "session".into(),
        15,
        None,
        None,
    );
    ring.merge_markers(vec![ring_buffer::MarkerRecord {
        time_100ns: 5,
        label: String::new(),
        color_index: None,
    }])
    .unwrap();
    let manifest = ring.manifest().clone();
    let controller = CaptureController::new();
    controller
        .sessions
        .lock()
        .unwrap()
        .insert("session".into(), ring);
    *controller.last_timeline.lock().unwrap() = Some(manifest);
    install_active(&controller, "session", idle_session());

    controller
        .update_marker("session", 5, "boss".into())
        .expect("a live container's marker rename is staged");

    assert_eq!(
        controller.session_manifest("session").unwrap().markers[0].label,
        "boss",
        "the process shows the new label at once"
    );
    assert_eq!(
        controller
            .last_timeline
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .markers[0]
            .label,
        "boss",
        "and so does the snapshot the confirmation page serves"
    );
    let staged = {
        let active = controller.active.lock().unwrap();
        let staged = active
            .get("session")
            .unwrap()
            .pending_meta
            .lock()
            .unwrap()
            .clone();
        staged
    };
    assert!(
        matches!(
            &staged[..],
            [ring_buffer::container::MetaEvent::MarkerRenamed { time_100ns: 5, label }] if label == "boss"
        ),
        "{staged:?}"
    );

    controller
        .delete_marker("session", 5)
        .expect("and so is the delete");
    assert!(controller
        .session_manifest("session")
        .unwrap()
        .markers
        .is_empty());
    let staged = {
        let active = controller.active.lock().unwrap();
        let staged = active
            .get("session")
            .unwrap()
            .pending_meta
            .lock()
            .unwrap()
            .clone();
        staged
    };
    assert!(
        matches!(
            staged.last(),
            Some(ring_buffer::container::MetaEvent::MarkerRemoved { time_100ns: 5 })
        ),
        "{staged:?}"
    );
    // An instant the manifest does not hold still reports NotFound, which is
    // the path `marker_save_error` keeps existing for.
    assert!(controller
        .update_marker("session", 999, "x".into())
        .is_err());
}

/// Task3240: the `last_timeline` snapshot has to follow a marker edit for a
/// session the catalog is *not* holding in memory, and it has to do so for a
/// `.lvb` container as well as a directory.
///
/// It used to re-read the session off disk with
/// `RingBuffer::open(buffer_root().join(session_id))`, which resolves only the
/// directory layout: a container lives at `<root>\<id>.lvb`, so the open always
/// `Err`ed and `if let Ok(..)` swallowed it -- the snapshot silently kept the
/// markers it was built with. The fix applies the same edit to the snapshot
/// instead of re-reading, so the two layouts cannot diverge and the edit costs
/// no disk read at all.
///
/// `sessions` is deliberately left empty so `edit_marker_at` takes its `None`
/// arm; `edit_marker_at` (rather than the public `add_marker`) so the fixture
/// can live in a temp directory instead of the user's real buffer folder.
#[test]
fn a_marker_edit_updates_the_timeline_snapshot_of_a_container_session_on_disk() {
    let root = super::tmp_dir("marker-snapshot");
    let session_id = "capture-0000000000000000000000000000003240";
    let path = root.join(format!("{session_id}.lvb"));
    ring_buffer::container::ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, &[0xA0; 64])
        .checkpoint()
        .closed()
        .build(&path)
        .expect("fixture builds");
    // The snapshot the review pane would be showing. Read and dropped here so
    // no handle outlives the guard that removes the directory.
    let manifest = {
        let ring = ring_buffer::RingBuffer::open_container(path.clone()).expect("open container");
        ring.manifest().clone()
    };
    assert!(manifest.markers.is_empty(), "the fixture starts with none");

    let controller = CaptureController::new();
    *controller.last_timeline.lock().unwrap() = Some(manifest);
    let snapshot_markers = || {
        controller
            .last_timeline
            .lock()
            .unwrap()
            .as_ref()
            .expect("a loaded session")
            .markers
            .clone()
    };

    controller
        .edit_marker_at(
            &root,
            session_id,
            LiveMarkerEdit::Add { time_100ns: 5 },
            |ring| ring.add_marker(5),
            |root| ring_buffer::add_marker(root, session_id, 5),
            |markers| {
                markers.push(ring_buffer::MarkerRecord {
                    time_100ns: 5,
                    label: String::new(),
                    color_index: None,
                })
            },
        )
        .expect("add reaches the container");

    assert_eq!(
        ring_buffer::RingBuffer::open_container(path.clone())
            .expect("reopen")
            .manifest()
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![5],
        "the container took the write"
    );
    assert_eq!(
        snapshot_markers()
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![5],
        "and the snapshot the UI serves followed it without a reload"
    );

    controller
        .edit_marker_at(
            &root,
            session_id,
            LiveMarkerEdit::Remove { time_100ns: 5 },
            |ring| ring.delete_marker(5),
            |root| ring_buffer::delete_marker(root, session_id, 5),
            |markers| markers.retain(|marker| marker.time_100ns != 5),
        )
        .expect("delete reaches the container");

    assert!(
        ring_buffer::RingBuffer::open_container(path)
            .expect("reopen")
            .manifest()
            .markers
            .is_empty(),
        "the container took the removal"
    );
    assert!(
        snapshot_markers().is_empty(),
        "and the snapshot lost the marker too"
    );
}

/// The other half of task3240's claim: the directory layout -- the one the old
/// `RingBuffer::open(root.join(id))` did resolve -- reaches the same snapshot,
/// so the two shapes cannot disagree about what the review pane is showing.
#[test]
fn a_marker_edit_updates_the_timeline_snapshot_of_a_directory_session_on_disk() {
    let root = super::tmp_dir("marker-snapshot-dir");
    let session_id = "session";
    let manifest = {
        let mut ring =
            ring_buffer::RingBuffer::create(root.join(session_id), session_id.to_owned(), 15, None)
                .expect("create ring");
        ring.close().expect("close");
        ring_buffer::RingBuffer::open(root.join(session_id))
            .expect("reopen")
            .manifest()
            .clone()
    };

    let controller = CaptureController::new();
    *controller.last_timeline.lock().unwrap() = Some(manifest);

    controller
        .edit_marker_at(
            &root,
            session_id,
            LiveMarkerEdit::Add { time_100ns: 5 },
            |ring| ring.add_marker(5),
            |root| ring_buffer::add_marker(root, session_id, 5),
            |markers| {
                markers.push(ring_buffer::MarkerRecord {
                    time_100ns: 5,
                    label: String::new(),
                    color_index: None,
                })
            },
        )
        .expect("add reaches the directory");

    assert_eq!(
        ring_buffer::RingBuffer::open(root.join(session_id))
            .expect("reopen")
            .manifest()
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![5],
        "the directory session took the write"
    );
    assert_eq!(
        controller
            .last_timeline
            .lock()
            .unwrap()
            .as_ref()
            .expect("a loaded session")
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![5],
        "and the snapshot matches what the container case produced"
    );
}

/// Task1680: the marker hotkey's time source. Nothing recording answers `None`
/// (the 録画していません refusal); a session answers with the last video PTS the
/// worker fed the encoder -- including the held frames a still window produces,
/// which is the whole point. The value tracks the atomic rather than a drained
/// preview frame, so it keeps moving while WGC delivers nothing.
#[test]
fn the_recording_position_follows_the_fed_video_pts() {
    let controller = CaptureController::new();
    assert_eq!(controller.recording_position_100ns(), None);

    let (stop_tx, _stop_rx) = bounded::<()>(1);
    let (_frames_tx, frames_rx) = bounded::<CapturedFrame>(1);
    let (_encoder_tx, encoder_rx) = bounded::<encoder::EncoderEvent>(1);
    let position = Arc::new(AtomicI64::new(0));
    let session = CaptureSession {
        stop: stop_tx,
        worker: None,
        // No real worker: the done channel's sender is already gone, which
        // reads as a worker that ended (t260929-ea5e).
        jobs: bounded(1).0,
        extra_audio: bounded(1).0,
        recording_done: bounded(1).1,
        frames: frames_rx,
        encoder_events: encoder_rx,
        diagnostics: blank_diagnostics(),
        last_sequence: 0,
        recording_position: position.clone(),
        encoded_frame_count: Arc::new(AtomicU64::new(0)),
        last_measured_frame_count: 0,
        last_measured_at: Instant::now(),
    };
    install_active(&controller, "position-session", session);

    // Recording, but no frame fed yet: still a refusal rather than a marker at 0.
    assert_eq!(controller.recording_position_100ns(), None);

    position.store(713_905_581_012, Ordering::Relaxed);
    assert_eq!(controller.recording_position_100ns(), Some(713_905_581_012));
    // A held frame advances it with no preview frame in sight.
    position.store(714_313_119_159, Ordering::Relaxed);
    assert_eq!(controller.recording_position_100ns(), Some(714_313_119_159));

    controller.active.lock().unwrap().clear();
}

/// Task1990: the queue belongs to the running capture, not to the controller.
/// So a marker goes to that recording's own queue, and one pressed with nothing
/// recording has nowhere to go -- where before it sat in the shared queue until
/// the next `start` cleared it, which is the drop this replaces.
#[test]
fn push_marker_queues_on_the_running_capture_and_drops_when_nothing_records() {
    let controller = CaptureController::new();
    controller.push_marker(500);
    assert!(
        controller.pending_markers().is_none(),
        "no recording, no queue"
    );

    install_active(&controller, "session", idle_session());
    controller.push_marker(1_000);
    controller.push_marker(2_000);
    let queue = controller.pending_markers().expect("the recording's queue");
    assert_eq!(
        queue
            .lock()
            .unwrap()
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![1_000, 2_000],
        "the marker pressed while nothing recorded is not in here"
    );

    // The queue goes with the entry: nothing survives into the next recording.
    controller.active.lock().unwrap().clear();
    assert!(controller.pending_markers().is_none());
}

/// Task2190: the hotkey names the session the review pane is showing, so the
/// queue has to be reachable by id -- not just as "the last-started one", which
/// with two captures running is the session the user is *not* looking at.
#[test]
fn push_marker_for_queues_on_the_named_capture_not_the_last_started_one() {
    let controller = CaptureController::new();
    install_active(&controller, "capture-first", idle_session());
    install_active(&controller, "capture-second", idle_session());

    controller.push_marker_for("capture-first", 1_000);
    controller.push_marker_for("capture-first", 2_000);

    let times = |session_id: &str| {
        controller
            .pending_markers_for(session_id)
            .expect("the capture's queue")
            .lock()
            .unwrap()
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>()
    };
    assert_eq!(times("capture-first"), vec![1_000, 2_000]);
    assert!(
        times("capture-second").is_empty(),
        "the last-started capture must not collect a marker aimed elsewhere"
    );

    // An id nobody is recording has no queue, so the marker is dropped rather
    // than landing on some other session.
    assert!(controller.pending_markers_for("no-such-session").is_none());
    controller.push_marker_for("no-such-session", 3_000);
    assert_eq!(times("capture-first"), vec![1_000, 2_000]);
    assert!(times("capture-second").is_empty());

    controller.active.lock().unwrap().clear();
}

#[test]
fn set_session_title_reports_an_unknown_session() {
    let controller = CaptureController::new();
    assert!(controller
        .set_session_title("no-such-session", Some("x".into()))
        .is_err());
}

#[test]
fn the_disk_guard_creates_the_buffer_root_before_measuring_it() {
    // task156: this ordering is the fix. `free_bytes` cannot answer for a path
    // that does not exist, so a fresh install (or a hand-deleted buffer) made
    // every `start` fail on the guard before anything created the directory.
    let root = std::env::temp_dir().join(format!(
        "rs-guard-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let output_dir = root.join("buffer").join("capture-1");
    let disk_root = output_dir.parent().unwrap();
    assert!(!disk_root.exists());

    ensure_buffer_root_has_space(&output_dir, 0).expect("the guard should create and then pass");
    assert!(
        disk_root.exists(),
        "the buffer root should have been created"
    );

    // Idempotent: a second start must not trip over the directory it made.
    ensure_buffer_root_has_space(&output_dir, 0).expect("a second call should still pass");

    let _ = std::fs::remove_dir_all(&root);
}

/// t260928-08df: a buffer folder that cannot be made is its own kind, so the
/// toast says "check the folder" rather than the OS error. A path under a
/// *file* fails `create_dir_all` on every machine; a creatable one is the
/// control that the kind is not simply what every call returns.
#[test]
fn a_buffer_folder_that_cannot_be_made_is_a_folder_failure() {
    let root = std::env::temp_dir().join(format!("livia-08df-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("a-file");
    std::fs::write(&file, b"not a folder").unwrap();

    let refused = ensure_buffer_root_has_space(&file.join("buffer").join("capture-1"), 0);
    assert!(
        matches!(refused, Err(StartError::BufferFolder(ref detail)) if detail.starts_with("could not create the buffer directory")),
        "{refused:?}"
    );
    assert_eq!(
        ensure_buffer_root_has_space(&root.join("buffer").join("capture-1"), 0),
        Ok(()),
        "the control: a folder that can be made passes"
    );

    let _ = std::fs::remove_dir_all(&root);
}

/// task2010: the start-time guard scales with how many captures are already
/// writing to the disk. Numbers only -- the real disk is never filled.
///
/// Task2050 moved the wording into the locale table, so the guard is asked for
/// the reason and `ui_state` is asked for the sentence.
#[test]
fn the_disk_guard_asks_for_one_reserve_per_running_capture() {
    use crate::ring_buffer::{MIN_FREE_BYTES as FLOOR, PER_CAPTURE_RESERVE_BYTES as RESERVE};
    use crate::ui_state::locale::Locale;
    // t260928-e309 (F12 / F14, DS 言葉): a refused start says 「開始できません」,
    // and a number and its unit are spaced -- no longer task2010's
    // 「安全停止しました。既存clipは…」.
    const FIRST_REFUSAL: &str = "空き容量が 10 GB 以下のため録画を開始できません。";
    let japanese = |free, running| {
        disk_guard_refusal(free, running)
            .map(|refusal| crate::ui_state::lifecycle::disk_refusal(Locale::Ja, refusal))
    };

    // Plenty free: both the first and the second capture start.
    assert_eq!(disk_guard_refusal(FLOOR + RESERVE * 4, 0), None);
    assert_eq!(disk_guard_refusal(FLOOR + RESERVE * 4, 1), None);

    // The case this task exists for: over the floor, under floor+reserve. The
    // first capture still starts; the second is refused because *it* would be
    // the one with no room left.
    let between = FLOOR + RESERVE / 2;
    assert_eq!(disk_guard_refusal(between, 0), None);
    let second = japanese(between, 1).expect("the second capture should be refused");
    assert_ne!(
        second, FIRST_REFUSAL,
        "the second capture's refusal has to say what is actually missing"
    );
    assert!(
        second.contains("15 GB") && second.contains("録画中の 1 本"),
        "the refusal should name the requirement and the running count: {second}"
    );

    // Under the floor: refused from the first capture, with the wording that
    // shipped before any of this -- unchanged, byte for byte.
    assert_eq!(japanese(FLOOR - 1, 0).as_deref(), Some(FIRST_REFUSAL));

    // Boundaries. `<` was the original comparison, so exactly the floor passes.
    assert_eq!(disk_guard_refusal(FLOOR, 0), None);
    assert_eq!(disk_guard_refusal(FLOOR + RESERVE, 1), None);
    assert!(disk_guard_refusal(FLOOR + RESERVE - 1, 1).is_some());
}

/// Which source a config asks for -- the one function the worker uses to decide
/// what to record (task1430). Since task2110 nothing else reads it: `start` no
/// longer judges the combination, so this is the whole of the audio decision.
#[test]
fn the_audio_source_of_a_config_is_decided_in_one_place() {
    let mut config = null_window_config(std::path::PathBuf::from("unused"));
    config.process_id = 4242;

    assert_eq!(
        audio::AudioSource::for_config(&config),
        audio::AudioSource::SelectedProcess(4242)
    );

    config.capture_all_audio = true;
    assert_eq!(
        audio::AudioSource::for_config(&config),
        audio::AudioSource::AllAppsExceptSelf
    );

    // A screen has no process to point at, so the toggle does not enter into it.
    config.kind = crate::capture::targets::CaptureTargetKind::Monitor;
    assert_eq!(
        audio::AudioSource::for_config(&config),
        audio::AudioSource::SystemMix
    );
    config.capture_all_audio = false;
    assert_eq!(
        audio::AudioSource::for_config(&config),
        audio::AudioSource::SystemMix
    );
}

/// Task2050: the refusals that used to be Japanese literals inside `start` /
/// `switch` now read in both languages, like task2020's pair. Four of them when
/// task2050 wrote this; three since task2080 deleted the count cap's, two since
/// task2170 deleted the stop-in-flight one along with the gate that raised it.
#[test]
fn the_remaining_start_refusals_read_in_both_languages() {
    use crate::ui_state::lifecycle::disk_refusal;
    use crate::ui_state::locale::Locale;
    const GB: u64 = 1024 * 1024 * 1024;

    let floor = |locale| {
        disk_refusal(
            locale,
            DiskRefusal {
                required_bytes: 10 * GB,
                free_bytes: 2 * GB,
                already_running: 0,
            },
        )
    };
    let reserve = |locale| {
        disk_refusal(
            locale,
            DiskRefusal {
                required_bytes: 15 * GB,
                free_bytes: 12 * GB,
                already_running: 1,
            },
        )
    };

    for locale in [Locale::Ja, Locale::En] {
        for message in [floor(locale), reserve(locale)] {
            assert!(
                message.chars().count() > 5,
                "{locale:?} has no wording: {message:?}"
            );
        }
        // The floor case is "free some room"; the reserve case has to name the
        // numbers instead, in either language.
        assert_ne!(floor(locale), reserve(locale));
    }

    assert!(reserve(Locale::En).contains("15 GB"));
}

/// Task1990: stopping takes the entry out of the map, so the controller reads
/// idle again and its queues are gone with it. The map *is* the "is something
/// recording" answer now -- nothing else has to be cleared to make it true.
#[test]
fn stopping_removes_the_entry_from_the_map() {
    let controller = CaptureController::new();
    install_active(&controller, "session", idle_session());
    assert!(controller.is_active());

    assert_eq!(controller.stop_blocking(), CaptureStopReason::Requested);

    assert!(!controller.is_active());
    assert!(controller.active.lock().unwrap().is_empty());
    assert_eq!(controller.recording_position_100ns(), None);
}

/// Task1990's uniqueness guard. Two rings on one `.lvb` is the one outcome no
/// future cap change may allow: `ContainerWriter::create` truncates an existing
/// file back to its last checkpoint, which would cut the running recording in
/// half. Today the cap refuses first -- with the message the user has always
/// seen -- and the assert is on the outcome, so this keeps holding once
/// task2000 raises the cap and the id guard becomes the check that fires.
#[test]
fn starting_a_session_id_that_is_already_running_is_refused_and_touches_no_file() {
    let root = std::env::temp_dir().join(format!("livia-dup-start-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");
    let session_id = "capture-already-running";
    let container = root.join(format!("{session_id}.lvb"));
    std::fs::write(&container, b"the running recording's bytes").expect("fixture");

    let controller = CaptureController::new();
    install_active(&controller, session_id, idle_session());

    let refusal = controller.start(CaptureConfig {
        window_handle: "0x0".into(),
        process_id: 0,
        kind: crate::capture::targets::CaptureTargetKind::Window,
        frame_rate: 60,
        include_cursor: true,
        output_size: CaptureSize {
            width: 1920,
            height: 1080,
        },
        retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
        encoder_output_dir: root.join(session_id),
        container_path: None,
        capture_all_audio: false,
        codec: crate::settings::RecordingCodec::H264,
        extra_audio: Vec::new(),
    });

    assert_eq!(
        refusal.map_err(|error| error.to_string()),
        Err("a capture is already running".to_owned()),
        "the refusal the user has always seen"
    );
    assert!(
        controller.active.lock().unwrap().contains_key(session_id),
        "the running capture is still the one in the map"
    );
    assert_eq!(
        std::fs::read(&container).expect("still there"),
        b"the running recording's bytes",
        "the running recording's container was not created over"
    );
    controller.active.lock().unwrap().clear();
    let _ = std::fs::remove_dir_all(&root);
}

/// Task1990's real hazard. The sweep protected **one** session -- whatever
/// `active_session` named -- which was complete only while the cap was one. It
/// now protects every id in the map, and this pins the plural shape: two
/// running sessions, both over the capacity budget, both left alone.
#[test]
fn reclaim_protects_every_running_session_not_just_one() {
    let _root_lock = BUFFER_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("livia-reclaim-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");

    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let running = [
        format!("capture-{:032x}", now_nanos),
        format!("capture-{:032x}", now_nanos + 1),
    ];
    for id in &running {
        crate::ring_buffer::container::ContainerBuilder::new(id)
            .segment(0, 0, 20_000_000, &vec![0xA0; 4096])
            .checkpoint()
            .closed()
            .build(&root.join(format!("{id}.lvb")))
            .expect("fixture builds");
    }

    CaptureController::apply_buffer_root(Some(root.clone())).expect("root applies");
    let controller = CaptureController::new();
    for id in &running {
        install_active(&controller, id, idle_session());
    }
    // A budget of one byte: everything not protected is over capacity.
    let report = controller.reclaim_sessions(1, 30, None);
    controller.active.lock().unwrap().clear();
    CaptureController::apply_buffer_root(None).expect("root resets");
    let report = report.expect("sweep runs");

    assert_eq!(
        report.removed_sessions, 0,
        "a running recording is never reclaimed, however far over budget it is"
    );
    for id in &running {
        assert!(
            root.join(format!("{id}.lvb")).is_file(),
            "{id} was deleted out from under its writer"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

/// Task3970. Since task3700 a past session can sit on the review screen while
/// a recording stops and the sweep runs; the stop doors now hand it over as
/// `loaded_session_id`. Two finished sessions, both over budget: the loaded one
/// stays, and the other one going is the control that the sweep did run.
#[test]
fn reclaim_spares_the_session_the_review_screen_has_loaded() {
    let _root_lock = BUFFER_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("livia-reclaim-loaded-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");

    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let loaded = format!("capture-{:032x}", now_nanos);
    let other = format!("capture-{:032x}", now_nanos + 1);
    for id in [&loaded, &other] {
        crate::ring_buffer::container::ContainerBuilder::new(id)
            .segment(0, 0, 20_000_000, &vec![0xA0; 4096])
            .checkpoint()
            .closed()
            .build(&root.join(format!("{id}.lvb")))
            .expect("fixture builds");
    }

    CaptureController::apply_buffer_root(Some(root.clone())).expect("root applies");
    let controller = CaptureController::new();
    // A budget of one byte: everything not protected is over capacity.
    let report = controller.reclaim_sessions(1, 30, Some(loaded.clone()));
    CaptureController::apply_buffer_root(None).expect("root resets");
    let report = report.expect("sweep runs");

    assert!(
        root.join(format!("{loaded}.lvb")).is_file(),
        "the session on the review screen was reclaimed"
    );
    assert!(
        !root.join(format!("{other}.lvb")).exists(),
        "the unprotected session survived, so the sweep never ran"
    );
    assert_eq!(report.removed_sessions, 1, "{report:?}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A capture that is running, told apart from the others by its counters.
fn tagged_session(dropped_frames: u64, position_100ns: i64) -> CaptureSession {
    let mut session = idle_session();
    session.diagnostics.dropped_frames = dropped_frames;
    session.recording_position = Arc::new(AtomicI64::new(position_100ns));
    session
}

/// Spins until the spawned stop thread has released the `stopping` flag. Every
/// async stop here finishes in microseconds -- nothing is joined, the sessions
/// have no worker -- so this only exists to not race the thread spawn.
///
/// **Not usable when a session id was put in the set by hand** (the task2170
/// tests below): nothing releases that one, so this would spin to its deadline.
/// Use `wait_until_gone` there instead.
fn wait_for_stop(controller: &CaptureController) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller.is_stopping() {
        assert!(Instant::now() < deadline, "the async stop never finished");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Spins until `session_id` has left `active`. Same purpose as `wait_for_stop`
/// -- not racing the spawned thread -- keyed on the map rather than the stopping
/// set, so it works while another id is parked in that set (task2170).
fn wait_until_gone(controller: &CaptureController, session_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while controller
        .active_session_ids()
        .iter()
        .any(|id| id == session_id)
    {
        assert!(
            Instant::now() < deadline,
            "{session_id} was never taken out of the map"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn null_window_config(output_dir: std::path::PathBuf) -> CaptureConfig {
    CaptureConfig {
        window_handle: "0x0".into(),
        process_id: 0,
        kind: crate::capture::targets::CaptureTargetKind::Window,
        frame_rate: 60,
        include_cursor: true,
        output_size: CaptureSize {
            width: 1920,
            height: 1080,
        },
        retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
        encoder_output_dir: output_dir,
        container_path: None,
        capture_all_audio: false,
        codec: crate::settings::RecordingCodec::H264,
        extra_audio: Vec::new(),
    }
}

/// Nothing in `start` refuses a capture for being the Nth one (task2080; the
/// one-at-a-time error went in task2000, the fixed cap of two in task2080).
/// Neither the second nor the third start may come back with count wording.
///
/// Both starts are asserted on what they are *not* refused with rather than on
/// `Ok`: the target is a null window, so the capture worker each one spawns is
/// free to fail on its own -- what this pins is that execution got past the
/// count checks that used to sit at the top of `start`.
#[test]
fn no_start_is_refused_for_being_the_nth_capture() {
    let root = std::env::temp_dir().join(format!("livia-cap-nth-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let controller = CaptureController::new();
    // The wording task2080 deleted, as literals: the function that rendered it
    // is gone, so there is nothing left to call.
    let names_a_count = |error: &str| {
        error.contains("同時に録画できるのは") || error.contains("recordings can run at the same")
    };

    install_active(&controller, "capture-first", idle_session());
    if let Err(error) = controller
        .start(null_window_config(root.join("capture-second")))
        .map_err(|error| error.to_string())
    {
        assert!(
            !names_a_count(&error),
            "the second start was capped: {error}"
        );
        assert_ne!(
            error, "a capture is already running",
            "the one-at-a-time refusal went in task2000"
        );
    }

    install_active(&controller, "capture-second-stub", idle_session());
    if let Err(error) = controller
        .start(null_window_config(root.join("capture-third")))
        .map_err(|error| error.to_string())
    {
        assert!(
            !names_a_count(&error),
            "the third start was capped: {error}"
        );
    }

    // Task2110 removed task2020's combination rule: no audio setting refuses a
    // start any more either. Task2020's two refusals, as literals: the function
    // that rendered them is gone. All apps beside a running capture, and beside
    // a running all-apps one (each iteration leaves its stub installed).
    let names_audio = |error: &str| {
        error.contains("すべてのアプリの音")
            || error.contains("Record other applications too")
            || error.contains("every application's sound")
    };
    use crate::capture::targets::CaptureTargetKind::{Monitor, Window};
    for (name, kind, all_audio) in [
        ("capture-all-apps", Window, true),
        ("capture-all-apps-2", Window, true),
        ("capture-monitor", Monitor, false),
        ("capture-window", Window, false),
    ] {
        let mut config = null_window_config(root.join(name));
        config.kind = kind;
        config.capture_all_audio = all_audio;
        if let Err(error) = controller.start(config).map_err(|error| error.to_string()) {
            assert!(
                !names_audio(&error),
                "{name} was refused its audio: {error}"
            );
            assert!(!names_a_count(&error), "{name} was capped: {error}");
        }
        install_active(&controller, &format!("{name}-stub"), idle_session());
    }

    controller.stop_blocking();
    let _ = std::fs::remove_dir_all(&root);
}

/// Task2000's session-scoped stop. `stop_async` keeps meaning "stop recording"
/// -- the three UI stop paths call it and must keep stopping everything -- and
/// `stop_session_async` is the one that picks. Neither may announce a stop
/// while the other capture is still writing frames.
#[test]
fn stop_session_async_stops_one_capture_and_stop_async_stops_them_all() {
    let controller = CaptureController::new();
    let sink = Arc::new(crate::events::RecordingSink::default());
    controller.set_event_sink(sink.clone());
    install_active(&controller, "capture-first", idle_session());
    install_active(&controller, "capture-second", idle_session());
    *controller.lifecycle.lock().unwrap() = CaptureLifecycleStatus::recording();
    assert_eq!(
        controller.active_session_ids(),
        vec!["capture-first".to_owned(), "capture-second".to_owned()],
        "listed oldest start first, whatever order the map holds them in"
    );

    controller.stop_session_async("capture-first");
    wait_for_stop(&controller);

    assert_eq!(
        controller.active_session_ids(),
        vec!["capture-second".to_owned()],
        "only the named capture was stopped"
    );
    assert!(controller.is_active());
    let settled = controller.lifecycle_status();
    assert_eq!(
        (settled.state.as_str(), settled.diagnostic_code.as_deref()),
        ("stopped", Some("CAP-EXIT-001")),
        "the lifecycle reports the last stop event, even beside a running capture (task2090)"
    );
    // Task2000 kept the tray silent here; task2030 rewrites it instead, because
    // the tooltip now carries the count -- what must never happen is 待機中
    // while frames are still being written.
    let tooltips = sink.tooltips.lock().unwrap();
    assert_ne!(
        tooltips.last().map(String::as_str),
        Some(crate::ui_state::lifecycle::tray_tooltip_idle(
            crate::ui_state::locale::Locale::Ja
        )),
        "the tray must not say 待機中 while a capture is still running"
    );
    drop(tooltips);

    controller.stop_async();
    wait_for_stop(&controller);

    assert!(controller.active_session_ids().is_empty());
    assert!(!controller.is_active());
    assert_eq!(controller.lifecycle_status().state, "stopped");
    let tooltips = sink.tooltips.lock().unwrap();
    // Task2030: the first stop rewrote the tooltip to name the one capture
    // still running; 待機中 arrives only once the last one is gone, and it is
    // the last thing the tray is told.
    assert_eq!(
        tooltips.last().map(String::as_str),
        Some(crate::ui_state::lifecycle::tray_tooltip_idle(
            crate::ui_state::locale::Locale::Ja
        )),
        "the tray goes idle when the last capture is gone: {tooltips:?}"
    );
    assert_eq!(
        tooltips
            .iter()
            .filter(|tooltip| *tooltip
                == crate::ui_state::lifecycle::tray_tooltip_idle(
                    crate::ui_state::locale::Locale::Ja
                ))
            .count(),
        1,
        "and only then: {tooltips:?}"
    );
}

/// Task2170, the defect itself. With one process-wide flag, a stop of A made
/// `stop_session_async(B)` return `Requested` without spawning anything: B kept
/// recording while its capsule sat on 停止中…. A's id is put in the set by hand
/// rather than by a real stop, so "A is mid-stop" is a decided state and nothing
/// here waits on a join.
#[test]
fn stopping_one_capture_does_not_swallow_the_stop_of_another() {
    let controller = CaptureController::new();
    install_active(&controller, "capture-a", idle_session());
    install_active(&controller, "capture-b", idle_session());
    controller
        .stopping
        .lock()
        .unwrap()
        .insert("capture-a".to_owned());

    controller.stop_session_async("capture-b");
    wait_until_gone(&controller, "capture-b");

    assert_eq!(
        controller.active_session_ids(),
        vec!["capture-a".to_owned()],
        "B was stopped for real; A is still there for its own stop to finish"
    );
    controller.stopping.lock().unwrap().clear();
    controller.active.lock().unwrap().clear();
}

/// The same defect on the start side (task2170 判断5): the gate `start` used to
/// open with read the one flag, so a stop of A refused a start of anything at
/// all. A start mints its own session id and can never collide with the one
/// being stopped.
#[test]
fn a_start_is_not_refused_while_another_capture_is_stopping() {
    let root =
        std::env::temp_dir().join(format!("livia-start-while-stopping-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let controller = CaptureController::new();
    install_active(&controller, "capture-a", idle_session());
    controller
        .stopping
        .lock()
        .unwrap()
        .insert("capture-a".to_owned());

    // Null window, so the worker this spawns is free to fail on its own; what
    // is pinned is that execution got past the gate that used to sit on top.
    if let Err(error) = controller
        .start(null_window_config(root.join("capture-new")))
        .map_err(|error| error.to_string())
    {
        assert!(
            !error.contains("停止処理中") && !error.contains("still stopping"),
            "the start was refused by another capture's stop: {error}"
        );
    }

    controller.stopping.lock().unwrap().clear();
    controller.stop_blocking();
    let _ = std::fs::remove_dir_all(&root);
}

/// Pressing stop twice on the *same* capture stays a no-op the second time
/// (task2170 keeps task2000's idempotence). The first press is simulated by the
/// hand-inserted id, so no thread can finish and release the claim in between --
/// two live calls would race that.
#[test]
fn stopping_the_same_capture_twice_claims_it_once() {
    let controller = CaptureController::new();
    install_active(&controller, "capture-a", idle_session());
    controller
        .stopping
        .lock()
        .unwrap()
        .insert("capture-a".to_owned());

    controller.stop_session_async("capture-a");
    controller.stop_session_async("capture-a");

    let stopping = controller.stopping.lock().unwrap();
    assert_eq!(
        stopping.len(),
        1,
        "the second press queued a second stop: {stopping:?}"
    );
    drop(stopping);
    assert!(
        controller
            .active_session_ids()
            .contains(&"capture-a".to_owned()),
        "neither call may have spawned a stop while one was already in flight"
    );
    controller.stopping.lock().unwrap().clear();
    controller.active.lock().unwrap().clear();
}

/// `stop_async` is the tray's "stop everything". Task2170 rebuilt it on
/// `stop_session_async` per id, so each capture settles on its own and the
/// tooltip counts down instead of jumping to 待機中. Three running, because two
/// is the case the old drain also got right.
///
/// The stops run in parallel, so only the *last* tooltip is deterministic --
/// every thread takes its entry out of the map before it settles, so whichever
/// settles last reads an empty map. The intermediate counts are not asserted.
#[test]
fn stop_async_stops_every_running_capture() {
    let controller = CaptureController::new();
    let sink = Arc::new(crate::events::RecordingSink::default());
    controller.set_event_sink(sink.clone());
    for id in ["capture-a", "capture-b", "capture-c"] {
        install_active(&controller, id, idle_session());
    }
    *controller.lifecycle.lock().unwrap() = CaptureLifecycleStatus::recording();

    controller.stop_async();
    wait_for_stop(&controller);

    assert!(controller.active_session_ids().is_empty());
    assert!(!controller.is_active());
    assert!(
        controller.stopping.lock().unwrap().is_empty(),
        "every claim was released"
    );
    assert_eq!(
        sink.tooltips.lock().unwrap().last().map(String::as_str),
        Some(crate::ui_state::lifecycle::tray_tooltip_idle(
            crate::ui_state::locale::Locale::Ja
        )),
        "the tray ends on 待機中 once the last capture is gone"
    );
}

/// The no-argument getters kept their callers (the picker's poll, the marker
/// hotkey, the tray) working by answering for the **last started** capture,
/// which is the same capture they always answered for while only one could run.
/// The `_for` versions are what task2030's UI will read per tile.
#[test]
fn the_no_argument_getters_answer_for_the_last_started_capture() {
    let controller = CaptureController::new();
    install_active(&controller, "capture-first", tagged_session(11, 1_000));
    install_active(&controller, "capture-second", tagged_session(22, 2_000));

    assert_eq!(controller.diagnostics().unwrap().dropped_frames, 22);
    assert_eq!(controller.recording_position_100ns(), Some(2_000));
    assert_eq!(
        controller
            .diagnostics_for("capture-first")
            .unwrap()
            .dropped_frames,
        11
    );
    assert_eq!(
        controller.recording_position_100ns_for("capture-first"),
        Some(1_000)
    );
    // A capture that is not running has no live counters of its own; only the
    // no-argument version falls back to the last recording's.
    assert!(controller.diagnostics_for("capture-gone").is_none());
    assert_eq!(
        controller.recording_position_100ns_for("capture-gone"),
        None
    );

    controller.active.lock().unwrap().clear();
}

/// A capture that dies beside a running one has to reach the user (task2090).
/// Task2000 skipped the whole of `settle_lifecycle` while anything was still
/// active, which kept the tray honest but also swallowed the failure: the toast
/// path is the only reader of `lifecycle`, so the user pressed start, saw
/// nothing, and had no recording. Only the tray stays conditional.
#[test]
fn one_capture_dying_on_its_own_is_reported_while_the_other_keeps_recording() {
    let controller = CaptureController::new();
    let sink = Arc::new(crate::events::RecordingSink::default());
    controller.set_event_sink(sink.clone());
    let died = install_active(&controller, "capture-first", idle_session());
    let _alive = install_active(&controller, "capture-second", idle_session());
    *controller.lifecycle.lock().unwrap() = CaptureLifecycleStatus::recording();

    died.send(CaptureStopReason::InitializationFailed {
        stage: VIDEO_ENCODER_STAGE,
    })
    .expect("report");
    // Any poll is what notices; the picker's runs every second.
    assert!(controller.is_active());
    wait_for_stop(&controller);

    assert_eq!(
        controller.active_session_ids(),
        vec!["capture-second".to_owned()],
        "the dead capture was reaped, the live one left alone"
    );
    let status = controller.lifecycle_status();
    assert_eq!(
        (
            status.state.as_str(),
            status.diagnostic_code.as_deref(),
            status.failure_stage.as_deref()
        ),
        ("stopped", Some("CAP-DEV-001"), Some(VIDEO_ENCODER_STAGE)),
        "the failure the toast needs, published even though another capture runs"
    );
    // Process-global counter shared with every other test: only "this is a stop
    // event" is assertable, never a particular number.
    assert_ne!(status.seq, 0, "a stop event carries its own stamp");
    // Task2000 kept the tray silent here; task2030 rewrites it instead, because
    // the tooltip now carries the count -- what must never happen is 待機中
    // while frames are still being written.
    let tooltips = sink.tooltips.lock().unwrap();
    assert_ne!(
        tooltips.last().map(String::as_str),
        Some(crate::ui_state::lifecycle::tray_tooltip_idle(
            crate::ui_state::locale::Locale::Ja
        )),
        "the tray must not say 待機中 while a capture is still running"
    );
    drop(tooltips);

    controller.active.lock().unwrap().clear();
}

/// Task700. Two sessions were reclaimed on 2026-08-19 with the history quietly
/// one shorter and not a line anywhere -- searching that day's log for
/// `prune|reclaim|retention|delete` returned nothing. The sweep now has to
/// report what it took *and* why, because only the capacity half is worth
/// interrupting the user for.
#[test]
fn reclaim_reports_the_capacity_share_apart_from_the_aged_out_one() {
    let _root_lock = BUFFER_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let root = std::env::temp_dir().join(format!("livia-reclaim-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("root");

    // Ids carry the start time, which is what decides "too old". One from
    // roughly now, one from well past any lifetime.
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    const NANOS_PER_DAY: u128 = 24 * 60 * 60 * 1_000_000_000;
    let fresh = format!("capture-{:032x}", now_nanos);
    let ancient = format!("capture-{:032x}", now_nanos - 400 * NANOS_PER_DAY);

    // Container sessions, not directories: `CaptureController::new` sweeps every
    // pre-container session directory it finds (task450), so a hand-made one
    // would be gone before the sweep under test ever ran.
    for (id, bytes) in [(&fresh, 4096usize), (&ancient, 2048usize)] {
        crate::ring_buffer::container::ContainerBuilder::new(id)
            .segment(0, 0, 20_000_000, &vec![0xA0; bytes])
            .checkpoint()
            .closed()
            .build(&root.join(format!("{id}.lvb")))
            .expect("fixture builds");
    }

    CaptureController::apply_buffer_root(Some(root.clone())).expect("root applies");
    let controller = CaptureController::new();
    // A budget of one byte: everything that is not aged out is over capacity.
    // Not zero -- since round8 §3-2 that is 「なし」, i.e. no capacity limit.
    let report = controller.reclaim_sessions(1, 30, None);
    CaptureController::apply_buffer_root(None).expect("root resets");
    let report = report.expect("sweep runs");

    assert_eq!(report.removed_sessions, 2, "both sessions were reclaimed");
    assert_eq!(
        report.over_capacity_sessions, 1,
        "only the one that was not already past its lifetime counts as capacity"
    );
    assert!(
        report.over_capacity_bytes > 0 && report.over_capacity_bytes < report.freed_bytes,
        "the capacity share is real and smaller than the total: {report:?}"
    );
    assert!(
        !root.join(format!("{fresh}.lvb")).exists()
            && !root.join(format!("{ancient}.lvb")).exists(),
        "the sweep actually deleted them"
    );
    let _ = std::fs::remove_dir_all(&root);
}

// ---- task3910: the buffer-root override ----

/// The priority is a pure function so it can be checked without
/// `std::env::set_var`, which is process-wide and would race every other test
/// through `BUFFER_ROOT_LOCK`.
#[test]
fn the_buffer_root_override_outranks_the_configured_folder() {
    let over = Path::new(r"C:\Temp\livia-buffer-3910");
    let configured = Path::new(r"D:\Apps\Liveback");
    let local = Path::new(r"C:\Users\someone\AppData\Local");

    assert_eq!(
        crate::capture::resolve_buffer_root(Some(over), Some(configured), Some(local)),
        over,
        "the override wins over settings.json's bufferDirectory"
    );
    assert_eq!(
        crate::capture::resolve_buffer_root(None, Some(configured), Some(local)),
        configured,
        "with no override the configured folder is unchanged"
    );
    assert_eq!(
        crate::capture::resolve_buffer_root(None, None, Some(local)),
        local.join("Liveback").join("buffer"),
        "and with neither, the default is the one it always was"
    );
    // No `%LOCALAPPDATA%` is not a reason to write into the current directory --
    // the pre-3910 code fell back to the temp folder and still does.
    assert_eq!(
        crate::capture::resolve_buffer_root(None, None, None),
        std::env::temp_dir().join("Liveback").join("buffer")
    );
}

/// A launcher that computes the path and comes up empty must land on today's
/// behaviour, not on a relative root next to the exe.
#[test]
fn a_blank_buffer_root_override_reads_as_unset() {
    use std::ffi::OsStr;

    assert_eq!(crate::capture::buffer_root_override_from(None), None);
    assert_eq!(
        crate::capture::buffer_root_override_from(Some(OsStr::new(""))),
        None
    );
    assert_eq!(
        crate::capture::buffer_root_override_from(Some(OsStr::new("   \t "))),
        None
    );
    assert_eq!(
        crate::capture::buffer_root_override_from(Some(OsStr::new(r"  C:\Temp\livia  "))),
        Some(std::path::PathBuf::from(r"C:\Temp\livia")),
        "a real value is taken, trimmed"
    );
}

/// t260924-327f: the playback worker asks this at every segment crossing, and
/// on 2026-09-24 the answer took 9s -- `index pass locked_ms=9629` ended 2ms
/// before the stalled crossing did, because the index writer holds `sessions`
/// across its container appends. The answer lives in `active` alone, so it must
/// come back while `sessions` is held by someone else.
#[test]
fn the_last_started_answer_does_not_wait_for_the_session_catalog() {
    let controller = CaptureController::new();
    install_active(&controller, "first", idle_session());
    install_active(&controller, "second", idle_session());
    // The index writer's side of it: `sessions` held for the whole question.
    let held = controller.sessions.clone();
    let catalog = held.lock().unwrap();

    let asking = controller.clone();
    let (answer_tx, answer_rx) = crossbeam_channel::bounded(1);
    thread::spawn(move || {
        let _ = answer_tx.send((
            asking.is_last_started("second"),
            asking.is_last_started("first"),
            asking.is_last_started("closed"),
        ));
    });
    let answer = answer_rx.recv_timeout(Duration::from_secs(2));
    drop(catalog);
    assert_eq!(
        answer,
        Ok((true, false, false)),
        "answered without `sessions`, and still the last-started capture only"
    );
}
