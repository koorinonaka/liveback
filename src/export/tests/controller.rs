//! Export controller status/notification and codec-compat tests.
use super::*;

fn recording_controller() -> (Arc<RecordingSink>, ExportController) {
    let sink = Arc::new(RecordingSink::default());
    let controller = ExportController::new(sink.clone(), capture::CaptureController::new());
    (sink, controller)
}

fn in_progress_status(job_id: &str) -> ExportStatus {
    ExportStatus {
        job_id: job_id.into(),
        state: ExportState::Running,
        progress: ExportProgress {
            job_id: job_id.into(),
            part: 1,
            part_count: 2,
            processed_100ns: 0,
            total_100ns: 20_000_000,
            ratio_millionths: 0,
        },
        paths: Vec::new(),
        error: None,
        passthrough_video_samples: 0,
        reencoded_video_frames: 0,
        passthrough_audio_samples: 0,
    }
}

#[test]
fn progress_reaches_the_event_sink_and_the_stored_status() {
    let (sink, controller) = recording_controller();
    controller.state.lock().unwrap().status = Some(in_progress_status("job-1"));
    let progress = ExportProgress {
        job_id: "job-1".into(),
        part: 2,
        part_count: 2,
        processed_100ns: 10_000_000,
        total_100ns: 20_000_000,
        ratio_millionths: 500_000,
    };

    controller.update_progress(progress.clone());

    assert_eq!(
        sink.progress.lock().unwrap().as_slice(),
        std::slice::from_ref(&progress)
    );
    assert_eq!(controller.status().unwrap().progress, progress);
}

/// The pipeline's own OS notification went with clip saving (task1090): every
/// job now runs from a window that is looking at it, and the corner toast is
/// what reports the outcome.
#[test]
fn finishing_a_job_reports_the_final_status_without_notifying() {
    let (sink, controller) = recording_controller();
    controller.state.lock().unwrap().status = Some(in_progress_status("job-2"));

    controller.finish_job(
        "job-2",
        Ok((
            vec![PathBuf::from(r"D:\clips\clip.mp4")],
            ExportDiagnostics::default(),
        )),
    );

    let statuses = sink.statuses.lock().unwrap();
    assert_eq!(statuses.len(), 1);
    assert_eq!(statuses[0].state, ExportState::Completed);
    // A finished job reports 100%, not whatever the last tick happened to be.
    assert_eq!(statuses[0].progress.ratio_millionths, 1_000_000);
}

fn sample_video_fingerprint(frame_rate: u8) -> VideoFingerprint {
    VideoFingerprint {
        sequence_header: vec![0, 0, 1, 103, 66, 192, 42],
        codec: crate::encoder::VideoCodec::H264 { profile: 66 },
        level: 42,
        frame_size: 8246337209352,
        frame_rate,
    }
}

#[test]
fn codec_compatible_ignores_frame_rate_only_differences() {
    // Task086: real WGC recordings observed the very same encode
    // (identical sequence_header/profile/level/frame_size) classified as
    // frame_rate=30 for one segment and frame_rate=60 for the next, purely
    // from classify_frame_rate's noisy per-segment timing observation.
    let baseline = sample_video_fingerprint(30);
    let other = sample_video_fingerprint(60);
    assert_ne!(baseline, other, "sanity: frame_rate does differ");
    assert!(
        baseline.codec_compatible(&other),
        "a frame_rate-only difference must not be treated as an incompatible encode"
    );

    // The controls: a real SPS/PPS change is still incompatible.
    let baseline = sample_video_fingerprint(60);
    let mut other = sample_video_fingerprint(60);
    other.sequence_header = vec![0, 0, 1, 103, 66, 192, 99];
    assert!(
        !baseline.codec_compatible(&other),
        "a real SPS/PPS change must still be treated as incompatible"
    );
    // And the scenario Task086's own task file flags: a recording target
    // that's genuinely resized mid-session must still be caught, not
    // silently spliced together.
    let mut other = sample_video_fingerprint(60);
    other.frame_size = 1234567890;
    assert!(
        !baseline.codec_compatible(&other),
        "a real frame_size change must still be treated as incompatible"
    );
}

/// The bug behind "the export button does nothing" (task1050). The UI folds
/// `export_progress` into the status it is already holding, and nothing seeded
/// that status until `finish_job` -- so a running export published no state at
/// all and 書き出し中 never appeared.
#[test]
fn starting_a_job_announces_it_before_the_worker_reports_anything() {
    let (sink, controller) = recording_controller();

    let job = controller
        .start(ExportRequest {
            session_id: "no-such-session".into(),
            start_100ns: 0,
            end_100ns: 10_000_000,
            game_name: "game".into(),
            output_file: None,
        })
        .expect("the worker spawns");

    let statuses = sink.statuses.lock().unwrap();
    assert_eq!(statuses[0].job_id, job);
    assert_eq!(statuses[0].state, ExportState::Running);
    assert_eq!(statuses[0].progress.ratio_millionths, 0);
}

/// t260927-11da: an `EncoderStartError` reached the screen as its raw
/// `diagnostics` (the Windows API text). Every kind now shows a plain sentence
/// and logs the text; the detail carrying it is the control that the absence
/// check on the message would have seen it.
#[test]
fn an_encoder_start_error_keeps_its_diagnostics_off_the_screen() {
    use crate::encoder::{EncoderStartError, EncoderStartErrorKind as Kind};
    let diagnostics = "MFStartup failed: 0x80004005 Unspecified error";
    for kind in [
        Kind::NoHardwareEncoder,
        Kind::UnsupportedFormat,
        Kind::Device,
        Kind::Io,
    ] {
        let failure = ExportFailure::from(EncoderStartError {
            kind: kind.clone(),
            diagnostics: diagnostics.into(),
        });
        assert!(!failure.message.is_empty(), "{kind:?}");
        assert!(
            !failure.message.contains("MFStartup"),
            "{kind:?}: {}",
            failure.message
        );
        assert!(
            !failure.message.contains("0x80004005"),
            "{kind:?}: {}",
            failure.message
        );
        assert!(
            msg::last_detail().contains(diagnostics),
            "{kind:?}: {}",
            msg::last_detail()
        );
    }
}
