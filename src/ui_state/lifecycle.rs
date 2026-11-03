//! The auto-stop notification (task141): React's `notifiedStopCodeRef` block in
//! `useAppState.ts`, which watched the 1s lifecycle poll and toasted once per
//! anomalous stop. A recording can end on its own -- the target window closed,
//! it was minimized, the GPU path failed -- and all three happen exactly when
//! the app window is hidden, so the toast is the only signal the user gets.

use crate::capture::{
    CaptureLifecycleStatus, AUDIO_SESSION_STAGE, MID_RECORDING_STAGE, VIDEO_ENCODER_STAGE,
};
use crate::ui_state::locale::Locale;
use crate::ui_state::timeline::{format_duration, HNS_PER_SECOND};

crate::tr! {
    /// The two tooltip arms that name no target. They used to be
    /// `TRAY_TOOLTIP_IDLE` / `_RECORDING` consts in `lib.rs`, which is why the
    /// tray kept speaking Japanese under `Locale::En` while the other two arms
    /// of `tray_tooltip` were translated.
    tray_tooltip_idle { ja: "Liveback", en: "Liveback" }
    tray_tooltip_recording { ja: "Liveback — 録画中", en: "Liveback — Recording" }
    /// What a stop that is not a failure says, one per `CaptureStopReason`
    /// (`stop_message` below). They were plain Japanese literals in
    /// `capture/types.rs` once, which is what an `En` user saw.
    stop_source_closed {
        ja: "対象ウィンドウが閉じたため録画を安全に停止しました。対象を選択して再開してください。",
        en: "The target window closed, so the recording stopped safely. Pick a target to start again."
    }
    stop_initialization_failed {
        ja: "GPU か録画の処理でエラーが起きたため、録画を安全に停止しました。対象を選択して再開してください。",
        en: "A GPU or recording error stopped the recording safely. Pick a target to start again."
    }
    stop_requested { ja: "録画を安全に停止しました。", en: "The recording stopped safely." }
    stop_disk_full {
        ja: "保存先の空き容量が少なくなったため録画を安全に停止しました。容量を空けてから対象を選び直してください。",
        en: "The destination is running out of room, so the recording stopped safely. Free some space and pick a target again."
    }
    auto_stop_title_text { ja: "録画が停止しました", en: "Recording stopped" }
    /// Task193: the same failure used to be silent on screen -- the row's
    /// `● 録画中` just went away and the reason lived only in the log. The
    /// title stayed neutral because `start_capture` was reported both for a
    /// startup failure and for a capture that died mid-recording
    /// (`startup_stage` is not updated inside the loop), so "起動時" would have
    /// been wrong half the time. Since t260911-77c3 the two *are* told apart by
    /// the stage (`MID_RECORDING_STAGE`), so the title could now follow -- that
    /// is deliberately not done here and is its own task, because the wording of
    /// both titles is a user decision, not a mechanical consequence.
    capture_failed_title { ja: "録画に失敗しました", en: "Recording failed" }

    failure_encoder {
        ja: "映像エンコーダを準備できませんでした。設定の fps や解像度を見直してください。",
        en: "The video encoder could not start. Check the frame rate and resolution in settings."
    }
    failure_target {
        ja: "録画する対象を取得できませんでした。対象を選び直してください。",
        en: "The recording target could not be reached. Pick a target again."
    }
    failure_interrupted {
        ja: "録画を続けられなくなりました。対象を選び直して再開してください。",
        en: "The recording could not continue. Pick a target again to restart."
    }
    failure_gpu {
        ja: "GPU の録画の処理を準備できませんでした。詳しい原因はログに記録しました。",
        en: "The GPU recording path could not start. The details are in the log."
    }
    /// Task2150. Short on purpose: the toast body is one elided line, so the
    /// cause has to come first and the sentence has to end before it is cut.
    /// Says nothing about picking a target again -- the target never broke.
    failure_audio_session {
        ja: "音声デバイスが停止しました。接続を確認してください。",
        en: "The audio device stopped. Check the audio connection."
    }
    /// t260911-77c3. The recording was running and died; the capture target was
    /// never the problem, so this must not say 「対象を選び直して」 the way
    /// `failure_interrupted` does. Deliberately names no cause -- the user's
    /// decision on 2026-09-11 was that the internal term (a sink refusing a
    /// write) tells them nothing they can act on, and the one action that
    /// helps is to start again. Same one-line elision budget as the audio
    /// wording above.
    failure_mid_recording {
        ja: "録画を続けられませんでした。もう一度開始してください。",
        en: "The recording could not continue. Start it again."
    }

    /// Task2050, moved from `capture.rs`. It refuses a start, so since
    /// t260928-e309 (F12 / F14, DS 言葉) it says 「開始できません」 rather than
    /// the 「安全停止しました」 task2010 had pinned, with 「10 GB」 spaced.
    disk_floor_refusal {
        ja: "空き容量が 10 GB 以下のため録画を開始できません。",
        en: "Less than 10 GB is free, so recording cannot start."
    }

    /// Task2140. The audio trial failed, so this recording will have no sound --
    /// but it is still recording video (task198 / task1430), so nothing failed
    /// and the title must not say it did. `Warning`, like the retention sweep:
    /// not a result the user asked for, and it keeps until they have read it.
    audio_unavailable_title { ja: "音声なしで録画中", en: "Recording without audio" }
    /// The fallback for a capture whose target could not be named --
    /// `targets::title_for_handle` answers `None` for a window with an empty
    /// caption, and for a handle that is neither a live window nor a display.
    audio_unavailable_unnamed {
        ja: "音声を録音できませんでした。映像だけを記録しています。",
        en: "Audio could not be recorded. The recording has video only."
    }

    live_chip_stopping { ja: "停止中…", en: "Stopping…" }
    stop_done_toast { ja: "録画を止めました", en: "Recording stopped" }

    /// Task2030 判断7 / round9-response §2, the second line of the consolidated
    /// disk-full toast. **32 Japanese characters, which is longer than the one
    /// elided line `ui/controls.slint` gives a toast body (~29).** That is
    /// deliberate and design-owned: the recovery route is the act button
    /// 「設定を開く」, so a clipped tail loses no operation. Do not shorten it
    /// here -- a change to this wording goes back to design (round9-response
    /// §2), which is also why there is no length assertion on it below.
    disk_full_kept {
        ja: "録画済みの内容は残っています。空き容量を増やすと再開できます。",
        en: "What was recorded is kept. Free some space to start again."
    }

    /// The tray menu's stop entry while at most one capture is running. Moved
    /// here from the bin by task2030 so that it and the multi-capture spelling
    /// below cannot drift apart.
    tray_stop_capture { ja: "録画を停止", en: "Stop recording" }

}

/// Where the always-on warning bar comes up (task2030 判断2). **A warning line,
/// not a limit**: task2070 measured K=1..6 and found no cliff -- `queue_full`
/// and `dropped_frames` are 0 at every K, and the driver wall is the 13th
/// encode session machine-wide. 3 is only the first K at which the slowest
/// capture reaches the 50 fps line (49.71 fps against 51.65 at K=1), and over
/// ten minutes K=3 reads back above it. Nothing refuses a start on this number;
/// the bar says so and blocks nothing.
///
/// One constant, as round9-response §1-2 and round10 §3 both ask for (the
/// mock's `const WARN_N=3`).
pub const CONCURRENCY_WARNING_MIN: usize = 3;

/// The flyout's foot line once [`CONCURRENCY_WARNING_MIN`] or more are running
/// (t260927-711d, DS CaptureScreen 「状態」), `None` below it. One sentence now:
/// the bar across the window's foot that split it in two is gone. The number is
/// the running count itself, not the threshold: at four captures it says four.
pub fn concurrency_warning(locale: Locale, running: usize) -> Option<String> {
    (running >= CONCURRENCY_WARNING_MIN).then(|| match locale {
        Locale::Ja => format!(
            "{running} 本を録画中です。同時に録る本数が多いと、録画が重くなることがあります。"
        ),
        Locale::En => format!(
            "Recording {running} targets. Recording many at once can make recording heavier."
        ),
    })
}

/// The tray menu's stop entry (round9-response §1-6). Two or more running is
/// the only case that has to say so: this is the last "stop everything" door in
/// the app (判断5), and from a menu with no list in it the count is the only
/// warning that it will take down more than the recording being watched.
pub fn tray_stop_label(locale: Locale, running: usize) -> String {
    if running < 2 {
        return tray_stop_capture(locale).to_owned();
    }
    match locale {
        Locale::Ja => format!("すべての録画を停止（{running} 本）"),
        Locale::En => format!("Stop all recordings ({running})"),
    }
}

/// The tray's tooltip (round9-response §1-6). One capture is the only case that
/// can name a target -- a tooltip is one short line, so two or more report the
/// count instead. The idle string is [`tray_tooltip_idle`] unchanged.
pub fn tray_tooltip(locale: Locale, running: usize, title: Option<&str>) -> String {
    match (running, title) {
        (0, _) => tray_tooltip_idle(locale).to_owned(),
        (1, Some(title)) => match locale {
            Locale::Ja => format!("Liveback — 録画中: {title}"),
            Locale::En => format!("Liveback — Recording: {title}"),
        },
        // A window with no caption to borrow: the count wording would read as a
        // list of one, so the pre-task2030 string stands.
        (1, None) => tray_tooltip_recording(locale).to_owned(),
        (running, _) => match locale {
            Locale::Ja => format!("Liveback — {running} 本を録画中"),
            Locale::En => format!("Liveback — Recording {running} targets"),
        },
    }
}

/// The title bar chip's word (t260927-4c04 決めたこと 5): 「録画中」, or the
/// count once two or more run. From here rather than the `Tally` default,
/// which only speaks Japanese.
pub fn titlebar_tally_label(locale: Locale, running: usize) -> String {
    match (locale, running) {
        (Locale::Ja, 0 | 1) => "録画中".to_owned(),
        (Locale::Ja, n) => format!("{n} 本を録画中"),
        (Locale::En, 0 | 1) => "Recording".to_owned(),
        (Locale::En, n) => format!("Recording {n} targets"),
    }
}

/// The chip's buffer length as a clock (the DS TitleBar preview's `30:00`):
/// the retention window the capture was started with. `NO_RETENTION_LIMIT`
/// (0, the ring buffer switched off) has no length to show.
pub fn buffer_length_label(retention_minutes: u16) -> String {
    match retention_minutes {
        0 => String::new(),
        m => format_duration(i64::from(m) * 60 * HNS_PER_SECOND),
    }
}

/// The chip's `detail` (DS Tally 「バッファ長や経過時間」, t260928-b780 F13):
/// the buffer length, or -- with `NO_RETENTION_LIMIT` -- the time recorded so
/// far, `elapsed_100ns` in the segments' 100ns ticks, floored to whole minutes
/// in the same clock so the text only changes once a minute. `elapsed_100ns`
/// is asked only then: with a limit the chip never reads it.
pub fn tally_detail(retention_minutes: u16, elapsed_100ns: impl FnOnce() -> i64) -> String {
    if retention_minutes != 0 {
        return buffer_length_label(retention_minutes);
    }
    let minute = 60 * HNS_PER_SECOND;
    format_duration(elapsed_100ns().max(0) / minute * minute)
}

/// Whether the tray draws its 24px assets rather than the 16px ones
/// (t260927-f2eb), from the system DPI. The notification area is sized by
/// `SM_CXSMICON`: 20px at 120dpi, 24px at 144dpi, 32px at 192dpi. Only 16 and
/// 24 exist (the user's call: 24px only), and above 96dpi 24 is the closer of
/// the two every time -- shrunk at 120, stretched less than 16 at 192. 0 is
/// `GetDpiForSystem`'s failure value and keeps the 16px set.
pub fn tray_uses_24px(dpi: u32) -> bool {
    dpi > 96
}

/// Why the disk guard refused a start (task2050). `already_running == 0` is the
/// floor case and keeps its original wording; every capture already writing to
/// the disk raises the requirement, and then the numbers are what the user needs
/// to hear.
pub(crate) fn disk_refusal(locale: Locale, refusal: crate::capture::DiskRefusal) -> String {
    if refusal.already_running == 0 {
        return disk_floor_refusal(locale).to_owned();
    }
    const GB: u64 = 1024 * 1024 * 1024;
    let (required, free, running) = (
        refusal.required_bytes / GB,
        refusal.free_bytes / GB,
        refusal.already_running,
    );
    match locale {
        Locale::Ja => format!(
            "録画中の {running} 本に加えてもう 1 本開始するには空き容量が {required} GB 必要です（現在 {free} GB）。"
        ),
        Locale::En => format!(
            "Starting one more recording beside the {running} already running needs {required} GB free ({free} GB now)."
        ),
    }
}

/// The code for a user-requested stop. Not anomalous, so never notified: the
/// user is the one who asked.
const REQUESTED: &str = "CAP-EXIT-001";

/// A capture that failed to run, as opposed to a target that went away.
const CAPTURE_FAILED: &str = "CAP-DEV-001";

/// The one-line reason for a failed capture, from the worker's fixed stage name.
/// Deliberately not the HRESULT or the MFT diagnostics string: those are English,
/// long, and already in the log -- the toast has two lines.
///
/// The encoder arm reads the shared const rather than a literal: it spelled
/// `h264_encoder` until task2100, two months after task1760 renamed the stage,
/// so every encoder failure fell through to the generic bucket below.
pub fn capture_failure_detail(locale: Locale, stage: Option<&str>) -> &'static str {
    match stage {
        Some(VIDEO_ENCODER_STAGE) => failure_encoder(locale),
        Some(AUDIO_SESSION_STAGE) => failure_audio_session(locale),
        Some(MID_RECORDING_STAGE) => failure_mid_recording(locale),
        Some("window_handle" | "wgc_item") => failure_target(locale),
        // Kept for a status an older build wrote, and for the narrow case of the
        // loop genuinely failing to start: since t260911-77c3 a current build
        // reports a running capture's death as `MID_RECORDING_STAGE` instead.
        // Removing this arm would send those old statuses to the generic bucket.
        Some("start_capture") => failure_interrupted(locale),
        // Every other stage is a D3D/WGC/COM setup step: one bucket, because the
        // user's next move is the same for all of them. Unknown stages -- a new
        // one added later, or a status from an older build -- land here too.
        _ => failure_gpu(locale),
    }
}

/// Whether this stop is the failure `capture_failure_detail` describes.
pub fn is_capture_failure(status: &CaptureLifecycleStatus) -> bool {
    status.diagnostic_code.as_deref() == Some(CAPTURE_FAILED)
}

/// The title to put over `auto_stop_toast`'s body. The OS toast used to hand
/// `AUTO_STOP_TITLE` to every stop, so a capture that never started announced
/// itself as 「録画が停止しました」 while the in-app toast beside it said
/// 「録画に失敗しました」 over the identical body (task199).
pub fn auto_stop_title(locale: Locale, status: &CaptureLifecycleStatus) -> &'static str {
    if is_capture_failure(status) {
        capture_failed_title(locale)
    } else {
        auto_stop_title_text(locale)
    }
}

/// The toast body for a stop worth reporting, or `None`. `notified` is the
/// `seq` of the last stop toasted and is updated in place: the poll re-reads the
/// same stopped status every second, and the same `seq` is the same event, so
/// only the first sighting is news.
///
/// It used to key on the `diagnostic_code` instead, which silenced a *second*
/// capture dying under the same code -- exactly the shape of the NVENC wall,
/// where several captures fall in a row as `CAP-DEV-001` (task2090). Every stop
/// event carries its own `seq`, so a new death always rings.
pub fn auto_stop_toast(
    locale: Locale,
    notified: &mut Option<u64>,
    status: &CaptureLifecycleStatus,
) -> Option<String> {
    if status.state == "recording" {
        *notified = None;
        return None;
    }
    if status.state != "stopped" {
        return None;
    }
    let code = status.diagnostic_code.as_deref()?;
    if code == REQUESTED || *notified == Some(status.seq) {
        return None;
    }
    *notified = Some(status.seq);
    if is_capture_failure(status) {
        // Same wording as the in-app toast: one failure, one explanation.
        return Some(capture_failure_detail(locale, status.failure_stage.as_deref()).to_owned());
    }
    // A code with no wording (one a later build added) still gets a body:
    // the bare code beats an empty line.
    Some(stop_message(locale, code).map_or_else(|| code.to_owned(), str::to_owned))
}

/// What a stop says, one sentence per `CaptureStopReason`'s code
/// (t261101-5ff3: the engine sends the code, the screen words it). `None` for
/// a code this build does not know.
pub fn stop_message(locale: Locale, code: &str) -> Option<&'static str> {
    match code {
        "CAP-TGT-001" => Some(stop_source_closed(locale)),
        CAPTURE_FAILED => Some(stop_initialization_failed(locale)),
        REQUESTED => Some(stop_requested(locale)),
        DISK_FULL => Some(stop_disk_full(locale)),
        _ => None,
    }
}

/// Where one notice is written: the in-app corner toast or the Windows toast.
/// Exactly one of the two, never both and never neither (`notice_sinks`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeSinks {
    /// The in-app corner toast.
    pub corner: bool,
    /// `desktop::toast`, i.e. Windows.
    pub os: bool,
}

/// The one rule for every notice that can go to Windows as well as the corner
/// (t260928-e309, DS 知らせ方「同じことを 2 か所で言わない」, the user's ruling
/// 「前面なら Toast、裏なら OS の通知の 1 本に」): in front, the corner only;
/// behind -- or in the tray, where there is no window -- Windows only, so
/// coming back to the window finds no second copy of what Windows already said.
///
/// It replaces four rules that had grown per event: the auto-capture start was
/// already this split (task1550); the auto-stop went to both (task3920), and
/// the export's completion and the marker hotkey to both when behind
/// (task130 / task4150). task3920's hole -- closing the target hands focus to
/// Liveback, so the stop lands in a corner the user may not have been watching
/// -- is accepted by the ruling: the window is in front, and the corner is
/// where it says so. `log_stop_report`'s `sink` field reads this same call.
pub fn notice_sinks(foreground: bool) -> NoticeSinks {
    NoticeSinks {
        corner: foreground,
        os: !foreground,
    }
}

/// The disk guard's stop code. Every capture writes to the same buffer root, so
/// the guard takes all of them within the same second and each one settles its
/// own lifecycle event -- N stops for one cause (task2030 判断7).
const DISK_FULL: &str = "CAP-DSK-001";

pub fn is_disk_full(status: &CaptureLifecycleStatus) -> bool {
    status.diagnostic_code.as_deref() == Some(DISK_FULL)
}

/// The stop toast, with a disk-full sweep folded into one (task2030 判断7).
///
/// `Some((body, sub))` is what to show; `sub` is set only for the consolidated
/// disk-full toast, which is the only stop that has a second line and an act
/// button. Everything else is `auto_stop_toast` unchanged, which is why that
/// function is still the one carrying the per-event `seq` bookkeeping.
///
/// `stopped` is how many captures were running when the disk filled. Below two
/// there is nothing to consolidate and the single-capture wording task1420
/// pinned is what comes out -- that path is a regression contract, not a design
/// decision. `sweep` remembers that the sweep has already been reported and is
/// cleared as soon as anything is recording again.
pub fn stop_toast(
    locale: Locale,
    notified: &mut Option<u64>,
    sweep: &mut bool,
    status: &CaptureLifecycleStatus,
    stopped: usize,
) -> Option<(String, Option<&'static str>)> {
    if status.state == "recording" {
        *sweep = false;
    }
    // Always, even when the answer is thrown away below: it is what keeps one
    // stop event from being re-reported by the next poll that re-reads it.
    let body = auto_stop_toast(locale, notified, status);
    if !is_disk_full(status) {
        return body.map(|body| (body, None));
    }
    // The rest of the sweep is the same event as its first capture. Not keyed
    // on `seq` like `auto_stop_toast`: every capture the guard stops carries
    // its own, and reporting each one is exactly what this exists to avoid.
    //
    // Ahead of `stopped < 2` since task2210, because the count can now shrink
    // mid-sweep: three running, the guard reports all three, then a target
    // window closes as `CAP-TGT-001` and drops the count to one -- and the
    // guard's *next* capture would fall through to the single-capture wording
    // and trail a second toast behind the consolidated one. `sweep` is raised
    // by the consolidated branch and nowhere else, so reading it first cannot
    // swallow anything that was never consolidated.
    if *sweep {
        return None;
    }
    if stopped < 2 {
        return body.map(|body| (body, None));
    }
    *sweep = true;
    Some((
        disk_full_stopped(locale, stopped),
        Some(disk_full_kept(locale)),
    ))
}

/// The consolidated disk-full toast's first line (round9-response §2).
fn disk_full_stopped(locale: Locale, stopped: usize) -> String {
    match locale {
        Locale::Ja => {
            format!("ディスクの空き容量が不足したため、{stopped} 本の録画をすべて停止しました")
        }
        Locale::En => {
            format!("The disk ran out of room, so all {stopped} recordings were stopped")
        }
    }
}

/// Remembers that the user asked to stop this capture (task2030 判断7).
///
/// At most once per capture: the row and the tile stay clickable for the second
/// or so it takes the stop to land -- and the tray's "stop everything" asks for
/// every running capture, including any already asked for individually -- so
/// without this a double press would be two toasts for one recording.
pub fn request_stop(pending: &mut Vec<String>, session_id: &str) {
    if pending.iter().any(|asked| asked == session_id) {
        return;
    }
    pending.push(session_id.to_owned());
}

/// Which of the stops the user asked for have now finished, dropped from
/// `pending` as they are counted (task2030 判断7: one toast per capture).
///
/// The stop is fire-and-forget -- `stop_session_async` hands the join to its own
/// thread -- so the click itself proves nothing. A session leaving the active
/// set is the event, and it is the only one that can be attributed to a capture:
/// `lifecycle` carries no session id, and a user-requested stop is deliberately
/// dropped by `auto_stop_toast` (it is `CAP-EXIT-001`).
pub fn settled_stops(pending: &mut Vec<String>, active: &[String]) -> usize {
    let before = pending.len();
    pending.retain(|id| active.iter().any(|running| running == id));
    before - pending.len()
}

/// How many captures the disk guard is taking down, as `stop_toast`'s `stopped`
/// (task2200).
///
/// The naive reading -- "how many were running at the last poll" -- does not
/// merely name the wrong number, it loses the consolidation entirely. The guard
/// is a per-worker 10s poll (`capture/worker.rs`) and the workers are out of
/// phase, so two captures die 0.9--1.6s apart; the picker sends `Capturing`
/// before `Lifecycle` inside the same 1s tick, so by the time the *first* death
/// reaches `stop_toast` the live count has already fallen 2 -> 1 and the
/// consolidated branch (`stopped < 2`) is never taken. Measured on the machine
/// in task2030: two sweeps, neither consolidated.
///
/// So the count is sticky -- up with the live count, down only on an explicit
/// shrink. A cascade cannot shrink it, because nothing goes back to `recording`
/// mid-sweep (`CaptureLifecycleStatus::recording()` is written only by
/// `CaptureController::start`). That is also what keeps the *second* death out
/// of the single-capture branch: it still sees 2, so `sweep` swallows it
/// instead of trailing a task1420 toast 1.6s behind the consolidated one.
///
/// Sticky is not the same as never shrinking: a capture that died for its own
/// reason is no part of the disk's sweep either, and leaving it in the count
/// pads the toast exactly the way an unaccounted user stop did (task2210).
/// `settled` covers the stops the user asked for; `observe_lifecycle` covers the
/// rest.
#[derive(Default)]
pub struct RunningCount {
    live: usize,
    count: usize,
    /// The `seq` of the last stop already taken out of the count. The lifecycle
    /// is one process-wide slot that keeps reporting the same stopped status
    /// every second until something records again, so without this the shrink
    /// re-runs on every poll -- and a shrink that never stops running is a
    /// count permanently pinned to `live`, which is the sticky behaviour gone
    /// and task2200's bug back. Same trick, and the same reason, as
    /// `auto_stop_toast`'s `notified`.
    last_stop: Option<u64>,
}

impl RunningCount {
    /// The active set as this poll sees it. Up immediately, never down.
    pub fn observe(&mut self, running: usize) {
        self.live = running;
        self.count = self.count.max(running);
    }

    /// `n` stops the user asked for have landed (`settled_stops`), so they are
    /// no part of whatever sweep comes next.
    ///
    /// No floor at `live`: `live` is the previous poll's reading and is stale
    /// here, so `max(count - n, live)` would cancel the shrink outright. The
    /// next `observe` puts the floor back at whatever is really running.
    /// Without this, "the user stopped one, then the disk took the other" would
    /// claim 「2 本の録画をすべて停止しました」 -- a lie.
    pub fn settled(&mut self, n: usize) {
        self.count = self.count.saturating_sub(n);
    }

    /// Anything recording again ends the sweep. Deliberately the same trigger
    /// `stop_toast` clears its `sweep` flag on, so the two cannot drift apart.
    ///
    /// A stop that is not the disk's also shrinks the count (task2210): two
    /// captures running, one target window closes as `CAP-TGT-001`, and the
    /// guard then takes the survivor -- without this the toast says 「2 本の録画
    /// をすべて停止しました」 about a recording the disk never touched. A disk
    /// stop must *not* shrink it: that is the sweep itself, and shrinking there
    /// is precisely what task2200 fixed.
    ///
    /// `count = live`, not `count -= 1`, for two reasons. It is idempotent
    /// against `settled`, which has already taken a user-requested stop out by
    /// the time that stop's own `CAP-EXIT-001` arrives here -- subtracting
    /// would count it twice. And `live` is fresh at this point rather than
    /// stale: `take_active` drops the capture from the active set seconds
    /// before `settle_lifecycle` writes this status, and the picker sends
    /// `Msg::Capturing` before `Msg::Lifecycle` within the same poll, so the
    /// death is already in `live` when it gets here.
    pub fn observe_lifecycle(&mut self, status: &CaptureLifecycleStatus) {
        if status.state == "recording" {
            self.count = self.live;
            return;
        }
        if status.state != "stopped" || is_disk_full(status) || self.last_stop == Some(status.seq) {
            return;
        }
        self.last_stop = Some(status.seq);
        self.count = self.live;
    }

    /// What to hand `stop_toast` as `stopped`.
    pub fn count(&self) -> usize {
        self.count
    }
}

/// One running capture, as the mute notice needs to see it: the session id, what
/// to call it on screen, and whether its audio trial failed (task2140).
pub type AudioStatus<'a> = (&'a str, Option<&'a str>, bool);

/// The toast body for recordings that came out mute, or `None`.
///
/// `audio_unavailable` had three writers and no reader anywhere in `src/`: the
/// worker logged 「recording without an audio track」, set the flag, and carried
/// on: task169 had taken away the diagnostics drawer the flag used to reach, so
/// a recording that lost its sound finished in silence about it. This is the
/// reader. The recording is *not* stopped -- an unavailable audio session still
/// permits isolated video capture (task198 / task1430), which is why the notice
/// is a Warning and not the failure path.
///
/// `notified` is the ids already told, and is pruned to the captures still
/// running: the drain calls this ten times a second, and one recording must say
/// this once. Ids are `default_encoder_output_dir`'s nanosecond hex and never
/// repeat, so a pruned id can never come back.
///
/// Every newly-mute capture goes into one body rather than one toast each:
/// concurrent recordings are ordinary since task2080, and the corner holds one
/// toast at a time (latest wins), so a toast per capture would mean the first
/// one being overwritten before it could be read.
pub fn audio_unavailable_toast(
    locale: Locale,
    notified: &mut Vec<String>,
    running: &[AudioStatus<'_>],
) -> Option<String> {
    notified.retain(|told| running.iter().any(|(id, ..)| *id == told.as_str()));
    let fresh: Vec<AudioStatus<'_>> = running
        .iter()
        .copied()
        .filter(|(id, _, mute)| *mute && !notified.iter().any(|told| told == id))
        .collect();
    if fresh.is_empty() {
        return None;
    }
    notified.extend(fresh.iter().map(|(id, ..)| (*id).to_owned()));
    let names: Vec<&str> = fresh.iter().filter_map(|(_, title, _)| *title).collect();
    // All or nothing: naming two of three recordings would read as a complete
    // list and be wrong about which one lost its sound.
    if names.len() != fresh.len() {
        return Some(audio_unavailable_unnamed(locale).to_owned());
    }
    Some(match locale {
        Locale::Ja => format!(
            "{} の音声を録音できませんでした。映像だけを記録しています。",
            names.join("、")
        ),
        Locale::En => format!(
            "Audio could not be recorded for {}. The recording has video only.",
            names.join(", ")
        ),
    })
}

/// The review screen's `LIVE` indicator (task237). Round7 §2-3 took the stop off it --
/// it is a blinking dot and a word, and the verb sits in the title bar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveChip {
    /// Not recording, or the playhead has been pulled back off the live edge.
    Hidden,
    Live,
    /// The stop is under way. Says so.
    Stopping,
}

/// Deliberately the picker tile's word, not a second spelling of it.
pub use crate::ui_state::targets::recording as live_chip_live;

/// The chip follows **the session on the stage**, not the playhead (task670,
/// narrowed by task2030). With several captures running, `recording` means "the
/// loaded session is one of them" and `stopping` means "the stop the user asked
/// for on *this* session has not landed yet" -- the controller's `is_stopping()`
/// is process-wide and would put 停止中… on session B while A is being stopped.
/// The capsule stops the one it is showing (判断5); the count and the other
/// captures live on the history screen.
///
/// It used to also
/// require `at_live_edge`, which the playback worker only reports while it is
/// parked *at* the edge: a couple of seconds of rewind took the indicator off
/// the screen after a few seconds of rewind. It stands whatever the playhead is
/// looking at, including an old session -- what it reports is the capture.
pub fn live_chip(recording: bool, stopping: bool) -> LiveChip {
    // A stop already asked for outranks everything: 停止中… has to survive
    // `recording` going false, which happens on the same message that clears
    // `stopping`. Blinking out mid-stop reads as "the click did nothing".
    if stopping {
        return LiveChip::Stopping;
    }
    if !recording {
        return LiveChip::Hidden;
    }
    LiveChip::Live
}

pub fn live_chip_label(locale: Locale, chip: LiveChip) -> &'static str {
    match chip {
        LiveChip::Hidden => "",
        LiveChip::Live => live_chip_live(locale),
        LiveChip::Stopping => live_chip_stopping(locale),
    }
}

#[cfg(test)]
mod tests;
