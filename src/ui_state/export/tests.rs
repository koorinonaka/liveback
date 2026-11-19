use super::*;
use crate::export::failure::Failure;
use std::path::PathBuf;

fn status(state: ExportState, ratio: u32) -> ExportStatus {
    ExportStatus {
        job_id: "export-1".into(),
        state,
        progress: ExportProgress {
            job_id: "export-1".into(),
            part: 0,
            part_count: 1,
            processed_100ns: 0,
            total_100ns: 10_000_000,
            ratio_millionths: ratio,
        },
        paths: Vec::new(),
        error: None,
        passthrough_video_samples: 0,
        reencoded_video_frames: 0,
        passthrough_audio_samples: 0,
    }
}

#[test]
fn a_failure_falls_back_to_the_generic_body() {
    let running = status(ExportState::Running, 0);
    let mut failed = status(ExportState::Failed, 0);
    assert_eq!(
        completion_toast(Locale::Ja, Some(&running), &failed),
        Some((
            notify_failed(Locale::Ja),
            notify_failed_body(Locale::Ja).to_owned()
        ))
    );
    failed.error = Some(Failure::NotEnoughFreeSpace);
    assert_eq!(
        completion_toast(Locale::Ja, Some(&running), &failed),
        Some((
            notify_failed(Locale::Ja),
            "空き容量が不足しているため書き出しを開始できません".to_owned()
        ))
    );
    // `Kind::Failed`'s sentence is the title itself: the body must not repeat
    // it (t261101-e9ab).
    for locale in [Locale::Ja, Locale::En] {
        failed.error = Some(crate::export::failure::finalize_failed("denied"));
        let (title, body) = completion_toast(locale, Some(&running), &failed).unwrap();
        assert_ne!(title, body, "{locale:?}");
    }
}

/// Task169: the UI can miss the running state between two exports, so the
/// second one used to arrive as Completed-after-Completed and say nothing.
#[test]
fn a_second_export_is_announced_even_if_the_running_state_was_missed() {
    let first = status(ExportState::Completed, 1_000_000);
    let second = ExportStatus {
        job_id: "export-2".into(),
        ..status(ExportState::Completed, 1_000_000)
    };
    assert!(completion_toast(Locale::Ja, Some(&first), &second).is_some());
    assert!(outcome_toast(Locale::Ja, Some(&first), &second).is_some());
    // The same job seen twice is still the same news.
    assert_eq!(completion_toast(Locale::Ja, Some(&first), &first), None);
    assert!(outcome_toast(Locale::Ja, Some(&first), &first).is_none());
}

#[test]
fn the_corner_gets_the_outcome_and_the_reveal_button() {
    let running = status(ExportState::Running, 500_000);
    let mut done = status(ExportState::Completed, 1_000_000);
    done.paths = vec![PathBuf::from(r"D:\clips\a.mp4")];
    assert_eq!(
        outcome_toast(Locale::Ja, Some(&running), &done),
        Some(OutcomeToast {
            title: notify_completed(Locale::Ja),
            detail: r"D:\clips\a.mp4".to_owned(),
            variant: ToastVariant::Success,
            action: crate::ui_state::shell::control_reveal(Locale::Ja),
        })
    );
    // Nothing was written, so there is no folder to offer.
    let mut failed = status(ExportState::Failed, 0);
    failed.error = Some(Failure::NotEnoughFreeSpace);
    assert_eq!(
        outcome_toast(Locale::Ja, Some(&running), &failed),
        Some(OutcomeToast {
            title: notify_failed(Locale::Ja),
            detail: "空き容量が不足しているため書き出しを開始できません".to_owned(),
            variant: ToastVariant::Error,
            action: "",
        })
    );
    // Repeats are suppressed here exactly as they are for the OS
    // notification.
    assert_eq!(outcome_toast(Locale::Ja, Some(&done), &done), None);
}

/// The OS notification stays quiet about a cancel; the corner does not.
#[test]
fn a_cancel_reaches_the_corner_only() {
    let running = status(ExportState::Running, 0);
    let cancelled = status(ExportState::Cancelled, 0);
    assert_eq!(
        completion_toast(Locale::Ja, Some(&running), &cancelled),
        None
    );
    assert_eq!(
        outcome_toast(Locale::Ja, Some(&running), &cancelled),
        Some(OutcomeToast {
            title: export_cancelled(Locale::Ja),
            detail: String::new(),
            variant: ToastVariant::Info,
            action: "",
        })
    );
    assert_eq!(
        outcome_toast(Locale::Ja, Some(&cancelled), &cancelled),
        None
    );
}

#[test]
fn a_long_path_keeps_its_file_name() {
    let running = status(ExportState::Running, 0);
    let mut done = status(ExportState::Completed, 1_000_000);
    done.paths = vec![PathBuf::from(
        r"C:\Users\Example\Videos\Liveback\サンプルゲーム_20260816_123456.mp4",
    )];
    let detail = outcome_toast(Locale::Ja, Some(&running), &done)
        .expect("a finish")
        .detail;
    assert!(detail.starts_with('…'), "{detail}");
    assert!(
        detail.ends_with("サンプルゲーム_20260816_123456.mp4"),
        "{detail}"
    );
    assert_eq!(detail.chars().count(), DETAIL_CHARS + 1);
}

#[test]
fn nothing_started_is_idle() {
    assert!(!is_busy(None));
    assert_eq!(outcome(Locale::Ja, None), None);
}

#[test]
fn running_and_cancelling_both_hold_the_pipeline() {
    assert!(is_busy(Some(&status(ExportState::Running, 0))));
    assert!(is_busy(Some(&status(ExportState::Cancelling, 0))));
    // …and the three ways a job can end all let go of it. Task2130 hangs
    // the block-out cover on this flag, so a state missing from here would
    // leave the window covered with no way to cancel.
    assert!(!is_busy(Some(&status(ExportState::Completed, 1_000_000))));
    assert!(!is_busy(Some(&status(ExportState::Failed, 0))));
    assert!(!is_busy(Some(&status(ExportState::Cancelled, 0))));
}

#[test]
fn progress_is_rounded_percent() {
    assert_eq!(percent(&status(ExportState::Running, 0).progress), 0);
    assert_eq!(percent(&status(ExportState::Running, 424_999).progress), 42);
    assert_eq!(percent(&status(ExportState::Running, 425_000).progress), 43);
    assert_eq!(
        percent(&status(ExportState::Running, 1_000_000).progress),
        100
    );
    // A pipeline overshoot must not print 101%.
    assert_eq!(
        percent(&status(ExportState::Running, 1_400_000).progress),
        100
    );
}

#[test]
fn each_state_has_its_outcome() {
    // A running job has no outcome yet.
    assert_eq!(
        outcome(Locale::Ja, Some(&status(ExportState::Running, 500_000))),
        None
    );
    assert_eq!(
        outcome(Locale::Ja, Some(&status(ExportState::Cancelling, 500_000))),
        None
    );

    // Completion lists every written file.
    let mut done = status(ExportState::Completed, 1_000_000);
    done.paths = vec![
        PathBuf::from(r"D:\clips\a.mp4"),
        PathBuf::from(r"D:\clips\b.mp4"),
    ];
    assert_eq!(
        outcome(Locale::Ja, Some(&done)),
        Some(Outcome::Completed(vec![
            r"D:\clips\a.mp4".to_owned(),
            r"D:\clips\b.mp4".to_owned(),
        ]))
    );

    // Failure and cancellation carry their message.
    let mut failed = status(ExportState::Failed, 300_000);
    failed.error = Some(Failure::NotEnoughFreeSpace);
    assert_eq!(
        outcome(Locale::Ja, Some(&failed)),
        Some(Outcome::Failed(
            "空き容量が不足しているため書き出しを開始できません".into()
        ))
    );

    let mut cancelled = status(ExportState::Cancelled, 300_000);
    cancelled.error = Some(Failure::Cancelled);
    assert_eq!(
        outcome(Locale::Ja, Some(&cancelled)),
        Some(Outcome::Cancelled(export_cancelled(Locale::Ja).into()))
    );

    // Both fall back rather than rendering an empty line.
    let bare = status(ExportState::Cancelled, 0);
    assert_eq!(
        outcome(Locale::Ja, Some(&bare)),
        Some(Outcome::Cancelled(export_cancelled(Locale::Ja).into()))
    );
    let bare_failure = status(ExportState::Failed, 0);
    assert_eq!(
        outcome(Locale::Ja, Some(&bare_failure)),
        Some(Outcome::Failed(export_failed_fallback(Locale::Ja).into()))
    );
}

/// Drives a real export and cancels it. Manual because it needs a
/// populated buffer and writes into the real output directory -- but the
/// UI cannot verify this by eye: a passthrough export of a whole session
/// finishes in well under a second, faster than the window can be polled.
#[test]
#[ignore = "real export cancel (task129): set LIVIA_PLAYBACK_SESSION and run with \
                --ignored --nocapture"]
fn cancelling_a_real_export_returns_to_a_clean_state() {
    use crate::capture::CaptureController;
    use crate::events::NullSink;
    use crate::export::{ExportController, ExportRequest};
    use crate::ui_state::timeline::TimelineSnapshot;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    let session_id = std::env::var("LIVIA_PLAYBACK_SESSION")
        .expect("set LIVIA_PLAYBACK_SESSION to the session id to export");
    let capture = CaptureController::new();
    let manifest = capture.load_session(&session_id).expect("session loads");
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let exporter = ExportController::new(Arc::new(NullSink), capture);

    let job = exporter
        .start(ExportRequest {
            session_id: session_id.clone(),
            start_100ns: snapshot.start_100ns(),
            end_100ns: snapshot.live_edge_100ns - 1,
            game_name: "task129-cancel".to_owned(),
            output_file: None,
            output_directory: None,
            audio_mix: Default::default(),
        })
        .expect("export starts");
    println!("started {job}");
    // A second start while one owns the pipeline is refused -- the same
    // guarantee the UI leans on when it hides the start button.
    assert!(exporter
        .start(ExportRequest {
            session_id: session_id.clone(),
            start_100ns: snapshot.start_100ns(),
            end_100ns: snapshot.live_edge_100ns - 1,
            game_name: "task129-second".to_owned(),
            output_file: None,
            output_directory: None,
            audio_mix: Default::default(),
        })
        .is_err());
    assert!(is_busy(exporter.status().as_ref()));

    exporter.cancel().expect("cancel is accepted");
    let deadline = Instant::now() + Duration::from_secs(60);
    let final_state = loop {
        let status = exporter.status().expect("status exists");
        if !is_busy(Some(&status)) {
            break status;
        }
        assert!(Instant::now() < deadline, "export never settled");
        std::thread::sleep(Duration::from_millis(50));
    };
    println!(
        "settled as {:?}, outcome {:?}",
        final_state.state,
        outcome(Locale::Ja, Some(&final_state))
    );
    // Whichever it won, the UI is back to an idle screen with a message.
    assert!(outcome(Locale::Ja, Some(&final_state)).is_some());
    for path in &final_state.paths {
        println!("wrote {}", path.display());
    }
}

/// Task2970: a commit can still arrive with the field already closed --
/// an unload, or a callback in flight when the slot went behind the
/// block-out -- so this is the guard. (Until task3430 the blur itself was
/// such a path; it cancels now.)
#[test]
fn a_clip_commit_needs_an_open_field_a_range_and_an_idle_pipeline() {
    assert!(clip_commit_accepted(true, true, None));
    // The field was torn down by something else -- an unload, or the
    // export that just started covering the panel.
    assert!(!clip_commit_accepted(false, true, None));
    assert!(!clip_commit_accepted(true, false, None));
    assert!(!clip_commit_accepted(
        true,
        true,
        Some(&status(ExportState::Running, 0))
    ));
    assert!(!clip_commit_accepted(
        true,
        true,
        Some(&status(ExportState::Cancelling, 0))
    ));
    // A previous job that has ended does not hold the pipeline.
    assert!(clip_commit_accepted(
        true,
        true,
        Some(&status(ExportState::Completed, 1_000_000))
    ));
}

#[test]
fn only_the_clip_job_s_own_completion_writes_a_comment() {
    let pending = ("export-1".to_owned(), "テスト".to_owned());
    let mut done = status(ExportState::Completed, 1_000_000);
    done.paths = vec![PathBuf::from(r"D:\clips\a.mp4")];

    let finish = clip_finish(Some(&pending), &done).expect("the clip's own finish");
    assert_eq!(finish.comment, Some("テスト"));
    assert_eq!(finish.paths, &[PathBuf::from(r"D:\clips\a.mp4")]);

    // The 指定フォルダに書き出し the user started right after cancelling a
    // clip draft is a different job id, and gets nothing written into it.
    let other = ExportStatus {
        job_id: "export-2".into(),
        ..done.clone()
    };
    assert!(clip_finish(Some(&pending), &other).is_none());
    // Nothing pending, and states that are not a finish.
    assert!(clip_finish(None, &done).is_none());
    assert!(clip_finish(Some(&pending), &status(ExportState::Running, 0)).is_none());
    assert!(clip_finish(Some(&pending), &status(ExportState::Failed, 0)).is_none());
    assert!(clip_finish(Some(&pending), &status(ExportState::Cancelled, 0)).is_none());
}

/// Enter on an empty field still saves the clip -- it just writes no
/// metadata. And a gap-spanning selection writes the same comment into
/// every file it produced.
#[test]
fn an_empty_comment_still_finishes_and_every_part_gets_the_same_one() {
    let blank = ("export-1".to_owned(), String::new());
    let mut done = status(ExportState::Completed, 1_000_000);
    done.paths = vec![PathBuf::from(r"D:\clips\a.mp4")];
    let finish = clip_finish(Some(&blank), &done).expect("still a finish");
    assert_eq!(finish.comment, None);

    let pending = ("export-1".to_owned(), "同じ文言".to_owned());
    done.paths = vec![
        PathBuf::from(r"D:\clips\a_part01.mp4"),
        PathBuf::from(r"D:\clips\a_part02.mp4"),
    ];
    let finish = clip_finish(Some(&pending), &done).expect("a finish");
    assert_eq!(finish.paths.len(), 2);
    assert_eq!(finish.comment, Some("同じ文言"));
}

#[test]
fn a_saved_screenshot_names_the_file_and_offers_the_folder() {
    let toast = screenshot_toast(Locale::Ja, &Ok(PathBuf::from(r"D:\clips\shot.png")));
    assert_eq!(toast.title, screenshot_saved(Locale::Ja));
    // The file name only -- the folder is フォルダで表示's job.
    assert_eq!(toast.detail, "shot.png");
    assert_eq!(toast.variant, ToastVariant::Success);
    assert_eq!(
        toast.action,
        crate::ui_state::shell::control_reveal(Locale::Ja)
    );

    // A failed one is an error with nowhere to go.
    let toast = screenshot_toast(Locale::Ja, &Err(screenshot_error(Locale::Ja).to_owned()));
    assert_eq!(toast.title, screenshot_error(Locale::Ja));
    assert_eq!(toast.variant, ToastVariant::Error);
    assert_eq!(toast.action, "", "there is no folder to open");
}

/// t260928-bae5: the DS's words (言葉の表, ProgressBar 「中止」).
#[test]
fn the_clip_and_the_stop_use_the_ds_words() {
    assert_eq!(clip_saved(Locale::Ja), "クリップを保存しました");
    assert_eq!(clip_failed(Locale::Ja), "クリップを保存できませんでした");
    assert_eq!(export_cancelled(Locale::Ja), "書き出しを中止しました");
    assert_eq!(export_abort_failed(Locale::Ja), "中止できませんでした");
}

/// t260928-bae5 F18: the comment is cleared on completion and comes back
/// on a failure or a cancel -- only for the clip job itself.
#[test]
fn the_clip_job_s_end_decides_the_comment_field() {
    let pending = ("export-1".to_owned(), "boss fight".to_owned());
    let other = ("export-9".to_owned(), "boss fight".to_owned());
    let ended = |state| status(state, 0);
    assert_eq!(
        clip_comment_after(Some(&pending), &ended(ExportState::Running)),
        None
    );
    assert_eq!(
        clip_comment_after(Some(&pending), &ended(ExportState::Cancelling)),
        None
    );
    assert_eq!(
        clip_comment_after(Some(&pending), &ended(ExportState::Completed)),
        Some("")
    );
    assert_eq!(
        clip_comment_after(Some(&pending), &ended(ExportState::Failed)),
        Some("boss fight")
    );
    assert_eq!(
        clip_comment_after(Some(&pending), &ended(ExportState::Cancelled)),
        Some("boss fight")
    );
    assert_eq!(
        clip_comment_after(Some(&other), &ended(ExportState::Failed)),
        None
    );
    assert_eq!(clip_comment_after(None, &ended(ExportState::Failed)), None);
    assert!(is_clip_failure(Some(&pending), &ended(ExportState::Failed)));
    assert!(!is_clip_failure(
        Some(&pending),
        &ended(ExportState::Cancelled)
    ));
    assert!(!is_clip_failure(Some(&other), &ended(ExportState::Failed)));
    assert!(!is_clip_failure(None, &ended(ExportState::Failed)));
}
