//! End-to-end capture automation against real test windows.
use super::transform::{
    verify_a_one_pixel_overhang_is_cropped_not_resampled,
    verify_d3d11_debug_layer_is_available_for_capture_diagnostics,
    verify_gpu_transform_pipeline_compiles_on_the_active_adapter,
    verify_gpu_transform_writes_a_fixed_bgra8_texture, verify_hdr_float_source_tone_maps_to_bgra8,
    verify_hdr_tone_map_returns_sdr_sources_to_their_original_srgb_values,
    verify_sdr_transform_preserves_source_pixel_values,
    verify_the_thumbnail_readback_is_shrunk_on_the_gpu,
};
use super::*;

fn verify_capture_automation_resize_keeps_the_session_output_fixed() {
    let window = CaptureTestWindow::create(false);
    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: true,
            output_size: CaptureSize {
                width: 1920,
                height: 1080,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("WGC session should start before resize");
    std::thread::sleep(std::time::Duration::from_millis(750));
    let before = session.diagnostics();
    window.resize(960, 720);
    std::thread::sleep(std::time::Duration::from_millis(750));
    let after = session.diagnostics();
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);

    assert!(matches!(
        events.try_recv(),
        Ok(CaptureStopReason::Requested | CaptureStopReason::SourceClosed)
    ));
    assert_ne!(before.input_size, after.input_size);
    assert_eq!(after.output_size.width, 1920);
    assert_eq!(after.output_size.height, 1080);
}

/// The inverse of what this used to assert (task163): minimizing is no longer
/// a stop. WGC delivers nothing at all for a minimized window, so this is also
/// the case the frame-hold watchdog exists for -- the session has to stay up
/// with no frames arriving, which is exactly what `recv_timeout` checks by
/// timing out instead of receiving a reason.
fn minimizing_a_capture_target_keeps_the_session_recording() {
    let window = CaptureTestWindow::create(false);
    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: true,
            output_size: CaptureSize {
                width: 1920,
                height: 1080,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("WGC session should start before minimize");
    std::thread::sleep(std::time::Duration::from_millis(750));
    window.minimize();
    assert_eq!(
        events.recv_timeout(std::time::Duration::from_secs(3)).ok(),
        None,
        "a minimized target must not stop the recording"
    );
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);
    // The stop the user asked for still arrives, so the channel itself works
    // and the assertion above was a real silence rather than a broken pipe.
    assert!(matches!(
        events.try_recv(),
        Ok(CaptureStopReason::Requested | CaptureStopReason::SourceClosed)
    ));
}

fn closing_a_capture_target_stops_and_finalizes_safely() {
    let window = CaptureTestWindow::create(false);
    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: true,
            output_size: CaptureSize {
                width: 1920,
                height: 1080,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("WGC session should start before target close");
    std::thread::sleep(std::time::Duration::from_millis(750));
    window.close();
    assert_eq!(
        events
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("closed target should stop capture"),
        CaptureStopReason::SourceClosed
    );
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);
    assert!(session.diagnostics().encoder_errors.is_empty());
}

// Task176: was `#[ignore]`d because "this environment's GPU/WGC/COM resources are
// flaky under sustained real-capture load across a full test run". A fresh worker
// process per case is exactly that sustained load removed, so it runs again.
fn audio_telemetry_does_not_starve_finalized_segments_out_of_the_encoder_event_channel() {
    // Task088 follow-up regression: `encoder_events` is a `bounded(32)` channel shared
    // between `Finalized(segment)` and `AudioTelemetry`. `AudioTelemetry` used to be sent
    // on every accepted video frame (rare relative to the channel size); the event-driven
    // audio fix made the audio side fire on every WASAPI period (~10ms) instead, and
    // nothing drains this channel until `diagnostics()` is called below. If
    // `AudioTelemetry` weren't paced by video frame arrival, ~5 seconds of ~100
    // telemetry events/sec would fill the channel long before either segment below
    // finalizes and silently drop `Finalized` behind it (observed in manual
    // verification: real segment files present on disk, `manifest.json` empty).
    let window = CaptureTestWindow::create(false);
    let output_dir = test_encoder_output_dir();
    let (stopped, _events) = bounded(1);
    let (finalized_segments, segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: std::process::id(),
            frame_rate: 30,
            include_cursor: false,
            // Deliberately tiny: this test only cares about the audio/channel behavior,
            // and this environment's GPU/COM resources get scarce by the tail end of a
            // full `cargo test` run -- a small encode target keeps this test's own
            // footprint light so it doesn't tip an already-loaded run over the edge.
            output_size: CaptureSize {
                width: 320,
                height: 180,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("capture with own-process audio should start");
    // This synthetic test window never redraws, so WGC delivers (at most) its initial
    // frame and then starves -- exactly the sparse-video condition that made this bug
    // possible in the first place. `should_force_keyframe` is only evaluated on video
    // frame arrival, so segment 0 stays open the whole 8s and only finalizes when `stop`
    // forces it; the point of this test is that ~800 undrained `AudioTelemetry` events
    // (real WASAPI ticks, no diagnostics() call in between) do not fill the channel and
    // crowd out that one `Finalized` event when it is finally sent.
    std::thread::sleep(std::time::Duration::from_secs(2));
    session.stop();
    let diagnostics = session.diagnostics();
    // Since task430 the two no longer share a channel at all: telemetry goes to
    // the UI's bounded(32) queue, the finalized segment to the index writer's
    // unbounded one. Reading it here is reading exactly what that writer would
    // have folded into the manifest.
    let finalized: Vec<_> = segments.try_iter().collect();
    let _ = std::fs::remove_dir_all(&output_dir);
    assert!(
        !finalized.is_empty(),
        "the open segment's Finalized event must survive ~800 undrained AudioTelemetry \
         sends, not be silently dropped behind them"
    );
    assert!(diagnostics.encoder_errors.is_empty());
}

struct LogBuffer(std::sync::Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn persistently_zero_sized_window_fails_safely_at_the_frame_pool_stage() {
    let window = CaptureTestWindow::create(true);
    window.resize(0, 0);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: true,
            output_size: CaptureSize {
                width: 0,
                height: 0,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("capture thread should spawn for a zero-sized window");
    assert!(matches!(
        events
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a window that stays zero-sized should fail startup, not hang"),
        CaptureStopReason::InitializationFailed { .. }
    ));
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);
}

fn transiently_zero_sized_window_recovers_once_it_reports_a_real_size() {
    // SetWindowPos on another thread would block waiting for this (window-owning)
    // thread to pump messages, so the resize must happen here, not on a spawned
    // thread: the capture worker thread never touches this window's message queue.
    let window = CaptureTestWindow::create(true);
    window.resize(0, 0);
    std::thread::sleep(std::time::Duration::from_millis(100));

    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: true,
            output_size: CaptureSize {
                width: 0,
                height: 0,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("capture thread should spawn for a zero-sized window");
    std::thread::sleep(std::time::Duration::from_millis(150));
    window.resize(640, 360);
    std::thread::sleep(std::time::Duration::from_millis(750));
    assert!(
        events.try_recv().is_err(),
        "capture should still be running once the window reports a real size"
    );
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);
    assert!(session.diagnostics().encoder_errors.is_empty());
}

fn startup_stage_log_excludes_pid_hwnd_title_and_path() {
    let log = std::sync::Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer_log = log.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || LogBuffer(writer_log.clone()))
        .with_ansi(false)
        .without_time()
        .finish();

    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let invalid_handle = "not-a-valid-hwnd".to_owned();
    // The capture worker runs on its own thread, which does not inherit a
    // thread-local `with_default` subscriber, so this test needs the global one.
    let _ = tracing::subscriber::set_global_default(subscriber);
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: invalid_handle.clone(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: true,
            output_size: CaptureSize {
                width: 0,
                height: 0,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("capture thread should spawn even with an invalid window handle");
    assert!(matches!(
        events
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("invalid window handle should fail startup quickly"),
        CaptureStopReason::InitializationFailed {
            stage: "window_handle"
        }
    ));
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);

    let text = String::from_utf8(log.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("stage") && text.contains("window_handle"),
        "log should record the fixed stage name: {text}"
    );
    assert!(
        !text.contains(&invalid_handle),
        "log must not leak the raw window handle value: {text}"
    );
    assert!(
        !text.to_lowercase().contains("hwnd") && !text.contains('\\'),
        "log must not contain hwnd or path-shaped content: {text}"
    );
}

/// Every case the worker knows how to run, and the only list the parent iterates.
/// One table rather than a name array plus a `match` on the other side: with two
/// lists a case added to only one of them silently never runs, which is exactly
/// how the whole harness sat dead behind a mistyped test filter.
const GPU_AUTOMATION_CASES: &[(&str, fn())] = &[
    (
        "pipeline",
        verify_gpu_transform_pipeline_compiles_on_the_active_adapter,
    ),
    (
        "debug",
        verify_d3d11_debug_layer_is_available_for_capture_diagnostics,
    ),
    ("sdr", verify_gpu_transform_writes_a_fixed_bgra8_texture),
    (
        "sdr-pixel-values",
        verify_sdr_transform_preserves_source_pixel_values,
    ),
    (
        "odd-source-crop",
        verify_a_one_pixel_overhang_is_cropped_not_resampled,
    ),
    (
        "thumbnail-downscale",
        verify_the_thumbnail_readback_is_shrunk_on_the_gpu,
    ),
    ("hdr", verify_hdr_float_source_tone_maps_to_bgra8),
    (
        "hdr-pixel-values",
        verify_hdr_tone_map_returns_sdr_sources_to_their_original_srgb_values,
    ),
    ("normal-30-1080p", || {
        run_capture_automation_case(CaptureAutomationCase::new(30, true, false, 1920, 1080))
    }),
    ("borderless-60-1440p", || {
        run_capture_automation_case(CaptureAutomationCase::new(60, false, true, 2560, 1440))
    }),
    ("normal-60-4k", || {
        run_capture_automation_case(CaptureAutomationCase::new(60, true, false, 3840, 2160))
    }),
    (
        "resize",
        verify_capture_automation_resize_keeps_the_session_output_fixed,
    ),
    // Task176: these five used to be in-process `#[test]`s. A `CaptureSession`
    // crash in any of them took the whole test binary down with it
    // (`STATUS_ACCESS_VIOLATION`), erasing 350+ unrelated results. Bodies and
    // assertions are unchanged -- only where they run moved.
    (
        "minimize",
        minimizing_a_capture_target_keeps_the_session_recording,
    ),
    ("close", closing_a_capture_target_stops_and_finalizes_safely),
    (
        "zero-size-persistent",
        persistently_zero_sized_window_fails_safely_at_the_frame_pool_stage,
    ),
    (
        "zero-size-transient",
        transiently_zero_sized_window_recovers_once_it_reports_a_real_size,
    ),
    (
        "startup-stage-log",
        startup_stage_log_excludes_pid_hwnd_title_and_path,
    ),
    (
        "audio-telemetry",
        audio_telemetry_does_not_starve_finalized_segments_out_of_the_encoder_event_channel,
    ),
];

#[test]
#[ignore = "takes over the real desktop: creates, resizes and minimizes real windows. run explicitly with `cargo test capture_gpu_automation_parent -- --ignored`"]
fn capture_gpu_automation_parent_runs_every_case_in_a_fresh_process() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if std::env::var_os(CAPTURE_WORKER_CASE).is_some() {
        return;
    }
    for (case, _) in GPU_AUTOMATION_CASES {
        let output = std::process::Command::new(
            std::env::current_exe().expect("test executable path should be available"),
        )
        .arg("capture::tests::automation::capture_gpu_automation_worker")
        .arg("--exact")
        // The worker is `#[ignore]`d too, so without this the child would report
        // `0 passed; 1 ignored` and exit 0 -- the parent would call every case
        // green with no assertion having run at all.
        .arg("--include-ignored")
        .env(CAPTURE_WORKER_CASE, case)
        .output()
        .expect("GPU automation worker should start");
        assert!(
            output.status.success(),
            // A test panic lands on the worker's stdout, not stderr, so printing
            // only stderr threw away the actual failure reason.
            "GPU automation worker {case} failed ({}):\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "child-process half of `capture_gpu_automation_parent`, which spawns it with `--exact --include-ignored`. standalone it is a no-op unless LIVIA_CAPTURE_WORKER_CASE is set"]
fn capture_gpu_automation_worker() {
    let Ok(case) = std::env::var(CAPTURE_WORKER_CASE) else {
        return;
    };
    let Some((_, run)) = GPU_AUTOMATION_CASES.iter().find(|(name, _)| *name == case) else {
        panic!("unknown GPU automation case: {case}");
    };
    run();
}

fn run_capture_automation_case(case: CaptureAutomationCase) {
    let window = CaptureTestWindow::create(case.borderless);
    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: case.frame_rate,
            include_cursor: case.include_cursor,
            output_size: CaptureSize {
                width: case.output_width,
                height: case.output_height,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("WGC session should start for the visible automation window");
    std::thread::sleep(std::time::Duration::from_millis(750));
    let diagnostics = session.diagnostics();
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);
    assert!(matches!(
        events.try_recv(),
        Ok(CaptureStopReason::Requested | CaptureStopReason::SourceClosed)
    ));
    assert!(diagnostics.input_size.width > 0);
    assert!(diagnostics.input_size.height > 0);
    assert_eq!(diagnostics.output_size.width, case.output_width);
    assert_eq!(diagnostics.output_size.height, case.output_height);
    assert_eq!(diagnostics.cursor_included, case.include_cursor);
}

#[derive(Clone, Copy)]
struct CaptureAutomationCase {
    frame_rate: u8,
    include_cursor: bool,
    borderless: bool,
    output_width: i32,
    output_height: i32,
}

impl CaptureAutomationCase {
    const fn new(
        frame_rate: u8,
        include_cursor: bool,
        borderless: bool,
        output_width: i32,
        output_height: i32,
    ) -> Self {
        Self {
            frame_rate,
            include_cursor,
            borderless,
            output_width,
            output_height,
        }
    }
}

/// Task202: records a real WGC session against a window that repaints
/// continuously, then reports each segment's opening video sample durations.
/// Headless -- no app GUI -- so the 250-290ms hole every segment starts with
/// can be attributed from the `task202_boundary` records rather than guessed.
///
/// Run with `--ignored --nocapture` and nothing covering the test window at
/// (100,100)-(740,460); it needs the encoder and WGC to itself.
#[test]
#[ignore = "task202 real capture measurement: run alone with --nocapture"]
fn task202_measures_the_frames_recorded_across_a_segment_boundary() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // The `task202_boundary` records are the point of this run.
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let window = CaptureTestWindow::create(false);
    window.pin_topmost();
    let output_dir = test_encoder_output_dir();
    let (stopped, _events) = bounded(1);
    let (finalized_segments, _segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: 0,
            frame_rate: 60,
            include_cursor: false,
            // Matches the session the hole was first measured in, so the
            // readback and encode costs are the same order.
            output_size: CaptureSize {
                width: 1920,
                height: 1080,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("WGC session should start");

    // Alternates ~120Hz repainting with dead-still stretches. WGC delivers
    // nothing at all while the content holds, which is the case the task163
    // frame-hold watchdog exists to fill -- and the one a real target (a video
    // window between decoded frames, a paused game) spends much of its time in.
    // `LIVIA_TASK202_STILL_MS` sets the still stretch; 0 keeps it always moving.
    let still_ms: u64 = std::env::var("LIVIA_TASK202_STILL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(400);
    let painting_until = std::time::Instant::now() + std::time::Duration::from_secs(9);
    let mut tick = 0u32;
    while std::time::Instant::now() < painting_until {
        for _ in 0..60 {
            window.fill(tick);
            tick = tick.wrapping_add(1);
            std::thread::sleep(std::time::Duration::from_millis(8));
        }
        std::thread::sleep(std::time::Duration::from_millis(still_ms));
    }
    session.stop();

    let mut segments: Vec<_> = std::fs::read_dir(&output_dir)
        .expect("output dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
        .collect();
    segments.sort();
    println!("{} segment(s) in {}", segments.len(), output_dir.display());
    let mut worst = 0.0f64;
    let mut stall_count = 0usize;
    for path in &segments {
        let summary = crate::encoder::dump_segment_track_summary(path).expect("summary");
        let video = summary.iter().find(|track| !track.is_audio);
        let durations = crate::encoder::dump_video_sample_durations(path).unwrap_or_default();
        let timescale = video.map(|track| track.timescale).unwrap_or(60_000).max(1);
        let to_ms = |ticks: u32| f64::from(ticks) * 1000.0 / f64::from(timescale);
        let max = durations.iter().copied().max().unwrap_or(0);
        // The hole this task exists for: a quarter second with nothing
        // recorded. The watchdog's own interval is 2 frames, so anything past
        // ~10 frames is the loop failing to run it, not the content.
        let stalls: Vec<f64> = durations
            .iter()
            .copied()
            .filter(|ticks| to_ms(*ticks) >= 150.0)
            .map(to_ms)
            .collect();
        println!(
            "{}: {} video samples, {:.3}s of timeline, max sample {:.0}ms, \
             stalls >=150ms: {stalls:?}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            video.map(|track| track.sample_count).unwrap_or(0),
            video
                .map(|track| track.total_sample_duration_ticks as f64 / f64::from(timescale))
                .unwrap_or(0.0),
            to_ms(max),
        );
        worst = worst.max(to_ms(max));
        stall_count += stalls.len();
    }
    println!(
        "worst sample {worst:.0}ms across {} segment(s), {stall_count} stall(s) >=150ms",
        segments.len()
    );
    let _ = std::fs::remove_dir_all(&output_dir);
    assert!(
        segments.len() >= 3,
        "expected several segments from a 9s recording, got {}",
        segments.len()
    );
    assert_eq!(
        stall_count, 0,
        "the recording still contains quarter-second holes ({worst:.0}ms worst); the \
         frame-hold watchdog is not being run on its own deadline"
    );
}

/// Task480: a target that never changes, recorded *with* an audio track.
///
/// The mirror of task410. WGC hands over nothing while the content holds, so
/// the video track only advances by the task163 frame-hold watchdog, while the
/// audio track advances on wall time regardless -- and the fMP4 sink, which
/// interleaves the two, eventually refuses `push_aac_sample` with
/// `MF_E_NOTACCEPTING` and takes the whole capture down (observed at 145s in
/// the real app).
///
/// Note the `process_id`: every other automation test passes 0, which
/// `run_capture` reads as "no audio track at all" (worker.rs), so none of them
/// can reach this state. Pointing it at this test's own process gives a real
/// AAC track whose packets are the synthetic silence task410 added.
///
/// Run with `--ignored --nocapture` and nothing covering the test window at
/// (100,100)-(740,460).
#[test]
#[ignore = "task480 real capture measurement: runs for minutes, run alone with --nocapture"]
fn task480_a_still_target_keeps_recording_with_an_audio_track() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .with_test_writer()
        .try_init();
    let seconds: u64 = std::env::var("LIVIA_TASK480_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(200);
    // Production's own settings, not the 60 the other automation tests use:
    // `frameRate` is 120 there, which halves `hold_interval` to 16ms.
    let frame_rate: u8 = std::env::var("LIVIA_TASK480_FPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);
    // The recorded target was a console window, whose caret blinks at ~1Hz --
    // that is where production's "arrived at ~1s intervals" came from. 0 keeps
    // the window dead still.
    let repaint_ms: u64 = std::env::var("LIVIA_TASK480_REPAINT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1000);
    let window = CaptureTestWindow::create(false);
    window.pin_topmost();
    // One paint, then only the caret-rate repaints below (if any).
    window.fill(1);
    let output_dir = test_encoder_output_dir();
    let (stopped, events) = bounded(1);
    let (finalized_segments, segments) = crossbeam_channel::unbounded();
    let mut session = CaptureSession::start(
        CaptureConfig {
            window_handle: window.handle(),
            kind: crate::capture::targets::CaptureTargetKind::Window,
            process_id: std::process::id(),
            frame_rate,
            include_cursor: false,
            output_size: CaptureSize {
                width: 1920,
                height: 1080,
            },
            retention_minutes: ring_buffer::DEFAULT_RETENTION_MINUTES,
            encoder_output_dir: output_dir.clone(),
            container_path: None,
            capture_all_audio: false,
            codec: crate::settings::RecordingCodec::H264,
        },
        stopped,
        finalized_segments,
    )
    .expect("WGC session should start");

    let started = std::time::Instant::now();
    let mut died_after = None;
    let mut tick = 1u32;
    let mut last_repaint = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(seconds) {
        if let Ok(reason) = events.try_recv() {
            died_after = Some((started.elapsed(), reason));
            break;
        }
        if repaint_ms > 0 && last_repaint.elapsed() >= std::time::Duration::from_millis(repaint_ms)
        {
            last_repaint = std::time::Instant::now();
            tick = tick.wrapping_add(1);
            window.fill(tick);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let finalized = segments.len();
    session.stop();
    let _ = std::fs::remove_dir_all(&output_dir);

    println!(
        "still target held for {:.1}s, {finalized} finalized segment(s)",
        started.elapsed().as_secs_f64()
    );
    assert!(
        died_after.is_none(),
        "the capture died on a still target after {:?} -- {:?}",
        died_after.as_ref().map(|(elapsed, _)| *elapsed),
        died_after.as_ref().map(|(_, reason)| reason),
    );
    assert!(
        finalized > 0,
        "a still target recorded {seconds}s without finalizing a single segment"
    );
}

struct CaptureTestWindow(HWND);

impl CaptureTestWindow {
    fn create(borderless: bool) -> Self {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, ShowWindow, SW_SHOW, WS_OVERLAPPEDWINDOW, WS_POPUP, WS_VISIBLE,
        };

        let style = if borderless {
            WS_POPUP | WS_VISIBLE
        } else {
            WS_OVERLAPPEDWINDOW | WS_VISIBLE
        };

        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                windows::core::w!("STATIC"),
                windows::core::w!("Liveback Capture Test"),
                style,
                100,
                100,
                640,
                360,
                None,
                None,
                None,
                None,
            )
            .expect("visible test window should be created")
        };
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
        }
        Self(hwnd)
    }

    fn handle(&self) -> String {
        format!("0x{:X}", self.0 .0 as usize)
    }

    fn resize(&self, width: i32, height: i32) {
        use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOMOVE, SWP_NOZORDER};

        unsafe {
            SetWindowPos(self.0, None, 0, 0, width, height, SWP_NOMOVE | SWP_NOZORDER)
                .expect("test window resize should succeed");
        }
    }

    /// WGC starves an occluded target to almost nothing, which would make a
    /// frame-rate measurement meaningless.
    fn pin_topmost(&self) {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowPos, HWND_TOPMOST, SWP_NOMOVE, SWP_NOSIZE,
        };
        unsafe {
            let _ = SetWindowPos(
                self.0,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
        }
    }

    /// Repaints the whole client area in a colour that changes with `tick`, so
    /// WGC has a real change to deliver on every call.
    fn fill(&self, tick: u32) {
        use windows::Win32::Graphics::Gdi::{
            CreateSolidBrush, DeleteObject, FillRect, GetDC, ReleaseDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
        unsafe {
            let mut rect = Default::default();
            if GetClientRect(self.0, &mut rect).is_err() {
                return;
            }
            let dc = GetDC(Some(self.0));
            if dc.is_invalid() {
                return;
            }
            let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(
                (tick.wrapping_mul(2_654_435_761)) & 0x00FF_FFFF,
            ));
            FillRect(dc, &rect, brush);
            let _ = DeleteObject(brush.into());
            ReleaseDC(Some(self.0), dc);
        }
    }

    fn minimize(&self) {
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_MINIMIZE};

        unsafe {
            let _ = ShowWindow(self.0, SW_MINIMIZE);
        }
    }

    fn close(&self) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}

impl Drop for CaptureTestWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.0);
        }
    }
}
