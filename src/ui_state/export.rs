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
    let comment = pending_for(pending, status)?;
    if status.state != ExportState::Completed {
        return None;
    }
    Some(ClipFinish {
        paths: &status.paths,
        comment: Some(comment).filter(|comment| !comment.is_empty()),
    })
}

/// The pending clip's comment, when `status` is that clip's job.
fn pending_for<'a>(
    pending: Option<&'a (String, String)>,
    status: &ExportStatus,
) -> Option<&'a str> {
    pending
        .filter(|(job_id, _)| job_id == &status.job_id)
        .map(|(_, comment)| comment.as_str())
}

/// What the end of the clip job does to the comment field (t260928-bae5 F18):
/// `Some("")` once it completed -- the comment went into the file -- and the
/// comment itself back after a failure or a cancel, so nothing typed is lost.
/// `None` while it runs, and for any job that is not the clip's.
pub fn clip_comment_after<'a>(
    pending: Option<&'a (String, String)>,
    status: &ExportStatus,
) -> Option<&'a str> {
    let comment = pending_for(pending, status)?;
    match status.state {
        ExportState::Completed => Some(""),
        ExportState::Failed | ExportState::Cancelled => Some(comment),
        ExportState::Running | ExportState::Cancelling => None,
    }
}

/// Whether `status` is the clip job failing, which the corner titles
/// 「クリップを保存できませんでした」 rather than the export's words
/// (t260928-bae5 F5).
pub fn is_clip_failure(pending: Option<&(String, String)>, status: &ExportStatus) -> bool {
    status.state == ExportState::Failed && pending_for(pending, status).is_some()
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
            action: crate::ui_state::shell::control_reveal(locale),
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
    if same_job(previous, next) {
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

/// The same job polled again in the same state: not news.
fn same_job(previous: Option<&ExportStatus>, next: &ExportStatus) -> bool {
    previous.is_some_and(|previous| previous.state == next.state && previous.job_id == next.job_id)
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
        if same_job(previous, next) {
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
        action: if completed {
            crate::ui_state::shell::control_reveal(locale)
        } else {
            ""
        },
    })
}

#[cfg(test)]
mod tests;
