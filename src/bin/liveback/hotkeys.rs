//! The global hotkey and the action behind it (task130), ported from the
//! `tauri_plugin_global_shortcut` handler in `lib.rs`.
//!
//! Registration state is [`livia::ui_state::hotkeys`]; the platform
//! registrar is [`super::desktop::HotkeyRegistrar`]. What is left here is the
//! glue: polling the event channel from the UI thread and doing the work.
//!
//! There were two: this one and a clip save. Task1090 removed clip saving, and
//! trimmed this one down to what its name says -- it adds a marker, and no
//! longer raises the window or loads the live session on the way past.
//!
//! t261025-3669 brought a second one back: the record toggle, unset by
//! default. It starts or stops recording the window in front.
//!
//! t261025-034f added a third, also unset by default: it raises the window
//! and opens the recording on review.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use livia::capture::targets::{self as capture_targets, CaptureTargetKind};
use livia::capture::CaptureController;
use livia::settings::AppSettings;
use livia::ui_state::lifecycle::NoticeSinks;
use livia::ui_state::playback as pb;
use livia::ui_state::settings as settings_ui;
use livia::ui_state::timeline as tl;
use livia::ui_state::toast;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

pub(super) use livia::ui_state::hotkeys::Role;

use super::desktop::{self, HotkeyRegistrar};
use super::{publish_toast, tr_locale, AppWindow, Cmd, Msg, StopRequests};

/// Registration state plus the registrar it was produced with. A machine where
/// `GlobalHotKeyManager` itself refuses to start has no registrar at all, which
/// the settings screen reports the same way it reports a taken accelerator.
pub(super) struct Hotkeys {
    registrar: Option<HotkeyRegistrar>,
    state: livia::ui_state::hotkeys::Hotkeys,
}

impl Hotkeys {
    pub(super) fn new() -> Self {
        Self {
            registrar: HotkeyRegistrar::new(),
            state: Default::default(),
        }
    }

    /// An empty `accelerator` unsets the role. That needs no registrar, so a
    /// machine without one still reports an unset record row as fine.
    pub(super) fn apply(&mut self, role: Role, accelerator: &str) -> Result<(), String> {
        let Some(registrar) = self.registrar.as_ref() else {
            if accelerator.is_empty() {
                return Ok(());
            }
            return Err(settings_ui::hotkeys_unavailable(super::tr_locale()).into());
        };
        self.state.apply(registrar, role, accelerator)
    }

    /// Gives the chord back to the OS while a settings field is armed, so the
    /// field can capture a chord this app currently holds (task160). Nothing to
    /// give back on a machine with no registrar.
    pub(super) fn suspend(&mut self) {
        if let Some(registrar) = self.registrar.as_ref() {
            self.state.suspend(registrar);
        }
    }

    /// Takes it back once the field disarms. Reported like `apply`: the
    /// settings screen turns a failure into `HOTKEY_REGISTER_ERROR` rather than
    /// leaving the user with a hotkey that silently stopped working. The error
    /// carries the role that failed; a machine with no registrar reports the
    /// marker, as it always has.
    pub(super) fn resume(&mut self) -> Result<(), (Role, String)> {
        let Some(registrar) = self.registrar.as_ref() else {
            return Err((
                Role::Marker,
                settings_ui::hotkeys_unavailable(super::tr_locale()).into(),
            ));
        };
        self.state.resume(registrar)
    }

    /// Whether the chord is dead (task730): a machine with no registrar counts.
    /// `set` is whether the role has a chord at all -- an unset record row is
    /// not dead, there is just nothing on it.
    pub(super) fn unavailable(&self, role: Role, set: bool) -> bool {
        set && (self.registrar.is_none() || self.state.unavailable(role))
    }

    fn role_of(&self, id: u32) -> Option<Role> {
        self.state.role_of(id)
    }
}

/// What the record toggle needs from the app to start or stop a capture: the
/// same handles the picker tile's click uses (t261025-3669).
pub(super) struct RecordToggle<'a> {
    /// The picker's start-in-flight guard, so a press landing before a start
    /// resolves does not start a second one.
    pub(super) starting: &'a AtomicBool,
    pub(super) settings: &'a Arc<Mutex<AppSettings>>,
    pub(super) tx: &'a Sender<Msg>,
    pub(super) cmd_tx: &'a Sender<Cmd>,
    pub(super) stop_requests: &'a StopRequests,
    /// The session the review pane has loaded, which the stop's reclaim must
    /// spare (task3970) -- read by the caller, which holds `review` borrowed.
    pub(super) loaded: Option<String>,
}

/// Drains the hotkey channel, called from the UI thread's drain tick.
///
/// `capturing_chord` is the settings screen's armed state: pressing the chord
/// that is *currently registered* while re-binding it would otherwise fire the
/// old binding -- pressing Ctrl+Shift+R to rebind it would add a marker on the
/// way past (task145). Events are still drained, just not acted on, so nothing
/// backs up in the channel to fire later.
///
/// Task160 unregisters the chords for the duration of the arm, so in principle
/// nothing can arrive any more. The suppression stays as the second line of
/// defence: an event queued in the channel a moment *before* the unregister is
/// still waiting to be read here.
///
/// `ui` / `status_line` / `hwnd` are here for the acknowledgement, which goes
/// to one surface (t260928-e309 F8, `lifecycle::notice_sinks`): the corner
/// when the window is in front, Windows when it is behind. task4150 had raised
/// the corner toast both ways so a press behind a game left an in-app trace;
/// the user's ruling is that the same notice is not said in two places, so
/// behind a game the Windows toast is the whole acknowledgement -- and an OS
/// toast under an unregistered AUMID (a build that never went through the
/// installer) is dropped by Windows without an error (`desktop::toast`).
///
/// `displayed` is the session the review pane is showing, or `None` when it has
/// nothing loaded (task2190). With two captures running the press has to land
/// where the user is looking -- the same session the panel's ＋ button marks --
/// rather than on whichever started last. `None` keeps the last-started
/// resolution so the tray-resident path, where the window never opens, still
/// marks the recording.
///
/// It is values rather than a Slint call on purpose: the caller holds `review`
/// mutably borrowed across this, so invoking `ReviewVm::add_marker` from here
/// would panic the handler's own `borrow_mut`.
///
/// t260921-6ba9: `displayed` also carries the playhead and the LIVE badge, and
/// a press made while the user is watching a rewound review pane lands on the
/// playhead (`pb::hotkey_mark_target`) -- through `add_marker`, like the ＋
/// button, so a stopped session takes it too. Returns the instants placed that
/// way: the caller pushes them into `review.markers`, since a stopped session
/// has no manifest refresh that would bring them back.
///
/// t261025-3669: `record` is what the record chord's press needs; see
/// [`toggle_recording`].
#[allow(clippy::too_many_arguments)]
pub(super) fn drain(
    hotkeys: &RefCell<Hotkeys>,
    controller: &CaptureController,
    capturing_chord: bool,
    displayed: Option<DisplayedReview<'_>>,
    record: RecordToggle<'_>,
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    hwnd: Option<HWND>,
) -> Vec<i64> {
    let mut placed = Vec::new();
    let foreground = hwnd.is_some_and(desktop::is_foreground);
    // Which surface a press is handed to, decided once beside `foreground`
    // (t260928-e309 F8, `lifecycle::notice_sinks`): the corner in front,
    // Windows behind, never both. It names the call this function *makes*, not
    // what the user ends up seeing: an OS toast can be dropped by Windows (see
    // `desktop::toast`). Anything stronger would be a claim this code cannot
    // check.
    let sinks = livia::ui_state::lifecycle::notice_sinks(foreground);
    let surface = if sinks.corner { "in_app" } else { "os_toast" };
    while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
        // Both edges arrive; acting on release too would double every press.
        if event.state != HotKeyState::Pressed || capturing_chord {
            continue;
        }
        let Some(role) = hotkeys.borrow().role_of(event.id) else {
            continue;
        };
        if role == Role::Record {
            toggle_recording(&record, controller, ui, status_line, sinks, surface);
            continue;
        }
        if role == Role::Show {
            show_and_review(controller, record.cmd_tx, hwnd);
            continue;
        }
        // What the key is named for, and now the whole of it (task1090). It
        // used to raise the window and load the live session too, which made
        // "mark this moment" cost the user their foreground app.
        // The displayed session when there is one, even if it turns out not to
        // be recording -- falling back to last-started there would mark a
        // session the user is not looking at, which is the bug this fixes. A
        // stopped one recording nothing simply refuses, as before -- unless
        // the user is watching it, which takes the playhead below.
        let rule = displayed.map_or(pb::HotkeyMarkTarget::LiveEdge, |shown| {
            pb::hotkey_mark_target(
                foreground,
                shown.on_review_pane,
                shown.loaded,
                shown.live_badge_visible,
            )
        });
        if let (pb::HotkeyMarkTarget::Playhead, Some(shown)) = (rule, displayed) {
            let timestamp_100ns = shown.position_100ns;
            match controller.add_marker(shown.session_id, timestamp_100ns) {
                Ok(()) => {
                    placed.push(timestamp_100ns);
                    tracing::info!(
                        event = "marker_hotkey_pressed",
                        timestamp_100ns,
                        session_id = shown.session_id,
                        target = "displayed",
                        rule = rule.name(),
                        foreground,
                        surface,
                        "Marker hotkey added a marker"
                    );
                    announce_added(
                        ui,
                        status_line,
                        sinks,
                        Some(timestamp_100ns - shown.start_100ns),
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        event = "marker_hotkey_refused",
                        reason = "add_failed",
                        rule = rule.name(),
                        error = %error,
                        foreground,
                        surface,
                        "Marker hotkey refused"
                    );
                    notify(
                        ui,
                        status_line,
                        sinks,
                        settings_ui::marker_hotkey_refused(tr_locale()),
                        "",
                        toast::ToastVariant::Warning,
                    );
                }
            }
            continue;
        }
        let target = match displayed {
            Some(shown) => controller
                .recording_position_100ns_for(shown.session_id)
                .map(|time| (Some(shown.session_id.to_owned()), time)),
            None => controller
                .recording_position_100ns()
                .map(|time| (None, time)),
        };
        let Some((target_id, timestamp_100ns)) = target else {
            // Runs with the window untouched, so the notification is the only
            // trace a refused press leaves -- for the user and, with the log
            // line below, for us.
            tracing::warn!(
                event = "marker_hotkey_refused",
                reason = "not_recording",
                foreground,
                surface,
                "Marker hotkey refused"
            );
            // `Warning`, like the retention sweep: nothing failed, but it
            // is not the result they asked for and has to keep until they have
            // read it.
            notify(
                ui,
                status_line,
                sinks,
                settings_ui::marker_hotkey_refused(tr_locale()),
                settings_ui::marker_hotkey_not_recording(tr_locale()),
                toast::ToastVariant::Warning,
            );
            continue;
        };
        match &target_id {
            Some(id) => controller.push_marker_for(id, timestamp_100ns),
            None => controller.push_marker(timestamp_100ns),
        }
        // Which session took it, and by which of the two rules -- the only
        // trace of the choice, and the evidence task2190's verification reads.
        // On the fallback path the id is read back rather than returned:
        // `active_session_ids` is oldest-first, so the last one is the one
        // `push_marker` resolved to.
        tracing::info!(
            event = "marker_hotkey_pressed",
            timestamp_100ns,
            session_id = target_id
                .clone()
                .or_else(|| controller.active_session_ids().pop())
                .unwrap_or_default(),
            target = if target_id.is_some() {
                "displayed"
            } else {
                "fallback_last_started"
            },
            rule = rule.name(),
            foreground,
            surface,
            "Marker hotkey added a marker"
        );
        // Relative to the displayed session's start, like the timeline. The
        // fallback has no snapshot to take a start from, so it names nothing
        // rather than an absolute instant (`21:53:21`) no screen shows.
        let elapsed = displayed.map(|shown| timestamp_100ns - shown.start_100ns);
        announce_added(ui, status_line, sinks, elapsed);
    }
    placed
}

/// The record chord's press (t261025-3669, the user's rulings of 2026-10-24):
/// the window in front at the moment of the press is the target. Already
/// recording → stop that one recording, the picker tile's toggle; any other
/// recording is left alone. Not recording → start it through the same
/// `spawn_start_capture` the tile and auto-record use. Liveback's own windows,
/// the desktop and anything the picker would not list are refused.
///
/// `record_hotkey_pressed` is written before any branch, like
/// `marker_hotkey_pressed`: the line alone proves the chord reached us.
fn toggle_recording(
    record: &RecordToggle<'_>,
    controller: &CaptureController,
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    sinks: NoticeSinks,
    surface: &str,
) {
    let front = unsafe { GetForegroundWindow() };
    let mut process_id = 0u32;
    if !front.is_invalid() {
        unsafe { GetWindowThreadProcessId(front, Some(&mut process_id)) };
    }
    // Spelled the way `list_capture_targets` and `active_targets` spell it.
    let handle = format!("0x{:X}", front.0 as usize);
    tracing::info!(
        event = "record_hotkey_pressed",
        window = %handle,
        process_id,
        surface,
        "Record hotkey pressed"
    );
    let refuse = |reason: &str, body: &str| {
        tracing::warn!(
            event = "record_hotkey_refused",
            reason,
            window = %handle,
            process_id,
            surface,
            "Record hotkey refused"
        );
        notify(
            ui,
            status_line,
            sinks,
            settings_ui::record_hotkey_refused(tr_locale()),
            body,
            toast::ToastVariant::Warning,
        );
    };
    if front.is_invalid() {
        refuse(
            "no_foreground",
            settings_ui::record_hotkey_refused_target(tr_locale()),
        );
        return;
    }
    // Every window this process owns, not only the main one: recording
    // Liveback is the picker's job.
    if process_id == std::process::id() {
        refuse("self", settings_ui::record_hotkey_refused_self(tr_locale()));
        return;
    }
    if let Some((session_id, _)) = controller
        .active_targets()
        .into_iter()
        .find(|(_, target)| target.eq_ignore_ascii_case(&handle))
    {
        super::shell::stop_one(
            controller,
            record.stop_requests,
            super::shell::StopWhich::One {
                session_id: &session_id,
                cmd_tx: record.cmd_tx,
                loaded: record.loaded.clone(),
            },
            "record_hotkey",
        );
        notify(
            ui,
            status_line,
            sinks,
            settings_ui::record_hotkey_stopped(tr_locale()),
            "",
            toast::ToastVariant::Success,
        );
        return;
    }
    let target = capture_targets::list_capture_targets()
        .ok()
        .and_then(|targets| {
            targets.into_iter().find(|target| {
                target.kind == CaptureTargetKind::Window
                    && target.window_handle.eq_ignore_ascii_case(&handle)
            })
        })
        .filter(|target| target.selectable);
    let Some(target) = target else {
        refuse(
            "not_capturable",
            settings_ui::record_hotkey_refused_target(tr_locale()),
        );
        return;
    };
    let Some(executable_id) = target.executable_id.clone() else {
        refuse(
            "no_executable_id",
            settings_ui::record_hotkey_refused_target(tr_locale()),
        );
        return;
    };
    // The picker's own guard. Said in the log only: a start is already on its
    // way, and its own toast follows.
    if record.starting.swap(true, Ordering::AcqRel) {
        tracing::info!(
            event = "record_hotkey_refused",
            reason = "start_in_flight",
            window = %handle,
            process_id,
            surface,
            "Record hotkey refused"
        );
        return;
    }
    tracing::info!(
        event = "record_hotkey_start",
        window = %handle,
        executable = target.executable_name.as_deref().unwrap_or_default(),
        "Record hotkey starts a capture"
    );
    let title = target.title.clone();
    super::picker::spawn_start_capture(
        controller.clone(),
        record.settings.clone(),
        record.tx.clone(),
        record.cmd_tx.clone(),
        target,
        Some(executable_id),
    );
    notify(
        ui,
        status_line,
        sinks,
        settings_ui::record_hotkey_starting(tr_locale()),
        &title,
        toast::ToastVariant::Success,
    );
}

/// The show-window chord's press (t261025-034f, the user's rulings of
/// 2026-10-25): raise the window, and when something is recording, open it on
/// review -- the recording of the window that was in front at the press, else
/// the one started last. Liveback already in front → nothing at all; there is
/// no toggle back to the previous app. The window in front is read *before*
/// `show_window`, which would make it Liveback.
///
/// `show_hotkey_pressed` is written before any branch, like the other two.
/// No toast: the raised window is the acknowledgement.
fn show_and_review(controller: &CaptureController, cmd_tx: &Sender<Cmd>, hwnd: Option<HWND>) {
    let front = unsafe { GetForegroundWindow() };
    let mut process_id = 0u32;
    if !front.is_invalid() {
        unsafe { GetWindowThreadProcessId(front, Some(&mut process_id)) };
    }
    let handle = format!("0x{:X}", front.0 as usize);
    tracing::info!(
        event = "show_hotkey_pressed",
        window = %handle,
        process_id,
        "Show hotkey pressed"
    );
    // Any window of this process, like the record chord's `reason=self`.
    if process_id == std::process::id() {
        tracing::info!(
            event = "show_hotkey_ignored",
            reason = "already_foreground",
            window = %handle,
            "Show hotkey ignored"
        );
        return;
    }
    let Some(hwnd) = hwnd else {
        tracing::warn!(
            event = "show_hotkey_ignored",
            reason = "no_window",
            "Show hotkey ignored"
        );
        return;
    };
    let session_id = controller
        .active_targets()
        .into_iter()
        .find(|(_, target)| target.eq_ignore_ascii_case(&handle))
        .map(|(session_id, _)| session_id);
    desktop::show_window(hwnd);
    let target = match &session_id {
        Some(_) => "foreground_window",
        None if controller.active_session_ids().is_empty() => "none",
        None => "last_started",
    };
    tracing::info!(
        event = "show_hotkey_shown",
        target,
        session_id = session_id.as_deref().unwrap_or_default(),
        "Show hotkey raised the window"
    );
    if target != "none" {
        let _ = cmd_tx.send(Cmd::LoadActive(session_id));
    }
}

/// One press's acknowledgement, to the one surface `sinks` names.
fn notify(
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    sinks: NoticeSinks,
    title: &str,
    body: &str,
    variant: toast::ToastVariant,
) {
    if sinks.corner {
        publish_toast(ui, status_line, title, body, variant, "");
    } else {
        desktop::toast(title, body);
    }
}

/// What the review pane is showing, copied out by the caller before `drain`
/// (it holds `review` mutably borrowed). `loaded` is false when the pane has
/// a snapshot but no engine to read the LIVE badge from.
#[derive(Clone, Copy)]
pub(super) struct DisplayedReview<'a> {
    pub(super) session_id: &'a str,
    pub(super) position_100ns: i64,
    /// `snapshot.start_100ns()`: what the timeline and marker list subtract.
    pub(super) start_100ns: i64,
    pub(super) on_review_pane: bool,
    pub(super) loaded: bool,
    pub(super) live_badge_visible: bool,
}

/// Two seconds, not `HOLD_MS` (t260928-983d, the user's call): a press is
/// often one of several in a row, and all it has to say is that it landed.
const MARKER_ADDED_HOLD_MS: u64 = 2_000;

/// Silence would leave the user unsure the press landed at all. In front it
/// expires after `MARKER_ADDED_HOLD_MS`; behind, Windows says it instead
/// (t260928-e309 F8). The marker on the review timeline is what proves this
/// press landed either way. The body names the instant
/// (t260921-6ba9): one key now means two places, so it says which -- as the
/// session-relative position the timeline shows, or nothing when there is no
/// session start to measure from.
fn announce_added(
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    sinks: NoticeSinks,
    elapsed_100ns: Option<i64>,
) {
    let body = elapsed_100ns
        .map(|elapsed| tl::format_position(elapsed, false))
        .unwrap_or_default();
    notify(
        ui,
        status_line,
        sinks,
        settings_ui::marker_hotkey_added(tr_locale()),
        &body,
        toast::ToastVariant::Success,
    );
    if sinks.corner {
        status_line.borrow_mut().set_hold_ms(MARKER_ADDED_HOLD_MS);
    }
}
