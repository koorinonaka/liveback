//! Export/screenshot state for the slint review screen (task129). The pipeline
//! itself (`crate::export`) is untouched: this only folds the `ExportStatus` it
//! publishes into the handful of strings and flags the screen renders, so the
//! five states the React version distinguished (待機 / 実行中 / 完了 / 失敗 /
//! キャンセル) stay testable without running a job.

use crate::export::{ExportProgress, ExportState, ExportStatus};
use crate::ui_state::locale::Locale;
use crate::ui_state::toast::{keep_the_tail, ToastVariant};

crate::tr! {
    export_running { ja: "書き出し中", en: "Exporting" }
    /// The pipeline sends this as `error` on a cancel, but the UI must not be
    /// left blank if it ever arrives without one.
    /// 「中止」, the word on the button that asked for it (DS ProgressBar,
    /// t260928-bae5 F5).
    export_cancelled { ja: "書き出しを中止しました", en: "Export stopped" }
    /// The 「中止」 itself was refused (t260928-bae5 F5); the reason is the detail.
    export_abort_failed { ja: "中止できませんでした", en: "The export could not be stopped" }
    reveal { ja: "フォルダで表示", en: "Show in folder" }
    reveal_error { ja: "フォルダを開けませんでした", en: "The folder could not be opened" }
    screenshot_error {
        ja: "静止画を保存できませんでした",
        en: "The still could not be saved"
    }
    screenshot_saved { ja: "静止画を保存しました", en: "Still saved" }
    notify_completed { ja: "書き出しが完了しました", en: "Export finished" }
    notify_failed { ja: "書き出しに失敗しました", en: "Export failed" }
    notify_failed_body {
        ja: "書き出し中にエラーが発生しました。",
        en: "Something went wrong during the export."
    }
    export_failed_fallback { ja: "書き出しに失敗しました", en: "The export failed" }
    // ---------- clips (task2970) ----------
    /// DS 言葉の表 「クリップを保存しました」 (t260928-bae5 F3).
    clip_saved { ja: "クリップを保存しました", en: "Clip saved" }
    /// A clip that could not be started or did not finish (t260928-bae5 F5);
    /// the reason goes in the detail.
    clip_failed { ja: "クリップを保存できませんでした", en: "The clip could not be saved" }
    /// The video is already on disk when this fires, so it is a warning rather
    /// than a failure: only the comment was lost.
    clip_comment_failed {
        ja: "クリップは保存しましたが、コメントを書けませんでした",
        en: "The clip was saved, but its comment could not be written"
    }
}

/// Whether the comment typed into the clip field may be turned into a job
/// (task2970).
///
/// `editing` is the load-bearing half. Task3430 made the blur a cancel rather
/// than a commit, which removes the loudest way a stale commit used to arrive
/// -- but not the guard's job: the moment an export starts (the whole slot
/// goes behind the block-out) or the session is unloaded, whatever commit is
/// already in flight must not start a second job, or a job against a session
/// that is no longer on screen.
pub fn clip_commit_accepted(editing: bool, has_range: bool, status: Option<&ExportStatus>) -> bool {
    editing && has_range && !is_busy(status)
}

/// What a finished job owes the clip that started it (task2970).
pub struct ClipFinish<'a> {
    /// Every file the job wrote. A selection spanning a gap comes out as
    /// several, and they all get the same comment -- they are one clip.
    pub paths: &'a [std::path::PathBuf],
    /// `None` when the user pressed Enter on an empty field, which is a
    /// deliberate "save it, no comment".
    pub comment: Option<&'a str>,
}

/// `Some` only when `status` is *the* clip job's completion. Matching on the
/// job id rather than on "a clip was asked for recently" is what stops a
/// cancelled draft from being written into the next 指定フォルダに書き出し.
///
/// The caller still clears `pending` on every terminal state; this only decides
/// what to write.
pub fn clip_finish<'a>(
    pending: Option<&'a (String, String)>,
    status: &'a ExportStatus,
) -> Option<ClipFinish<'a>> {
    let (job_id, comment) = pending?;
    if job_id != &status.job_id || status.state != ExportState::Completed {
        return None;
    }
    Some(ClipFinish {
        paths: &status.paths,
        comment: Some(comment.as_str()).filter(|comment| !comment.is_empty()),
    })
}

/// What the end of the clip job does to the comment field (t260928-bae5 F18):
/// `Some("")` once it completed -- the comment went into the file -- and the
/// comment itself back after a failure or a cancel, so nothing typed is lost.
/// `None` while it runs, and for any job that is not the clip's.
pub fn clip_comment_after<'a>(
    pending: Option<&'a (String, String)>,
    status: &ExportStatus,
) -> Option<&'a str> {
    let (job_id, comment) = pending?;
    if job_id != &status.job_id {
        return None;
    }
    match status.state {
        ExportState::Completed => Some(""),
        ExportState::Failed | ExportState::Cancelled => Some(comment.as_str()),
        ExportState::Running | ExportState::Cancelling => None,
    }
}

/// Whether `status` is the clip job failing, which the corner titles
/// 「クリップを保存できませんでした」 rather than the export's words
/// (t260928-bae5 F5).
pub fn is_clip_failure(pending: Option<&(String, String)>, status: &ExportStatus) -> bool {
    status.state == ExportState::Failed
        && pending.is_some_and(|(job_id, _)| job_id == &status.job_id)
}

/// `running` and `cancelling` both mean "a job owns the pipeline": the button
/// stays a cancel button and a second start must not be offered (the controller
/// would reject it anyway).
///
/// Task2130 made this the whole condition for the block-out cover as well, so
/// the three terminal states below are the only thing that lifts it. Anything
/// that can end a job without landing in one of them leaves the screen dead
/// with the cancel button under a scrim -- which is why the test asserts all
/// five states rather than just the two that answer `true`.
pub fn is_busy(status: Option<&ExportStatus>) -> bool {
    matches!(
        status.map(|status| status.state),
        Some(ExportState::Running) | Some(ExportState::Cancelling)
    )
}

/// Millionths -> whole percent, matching React's
/// `Math.round(ratioMillionths / 10_000)`.
pub fn percent(progress: &ExportProgress) -> u32 {
    ((f64::from(progress.ratio_millionths) / 10_000.0).round() as i64).clamp(0, 100) as u32
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Finished; every written file, so each can get its own reveal button.
    Completed(Vec<String>),
    Failed(String),
    Cancelled(String),
}

/// What to show once the job is no longer running. `None` while it still is,
/// or before anything has been started.
pub fn outcome(locale: Locale, status: Option<&ExportStatus>) -> Option<Outcome> {
    let status = status?;
    match status.state {
        ExportState::Running | ExportState::Cancelling => None,
        ExportState::Completed => Some(Outcome::Completed(
            status
                .paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
        )),
        ExportState::Failed => {
            Some(Outcome::Failed(status.error.clone().unwrap_or_else(|| {
                export_failed_fallback(locale).to_owned()
            })))
        }
        ExportState::Cancelled => Some(Outcome::Cancelled(
            status
                .error
                .clone()
                .unwrap_or_else(|| export_cancelled(locale).to_owned()),
        )),
    }
}

/// What the corner says about a screenshot (task1040).
pub struct ScreenshotToast {
    pub title: String,
    pub detail: String,
    pub variant: ToastVariant,
    pub action: &'static str,
}

/// A screenshot's outcome as a corner toast. It used to be written to the
/// notice card over the video, which has no expiry at all -- every shot left a
/// line there until the session was reloaded.
///
/// Only the file name goes in the detail: the folder is the same one every
/// time, and フォルダで表示 is what the path is actually for.
pub fn screenshot_toast(
    locale: Locale,
    saved: &Result<std::path::PathBuf, String>,
) -> ScreenshotToast {
    match saved {
        Ok(path) => ScreenshotToast {
            title: screenshot_saved(locale).to_owned(),
            detail: path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            variant: ToastVariant::Success,
            action: reveal(locale),
        },
        // No フォルダで表示: there is nothing at the other end of it. An error
        // also stays up until something replaces it, which is what a failed
        // save deserves over a six-second fade.
        Err(error) => ScreenshotToast {
            title: error.clone(),
            detail: String::new(),
            variant: ToastVariant::Error,
            action: "",
        },
    }
}

/// The OS toast a finished job earns, or `None` for no toast (task130). Ported
/// from the `notifyIfHidden` call in `useExportStatus.ts`, including its
/// suppression of a status that did not change state. The clip-job suppression
/// went with clip saving (task1090).
///
/// Whether the window is focused is the caller's business, as it was React's.
pub fn completion_toast(
    locale: Locale,
    previous: Option<&ExportStatus>,
    next: &ExportStatus,
) -> Option<(&'static str, String)> {
    // The same job polled again is not news; a *second* job that ends the same
    // way is. Comparing the state alone made every export after the first one
    // silent, because the UI can coalesce the running state away and see
    // Completed follow Completed (task169, found while verifying the corner
    // toast).
    if previous
        .is_some_and(|previous| previous.state == next.state && previous.job_id == next.job_id)
    {
        return None;
    }
    match next.state {
        ExportState::Completed => Some((
            notify_completed(locale),
            next.paths
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", "),
        )),
        ExportState::Failed => Some((
            notify_failed(locale),
            next.error
                .clone()
                .unwrap_or_else(|| notify_failed_body(locale).to_owned()),
        )),
        _ => None,
    }
}

/// What the corner toast says about a finished job (task169). The screen used
/// to keep a card up for this, with no expiry; the toast is now the only place
/// the outcome is reported, so it has to carry the reveal button too.
#[derive(Debug, PartialEq, Eq)]
pub struct OutcomeToast {
    pub title: &'static str,
    pub detail: String,
    pub variant: ToastVariant,
    /// Empty when there is no written file to open.
    pub action: &'static str,
}

/// The toast elides its detail from the right, which is the end of a path --
/// the half that names the file. Anything longer than this keeps its tail.
/// The detail is one mono line (t260928-5f3b): IBM Plex Mono advances 0.6em,
/// so 11.5px is 6.9px a character and the Toast's 286px text column holds
/// 41. The kept tail and its "…" stay under that; 44 had the tail cut.
const DETAIL_CHARS: usize = 39;

/// [`completion_toast`] widened for the in-app corner (task169): same two
/// suppressions, plus the cancel the OS notification deliberately stays quiet
/// about. A cancel is still worth a line on screen -- the user asked for it,
/// but nothing was written, and the export panel no longer says so.
pub fn outcome_toast(
    locale: Locale,
    previous: Option<&ExportStatus>,
    next: &ExportStatus,
) -> Option<OutcomeToast> {
    if next.state == ExportState::Cancelled {
        if previous
            .is_some_and(|previous| previous.state == next.state && previous.job_id == next.job_id)
        {
            return None;
        }
        return Some(OutcomeToast {
            title: export_cancelled(locale),
            detail: String::new(),
            variant: ToastVariant::Info,
            action: "",
        });
    }
    let (title, detail) = completion_toast(locale, previous, next)?;
    let completed = next.state == ExportState::Completed;
    Some(OutcomeToast {
        title,
        detail: if completed {
            keep_the_tail(&detail, DETAIL_CHARS)
        } else {
            detail
        },
        variant: if completed {
            ToastVariant::Success
        } else {
            ToastVariant::Error
        },
        action: if completed { reveal(locale) } else { "" },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
        failed.error = Some("容量が足りません".into());
        assert_eq!(
            completion_toast(Locale::Ja, Some(&running), &failed),
            Some((notify_failed(Locale::Ja), "容量が足りません".to_owned()))
        );
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
                action: reveal(Locale::Ja),
            })
        );
        // Nothing was written, so there is no folder to offer.
        let mut failed = status(ExportState::Failed, 0);
        failed.error = Some("容量が足りません".into());
        assert_eq!(
            outcome_toast(Locale::Ja, Some(&running), &failed),
            Some(OutcomeToast {
                title: notify_failed(Locale::Ja),
                detail: "容量が足りません".to_owned(),
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
        failed.error = Some("書き出し先の空き容量が足りません".into());
        assert_eq!(
            outcome(Locale::Ja, Some(&failed)),
            Some(Outcome::Failed("書き出し先の空き容量が足りません".into()))
        );

        let mut cancelled = status(ExportState::Cancelled, 300_000);
        cancelled.error = Some(export_cancelled(Locale::Ja).into());
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
        assert_eq!(toast.action, reveal(Locale::Ja));

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
}
