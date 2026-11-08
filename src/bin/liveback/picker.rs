//! The target-picker page (task124): its UI-thread state, the polling
//! workers, the message application, the tile renderer, and the callback
//! wiring moved out of `main`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use livia::capture::targets::{self as capture_targets, CaptureTarget, CaptureTargetKind};
use livia::capture::{default_encoder_output_dir, CaptureConfig, CaptureController, CaptureSize};
use livia::settings::{self, AppSettings};
use livia::ui_state::auto_capture::{self, AutoCaptureWatch};
use livia::ui_state::lifecycle;
use livia::ui_state::targets::{self as ui_targets, PickerTab, TargetsEmpty, TargetsState};
use livia::ui_state::toast::Toast;
use slint::{ComponentHandle, Image, VecModel};

use super::tr_locale;
use super::{
    pixels_image, AppWindow, Cmd, Msg, PickerMsg, SettingsVm, TallyApp, TargetTile, TileMedia,
};

/// Whether the picker modal is on screen (task1170). The worker below grabs a
/// thumbnail every 120ms whether or not anyone can see one, and `PrintWindow`
/// forces the target to repaint to do it -- measured at ~2% of a 32-core
/// machine, held for the 140 minutes the app sat idle. Gated on this, an app
/// nobody is looking at costs nothing.
///
/// A `static` rather than an `Arc` threaded through `spawn_target_worker`:
/// `shell::set_picker_open` is the single place the modal opens or closes
/// (`picker-open` is an `in property`, so the UI cannot write it), and a flag
/// there is a two-line change instead of a signature that ripples.
pub(super) static PICKER_VISIBLE: AtomicBool = AtomicBool::new(false);
/// How often the window list and the capture-active flag are re-read. Same 1s
/// the React picker polled at.
const LIST_INTERVAL: Duration = Duration::from_secs(1);
/// How soon after the picker opens the first list may still re-sort the grid
/// (t261025-b28f): the worker wakes every 120ms, so it lands well inside this,
/// and no later list can move tiles once the user is aiming at one.
const RESORT_WINDOW: Duration = Duration::from_millis(500);
/// Gap between two thumbnail grabs. `PrintWindow` costs real time on a large
/// window, and the picker walks every listed window round-robin: at 120ms a
/// 30-window list comes back around every ~3.6s for the cost of a few percent
/// of one core.
///
/// The React version instead grabbed only the tiles an `IntersectionObserver`
/// said were on screen, 3 at a time, every second. Slint has no cheap
/// equivalent, so this trades a slower refresh for a bounded cost. If the list
/// ever grows past a few dozen windows, feed the visible range down from the
/// grid and filter here.
const THUMBNAIL_INTERVAL: Duration = Duration::from_millis(120);
/// Asked for explicitly rather than taken from the capture module's default, so
/// the size the grid draws at and the size it requests stay visibly paired: a
/// tile's media plate is at least ~206px wide, and a 2x request keeps it crisp
/// on a 150%-scaled display without paying for a full-size copy (task143).
const THUMBNAIL_MAX_WIDTH: u32 = 480;
const THUMBNAIL_MAX_HEIGHT: u32 = 270;

/// How many polls one summary line covers. Counted on `PickerMsg::Capturing` since
/// task t260911-66da -- the list is only sent when it moved, so it is no longer
/// the heartbeat. Both ride the same 1s block ([`LIST_INTERVAL`]), so this is
/// still the 30s window task a015's acceptance criteria are stated over, and
/// `targets` against `capturing` in one line is what says whether a still desk
/// sent anything at all.
const SUMMARY_TICKS: u32 = 30;

/// What the drain applied to the picker since the last summary (task a015).
///
/// The point of the counters is the comparison the acceptance criteria make:
/// how many times `flags.targets` rose against how many `PickerMsg::Thumbnail` /
/// `PickerMsg::AppIcon` were applied. Counted only while the modal is on screen --
/// the list poll runs for the life of the process, and a log line a second in
/// every run nobody is measuring is exactly the noise this must not add.
#[derive(Default)]
struct ApplyCounts {
    targets: u32,
    targets_changed: u32,
    capturing: u32,
    capturing_changed: u32,
    thumbnails: u32,
    app_icons: u32,
}

/// UI-thread state for the picker. `TargetsState` holds everything that has a
/// unit test; this adds only the two things that cannot live in a pure module:
/// decoded thumbnails and the transient error banner.
#[derive(Default)]
pub(super) struct Picker {
    pub(super) state: TargetsState,
    /// `None` = asked and got nothing back, which is a different tile than "not
    /// asked yet" (absent key).
    thumbnails: HashMap<String, Option<Image>>,
    /// Keyed by executable id, not by target: every window of one process wears
    /// the same badge icon, and the extraction is the expensive half.
    icons: HashMap<String, Option<Image>>,
    alert: ui_targets::TargetsAlert,
    /// The executables registered for auto-capture (task1980). Arrives on
    /// `PickerMsg::PickerRegistrations`, which the 1s poll sends only when one of the
    /// three settings fields moved (task t260911-66da) -- that message is what
    /// picks up a removal made on the settings screen, and it is deliberately
    /// not the list message: the list has to be free to go unsent.
    auto_capture: Vec<livia::settings::AutoCaptureApp>,
    /// The folder rules (task3600), carried by that same message: since
    /// task3670 the badge reports `covered_by`, so a tile qualifying only
    /// through a folder rule lights up too.
    auto_capture_folders: Vec<livia::settings::AutoCaptureFolder>,
    /// Full image path per process id, for the folder half of that answer.
    ///
    /// Resolved when the list moves or the rules move -- not on every render --
    /// by `rebuild_image_paths`: `target_tile` runs on every render, and a
    /// thumbnail arriving every 120ms is a render, so resolving there would pay
    /// an `OpenProcess` per visible tile roughly eight times a second. Both
    /// arms rebuild it, because either half of the answer can move on its own
    /// now (task t260911-66da). Left empty whenever `auto_capture_folders` is,
    /// which is the gate task3600 put on the poll's `resolve_path` for the same
    /// reason -- an install that registers no folder resolves no paths at all.
    image_paths: HashMap<u32, std::path::PathBuf>,
    /// `last_capture_executable`, the third field the list arm used to read
    /// under that same lock (task t260911-66da). It decides the preselected
    /// tile, and it arrives on `PickerMsg::PickerRegistrations` now.
    last_capture_executable: Option<String>,
    /// When the modal last opened, until the list that follows it lands: the
    /// list is read only while someone needs it (t261025-b28f), so the one
    /// `sort_for_open` ran on may be minutes old, and the fresh one the worker
    /// sends on the open is sorted again if it comes within `RESORT_WINDOW`.
    opened_at: Option<Instant>,
    /// task a015's instrument. Reset whenever the modal is not on screen,
    /// so a summary always covers one uninterrupted stretch of looking at it.
    counts: ApplyCounts,
}

impl Picker {
    /// One line per [`SUMMARY_TICKS`] polls while the modal is open (task
    /// a015). Emitted from the `PickerMsg::Capturing` arm since task t260911-66da:
    /// that arm is the 1s heartbeat now, because the list message is only sent
    /// when the list moved and a desk where nothing opens or closes sends none
    /// at all -- which is the very thing the summary has to be able to report.
    /// The thumbnail arm fires eight times a second and would make the window
    /// depend on how many windows are listed.
    fn report_counts(&mut self) {
        if self.counts.capturing < SUMMARY_TICKS {
            return;
        }
        tracing::info!(
            target: "task_a015_drain_flags",
            event = "picker_drain_summary",
            targets = self.counts.targets,
            targets_changed = self.counts.targets_changed,
            capturing = self.counts.capturing,
            capturing_changed = self.counts.capturing_changed,
            thumbnails = self.counts.thumbnails,
            app_icons = self.counts.app_icons,
            "picker drain applies since the last summary"
        );
        self.counts = ApplyCounts::default();
    }
}

/// The title bar's recording chip and the rail's lamp (t260927-4c04 決めたこと
/// 3 / 5). Read off the picker's own copy of the poll -- the running list, the
/// targets it names and their app icons -- so it moves with the same
/// `flags.targets` that re-renders the tiles. The buffer length is the one the
/// capture was started with (its manifest's `retention_minutes`), not the
/// setting now: a flip mid-recording leaves the running session alone.
pub(super) fn render_titlebar_tally(
    ui: &AppWindow,
    picker: &Picker,
    controller: &CaptureController,
) {
    let running: Vec<(&CaptureTarget, &str)> = picker
        .state
        .all()
        .iter()
        .filter_map(|target| {
            picker
                .state
                .recording_session(&target.id)
                .map(|session| (target, session))
        })
        .collect();
    let count = picker.state.running_count();
    let first = running.first();
    let title = first.map_or("", |(target, _)| {
        if target.kind == CaptureTargetKind::Monitor {
            ui_targets::monitor_name_and_resolution(&target.title).0
        } else {
            target.title.as_str()
        }
    });
    let (detail, elapsed) = first
        .map(|(_, session)| tally_detail(controller, session))
        .unwrap_or_default();
    // Every running capture gets a mark, not only those whose icon came
    // (t260928-b780 F3 / F11): the history's three rungs.
    let apps: Vec<TallyApp> = running
        .iter()
        .take(3)
        .map(|(target, _)| tally_app(picker, target))
        .collect();
    ui.set_titlebar_tally_count(count as i32);
    if count > 0 {
        super::shell::wake_tally();
    }
    ui.set_titlebar_tally_label(lifecycle::titlebar_tally_label(tr_locale(), count).into());
    ui.set_titlebar_tally_target(title.into());
    // A new model dirties the chip even when every mark is the same, and a
    // thumbnail landing calls this (t261025-b28f).
    super::set_if_changed(ui.get_titlebar_tally_apps(), apps, |m| {
        livia::insight_scope!("ui_titlebar_tally");
        ui.set_titlebar_tally_apps(m);
    });
    ui.set_titlebar_tally_detail(detail.into());
    ui.set_titlebar_tally_elapsed(elapsed);
}

/// One capture's mark on the chip, the history's `AppMark` rungs (user ruling
/// 2026-09-28, t260928-b780): a screen is the `monitor` glyph; a window is its
/// exe's icon, or -- when that would not come -- the monogram of the same name
/// the picker's tile and the history letter it from, so all three agree.
fn tally_app(picker: &Picker, target: &CaptureTarget) -> TallyApp {
    if target.kind == CaptureTargetKind::Monitor {
        return TallyApp {
            icon: Image::default(),
            letter: "".into(),
            hue: 0,
            monitor: true,
        };
    }
    let icon = target
        .executable_id
        .as_deref()
        .and_then(|executable| picker.icons.get(executable))
        .cloned()
        .flatten();
    let name = livia::ui_state::sessions::app_name(target.executable_name.as_deref())
        .unwrap_or_else(|| target.title.clone());
    TallyApp {
        icon: icon.unwrap_or_default(),
        letter: livia::ui_state::sessions::monogram_letter(&name).into(),
        hue: livia::ui_state::sessions::monogram_hue(&name),
        monitor: false,
    }
}

/// The chip's `detail` for one session and whether it is an elapsed time
/// that goes stale (no retention limit, t260928-b780 F13). The elapsed time is
/// the span of the finalized segments, `end - oldest start` -- with no limit
/// nothing is pruned, so that is the whole recording, one segment behind the
/// live edge, which the whole-minute clock hides. One helper for the full
/// render and the timer's tick, so the two never disagree.
fn tally_detail(controller: &CaptureController, session: &str) -> (String, bool) {
    let Some(manifest) = controller.timeline_for(session) else {
        return (String::new(), false);
    };
    let elapsed = || {
        controller
            .latest_finalized_range_for(session)
            .map_or(0, |range| range.end_100ns - range.oldest_start_100ns)
    };
    (
        lifecycle::tally_detail(manifest.retention_minutes, elapsed),
        manifest.retention_minutes == livia::ring_buffer::NO_RETENTION_LIMIT,
    )
}

/// The title bar Timer's tick (t260928-b780 F13): re-reads the first running
/// capture's elapsed time and writes the chip's `detail` only when its text
/// changed -- whole minutes, so once a minute -- and leaves the marks alone
/// (rebuilding their model would dirty the chip every tick).
fn refresh_tally_detail(ui: &AppWindow, picker: &Picker, controller: &CaptureController) {
    let first = picker
        .state
        .all()
        .iter()
        .find_map(|target| picker.state.recording_session(&target.id));
    let (detail, _) = first
        .map(|session| tally_detail(controller, session))
        .unwrap_or_default();
    if ui.get_titlebar_tally_detail().as_str() != detail {
        ui.set_titlebar_tally_detail(detail.into());
    }
}

/// A start's trouble, said where it can be read (t260927-711d, DS CaptureScreen
/// 「録り始められなかった」): the flyout closed on the press, so a failure is an
/// error toast -- the reason is its detail -- and it closes the flyout if it
/// was opened again meanwhile. A start that could not be remembered for next
/// time is only logged (t260928-08df, user ruling): the recording itself is
/// running, and all that was lost is the next opening's ordering hint.
pub(super) fn announce_start(
    ui: &AppWindow,
    status_line: &Rc<RefCell<Toast>>,
    message: &PickerMsg,
) {
    match message {
        PickerMsg::StartFailed(reason) => {
            super::shell::set_picker_open(ui, false);
            super::publish_error(
                ui,
                status_line,
                ui_targets::start_failed_title(tr_locale()),
                reason,
            );
        }
        PickerMsg::Started {
            settings_saved: false,
            ..
        } => tracing::warn!(
            event = "capture_start_save_failed",
            "recording started, but last_capture_executable could not be saved"
        ),
        _ => {}
    }
}

/// One target's tile.
fn target_tile(picker: &Picker, target: &livia::capture::targets::CaptureTarget) -> TargetTile {
    let thumbnail = picker.thumbnails.get(&target.id);
    let icon = target
        .executable_id
        .as_deref()
        .and_then(|executable| picker.icons.get(executable))
        .cloned()
        .flatten();
    // A screen's number (and メイン) is its title and its resolution a tag of
    // its own (t260927-711d); a window's title is all one piece.
    let screen = target.kind == CaptureTargetKind::Monitor;
    let monogram_name = (!screen).then(|| {
        livia::ui_state::sessions::app_name(target.executable_name.as_deref())
            .unwrap_or_else(|| target.title.clone())
    });
    let resolution = if screen {
        ui_targets::monitor_name_and_resolution(&target.title).1
    } else {
        ""
    };
    // task1980. A target with no executable behind it cannot be an
    // auto-record target at all, which is what keeps the badge off a
    // monitor tile.
    //
    // task3670: `covered_by`, not `is_registered` -- the badge reports
    // whether a launch of this app would record itself, and a folder rule
    // is one of the two ways that is true. The path comes out of the map
    // the target poll filled; absent (nothing registered, or the process
    // would not open) it answers from the executable list alone.
    //
    // task3690: through the guard, so a rule `poll` refuses -- one typed into
    // `settings.json` past the settings screen's toast -- leaves the badge off
    // instead of promising a recording that never starts.
    let registered = target
        .executable_id
        .as_deref()
        .and_then(|executable| {
            auto_capture::covered_by(
                &picker.auto_capture,
                &picker.auto_capture_folders,
                auto_capture::FolderGuard::machine(),
                executable,
                picker
                    .image_paths
                    .get(&target.process_id)
                    .map(std::path::PathBuf::as_path),
            )
        })
        .is_some();
    TargetTile {
        id: target.id.as_str().into(),
        title: ui_targets::tile_title(tr_locale(), target).into(),
        resolution: resolution.into(),
        blocked_reason: ui_targets::blocked_reason(tr_locale(), target)
            .unwrap_or_default()
            .into(),
        // Only "you cannot capture this at all" greys a tile. The one
        // being recorded is still clickable -- its click stops that
        // capture (task2030 判断1) -- so it is `recording`, not
        // `disabled`.
        disabled: !target.selectable,
        // The tile is a toggle (task175, task2030): this one stops, every
        // other selectable one starts one more beside whatever is running.
        recording: picker.state.tile_action(&target.id) == ui_targets::TileAction::Stop,
        screen,
        media: match thumbnail {
            None => TileMedia::Loading,
            Some(None) => TileMedia::None,
            Some(Some(_)) => TileMedia::Ready,
        },
        thumbnail: thumbnail.cloned().flatten().unwrap_or_default(),
        has_icon: icon.is_some(),
        app_icon: icon.unwrap_or_default(),
        // The history's monogram for a window whose icon would not come
        // (t260928-08df): same name, so the same letter and hue.
        app_letter: monogram_name
            .as_deref()
            .map(livia::ui_state::sessions::monogram_letter)
            .unwrap_or_default()
            .into(),
        app_hue: monogram_name
            .as_deref()
            .map_or(0, livia::ui_state::sessions::monogram_hue),
        auto_capture: registered,
        auto_capture_label: auto_capture::tile_registered(tr_locale()).into(),
    }
}

pub(super) fn render(ui: &AppWindow, picker: &Picker, model: &VecModel<TargetTile>) {
    let capturing = picker.state.capturing();
    let tiles: Vec<TargetTile> = picker
        .state
        .visible()
        .map(|target| target_tile(picker, target))
        .collect();

    // Rows are patched in place, never swapped wholesale. A thumbnail lands
    // every ~120ms, and replacing the model rebuilds every tile element: the
    // press and the release of a single click landed on two different
    // `TouchArea`s, so no tile could ever be clicked.
    super::sync_rows(model, tiles);
    // What stands where the grid would (DS CaptureScreen 「状態」): a failed
    // list outranks both nothings, since neither can be known without a list.
    let locale = tr_locale();
    let (empty_kind, empty_title, empty_sub) = match (picker.alert.text(), picker.state.empty()) {
        (Some(reason), _) => (3, ui_targets::list_error_title(locale).to_owned(), reason),
        (None, None) => (0, String::new(), ""),
        (None, Some(TargetsEmpty::NoMatch)) => (
            1,
            ui_targets::search_empty_text(locale, picker.state.tab(), picker.state.search()),
            "",
        ),
        (None, Some(TargetsEmpty::NoTargets)) => (
            2,
            ui_targets::empty_title(locale).to_owned(),
            ui_targets::empty_sub(locale),
        ),
    };
    ui.set_targets_empty_kind(empty_kind);
    ui.set_targets_empty_text(empty_title.into());
    ui.set_targets_empty_sub(empty_sub.into());
    let counts: Vec<slint::SharedString> = [PickerTab::Windows, PickerTab::Monitors]
        .into_iter()
        .map(|tab| picker.state.tab_count(tab).to_string().into())
        .collect();
    ui.set_picker_tab_counts(Rc::new(VecModel::from(counts)).into());
    ui.set_picker_rec_label(ui_targets::rec(locale).into());
    ui.set_capturing(capturing);
    // Task2030 判断2, a warning and not a gate: at `CONCURRENCY_WARNING_MIN`
    // running it takes the flyout's foot line (t260927-711d) -- the only place
    // a further capture can be started -- and blocks nothing.
    let (foot, foot_warning) =
        ui_targets::foot_note(locale, picker.state.tab(), picker.state.running_count());
    ui.set_targets_foot_text(foot.into());
    ui.set_targets_foot_warning(foot_warning);
    // The settings screen's 変更 button reads its own copy of this, and only
    // `render_settings` used to write it -- which the 1s capture poll never
    // calls. So starting a capture left the buffer-root button live and its
    // "録画中は変更できません" reason hidden until some *other* setting changed
    // (found verifying task173/185). Written here, at the one place the capture
    // flag moves, so the two copies cannot drift apart.
    ui.global::<SettingsVm>()
        .set_buffer_change_enabled(!capturing);
}

/// The auto-capture poll's account of itself (task1970): what it armed, what
/// it passed over, and what it gave up on.
fn log_auto_capture_plan(plan: &auto_capture::AutoCapturePlan, capturing: bool) {
    // Once each, never every second: a rule typed into `settings.json` by hand
    // has to be visible without drowning the log (task3600, ユーザー裁定3).
    // The count, not the folders: logs carry no user-chosen paths.
    if !plan.rejected_folders.is_empty() {
        tracing::warn!(
            target: "task1970_auto_capture",
            event = "auto_capture_folder_rejected",
            rejected = plan.rejected_folders.len(),
            "folder rule is a drive root, Windows, or Program Files itself; ignoring it"
        );
    }
    if let Some(executable) = &plan.armed {
        // Which list it came from (task3600): a folder rule names no
        // executable, so the log has to say a folder rule matched -- whether,
        // not which: logs carry no user-chosen paths.
        tracing::info!(
            target: "task1970_auto_capture",
            event = "auto_capture_armed",
            %executable,
            folder_rule = matches!(plan.rule, Some(auto_capture::AutoCaptureRule::Folder(_))),
            "registered executable appeared; waiting for a capturable window"
        );
    }
    for executable in &plan.skipped {
        tracing::info!(
            target: "task1970_auto_capture",
            event = "auto_capture_skipped",
            %executable,
            capturing,
            "registered executable appeared but was not started"
        );
    }
    if let Some(executable) = &plan.gave_up {
        tracing::warn!(
            target: "task1970_auto_capture",
            event = "auto_capture_gave_up",
            %executable,
            "start kept being refused; not retrying until it is launched again"
        );
    }
}

/// Reads the window list and grabs thumbnails. Everything here blocks -- window
/// enumeration, `PrintWindow`, `is_active()`'s mutex -- which is exactly why it
/// is not on the event loop.
pub(super) fn spawn_target_worker(
    controller: CaptureController,
    settings: Arc<Mutex<AppSettings>>,
    tx: Sender<Msg>,
) {
    thread::spawn(move || {
        let mut known: Vec<CaptureTarget> = Vec::new();
        let mut cursor = 0usize;
        let mut listed_at: Option<Instant> = None;
        let mut icons_asked: std::collections::HashSet<String> = std::collections::HashSet::new();
        // task4290: what each target's tile was last given, so a motionless
        // window is captured but not re-sent. See `ThumbnailDigests`.
        let mut thumbnail_digests = ui_targets::ThumbnailDigests::default();
        // task t260911-66da: the same idea one level up. The list itself was
        // sent once a second whether or not a window had moved, and the
        // receiver had to take it -- that arm was where the picker re-read the
        // settings its badges draw. Split in two, both sides are gated here.
        let mut sent_list = ui_targets::SentList::default();
        let mut sent_registrations = ui_targets::SentRegistrations::default();
        // Auto-start detection rides this poll rather than a hook of its own
        // (task1970): the list below is read every second whenever a rule
        // exists (t261025-b28f), so the whole feature is a set comparison over
        // something that is being computed anyway.
        let mut auto_capture = AutoCaptureWatch::default();
        // Folder rules are read through this (task3600, ユーザー裁定3; unified
        // onto the shared cache in task3690's `FolderGuard::machine()` --
        // follow-up of task 3670/3690). The machine's Windows and Program
        // Files directories do not move while the app runs, and reading them
        // inside the 1s poll would put an environment lookup on a hot path
        // for nothing -- `machine()` already builds this once and caches it
        // for the life of the process, same as this thread was doing on its
        // own before.
        let folder_guard = auto_capture::FolderGuard::machine();
        // SourceClosed rearm (task2550), the folder handoff (task3600) and the
        // rearm hold (t260928-5cb2): which auto-started recording is on foot,
        // which stop was consumed, a rearm waiting for the stop to drain
        // (t261101-566d moved them out of this function).
        let mut tracker = auto_capture::AutoStartTracker::default();
        let mut was_visible = false;
        loop {
            // The picker opening lists at once, and sends even an unchanged
            // list: while it was hidden the list may not have been read for
            // minutes (t261025-b28f), and `apply` re-sorts on this one.
            let visible = PICKER_VISIBLE.load(Ordering::Relaxed);
            if visible && !was_visible {
                listed_at = None;
                sent_list = ui_targets::SentList::default();
            }
            was_visible = visible;
            if listed_at.is_none_or(|at| at.elapsed() >= LIST_INTERVAL) {
                listed_at = Some(Instant::now());
                // The three fields `PickerMsg::Targets(Ok)` used to re-read on the
                // receiving side (task1980 / task3670), read here instead --
                // on the lock the auto-capture watch below was taking anyway,
                // so the split costs no extra lock (task t260911-66da). Sent
                // ahead of the list, so a tick that moves both resolves the
                // list against the rules it now has.
                let (last_capture_executable, registered, folders) = settings
                    .lock()
                    .map(|current| {
                        (
                            current.last_capture_executable.clone(),
                            current.auto_capture_executables.clone(),
                            current.auto_capture_folders.clone(),
                        )
                    })
                    .unwrap_or_default();
                if sent_registrations.changed(&last_capture_executable, &registered, &folders)
                    && tx
                        .send(Msg::Picker(PickerMsg::PickerRegistrations {
                            last_capture_executable: last_capture_executable.clone(),
                            apps: registered.clone(),
                            folders: folders.clone(),
                        }))
                        .is_err()
                {
                    return;
                }
                // Read before `active_targets()` (task2550): a stop settles
                // its lifecycle only after the entry has left the active map,
                // so a stop visible here is guaranteed gone from the list
                // below. The other order could consume the stop's `seq` while
                // the handle still looked active and lose the rearm.
                let lifecycle = controller.lifecycle_status();
                // `active_targets()` is also what drives the lazy detection of a
                // capture that stopped on its own (target window closed), so
                // this poll is load-bearing, not just a display refresh.
                let running = controller.active_targets();
                // The window list has four readers: the open picker, the
                // auto-capture rules (both kinds), and the title bar's marks
                // and icons of what records. None of them -- and not the
                // stop detection above -- needs it otherwise (t261025-b28f).
                if visible || !registered.is_empty() || !folders.is_empty() || !running.is_empty() {
                    livia::insight_scope!("picker_list_targets");
                    // Windows and screens, in one list and one order (task170).
                    // The tab is only a filter over it, and the sort that used
                    // to run here now runs once, when the modal opens:
                    // re-sorting on every poll moved tiles out from under the
                    // cursor.
                    let listed = capture_targets::list_capture_targets()
                        .map(|mut targets| {
                            targets.extend(
                                capture_targets::list_monitor_targets(|number, width, height| {
                                    ui_targets::monitor_display_name(
                                        tr_locale(),
                                        number,
                                        width,
                                        height,
                                    )
                                })
                                .unwrap_or_default(),
                            );
                            targets
                        })
                        // The reason is the flyout's error state (t260928-08df):
                        // the engine's English goes to the log, a sentence to it.
                        .map_err(|raw| {
                            tracing::warn!(event = "capture_targets_list_failed", %raw);
                            ui_targets::list_error_reason(tr_locale()).to_owned()
                        });
                    match &listed {
                        Ok(targets) => known.clone_from(targets),
                        Err(_) => known.clear(),
                    }
                    // Pruned here, against the list `apply` prunes the UI's
                    // thumbnail map with a moment later (task4290): the two
                    // have to forget a departed window together, or a window
                    // that came back would be held as "unchanged" against a
                    // tile that no longer has its picture.
                    thumbnail_digests.retain_listed(&known);
                    if sent_list.changed(&listed)
                        && tx.send(Msg::Picker(PickerMsg::Targets(listed))).is_err()
                    {
                        return;
                    }
                }
                let (active, stopping) = (!running.is_empty(), controller.is_stopping());
                // The title bar chip draws a running capture's icon with the
                // picker closed too -- an auto-start never opens it -- so the
                // icons of what is recording are asked for ungated, once each
                // like the round-robin below.
                for target in known.iter().filter(|target| {
                    running
                        .iter()
                        .any(|(_, handle)| *handle == target.window_handle)
                }) {
                    if !ask_icon(&mut icons_asked, &tx, target) {
                        return;
                    }
                }
                // Nothing registered and nothing recording: no snapshot, so an
                // install that never uses the feature pays nothing for it.
                // `None` is a snapshot that failed. Taken ahead of the message
                // so the 音量 tab's App rows read it off the UI thread
                // (t261025-b28f); the rules below read the same one.
                let wants_snapshot = !registered.is_empty() || !folders.is_empty() || active;
                let snapshot = if wants_snapshot {
                    capture_targets::running_processes().map(Arc::new)
                } else {
                    Some(Arc::new(capture_targets::RunningProcesses::default()))
                };
                // The stop handling (task2550) reads this poll's `running`,
                // before it is moved into the message below. It sends
                // nothing; what it decides is applied ahead of `poll`.
                let forgets = tracker.before_plan(&auto_capture::PollSeen {
                    running: &running,
                    state: &lifecycle.state,
                    seq: lifecycle.seq,
                    diagnostic_code: lifecycle.diagnostic_code.as_deref(),
                    processes: snapshot.as_deref(),
                    registered: &registered,
                    stopping,
                });
                for action in forgets {
                    if let auto_capture::Action::ForgetHandedOver(executable) = action {
                        auto_capture.forget_handed_over(&executable);
                    }
                }
                if tx
                    .send(Msg::Picker(PickerMsg::Capturing {
                        running,
                        target_minimized: controller.target_minimized(),
                        processes: snapshot.clone().filter(|_| wants_snapshot),
                    }))
                    .is_err()
                {
                    return;
                }
                // Everything below runs after the list message, so the UI has
                // already taken this poll's targets by the time a start lands
                // and can look the id up in its own state. The registrations
                // it reads are the ones hoisted above (task t260911-66da) --
                // still one lock a tick, now shared with the picker's badges.
                // The snapshot is the one taken above (`poll` early-returns on
                // two empty lists anyway). `None` is a snapshot that failed --
                // passing the empty set would read as every process having
                // died and re-fire everything on the next good poll, so that
                // tick is skipped instead.
                let plan = match &snapshot {
                    Some(processes) => {
                        // The executable half matches on names alone; the
                        // entries' display metadata (task2560) has nothing to
                        // say here. The folder half (task3600) needs the pids,
                        // and resolves a path only for names new this poll.
                        let names: Vec<String> =
                            registered.iter().map(|entry| entry.name.clone()).collect();
                        let inputs = auto_capture::AutoCaptureInputs {
                            running: &processes.names,
                            pids: &processes.pids,
                            resolve_path: &|process_id| {
                                capture_targets::executable_path_for_process(process_id)
                                    .map(std::path::PathBuf::from)
                            },
                            guard: folder_guard,
                        };
                        auto_capture.poll(&names, &folders, &inputs, &known, active || stopping)
                    }
                    None => auto_capture::AutoCapturePlan::default(),
                };
                log_auto_capture_plan(&plan, active || stopping);
                // Remembered so a SourceClosed of *this* recording can be told
                // apart from one of a manually started recording (task2550).
                // Repeats with the retry, same values.
                let start_target = plan.start.as_ref().and_then(|id| {
                    let target = known.iter().find(|target| target.id == *id)?;
                    let executable = target.executable_id.clone()?;
                    Some((id.clone(), target.window_handle.clone(), executable))
                });
                let (actions, held) =
                    tracker.after_plan(&plan, start_target, stopping, snapshot.as_deref());
                if !apply_auto_start_actions(actions, &controller, &tx) {
                    return;
                }
                // Same poll, second reading: the bool above says *whether* a
                // capture is running, this says why the last one stopped, which
                // is what the auto-stop toast needs (task141).
                if tx.send(Msg::Lifecycle(lifecycle, held)).is_err() {
                    return;
                }
            }

            // Only while the modal is up (task1170). The 1s block above stays
            // ungated: `is_active()` there is what notices a capture that
            // stopped on its own, and the LIVE chip reads the same poll.
            //
            // `icons_asked` and the UI's thumbnail map both survive the closed
            // stretch, so re-opening shows the previous pictures and refreshes
            // them in place rather than starting from empty tiles.
            if PICKER_VISIBLE.load(Ordering::Relaxed) && !known.is_empty() {
                if cursor >= known.len() {
                    cursor = 0;
                }
                let target = &known[cursor];
                cursor += 1;
                // Once per executable, on the same round-robin: the badge icon
                // never changes for a running process, so asking again would be
                // a `SendMessageTimeout` into someone else's message loop for
                // nothing.
                if !ask_icon(&mut icons_asked, &tx, target) {
                    return;
                }
                let pixels = capture_targets::capture_target_thumbnail_rgba(
                    &target.window_handle,
                    target.process_id,
                    target.kind,
                    Some(THUMBNAIL_MAX_WIDTH),
                    Some(THUMBNAIL_MAX_HEIGHT),
                )
                .ok()
                .flatten();
                // Captured every round as before -- a moving window still
                // refreshes in place -- but only handed over when the picture
                // is not the one the tile already holds (task4290). Every send
                // swaps the model's `Image`, so an identical frame is a redraw
                // for nothing, and a picker full of motionless windows was
                // paying for one ~8 times a second.
                let frame = pixels
                    .as_ref()
                    .map(|(width, height, rgba)| (*width, *height, rgba.as_slice()));
                if thumbnail_digests.changed(&target.id, frame)
                    && tx
                        .send(Msg::Picker(PickerMsg::Thumbnail(target.id.clone(), pixels)))
                        .is_err()
                {
                    return;
                }
            }
            thread::sleep(THUMBNAIL_INTERVAL);
        }
    });
}

/// Sends a target's badge icon the first time its executable is seen. `false`
/// when the UI has gone away.
fn ask_icon(
    icons_asked: &mut std::collections::HashSet<String>,
    tx: &Sender<Msg>,
    target: &CaptureTarget,
) -> bool {
    let Some(executable) = target.executable_id.clone() else {
        return true;
    };
    if !icons_asked.insert(executable.clone()) {
        return true;
    }
    let icon = window_icon_rgba(&target.window_handle);
    tx.send(Msg::Picker(PickerMsg::AppIcon(executable, icon)))
        .is_ok()
}

/// Performs what `AutoStartTracker::after_plan` asked for, in order. `false`
/// when the UI has gone away.
fn apply_auto_start_actions(
    actions: Vec<auto_capture::Action>,
    controller: &CaptureController,
    tx: &Sender<Msg>,
) -> bool {
    for action in actions {
        match action {
            auto_capture::Action::StopSession(session) => {
                controller.stop_session_async(&session);
            }
            auto_capture::Action::StartCapture { id, handed_over } => {
                if tx.send(Msg::AutoCaptureStart { id, handed_over }).is_err() {
                    return false;
                }
            }
            auto_capture::Action::RecycleSession {
                session,
                executable,
            } => {
                let result =
                    controller.discard_session(&session, livia::ring_buffer::Disposal::Recycle);
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_first_window_recycled",
                    %session,
                    %executable,
                    ok = result.is_ok(),
                    error = result.err().unwrap_or_default(),
                    "the launch's first recording went to the recycle bin"
                );
            }
            // Applied by the caller ahead of `poll`; `after_plan` never
            // returns it.
            auto_capture::Action::ForgetHandedOver(_) => {}
        }
    }
    true
}

/// The 14px badge icon on a window tile (round5 §5). `WM_GETICON` is what the
/// taskbar asks each window for, and the class icon is the fallback for the
/// ones that never answer -- between them they cover what Windows itself draws.
///
/// The returned icon is owned by the window or its class, so it is *not*
/// destroyed here; only the bitmaps `GetIconInfo` hands back are ours.
fn window_icon_rgba(window_handle: &str) -> Option<super::Pixels> {
    // `SendMessageTimeoutW(WM_GETICON)` waits on another process's message
    // loop, so a hung app on the picker list is felt here (task205).
    livia::insight_scope!("picker_window_icon");
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassLongPtrW, SendMessageTimeoutW, GCLP_HICON, GCLP_HICONSM, HICON, ICON_BIG,
        ICON_SMALL2, SMTO_ABORTIFHUNG, WM_GETICON,
    };

    let hwnd = capture_targets::parse_window_handle(window_handle).ok()?;
    unsafe {
        let mut handle = 0usize;
        for which in [ICON_SMALL2, ICON_BIG] {
            let mut result = 0usize;
            // A window whose owner is wedged must not wedge the picker with it.
            // A timeout *is* the expected answer for such a window, and the
            // `result != 0` below is what decides whether an icon came back --
            // so the return value has nothing to add (task260).
            let _ = SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(which as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                200,
                Some(&mut result),
            );
            if result != 0 {
                handle = result;
                break;
            }
        }
        for class in [GCLP_HICONSM, GCLP_HICON] {
            if handle != 0 {
                break;
            }
            handle = GetClassLongPtrW(hwnd, class);
        }
        if handle == 0 {
            return None;
        }
        icon_rgba(HICON(handle as *mut _))
    }
}

/// An `HICON`'s colour plane as RGBA.
///
// ponytail: the 1bpp mask is ignored. A colour bitmap with no alpha at all is
// treated as fully opaque, which is right for the flat 32bpp icons every app
// has shipped for two decades; a genuinely monochrome icon would come back as
// a square, and none of the windows in the picker have one.
pub(super) unsafe fn icon_rgba(
    icon: windows::Win32::UI::WindowsAndMessaging::HICON,
) -> Option<super::Pixels> {
    use windows::Win32::Graphics::Gdi::{DeleteObject, HGDIOBJ};
    use windows::Win32::UI::WindowsAndMessaging::{GetIconInfo, ICONINFO};

    let mut info = ICONINFO::default();
    GetIconInfo(icon, &mut info).ok()?;
    // The read happens between the two, so the bitmaps are freed on every path
    // out of it -- `GetIconInfo` hands over copies and expects them back.
    let pixels = colour_plane_rgba(info.hbmColor);
    // A failure here is a leaked GDI bitmap, and this runs once per icon of
    // every listed window -- the one place in the picker where a silent failure
    // accumulates (task260).
    if !info.hbmColor.is_invalid() {
        if let Err(error) = DeleteObject(HGDIOBJ(info.hbmColor.0)).ok() {
            tracing::warn!(%error, "could not free an icon's colour bitmap");
        }
    }
    if !info.hbmMask.is_invalid() {
        if let Err(error) = DeleteObject(HGDIOBJ(info.hbmMask.0)).ok() {
            tracing::warn!(%error, "could not free an icon's mask bitmap");
        }
    }
    pixels
}

/// A 32-bit DIB section's pixels, as RGBA slint can take. Shared with the clip
/// list (task2980), whose `IShellItemImageFactory::GetImage` hands back exactly
/// the same kind of HBITMAP this icon path gets out of `ICONINFO`.
///
/// # Safety
/// `colour` must be a live GDI bitmap the caller still owns; this only reads it.
pub(super) unsafe fn colour_plane_rgba(
    colour: windows::Win32::Graphics::Gdi::HBITMAP,
) -> Option<super::Pixels> {
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
        DIB_RGB_COLORS,
    };

    if colour.is_invalid() {
        return None;
    }
    let mut bitmap = BITMAP::default();
    let read = GetObjectW(
        windows::Win32::Graphics::Gdi::HGDIOBJ(colour.0),
        std::mem::size_of::<BITMAP>() as i32,
        Some(&mut bitmap as *mut _ as *mut _),
    );
    if read == 0 || bitmap.bmWidth <= 0 || bitmap.bmHeight <= 0 {
        return None;
    }
    let (width, height) = (bitmap.bmWidth as u32, bitmap.bmHeight as u32);
    let mut header = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: bitmap.bmWidth,
            // Negative: top-down, so the rows come back in the order the image
            // is drawn in rather than upside down.
            biHeight: -bitmap.bmHeight,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut pixels = vec![0u8; (width as usize) * (height as usize) * 4];
    let screen = GetDC(None);
    let copied = GetDIBits(
        screen,
        colour,
        0,
        height,
        Some(pixels.as_mut_ptr().cast()),
        &mut header,
        DIB_RGB_COLORS,
    );
    ReleaseDC(None, screen);
    if copied == 0 {
        return None;
    }
    let mut has_alpha = false;
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        has_alpha |= pixel[3] != 0;
    }
    if !has_alpha {
        for pixel in pixels.chunks_exact_mut(4) {
            pixel[3] = 255;
        }
    }
    Some((width, height, pixels))
}

/// Validate → start → persist the executable for next time, off the event loop:
/// `start` does the encoder and ring-buffer setup synchronously and would
/// freeze the window for as long as that takes (task075 hit the same thing in
/// the WebView build).
pub(super) fn spawn_start_capture(
    controller: CaptureController,
    settings: Arc<Mutex<AppSettings>>,
    tx: Sender<Msg>,
    cmd_tx: Sender<Cmd>,
    target: CaptureTarget,
    executable_id: Option<String>,
) {
    spawn_start_capture_with(
        controller,
        settings,
        (tx, cmd_tx),
        target,
        executable_id,
        None,
    );
}

/// The MCP's start (t261103-a803): the same start, answered on `reply` with
/// the new session id or the failure text, and without `Cmd::LoadStarted` --
/// a recording started from outside leaves the review screen where it is.
pub(super) fn spawn_start_capture_with(
    controller: CaptureController,
    settings: Arc<Mutex<AppSettings>>,
    (tx, cmd_tx): (Sender<Msg>, Sender<Cmd>),
    target: CaptureTarget,
    executable_id: Option<String>,
    reply: Option<std::sync::mpsc::Sender<Result<String, String>>>,
) {
    thread::spawn(move || {
        let fail = |text: String| {
            if let Some(reply) = &reply {
                let _ = reply.send(Err(text.clone()));
            }
            let _ = tx.send(Msg::Picker(PickerMsg::StartFailed(text)));
        };
        // The two failures with nothing of their own to say keep the generic
        // wording; only `start` produces a message worth showing (task157).
        //
        // `None` is a screen: there is no process to have restarted under the
        // handle, so there is nothing here to revalidate (task165).
        if executable_id.as_deref().is_some_and(|executable_id| {
            !capture_targets::is_capture_target_valid(
                &target.window_handle,
                target.process_id,
                executable_id,
            )
        }) {
            tracing::warn!(
                event = "capture_start_failed",
                reason = "target_invalid",
                window = %target.window_handle,
                "capture target failed revalidation"
            );
            fail(ui_targets::target_closed(tr_locale()).to_owned());
            return;
        }
        // Read ahead of the settings lock: it opens the process.
        let image_path = capture_targets::executable_path_for_process(target.process_id)
            .map(std::path::PathBuf::from);
        let (config, armed) = {
            let Ok(current) = settings.lock() else {
                tracing::warn!(
                    event = "capture_start_failed",
                    reason = "settings_lock_poisoned",
                    "could not read settings to build the capture config"
                );
                fail(ui_targets::start_internal(tr_locale()).to_owned());
                return;
            };
            let config = CaptureConfig {
                window_handle: target.window_handle.clone(),
                process_id: target.process_id,
                kind: target.kind,
                frame_rate: current.frame_rate,
                include_cursor: current.include_cursor,
                output_size: CaptureSize {
                    width: 0,
                    height: 0,
                },
                // The switch decides only whether the head is dropped
                // (task1410): off records just the same, it simply never
                // reaches a retention window. Read once, here, so a flip
                // mid-recording leaves the running session alone.
                retention_minutes: if current.ring_buffer_enabled {
                    current.retention_minutes
                } else {
                    livia::ring_buffer::NO_RETENTION_LIMIT
                },
                // Serde's `default` never runs for a config built in Rust.
                encoder_output_dir: default_encoder_output_dir(),
                // Read once, here, like the retention switch above: flipping
                // the toggle mid-recording leaves the running session on the
                // scope it started with (task1430).
                capture_all_audio: current.capture_all_audio,
                // Same "read once, here" rule (task1760): switching codecs
                // mid-recording would mean two codecs in one container.
                codec: current.codec,
                // The target's added sounds (t261002-577c). A change made
                // while it records reaches it through `apply_target_audio`.
                extra_audio: livia::capture::audio::planned_extras(
                    livia::settings::target_audio_key(
                        executable_id.as_deref(),
                        target.kind == livia::capture::targets::CaptureTargetKind::Monitor,
                    )
                    .and_then(|key| current.target_audio.get(&key)),
                    current.capture_all_audio,
                    target.kind == livia::capture::targets::CaptureTargetKind::Monitor,
                ),
                // The tile's own label, recorded as the session's title: the
                // engine words no screen names (t261101-5ff3).
                monitor_title: (target.kind == livia::capture::targets::CaptureTargetKind::Monitor)
                    .then(|| target.title.clone()),
            };
            // Armed (t260929-ea5e, ruling C): the replay buffer is on, or an
            // auto-capture rule covers this executable -- the same answer the
            // picker's badge gives. An armed target's WGC session outlives
            // the recording, so recording it again leaves no GPU memory behind.
            let armed = current.ring_buffer_enabled
                || executable_id.as_deref().is_some_and(|executable| {
                    auto_capture::covered_by(
                        &current.auto_capture_executables,
                        &current.auto_capture_folders,
                        auto_capture::FolderGuard::machine(),
                        executable,
                        image_path.as_deref(),
                    )
                    .is_some()
                });
            (config, armed)
        };
        // `start`, not `switch` (task2030 判断1). The tile used to *replace*
        // whatever was recording (task166); the concept of replacing is gone --
        // a click adds one more capture and leaves the others running, and the
        // way to stop one is to click its own tile.
        if let Err(error) = controller.start_armed(config, armed) {
            // The log keeps the engine's own words; the toast says what the
            // user can do about it (t260928-08df).
            tracing::warn!(
                event = "capture_start_failed",
                reason = "controller_start",
                %error,
                window = %target.window_handle,
                "capture start refused"
            );
            fail(ui_targets::start_failure_text(tr_locale(), &error));
            return;
        }
        // `start_armed` put the capture in the running set before it returned.
        if let Some(reply) = &reply {
            let session = controller
                .active_targets()
                .into_iter()
                .rev()
                .find(|(_, handle)| handle.eq_ignore_ascii_case(&target.window_handle))
                .map(|(session, _)| session)
                .ok_or_else(|| "the recording stopped as soon as it started".to_owned());
            let _ = reply.send(session);
        }
        // A screen has no executable to remember, and having nothing to save is
        // not a failed save (task165).
        let settings_saved = match executable_id {
            Some(executable_id) => save_last_executable(&settings, executable_id),
            None => true,
        };
        let _ = tx.send(Msg::Picker(PickerMsg::Started {
            id: target.id,
            settings_saved,
        }));
        // Picking a tile is a request to watch what it records (round5 §5: the
        // click starts the capture *and* closes the modal, mock 03 -> 04). The
        // load is what carries both: `Msg::Loaded` switches to 確認 and closes
        // the picker, the same landing the history's live row gives.
        if reply.is_none() {
            let _ = cmd_tx.send(Cmd::LoadStarted);
        }
        // The history list is command-driven, not polled, so a start that
        // nobody announces leaves an open history pane showing the state from
        // before it (task179). Every stop door does the same thing.
        let _ = cmd_tx.send(Cmd::ListSessions);
    });
}

fn save_last_executable(settings: &Mutex<AppSettings>, executable_id: String) -> bool {
    let set = |current: &mut AppSettings| current.last_capture_executable = Some(executable_id);
    super::settings_page::commit_settings_checked(settings, set).1
}

/// A registered executable's launch, arriving on the UI thread (task1970).
///
/// Deliberately the same route a tile click takes rather than a second start
/// beside it: the revalidation, the settings read and the failure banner are
/// all in `spawn_start_capture`, and a parallel path would drift from it.
///
/// Returns the executable as it is spelled on disk once a start was actually
/// asked for -- that is what the toast names, and `None` means nothing was
/// started, so nothing should be announced.
pub(super) fn start_auto_capture(
    picker: &mut Picker,
    starting: &AtomicBool,
    controller: &CaptureController,
    settings: &Arc<Mutex<AppSettings>>,
    (tx, cmd_tx): (&Sender<Msg>, &Sender<Cmd>),
    id: &str,
    // The recording itself lands later as `PickerMsg::Started` /
    // `PickerMsg::StartFailed`. Nothing on the grid moves here: the banner a start
    // used to clear is a toast since t260927-711d.
) -> Option<String> {
    // The worker read the same list a moment ago, but the tile can have gone
    // between then and now -- and `can_start` is also what says "this is
    // already the one recording".
    let Some(target) = picker.state.find(id).filter(|_| picker.state.can_start(id)) else {
        tracing::info!(
            target: "task1970_auto_capture",
            event = "auto_capture_skipped",
            %id,
            reason = "not_startable",
            "auto-capture target is gone or already recording"
        );
        return None;
    };
    let target = target.clone();
    // Auto capture always starts a window (2026-10-03: the monitor swap of
    // task2570 is gone). One without an executable id cannot be revalidated,
    // so the start is refused.
    let executable_id = target.executable_id.clone()?;
    let executable_name = target.executable_name.clone();
    // The same guard the click uses. A start that takes longer than the poll
    // interval would otherwise be asked for twice: the watch stays armed until
    // the recording is actually running, which is exactly what makes a refused
    // start retry on its own.
    if starting.swap(true, Ordering::AcqRel) {
        tracing::info!(
            target: "task1970_auto_capture",
            event = "auto_capture_skipped",
            %id,
            reason = "start_in_flight",
            "a start is already on its way"
        );
        return None;
    }
    spawn_start_capture(
        controller.clone(),
        settings.clone(),
        tx.clone(),
        cmd_tx.clone(),
        target,
        Some(executable_id),
    );
    executable_name
}

/// The badge's folder half (task3670), resolved off the list rather than in
/// `target_tile` -- see `Picker::image_paths`. Rebuilt rather than pruned: a
/// process id is reused by Windows, so a stale entry would name the wrong
/// image. Left empty whenever there is no folder rule to read the answer,
/// which is what keeps an install that registers none from opening a single
/// process (task3600's gate).
fn rebuild_image_paths(
    image_paths: &mut HashMap<u32, std::path::PathBuf>,
    folders: &[settings::AutoCaptureFolder],
    targets: &[CaptureTarget],
) {
    image_paths.clear();
    if folders.is_empty() {
        return;
    }
    for target in targets {
        if target.executable_id.is_none() || image_paths.contains_key(&target.process_id) {
            continue;
        }
        if let Some(path) = capture_targets::executable_path_for_process(target.process_id) {
            image_paths.insert(target.process_id, std::path::PathBuf::from(path));
        }
    }
}

/// Applies one message to the picker and answers **whether it moved anything
/// the grid draws** (task a015). The drain raises `flags.targets` off this
/// return rather than off a message having arrived: the 1s poll re-sends the
/// same list forever, and a re-render that changes no pixel still costs a
/// `present` -- task4290 measured that remainder and could not fix it inside
/// its own scope.
pub(super) fn apply(picker: &mut Picker, starting: &AtomicBool, message: PickerMsg) -> bool {
    let visible = PICKER_VISIBLE.load(Ordering::Relaxed);
    if !visible {
        picker.counts = ApplyCounts::default();
    }
    match message {
        PickerMsg::Targets(Ok(targets)) => {
            let live: std::collections::HashSet<&str> =
                targets.iter().map(|target| target.id.as_str()).collect();
            // A window that closed loses its cached frame along with its tile.
            picker.thumbnails.retain(|id, _| live.contains(id.as_str()));
            // The badge's folder half, off the rules the registration message
            // left behind rather than off a lock taken here (task t260911-66da).
            rebuild_image_paths(
                &mut picker.image_paths,
                &picker.auto_capture_folders,
                &targets,
            );
            // `refresh_targets`, not `set_targets`: while the modal is open the
            // grid must not reshuffle under the cursor, and while it is closed
            // there is nobody to reshuffle it for. The one sort runs on open.
            let last_executable = picker.last_capture_executable.clone();
            let change = picker
                .state
                .refresh_targets(targets, last_executable.as_deref());
            // The first list after an open, while still inside the window:
            // the open's own sort may have run on a stale list.
            let resorted = picker
                .opened_at
                .take()
                .is_some_and(|at| visible && at.elapsed() < RESORT_WINDOW);
            if resorted {
                picker.state.sort_for_open(last_executable.as_deref());
            }
            // A good list answers a list failure.
            let alert_changed = picker.alert.listed_ok();
            let changed = change.any() || alert_changed || resorted;
            if visible {
                picker.counts.targets += 1;
                if changed {
                    picker.counts.targets_changed += 1;
                    tracing::info!(
                        target: "task_a015_drain_flags",
                        event = "picker_targets_changed",
                        listed = change.listed,
                        selection = change.selection,
                        alert = alert_changed,
                        first_diff = change.first_diff.as_deref().unwrap_or(""),
                        "a list message moved something the grid draws"
                    );
                }
            }
            changed
        }
        PickerMsg::Targets(Err(reason)) => {
            // Both run: the failure is recorded whether or not the list it is
            // replacing was already empty, and its reason is what the flyout's
            // error state says (t260927-711d).
            let cleared = picker.state.set_targets(Vec::new(), None).any();
            let alerted = picker.alert.list_failed(reason);
            let changed = cleared || alerted;
            if visible {
                picker.counts.targets += 1;
                if changed {
                    picker.counts.targets_changed += 1;
                }
            }
            changed
        }
        PickerMsg::PickerRegistrations {
            last_capture_executable,
            apps,
            folders,
        } => {
            let apps_changed = picker.auto_capture != apps;
            let folders_changed = picker.auto_capture_folders != folders;
            let executable_changed = picker.last_capture_executable != last_capture_executable;
            let changed = apps_changed || folders_changed || executable_changed;
            let had_folders = !picker.auto_capture_folders.is_empty();
            picker.auto_capture = apps;
            picker.auto_capture_folders = folders;
            picker.last_capture_executable = last_capture_executable;
            // The folder half of the badge has to be resolved again when the
            // rules move, not only when the list does: with the list sent only
            // on a change, a rule added while nothing opens or closes would
            // otherwise never reach a tile -- and following a settings-screen
            // edit is the one feature the throttle could break. Skipped
            // outright while the rules were empty and still are, which is the
            // gate task3600 put on this: an install registering no folder
            // opens no process.
            if had_folders || !picker.auto_capture_folders.is_empty() {
                rebuild_image_paths(
                    &mut picker.image_paths,
                    &picker.auto_capture_folders,
                    picker.state.all(),
                );
            }
            // The preselect the list poll used to re-run once a second.
            let selection = picker
                .state
                .reselect(picker.last_capture_executable.as_deref());
            // Unlike the 1s list/capturing poll above, a registration only moves
            // when the user actually edits a rule -- there is no heartbeat to
            // throttle here -- so this logs unconditionally, PICKER_VISIBLE or
            // not (task a015's own gate would otherwise make a settings-screen
            // deletion invisible to instrumentation whenever the picker itself
            // is closed).
            if changed || selection {
                tracing::info!(
                    target: "task_a015_drain_flags",
                    event = "registration_changed",
                    apps = apps_changed,
                    folders = folders_changed,
                    last_capture_executable = executable_changed,
                    selection,
                    "a registration message moved something the grid draws"
                );
            }
            changed || selection
        }
        PickerMsg::Thumbnail(id, pixels) => {
            let image = pixels.and_then(pixels_image);
            picker.thumbnails.insert(id, image);
            // Unconditionally a change, and that is correct: task4290 put
            // `ThumbnailDigests` on the *sender*, so a frame identical to the
            // last one is never sent. Arrival is the difference.
            if visible {
                picker.counts.thumbnails += 1;
            }
            true
        }
        PickerMsg::AppIcon(executable, pixels) => {
            let image = pixels.and_then(pixels_image);
            picker.icons.insert(executable, image);
            // Asked for once per executable (`icons_asked`), so an icon only
            // ever arrives for a badge that has none yet.
            if visible {
                picker.counts.app_icons += 1;
            }
            true
        }
        PickerMsg::Capturing { running, .. } => {
            // The second message on the 1s poll, and it reached the picker
            // pane unconditionally too until task a015.
            let changed = picker.state.set_running(running);
            if visible {
                picker.counts.capturing += 1;
                if changed {
                    picker.counts.capturing_changed += 1;
                }
                // The 1s heartbeat since task t260911-66da: the list arm only
                // runs when the list moved, so a still desk would otherwise
                // never close a summary window -- and 「送出が0件」 is exactly
                // what the summary is there to report.
                picker.report_counts();
            }
            changed
        }
        PickerMsg::StartFailed(_) => {
            // Said in a toast by `announce_start`: the flyout has closed on
            // the press, so nothing on the grid moves.
            starting.store(false, Ordering::Release);
            false
        }
        PickerMsg::Started { id, .. } => {
            starting.store(false, Ordering::Release);
            picker.state.select(&id);
            picker.state.set_capturing_target(&id);
            // The selection and the optimistic recording tile both moved.
            true
        }
    }
}

/// Wires the picker page's callbacks. Everything below moved verbatim out
/// of `main` (the blocks only borrow what they used to capture from it).
pub(super) fn wire(
    ui: &AppWindow,
    picker: &Rc<RefCell<Picker>>,
    tiles: &Rc<VecModel<TargetTile>>,
    starting: &Rc<AtomicBool>,
    controller: &CaptureController,
    settings: &Arc<Mutex<AppSettings>>,
    // Bundled with the channels rather than an eighth parameter: it is the same
    // kind of thing -- a handle the callbacks report through. The review pane
    // rides along for the stop's reclaim, which must spare what it has loaded
    // (task3970).
    (tx, cmd_tx, stop_requests, review): (
        &Sender<Msg>,
        &Sender<Cmd>,
        &super::StopRequests,
        &Rc<RefCell<super::Review>>,
    ),
) {
    {
        let weak = ui.as_weak();
        let picker = picker.clone();
        let tiles = tiles.clone();
        ui.on_targets_search_changed(move |query| {
            let Some(ui) = weak.upgrade() else { return };
            let mut picker = picker.borrow_mut();
            picker.state.set_search(&query);
            render(&ui, &picker, &tiles);
        });
    }

    {
        let weak = ui.as_weak();
        let picker = picker.clone();
        let tiles = tiles.clone();
        let settings = settings.clone();
        // The modal's `init`, which slint runs once per open because the dialog
        // only exists while it is open. This is the single moment the grid is
        // allowed to reorder itself (design §5). The search starts empty too:
        // the field remounts blank, so a query kept here would filter the grid
        // with nothing on screen saying so (the user, 2026-09-28).
        ui.on_picker_opened(move || {
            let Some(ui) = weak.upgrade() else { return };
            let last_executable = settings
                .lock()
                .ok()
                .and_then(|current| current.last_capture_executable.clone());
            let mut picker = picker.borrow_mut();
            picker.state.set_search("");
            picker.state.sort_for_open(last_executable.as_deref());
            picker.opened_at = Some(Instant::now());
            render(&ui, &picker, &tiles);
        });
    }

    {
        let weak = ui.as_weak();
        let picker = picker.clone();
        let tiles = tiles.clone();
        ui.on_picker_tab_changed(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let mut picker = picker.borrow_mut();
            picker.state.set_tab(PickerTab::from_index(index));
            ui.set_picker_tab(picker.state.tab().index());
            render(&ui, &picker, &tiles);
        });
    }

    wire_target_actions(
        ui,
        picker,
        starting,
        controller,
        settings,
        (tx, cmd_tx, stop_requests, review),
    );
}

/// The tile's own actions: start, stop and the autorec badge.
fn wire_target_actions(
    ui: &AppWindow,
    picker: &Rc<RefCell<Picker>>,
    starting: &Rc<AtomicBool>,
    controller: &CaptureController,
    settings: &Arc<Mutex<AppSettings>>,
    // Bundled with the channels rather than an eighth parameter: it is the same
    // kind of thing -- a handle the callbacks report through. The review pane
    // rides along for the stop's reclaim, which must spare what it has loaded
    // (task3970).
    (tx, cmd_tx, stop_requests, review): (
        &Sender<Msg>,
        &Sender<Cmd>,
        &super::StopRequests,
        &Rc<RefCell<super::Review>>,
    ),
) {
    {
        let picker = picker.clone();
        let starting = starting.clone();
        let controller = controller.clone();
        let settings = settings.clone();
        let tx = tx.clone();
        let cmd_tx = cmd_tx.clone();
        let stop_requests = stop_requests.clone();
        let review = review.clone();
        ui.on_target_clicked(move |id| {
            let id = id.to_string();
            let picker = picker.borrow();
            tracing::info!(event = "tile_clicked", %id, "picker tile clicked");
            // The tile is a toggle (task2030 判断1): the one that *is* a
            // recording stops it, with no confirmation (判断4) -- clicking it
            // again starts a new one, so a mis-click costs a click.
            if let Some(session_id) = picker.state.recording_session(&id).map(str::to_owned) {
                super::shell::stop_one(
                    &controller,
                    &stop_requests,
                    super::shell::StopWhich::One {
                        session_id: &session_id,
                        cmd_tx: &cmd_tx,
                        loaded: super::review::loaded_session_id(&review.borrow()),
                    },
                    "picker_tile",
                );
                return;
            }
            if !picker.state.can_start(&id) {
                return;
            }
            let Some(target) = picker.state.find(&id).cloned() else {
                return;
            };
            // A screen has no process behind it and so no executable id; only a
            // window is dropped here for missing one (task165).
            let executable_id = target.executable_id.clone();
            if executable_id.is_none() && TargetsState::needs_executable(target.kind) {
                return;
            }
            // Guards a second click landing before the first start resolves,
            // the same job `startingCaptureRef` did in useAppState.
            if starting.swap(true, Ordering::AcqRel) {
                return;
            }
            spawn_start_capture(
                controller.clone(),
                settings.clone(),
                tx.clone(),
                cmd_tx.clone(),
                target,
                executable_id,
            );
        });
    }

    {
        let weak = ui.as_weak();
        let picker = picker.clone();
        let controller = controller.clone();
        ui.on_titlebar_tally_tick(move || {
            let Some(ui) = weak.upgrade() else { return };
            refresh_tally_detail(&ui, &picker.borrow(), &controller);
        });
    }
}
