//! The auto-stop notification (task141): React's `notifiedStopCodeRef` block in
//! `useAppState.ts`, which watched the 1s lifecycle poll and toasted once per
//! anomalous stop. A recording can end on its own -- the target window closed,
//! it was minimized, the GPU path failed -- and all three happen exactly when
//! the app window is hidden, so the toast is the only signal the user gets.

use crate::capture::{
    CaptureLifecycleStatus, AUDIO_SESSION_STAGE, MID_RECORDING_STAGE, VIDEO_ENCODER_STAGE,
};
use crate::ui_state::locale::Locale;

crate::tr! {
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
        ja: "映像エンコーダを準備できませんでした。設定のfpsや解像度を見直してください。",
        en: "The video encoder could not start. Check the frame rate and resolution in settings."
    }
    failure_target {
        ja: "キャプチャ対象を取得できませんでした。対象を選び直してください。",
        en: "The capture target could not be reached. Pick a target again."
    }
    failure_interrupted {
        ja: "キャプチャを続けられなくなりました。対象を選び直して再開してください。",
        en: "The capture could not continue. Pick a target again to restart."
    }
    failure_gpu {
        ja: "GPUのキャプチャ処理を準備できませんでした。詳しい原因はログに記録しました。",
        en: "The GPU capture path could not start. The details are in the log."
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

    /// Task2050, moved verbatim from `capture.rs`. The Japanese wording is the
    /// one that shipped before the cap was raised, and task2010 pinned it as a
    /// regression contract -- it stays byte for byte.
    disk_floor_refusal {
        ja: "空き容量が10GB以下のため録画を安全停止しました。既存clipは保持されます。",
        en: "Less than 10 GB is free, so recording was stopped safely. The clips already recorded are kept."
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

    live_chip_stop { ja: "停止", en: "Stop" }
    live_chip_stopping { ja: "停止中…", en: "Stopping…" }
    stop_done_toast { ja: "キャプチャを停止しました。", en: "Capture stopped." }

    /// Task2030 判断7 / round9-response §2, the second line of the consolidated
    /// disk-full toast. **32 Japanese characters, which is longer than the one
    /// elided line `ui/controls.slint` gives a toast body (~29).** That is
    /// deliberate and design-owned: the recovery route is the act button
    /// 「設定を開く」, so a clipped tail loses no operation. Do not shorten it
    /// here -- a change to this wording goes back to design (round9-response
    /// §2), which is also why there is no length assertion on it below.
    disk_full_kept {
        ja: "録画済みの内容は残っています · 空き容量を増やすと再開できます",
        en: "What was recorded is kept · free some space to start again"
    }

    /// The tray menu's stop entry while at most one capture is running. Moved
    /// here from the bin by task2030 so that it and the multi-capture spelling
    /// below cannot drift apart.
    tray_stop_capture { ja: "キャプチャ停止", en: "Stop capture" }

    /// Task2030 判断2. Split in two because the bar draws them differently:
    /// the count is 600-weight on-surface, the rest is on-surface-variant
    /// (round9-response §1-2, mock `.warnbar .wtxt`).
    concurrency_warning_tail {
        ja: " — 同時に録画する本数が多いと、パフォーマンスが低下する恐れがあります",
        en: " — recording several targets at once may cost you frames"
    }
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

/// The warning bar's two halves, or `None` below the threshold. The number is
/// the running count itself, not the threshold: at four captures it says four.
pub fn concurrency_warning(locale: Locale, running: usize) -> Option<(String, &'static str)> {
    (running >= CONCURRENCY_WARNING_MIN).then(|| {
        (
            match locale {
                Locale::Ja => format!("{running}本を録画中"),
                Locale::En => format!("Recording {running} targets"),
            },
            concurrency_warning_tail(locale),
        )
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
        Locale::Ja => format!("すべて停止（{running}本）"),
        Locale::En => format!("Stop all ({running})"),
    }
}

/// The tray's tooltip (round9-response §1-6). One capture is the only case that
/// can name a target -- a tooltip is one short line, so two or more report the
/// count instead. The idle string is `TRAY_TOOLTIP_IDLE` unchanged.
pub fn tray_tooltip(locale: Locale, running: usize, title: Option<&str>) -> String {
    match (running, title) {
        (0, _) => crate::TRAY_TOOLTIP_IDLE.to_owned(),
        (1, Some(title)) => match locale {
            Locale::Ja => format!("Liveback - 録画中: {title}"),
            Locale::En => format!("Liveback - Recording: {title}"),
        },
        // A window with no caption to borrow: the count wording would read as a
        // list of one, so the pre-task2030 string stands.
        (1, None) => crate::TRAY_TOOLTIP_RECORDING.to_owned(),
        (running, _) => match locale {
            Locale::Ja => format!("Liveback - {running}本を録画中"),
            Locale::En => format!("Liveback - Recording {running} targets"),
        },
    }
}

/// Whether the tray's stop entry is clickable, read back off the tooltip.
///
/// The event sink carries one string and no count, so the tray derives its
/// enabled state from the tooltip it was just handed. It has to be "not the idle
/// string": task2030 made every recording tooltip carry a name or a count, and
/// the bin's original equality test against `TRAY_TOOLTIP_RECORDING` therefore
/// matched nothing and greyed out 停止 for the whole of every recording
/// (task2030 verification sweep). Paired with `tray_tooltip` above, and tested
/// against it.
pub fn tray_stop_enabled(tooltip: &str) -> bool {
    tooltip != crate::TRAY_TOOLTIP_IDLE
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
            "録画中の{running}本に加えてもう1本開始するには空き容量が{required}GB必要です（現在{free}GB）。"
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
    // The message is always set beside the code, but a body is what the toast
    // is for -- falling back to the bare code beats an empty line.
    Some(status.message.clone().unwrap_or_else(|| code.to_owned()))
}

/// Where one stop report is written: the in-app corner, the Windows toast, or
/// both. Never neither -- a stop `stop_toast` decided is worth reporting always
/// leaves the corner record (task3920).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopSinks {
    /// The in-app corner toast. `Error` for a failure or the consolidated
    /// disk-full sweep, `Warning` otherwise -- which variant is the caller's,
    /// since only it holds the strings.
    pub corner: bool,
    /// `desktop::toast`, i.e. Windows.
    pub os: bool,
}

/// Which sinks a stop report goes to, given whether `stop_toast` consolidated it
/// and whether the window was in front of the user.
///
/// Task3920 measured the hole this closes. A `CAP-TGT-001` stop -- the recorded
/// target's window closed -- used to be routed by `foreground` alone: in front,
/// corner only; behind, Windows only. Closing the target is exactly when
/// Windows hands focus to whatever was behind it, so Liveback itself is often
/// the new foreground window and the stop was handed to Windows not at all
/// (measured 2026-09-08T04:36:41Z: `code=CAP-TGT-001 foreground=true
/// sink="corner"`, no `os_toast_show` after it). The user had asked why no
/// Windows toast appears when a recorded game exits; that is the answer, and
/// the same silence swallows the report when Windows suppresses the banner
/// instead (focus assist during a fullscreen game, candidate A of task3630 --
/// which the app cannot detect, `desktop.rs:332`).
///
/// So a stop that neither failed nor was consolidated now takes **both**: the
/// corner keeps a record that does not fade (task1420's `Warning`) whichever
/// way Windows decides, and Windows gets it whichever way the focus fell. The
/// cost is a doubled report when the window *is* in front -- accepted here,
/// against task193/task1420's tug-of-war, because a stop leaves no other trace:
/// a *start* keeps announcing itself through the live chip, the tray tooltip
/// and the taskbar badge, so `announce_auto_capture` is deliberately left on
/// the plain `foreground` split.
///
/// The failure and consolidated paths are unchanged and stay regression
/// contracts (task193 / task2030 判断7): their corner toast is an `Error` that
/// never fades and is already unconditional, so adding a second Windows report
/// in front of the user would be noise with nothing to buy.
pub fn stop_sinks(
    status: &CaptureLifecycleStatus,
    consolidated: bool,
    foreground: bool,
) -> StopSinks {
    let already_kept = consolidated || is_capture_failure(status);
    StopSinks {
        corner: true,
        os: !already_kept || !foreground,
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
            format!("ディスクの空き容量が不足したため、{stopped}本の録画をすべて停止しました")
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
    /// claim 「2本の録画をすべて停止しました」 -- a lie.
    pub fn settled(&mut self, n: usize) {
        self.count = self.count.saturating_sub(n);
    }

    /// Anything recording again ends the sweep. Deliberately the same trigger
    /// `stop_toast` clears its `sweep` flag on, so the two cannot drift apart.
    ///
    /// A stop that is not the disk's also shrinks the count (task2210): two
    /// captures running, one target window closes as `CAP-TGT-001`, and the
    /// guard then takes the survivor -- without this the toast says 「2本の録画
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
            "{}の音声を録音できませんでした。映像だけを記録しています。",
            names.join("、")
        ),
        Locale::En => format!(
            "Audio could not be recorded for {}. The recording has video only.",
            names.join(", ")
        ),
    })
}

/// What the transport's live chip is currently saying (task237). The indicator
/// *is* the way to stop: hovering it turns it into the verb, so the transport
/// gains an affordance without gaining a button. Same rule as the picker tile's
/// The review screen's `LIVE` indicator. Round7 §2-3 took the stop off it --
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
mod tests {
    use super::*;

    // `CaptureLifecycleStatus`'s constructors are `pub(super)` to `capture`, so
    // the statuses are built field by field here. `seq` stands in for the real
    // counter: any two distinct values are two distinct stop events.
    fn stopped(code: &str, message: Option<&str>) -> CaptureLifecycleStatus {
        stopped_seq(code, message, 1)
    }

    fn stopped_seq(code: &str, message: Option<&str>, seq: u64) -> CaptureLifecycleStatus {
        CaptureLifecycleStatus {
            state: "stopped".into(),
            diagnostic_code: Some(code.into()),
            message: message.map(Into::into),
            failure_stage: None,
            seq,
        }
    }

    fn failed(stage: Option<&str>) -> CaptureLifecycleStatus {
        CaptureLifecycleStatus {
            failure_stage: stage.map(Into::into),
            ..stopped(CAPTURE_FAILED, Some("停止しました"))
        }
    }

    /// Task199: the OS toast and the in-app toast are two renderings of one
    /// event, so they cannot disagree about what happened.
    #[test]
    fn the_os_toast_title_says_failed_only_when_the_capture_failed() {
        assert_eq!(
            auto_stop_title(Locale::Ja, &failed(Some(VIDEO_ENCODER_STAGE))),
            capture_failed_title(Locale::Ja)
        );
        assert_eq!(
            auto_stop_title(Locale::Ja, &failed(None)),
            capture_failed_title(Locale::Ja)
        );
        // A target that went away stopped the recording; it did not fail it.
        assert_eq!(
            auto_stop_title(
                Locale::Ja,
                &stopped("CAP-TGT-001", Some("対象が終了しました"))
            ),
            auto_stop_title_text(Locale::Ja)
        );
        assert_eq!(
            auto_stop_title(Locale::Ja, &recording()),
            auto_stop_title_text(Locale::Ja)
        );
    }

    fn recording() -> CaptureLifecycleStatus {
        CaptureLifecycleStatus {
            state: "recording".into(),
            diagnostic_code: None,
            message: None,
            failure_stage: None,
            seq: 0,
        }
    }

    fn idle() -> CaptureLifecycleStatus {
        CaptureLifecycleStatus {
            state: "idle".into(),
            diagnostic_code: None,
            message: None,
            failure_stage: None,
            seq: 0,
        }
    }

    #[test]
    fn every_anomalous_stop_is_notified() {
        let mut notified = None;
        assert_eq!(
            auto_stop_toast(
                Locale::Ja,
                &mut notified,
                &stopped("CAP-TGT-001", Some("停止しました"))
            ),
            Some("停止しました".to_owned())
        );
        assert_eq!(notified, Some(1));
        let mut notified = None;
        assert!(auto_stop_toast(
            Locale::Ja,
            &mut notified,
            &failed(Some(VIDEO_ENCODER_STAGE))
        )
        .is_some());
        assert_eq!(notified, Some(1));
    }

    /// Task2090, the reason the duplicate check moved off the code. The NVENC
    /// wall takes captures down one after another and every one of them is
    /// `CAP-DEV-001`; keyed on the code, the second death was silent -- no
    /// banner, and a recording the user had running was simply gone.
    #[test]
    fn a_second_capture_dying_under_the_same_code_is_still_news() {
        let mut notified = None;
        let first = CaptureLifecycleStatus {
            failure_stage: Some(VIDEO_ENCODER_STAGE.into()),
            ..stopped_seq(CAPTURE_FAILED, Some("停止しました"), 7)
        };
        let second = CaptureLifecycleStatus {
            seq: 8,
            ..first.clone()
        };
        assert!(auto_stop_toast(Locale::Ja, &mut notified, &first).is_some());
        // The 1s poll re-reading the same stop is not a second stop.
        assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &first), None);
        // A different stop event under the identical code is.
        assert!(auto_stop_toast(Locale::Ja, &mut notified, &second).is_some());
        assert_eq!(notified, Some(8));
        assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &second), None);
    }

    /// Task1420: the disk guard stops the recording, it does not fail it -- the
    /// session is closed and playable, so the toast says 「録画が停止しました」
    /// over the body that tells the user to free some room.
    #[test]
    fn running_out_of_disk_is_a_stop_and_is_notified_once() {
        let status = stopped("CAP-DSK-001", Some("容量を空けてください"));
        assert!(!is_capture_failure(&status));
        assert_eq!(
            auto_stop_title(Locale::Ja, &status),
            auto_stop_title_text(Locale::Ja)
        );
        let mut notified = None;
        assert_eq!(
            auto_stop_toast(Locale::Ja, &mut notified, &status),
            Some("容量を空けてください".to_owned())
        );
        assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &status), None);
    }

    #[test]
    fn a_user_requested_stop_is_not_news() {
        let mut notified = None;
        assert_eq!(
            auto_stop_toast(
                Locale::Ja,
                &mut notified,
                &stopped(REQUESTED, Some("録画を安全に停止しました。"))
            ),
            None
        );
        assert_eq!(notified, None);
    }

    #[test]
    fn the_same_stop_polled_again_is_not_a_second_stop() {
        let mut notified = None;
        let status = stopped("CAP-TGT-001", Some("対象が終了しました"));
        assert!(auto_stop_toast(Locale::Ja, &mut notified, &status).is_some());
        assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &status), None);
    }

    #[test]
    fn a_new_recording_makes_the_same_failure_news_again() {
        let mut notified = None;
        let status = stopped("CAP-TGT-001", Some("対象が終了しました"));
        assert!(auto_stop_toast(Locale::Ja, &mut notified, &status).is_some());
        assert_eq!(
            auto_stop_toast(Locale::Ja, &mut notified, &recording()),
            None
        );
        assert_eq!(notified, None);
        assert!(auto_stop_toast(Locale::Ja, &mut notified, &status).is_some());
    }

    #[test]
    fn idle_says_nothing() {
        let mut notified = None;
        assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &idle()), None);
    }

    #[test]
    fn a_body_less_stop_falls_back_to_the_code() {
        let mut notified = None;
        assert_eq!(
            auto_stop_toast(Locale::Ja, &mut notified, &stopped("CAP-TGT-001", None)),
            Some("CAP-TGT-001".to_owned())
        );
    }

    #[test]
    fn a_capture_failure_is_told_apart_from_a_target_that_went_away() {
        assert!(is_capture_failure(&failed(Some("start_capture"))));
        assert!(!is_capture_failure(&stopped("CAP-TGT-001", None)));
        assert!(!is_capture_failure(&recording()));
    }

    /// The two failures task193 came from: fps 120 rejected by the encoder, and
    /// the mid-recording death -- which reported `start_capture` until
    /// t260911-77c3 gave it `MID_RECORDING_STAGE`. The `start_capture` arm is
    /// still checked here: a status an older build wrote still reaches this
    /// function, and dropping the arm would send it to the generic GPU bucket.
    #[test]
    fn each_stage_gets_its_own_reason() {
        assert!(
            capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)).contains("エンコーダ")
        );
        assert!(capture_failure_detail(Locale::Ja, Some("start_capture")).contains("続けられ"));
        assert!(capture_failure_detail(Locale::Ja, Some(MID_RECORDING_STAGE)).contains("続けられ"));
        assert!(capture_failure_detail(Locale::Ja, Some("wgc_item")).contains("対象"));
        assert_ne!(
            capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)),
            capture_failure_detail(Locale::Ja, Some("start_capture"))
        );
        assert_ne!(
            capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)),
            capture_failure_detail(Locale::Ja, Some(MID_RECORDING_STAGE))
        );
    }

    /// The test the old suite was missing (task2100). `each_stage_gets_its_own
    /// _reason` above asserted a literal, so when task1760 renamed the stage the
    /// match arm went unreachable and every encoder failure said 「GPUの
    /// キャプチャ処理を準備できませんでした」 -- with the suite green throughout.
    /// This one goes through the const the worker itself assigns to
    /// `startup_stage`, so a rename either updates both ends or fails here.
    /// Do not rewrite it as a literal.
    #[test]
    fn the_stage_the_worker_actually_sets_reaches_the_encoder_reason() {
        let detail = capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE));
        assert!(
            detail.contains("エンコーダ"),
            "the encoder failure has to name the encoder: {detail}"
        );
        assert_ne!(
            detail,
            capture_failure_detail(Locale::Ja, None),
            "it must not fall through to the generic GPU bucket"
        );
    }

    /// Task2150. A recording killed by the audio session dying reported
    /// `start_capture`, so the toast told the user to pick a different target --
    /// advice that is wrong, because the target was never the problem. Same
    /// shape as the encoder test above: the stage comes from the const the
    /// worker assigns, never a literal.
    #[test]
    fn an_audio_session_death_is_not_blamed_on_the_capture_target() {
        for locale in [Locale::Ja, Locale::En] {
            let detail = capture_failure_detail(locale, Some(AUDIO_SESSION_STAGE));
            assert_ne!(
                detail,
                capture_failure_detail(locale, None),
                "it must not fall through to the generic GPU bucket"
            );
            assert_ne!(
                detail,
                capture_failure_detail(locale, Some(MID_RECORDING_STAGE)),
                "the generic mid-recording bucket loses the one actionable cause: {detail}"
            );
            assert_ne!(
                detail,
                capture_failure_detail(locale, Some("start_capture")),
                "the pre-77c3 mid-recording bucket is the wrong advice: {detail}"
            );
            assert_ne!(detail, capture_failure_detail(locale, Some("wgc_item")));
        }
        let ja = capture_failure_detail(Locale::Ja, Some(AUDIO_SESSION_STAGE));
        assert!(ja.contains("音声"), "the body has to name the cause: {ja}");
        assert!(
            !ja.contains("対象"),
            "the target is fine; never tell the user to pick another: {ja}"
        );
        // `ui/controls.slint` renders the body on one elided line with no wrap,
        // which holds roughly 29 Japanese characters. Longer text loses its tail
        // -- and the tail here is the only advice the user gets.
        assert!(
            ja.chars().count() <= 29,
            "the body has to survive elision, got {} chars: {ja}",
            ja.chars().count()
        );
    }

    /// t260911-77c3. On 2026-09-11 a recording died mid-run because the fMP4
    /// sink stopped taking video; the window was healthy the whole time, and the
    /// toast said 「キャプチャを続けられなくなりました。対象を選び直して再開
    /// してください。」 -- following it repairs nothing. Same shape as the audio
    /// test above: the stage comes from the const the worker assigns, never a
    /// literal (task2100).
    #[test]
    fn a_mid_recording_death_is_not_blamed_on_the_capture_target() {
        for locale in [Locale::Ja, Locale::En] {
            let detail = capture_failure_detail(locale, Some(MID_RECORDING_STAGE));
            assert_ne!(
                detail,
                capture_failure_detail(locale, None),
                "it must not fall through to the generic GPU bucket: {detail}"
            );
            assert_ne!(
                detail,
                capture_failure_detail(locale, Some("start_capture")),
                "the point of the new stage is that it is not the old advice: {detail}"
            );
            assert_ne!(detail, capture_failure_detail(locale, Some("wgc_item")));
            assert_ne!(
                detail,
                capture_failure_detail(locale, Some("window_handle"))
            );
        }
        let ja = capture_failure_detail(Locale::Ja, Some(MID_RECORDING_STAGE));
        let en = capture_failure_detail(Locale::En, Some(MID_RECORDING_STAGE));
        println!("mid_recording ja ({} chars): {ja}", ja.chars().count());
        println!("mid_recording en: {en}");
        assert!(
            !ja.contains("対象"),
            "the target is fine; never tell the user to pick another: {ja}"
        );
        assert!(
            !en.to_ascii_lowercase().contains("target"),
            "same in English: {en}"
        );
        // The same one-elided-line budget the audio wording is held to.
        assert!(
            ja.chars().count() <= 29,
            "the body has to survive elision, got {} chars: {ja}",
            ja.chars().count()
        );
    }

    #[test]
    fn an_unknown_or_absent_stage_falls_back_to_the_generic_reason() {
        let generic = capture_failure_detail(Locale::Ja, None);
        assert_eq!(
            capture_failure_detail(Locale::Ja, Some("a_stage_added_later")),
            generic
        );
        // Every setup step shares the bucket the fallback uses.
        assert_eq!(
            capture_failure_detail(Locale::Ja, Some("d3d_device")),
            generic
        );
    }

    /// The toast has two lines and the user reads Japanese: no HRESULT, no MFT
    /// diagnostics string.
    #[test]
    fn no_reason_leaks_a_diagnostic_string() {
        for stage in [
            None,
            Some("com"),
            Some("d3d_device"),
            Some("window_handle"),
            Some("wgc_item"),
            Some("wgc_frame_pool"),
            Some("wgc_session"),
            Some(VIDEO_ENCODER_STAGE),
            Some("transform_pipeline"),
            Some("nv12_converter"),
            Some("start_capture"),
            Some(AUDIO_SESSION_STAGE),
            Some(MID_RECORDING_STAGE),
        ] {
            let detail = capture_failure_detail(Locale::Ja, stage);
            assert!(!detail.contains("0x"), "{stage:?}: {detail}");
            assert!(
                !detail.is_ascii(),
                "{stage:?} should read as Japanese, not a diagnostic: {detail}"
            );
        }
    }

    /// Task2140. Every one of these used to be true of the shipped app: the
    /// flag was set, nothing read it, and the recording came out silent with
    /// only a log line to say so.
    #[test]
    fn a_recording_with_audio_says_nothing() {
        let mut notified = Vec::new();
        assert_eq!(
            audio_unavailable_toast(Locale::Ja, &mut notified, &[("s1", Some("メモ帳"), false)]),
            None
        );
        assert!(notified.is_empty());
        // Nothing recording at all is not news either.
        assert_eq!(
            audio_unavailable_toast(Locale::Ja, &mut notified, &[]),
            None
        );
    }

    #[test]
    fn a_mute_recording_is_named_in_both_languages() {
        let mut notified = Vec::new();
        let running = [("s1", Some("ペイント"), true)];
        let body = audio_unavailable_toast(Locale::Ja, &mut notified, &running).unwrap();
        assert!(body.contains("ペイント"), "it has to say which one: {body}");
        assert!(body.contains("音声"));
        let mut notified = Vec::new();
        let body = audio_unavailable_toast(Locale::En, &mut notified, &running).unwrap();
        assert!(body.contains("ペイント"), "{body}");
        assert!(body.contains("Audio"), "{body}");
    }

    /// The 1s..10s poll re-reads the same running capture forever; one recording
    /// says this once.
    #[test]
    fn the_same_mute_recording_is_only_announced_once() {
        let mut notified = Vec::new();
        let running = [("s1", Some("ペイント"), true)];
        assert!(audio_unavailable_toast(Locale::Ja, &mut notified, &running).is_some());
        assert_eq!(notified, vec!["s1".to_owned()]);
        assert_eq!(
            audio_unavailable_toast(Locale::Ja, &mut notified, &running),
            None
        );
        // The next recording is a different capture, so it is news again -- and
        // the finished one drops out of the memory rather than accumulating.
        let next = [("s2", Some("ペイント"), true)];
        assert!(audio_unavailable_toast(Locale::Ja, &mut notified, &next).is_some());
        assert_eq!(notified, vec!["s2".to_owned()]);
    }

    /// Concurrent captures are ordinary since task2080, and the corner holds one
    /// toast: two separate ones would mean the first being overwritten unread.
    #[test]
    fn two_mute_recordings_at_once_are_one_toast_naming_both() {
        let mut notified = Vec::new();
        let body = audio_unavailable_toast(
            Locale::Ja,
            &mut notified,
            &[("s1", Some("ペイント"), true), ("s2", Some("メモ帳"), true)],
        )
        .unwrap();
        assert!(
            body.contains("ペイント") && body.contains("メモ帳"),
            "{body}"
        );
        assert_eq!(notified.len(), 2);
        // A third capture joining later is its own news; the two already told
        // are not repeated.
        let body = audio_unavailable_toast(
            Locale::Ja,
            &mut notified,
            &[
                ("s1", Some("ペイント"), true),
                ("s2", Some("メモ帳"), true),
                ("s3", Some("電卓"), true),
            ],
        )
        .unwrap();
        assert!(body.contains("電卓"), "{body}");
        assert!(!body.contains("ペイント"), "already said: {body}");
    }

    /// `targets::title_for_handle` answers `None` for a window with an empty
    /// caption. Naming some of the mute captures would read as the whole list.
    #[test]
    fn an_unnamed_target_falls_back_to_wording_that_names_none_of_them() {
        let mut notified = Vec::new();
        assert_eq!(
            audio_unavailable_toast(Locale::Ja, &mut notified, &[("s1", None, true)]),
            Some(audio_unavailable_unnamed(Locale::Ja).to_owned())
        );
        let mut notified = Vec::new();
        assert_eq!(
            audio_unavailable_toast(
                Locale::En,
                &mut notified,
                &[("s1", Some("Paint"), true), ("s2", None, true)]
            ),
            Some(audio_unavailable_unnamed(Locale::En).to_owned())
        );
    }

    /// It reports a recording that is still running, so it must not borrow the
    /// failure title -- task198 / task1430 keep the video.
    #[test]
    fn the_mute_notice_does_not_claim_the_recording_failed() {
        assert_ne!(
            audio_unavailable_title(Locale::Ja),
            capture_failed_title(Locale::Ja)
        );
        assert_ne!(
            audio_unavailable_title(Locale::En),
            capture_failed_title(Locale::En)
        );
    }

    #[test]
    fn the_live_chip_shows_for_as_long_as_the_capture_runs() {
        assert_eq!(live_chip(false, false), LiveChip::Hidden);
        // Pulled back into the past, or looking at an old session entirely:
        // the capture is still running and is still stoppable from here
        // (task670).
        assert_eq!(live_chip(true, false), LiveChip::Live);
    }

    #[test]
    fn the_live_chip_never_becomes_the_verb() {
        // round7 §2-3: hovering it does nothing at all now.
        assert_eq!(live_chip(true, false), LiveChip::Live);
        // The resting word is the picker tile's, not a second spelling of it.
        assert_eq!(
            live_chip_label(Locale::Ja, LiveChip::Live),
            crate::ui_state::targets::recording(Locale::Ja)
        );
    }

    #[test]
    fn a_stop_in_flight_outranks_everything() {
        assert_eq!(live_chip(true, true), LiveChip::Stopping);
        // Even once the capture is gone: `stopping` clears on the same message
        // that clears `recording`, and until then the chip has to keep saying
        // what it is doing rather than blink out.
        assert_eq!(live_chip(false, true), LiveChip::Stopping);
        assert_eq!(
            live_chip_label(Locale::Ja, LiveChip::Stopping),
            live_chip_stopping(Locale::Ja)
        );
        assert_eq!(live_chip_label(Locale::Ja, LiveChip::Hidden), "");
    }

    /// Task2030 判断2. The bar is a warning, not a gate: it appears at N and
    /// says the running count, whatever that count is.
    #[test]
    fn the_warning_bar_appears_at_three_captures_and_counts_them() {
        assert_eq!(CONCURRENCY_WARNING_MIN, 3, "task2070 measured N=3");
        for running in 0..CONCURRENCY_WARNING_MIN {
            assert_eq!(
                concurrency_warning(Locale::Ja, running),
                None,
                "{running} captures is below the line"
            );
        }
        for running in CONCURRENCY_WARNING_MIN..=6 {
            let (lead, tail) = concurrency_warning(Locale::Ja, running).unwrap();
            assert!(
                lead.contains(&running.to_string()),
                "the number is the running count, not the threshold: {lead}"
            );
            assert!(lead.contains("録画中"), "{lead}");
            // 「恐れがあります」 -- task2070 found no cliff, so the bar must not
            // read as a breakage.
            assert!(tail.contains("恐れ"), "{tail}");
        }
        assert!(concurrency_warning(Locale::En, 4).is_some());
    }

    /// round9-response §1-6. Only one capture can be named; two or more are a
    /// count, and nothing recording is the string that shipped before task2030.
    #[test]
    fn the_tray_tooltip_names_one_capture_and_counts_the_rest() {
        assert_eq!(
            tray_tooltip(Locale::Ja, 0, Some("ペイント")),
            crate::TRAY_TOOLTIP_IDLE
        );
        let one = tray_tooltip(Locale::Ja, 1, Some("ペイント"));
        assert!(one.contains("ペイント"), "{one}");
        assert!(one.contains("録画中"), "{one}");
        assert_eq!(
            tray_tooltip(Locale::Ja, 1, None),
            crate::TRAY_TOOLTIP_RECORDING
        );
        let two = tray_tooltip(Locale::Ja, 2, Some("ペイント"));
        assert!(two.contains('2'), "{two}");
        assert!(
            !two.contains("ペイント"),
            "a tooltip is one short line: {two}"
        );
        assert!(tray_tooltip(Locale::En, 3, None).contains('3'));
    }

    /// The tray's stop entry is enabled off the tooltip alone (the sink carries
    /// no count), so the two have to be read together. Task2030 shipped the
    /// enable test as `tooltip == TRAY_TOOLTIP_RECORDING`, which after the same
    /// task's tooltip rewrite was true for exactly one shape -- one capture with
    /// no caption -- and greyed out 停止 for every real recording. Found on the
    /// machine in task2030's verification sweep.
    #[test]
    fn the_tray_stop_entry_is_enabled_for_every_recording_tooltip() {
        assert!(!tray_stop_enabled(&tray_tooltip(Locale::Ja, 0, None)));
        assert!(!tray_stop_enabled(&tray_tooltip(
            Locale::En,
            0,
            Some("Paint")
        )));
        for (running, title) in [
            (1, Some("ペイント")),
            (1, None),
            (2, Some("ペイント")),
            (3, None),
        ] {
            for locale in [Locale::Ja, Locale::En] {
                let tooltip = tray_tooltip(locale, running, title);
                assert!(
                    tray_stop_enabled(&tooltip),
                    "{running} running must leave the tray's stop clickable: {tooltip}"
                );
            }
        }
    }

    /// Task2030 判断7. Every capture writing to the filled disk settles its own
    /// lifecycle event, and the corner holds one toast: unconsolidated, the
    /// user would see the last of N and never know how many stopped.
    #[test]
    fn a_disk_full_sweep_is_one_toast_naming_the_count() {
        let mut notified = None;
        let mut sweep = false;
        let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 4);
        let (body, sub) = stop_toast(Locale::Ja, &mut notified, &mut sweep, &first, 3).unwrap();
        assert!(body.contains('3'), "it has to say how many stopped: {body}");
        assert!(body.contains("すべて停止"), "{body}");
        assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
        // The other two captures of the same sweep are the same event, under
        // their own `seq`s.
        let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
        assert_eq!(
            stop_toast(Locale::Ja, &mut notified, &mut sweep, &second, 3),
            None
        );
        // A new recording makes the next sweep news again.
        stop_toast(Locale::Ja, &mut notified, &mut sweep, &recording(), 0);
        assert!(!sweep);
        let later = stopped_seq(DISK_FULL, Some("容量を空けてください"), 9);
        assert!(stop_toast(Locale::Ja, &mut notified, &mut sweep, &later, 2).is_some());
    }

    /// One capture running out of disk keeps the wording task1420 pinned; there
    /// is nothing to consolidate, and the consolidated line would say 「1本の
    /// 録画をすべて停止しました」.
    #[test]
    fn one_capture_out_of_disk_keeps_the_single_capture_wording() {
        let mut notified = None;
        let mut sweep = false;
        let status = stopped(DISK_FULL, Some("容量を空けてください"));
        assert_eq!(
            stop_toast(Locale::Ja, &mut notified, &mut sweep, &status, 1),
            Some(("容量を空けてください".to_owned(), None))
        );
        assert!(!sweep, "nothing was consolidated, so nothing is remembered");
    }

    /// Task2200, the bug this file's `RunningCount` exists for. The guard's
    /// 10s per-worker poll took the two captures 1.6s apart on the machine
    /// (task2030: 16:47:10.29 / 16:47:11.89), and the picker re-reads the
    /// active set before draining the death, so the live count is already 1
    /// when the first `CAP-DSK-001` arrives -- which used to fall straight
    /// through `stopped < 2` into the single-capture wording.
    #[test]
    fn a_staggered_disk_sweep_is_one_toast_naming_the_count() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(2);
        running.observe(1);
        let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 4);
        let (body, sub) = stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &first,
            running.count(),
        )
        .unwrap();
        assert!(body.contains('2'), "it has to say how many stopped: {body}");
        assert!(body.contains("すべて停止"), "{body}");
        assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
        // 1.6s later the second capture dies, with nothing left running.
        running.observe(0);
        let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &second,
                running.count()
            ),
            None,
            "no task1420 toast may trail the consolidated one"
        );
    }

    /// The other end of the same race: both deaths land inside one poll, so
    /// the live count goes 2 -> 0 with nothing in between. The lifecycle is one
    /// process-wide slot and the second death overwrites the first, but the
    /// count is not read from it, so the number still adds up.
    #[test]
    fn a_disk_sweep_collapsed_into_one_poll_still_counts_two() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(2);
        running.observe(0);
        let status = stopped_seq(DISK_FULL, Some("容量を空けてください"), 7);
        let (body, sub) = stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &status,
            running.count(),
        )
        .unwrap();
        assert!(body.contains('2'), "{body}");
        assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
    }

    /// The user stopped one of two, then the disk took the other. Sticky
    /// without `settled` would call that 「2本の録画をすべて停止しました」,
    /// which is a lie about a recording the user ended themselves.
    #[test]
    fn a_stop_the_user_asked_for_does_not_pad_the_sweep() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(2);
        running.settled(1);
        running.observe(1);
        let status = stopped(DISK_FULL, Some("容量を空けてください"));
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &status,
                running.count()
            ),
            Some(("容量を空けてください".to_owned(), None)),
            "one capture left means the task1420 wording, not a consolidation"
        );
        assert!(!sweep);
    }

    /// One capture on the disk is unchanged by the count: single wording, no
    /// sweep remembered, and the next one rings again.
    #[test]
    fn a_lone_capture_out_of_disk_is_untouched_by_the_count() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(1);
        let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 1);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &first,
                running.count()
            ),
            Some(("容量を空けてください".to_owned(), None))
        );
        assert!(!sweep, "the single path never raises the sweep");
        running.observe(0);
        running.observe(1);
        let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 2);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &second,
                running.count()
            ),
            Some(("容量を空けてください".to_owned(), None)),
            "a later lone capture is its own event"
        );
    }

    /// Recording again ends the sweep for both states at once -- the count and
    /// `stop_toast`'s `sweep` are cleared on the same status, which is why they
    /// cannot disagree about whether the next disk hit is new.
    #[test]
    fn recording_again_rearms_the_consolidated_toast() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(2);
        let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 3);
        assert!(stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &first,
            running.count()
        )
        .is_some());
        running.observe(0);
        let live = recording();
        running.observe_lifecycle(&live);
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &live,
            running.count(),
        );
        assert!(!sweep);
        assert_eq!(running.count(), 0, "the old sweep's count does not survive");
        running.observe(2);
        let later = stopped_seq(DISK_FULL, Some("容量を空けてください"), 11);
        let (body, sub) = stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &later,
            running.count(),
        )
        .unwrap();
        assert!(body.contains('2'), "{body}");
        assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
    }

    /// Task2210, the lie task2200 left behind: one of two captures dies for its
    /// own reason, the disk then takes the survivor, and the sticky count still
    /// claims 「2本の録画をすべて停止しました」 about a recording the disk never
    /// touched.
    ///
    /// The live count is dropped *before* the death's lifecycle status, not
    /// after, and that is not an arbitrary ordering: `take_active` removes the
    /// capture from the active set seconds ahead of `settle_lifecycle`, and the
    /// picker sends `Msg::Capturing` before `Msg::Lifecycle` inside the same 1s
    /// poll. Writing it the other way round would be testing a sequence the app
    /// cannot produce.
    #[test]
    fn a_capture_that_died_on_its_own_is_not_padded_into_the_sweep() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(2);
        running.observe(1);
        let died = stopped_seq("CAP-TGT-001", Some("対象が終了しました"), 4);
        running.observe_lifecycle(&died);
        assert_eq!(running.count(), 1, "the closed target is out of the count");
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &died,
                running.count()
            ),
            Some(("対象が終了しました".to_owned(), None)),
            "the death is still reported on its own"
        );
        running.observe(0);
        let disk = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
        running.observe_lifecycle(&disk);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &disk,
                running.count()
            ),
            Some(("容量を空けてください".to_owned(), None)),
            "one capture was on the disk, so it is the task1420 wording"
        );
        assert!(!sweep, "nothing was consolidated, so nothing is remembered");
    }

    /// The shrink is one event, not one per poll. The lifecycle is a single
    /// process-wide slot that keeps handing back the same stopped status every
    /// second until something records again, so a shrink without the `seq`
    /// guard would re-run forever and pin the count to `live` -- which is the
    /// sticky count gone and task2200's bug back.
    #[test]
    fn a_stale_re_read_of_a_death_does_not_kill_the_sticky_count() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(3);
        running.observe(2);
        let died = stopped_seq("CAP-TGT-001", Some("対象が終了しました"), 4);
        running.observe_lifecycle(&died);
        assert_eq!(running.count(), 2);
        running.observe(2);
        running.observe_lifecycle(&died);
        // The disk takes the first of the two survivors: it leaves the active
        // set well before its own status is written, so this poll still reads
        // the *old* death while the live count is already 1.
        running.observe(1);
        running.observe_lifecycle(&died);
        assert_eq!(running.count(), 2, "the same death shrinks the count once");
        let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
        running.observe_lifecycle(&first);
        let (body, sub) = stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &first,
            running.count(),
        )
        .unwrap();
        assert!(body.contains('2'), "it names the two the disk took: {body}");
        assert!(body.contains("すべて停止"), "{body}");
        assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
        running.observe(0);
        let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 6);
        running.observe_lifecycle(&second);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &second,
                running.count()
            ),
            None,
            "the rest of the sweep is the same event"
        );
    }

    /// Why `sweep` is read before `stopped < 2`. The count can now fall in the
    /// middle of a sweep, and the guard's remaining captures would drop out of
    /// the consolidated branch into the single-capture wording -- trailing a
    /// task1420 toast behind the one that already named all three.
    #[test]
    fn a_death_inside_a_sweep_does_not_trail_a_single_toast() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(3);
        let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 7);
        running.observe_lifecycle(&first);
        let (body, sub) = stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &first,
            running.count(),
        )
        .unwrap();
        assert!(body.contains('3'), "{body}");
        assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
        // A target window closes while the rest of the sweep is still landing.
        running.observe(1);
        let died = stopped_seq("CAP-TGT-001", Some("対象が終了しました"), 8);
        running.observe_lifecycle(&died);
        assert_eq!(running.count(), 1);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &died,
                running.count()
            ),
            Some(("対象が終了しました".to_owned(), None)),
            "an ordinary death is unchanged by the sweep"
        );
        running.observe(0);
        let last = stopped_seq(DISK_FULL, Some("容量を空けてください"), 9);
        running.observe_lifecycle(&last);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &last,
                running.count()
            ),
            None,
            "the sweep was already reported, however small the count has become"
        );
    }

    /// A stop the user asked for arrives twice -- once as the session leaving
    /// the active set (`settled`), once as its own `CAP-EXIT-001` lifecycle --
    /// and must be taken out of the count once. `count = live` is what makes
    /// the second sighting a no-op; `count -= 1` would drop it to zero and the
    /// disk's toast would then name nothing at all.
    #[test]
    fn a_user_stop_shrinks_the_count_once_not_twice() {
        let mut notified = None;
        let mut sweep = false;
        let mut running = RunningCount::default();
        running.observe(2);
        running.settled(1);
        running.observe(1);
        let asked = stopped_seq(REQUESTED, Some("録画を安全に停止しました。"), 12);
        running.observe_lifecycle(&asked);
        assert_eq!(running.count(), 1, "settled already took it out");
        let disk = stopped_seq(DISK_FULL, Some("容量を空けてください"), 13);
        running.observe_lifecycle(&disk);
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &disk,
                running.count()
            ),
            Some(("容量を空けてください".to_owned(), None)),
            "one capture left is the task1420 wording, and it names no count"
        );
        assert!(!sweep);
    }

    /// Every other stop is `auto_stop_toast` unchanged -- including the seq
    /// bookkeeping, which has to keep running even on the polls whose answer
    /// the disk-full branch throws away.
    #[test]
    fn an_ordinary_stop_passes_straight_through() {
        let mut notified = None;
        let mut sweep = false;
        let status = stopped("CAP-TGT-001", Some("対象が終了しました"));
        assert_eq!(
            stop_toast(Locale::Ja, &mut notified, &mut sweep, &status, 3),
            Some(("対象が終了しました".to_owned(), None))
        );
        assert_eq!(
            stop_toast(Locale::Ja, &mut notified, &mut sweep, &status, 3),
            None,
            "the same event polled again is not a second stop"
        );
        let mut notified = None;
        let mut sweep = false;
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &stopped(REQUESTED, Some("録画を安全に停止しました。")),
                3
            ),
            None,
            "a stop the user asked for is reported by settled_stops, not here"
        );
    }

    /// Task2030 判断7's first half: one toast per capture the user stopped, and
    /// only once the capture is actually gone.
    #[test]
    fn a_requested_stop_is_reported_when_the_capture_leaves_the_active_set() {
        // Asking twice is one stop: the tile and the row stay clickable while
        // the stop is in flight, and the tray asks for everything running --
        // including whatever was already asked for on its own.
        let mut pending = Vec::new();
        request_stop(&mut pending, "s1");
        request_stop(&mut pending, "s1");
        request_stop(&mut pending, "s2");
        assert_eq!(pending, vec!["s1".to_owned(), "s2".to_owned()]);
        assert_eq!(settled_stops(&mut pending, &[]), 2);

        let mut pending = vec!["s1".to_owned(), "s2".to_owned()];
        let active = ["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];
        assert_eq!(settled_stops(&mut pending, &active), 0);
        assert_eq!(pending.len(), 2);
        // s2 finished; s1 is still flushing.
        assert_eq!(
            settled_stops(&mut pending, &["s1".to_owned(), "s3".to_owned()]),
            1
        );
        assert_eq!(pending, vec!["s1".to_owned()]);
        // Stop-everything from the tray: the rest land together, one each.
        let mut pending = vec!["s1".to_owned(), "s3".to_owned()];
        assert_eq!(settled_stops(&mut pending, &[]), 2);
        assert!(pending.is_empty());
        // And nothing is reported twice.
        assert_eq!(settled_stops(&mut pending, &[]), 0);
    }

    /// The OS toast and the in-app toast say the same thing.
    #[test]
    fn the_os_toast_body_is_the_stage_reason() {
        let mut notified = None;
        assert_eq!(
            auto_stop_toast(
                Locale::Ja,
                &mut notified,
                &failed(Some(VIDEO_ENCODER_STAGE))
            ),
            Some(capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)).to_owned())
        );
    }

    /// Task3920, the measured hole: the target's window closed, and closing it
    /// is exactly when focus lands on Liveback -- so routing on `foreground`
    /// alone handed the stop to nobody but a corner the user was not looking
    /// at (2026-09-08T04:36:41Z, `CAP-TGT-001 foreground=true sink="corner"`,
    /// no `os_toast_show` behind it). Both sinks now, whichever way focus fell.
    #[test]
    fn a_target_that_closed_is_reported_to_the_corner_and_to_windows() {
        for foreground in [true, false] {
            assert_eq!(
                stop_sinks(
                    &stopped("CAP-TGT-001", Some("対象が終了しました")),
                    false,
                    foreground
                ),
                StopSinks {
                    corner: true,
                    os: true
                },
                "foreground={foreground}"
            );
        }
    }

    /// Task193's contract: the failure's corner `Error` never fades, so it is
    /// still there when the user comes back -- and a second Windows report in
    /// front of them would buy nothing. Unchanged by task3920.
    #[test]
    fn a_failed_capture_keeps_its_error_and_only_tells_windows_from_behind() {
        assert_eq!(
            stop_sinks(&failed(Some(VIDEO_ENCODER_STAGE)), false, true),
            StopSinks {
                corner: true,
                os: false
            }
        );
        assert_eq!(
            stop_sinks(&failed(Some(VIDEO_ENCODER_STAGE)), false, false),
            StopSinks {
                corner: true,
                os: true
            }
        );
    }

    /// Task2030 判断7's contract: the consolidated disk-full sweep is one
    /// report with an act button, and it keeps the same routing it had.
    #[test]
    fn the_consolidated_disk_sweep_keeps_its_routing() {
        let status = stopped(DISK_FULL, Some("空き容量が不足しています"));
        assert_eq!(
            stop_sinks(&status, true, true),
            StopSinks {
                corner: true,
                os: false
            }
        );
        assert_eq!(
            stop_sinks(&status, true, false),
            StopSinks {
                corner: true,
                os: true
            }
        );
    }

    /// The stop the user asked for stays silent, and does so before `stop_sinks`
    /// is ever consulted: `stop_toast` is what drops it (task3630's design
    /// contract, and the reason its three `sink="none"` measurements are not a
    /// defect).
    #[test]
    fn a_requested_stop_never_reaches_a_sink() {
        let mut notified = None;
        let mut sweep = false;
        assert_eq!(
            stop_toast(
                Locale::Ja,
                &mut notified,
                &mut sweep,
                &stopped(REQUESTED, Some("停止しました")),
                1
            ),
            None
        );
    }
}
