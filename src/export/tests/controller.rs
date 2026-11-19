//! Export controller status/notification and codec-compat tests.
use super::*;
use crate::export::failure::last_detail;
use crate::ui_state::export_failure::text;
use crate::ui_state::locale::Locale;

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

/// t261009-5ebd: `run` gets its own job's outcome -- each path with the span
/// it holds -- and a job it did not start never reaches it.
#[test]
fn a_waiter_gets_its_own_jobs_parts_and_no_one_elses() {
    let (_sink, controller) = recording_controller();
    let (sender, outcome) = mpsc::channel();
    {
        let mut state = controller.state.lock().unwrap();
        state.status = Some(in_progress_status("job-7"));
        state.waiter = Some(("job-7".into(), sender));
    }
    // Another job finishing (its status no longer current) leaves the waiter
    // where it is.
    controller.finish_job(
        "job-6",
        Ok((
            vec![PathBuf::from(r"D:\x.mp4")],
            ExportDiagnostics::default(),
        )),
    );
    assert!(outcome.try_recv().is_err());
    assert!(controller.state.lock().unwrap().waiter.is_some());

    let diagnostics = ExportDiagnostics {
        part_spans_100ns: vec![(10, 20), (30, 40)],
        ..ExportDiagnostics::default()
    };
    controller.finish_job(
        "job-7",
        Ok((
            vec![
                PathBuf::from(r"D:\a_part01.mp4"),
                PathBuf::from(r"D:\a_part02.mp4"),
            ],
            diagnostics,
        )),
    );
    assert_eq!(
        outcome.try_recv().unwrap(),
        Ok(vec![
            ExportedPart {
                path: PathBuf::from(r"D:\a_part01.mp4"),
                start_100ns: 10,
                end_100ns: 20,
            },
            ExportedPart {
                path: PathBuf::from(r"D:\a_part02.mp4"),
                start_100ns: 30,
                end_100ns: 40,
            },
        ])
    );
    assert!(controller.state.lock().unwrap().waiter.is_none());

    // A failure carries its message.
    let (sender, outcome) = mpsc::channel();
    {
        let mut state = controller.state.lock().unwrap();
        state.status = Some(in_progress_status("job-8"));
        state.waiter = Some(("job-8".into(), sender));
    }
    controller.finish_job(
        "job-8",
        Err(ExportFailure::new(Failure::NotEnoughFreeSpace)),
    );
    assert_eq!(
        outcome.try_recv().unwrap(),
        Err(Failure::NotEnoughFreeSpace)
    );
}

/// `run` is refused at once while another job runs, like `start`, rather
/// than waiting behind it.
#[test]
fn run_is_refused_while_a_job_is_running() {
    let (_sink, controller) = recording_controller();
    controller.state.lock().unwrap().status = Some(in_progress_status("job-9"));
    let refused = controller.run(ExportRequest {
        session_id: "capture-x".into(),
        start_100ns: 0,
        end_100ns: 10,
        game_name: "g".into(),
        output_file: None,
        output_directory: Some(PathBuf::from(r"C:\out")),
        audio_mix: Default::default(),
    });
    assert_eq!(refused, Err(Failure::AlreadyRunning));
    assert!(controller.state.lock().unwrap().waiter.is_none());
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
            output_directory: None,
            audio_mix: Default::default(),
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
        for locale in [Locale::Ja, Locale::En] {
            let shown = text(&failure.failure, locale);
            assert!(!shown.is_empty(), "{kind:?}");
            assert!(!shown.contains("MFStartup"), "{kind:?}: {shown}");
            assert!(!shown.contains("0x80004005"), "{kind:?}: {shown}");
        }
        assert!(
            last_detail().contains(diagnostics),
            "{kind:?}: {}",
            last_detail()
        );
    }
}

fn request_into(session_id: &str, output_file: PathBuf) -> ExportRequest {
    ExportRequest {
        session_id: session_id.into(),
        start_100ns: 0,
        end_100ns: 20_000_000,
        game_name: "g".into(),
        output_file: Some(output_file),
        output_directory: None,
        audio_mix: Default::default(),
    }
}

/// t261101-e9ab: a session pruned or discarded while the review screen still
/// showed it used to fail with the catalog's own `no such session` on screen.
#[test]
fn a_missing_session_fails_in_the_users_words() {
    let (_sink, controller) = recording_controller();
    let out = crate::test_support::TmpDir::new("livia-export-missing-session").create();
    let failed = controller
        .run(request_into("gone", out.join("a.mp4")))
        .unwrap_err();
    assert_eq!(failed, Failure::NoRecording);
    for locale in [Locale::Ja, Locale::En] {
        let shown = text(&failed, locale);
        assert!(!shown.contains("session"), "{shown}");
    }
}

/// t261101-e9ab: the destination is refused before a single segment is
/// staged. The session's container is gone from disk, so staging cannot read
/// it: a job that stages first fails on the read, and only one that checks
/// the destination first says the destination is the problem.
#[test]
fn a_bad_destination_fails_before_staging() {
    let (_sink, controller) = recording_controller();
    let buffer = crate::test_support::TmpDir::new("livia-export-bad-destination").create();
    let ring = crate::capture::tests::ring_with_segment(&buffer, "s", 0, b"bytes");
    controller.capture.insert_session("s".into(), ring);
    std::fs::remove_dir_all(&*buffer).unwrap();
    let failed = controller
        .run(request_into("s", buffer.join("missing").join("a.mp4")))
        .unwrap_err();
    assert_eq!(failed, Failure::OutputParentMissing);
}

/// t261101-e9ab: segment indexes restart at 0 in every session, so a staging
/// folder named by the pid alone can hand one job another job's leftovers.
#[test]
fn each_job_stages_in_its_own_folder() {
    assert_ne!(staging_directory("export-1"), staging_directory("export-2"));
}
