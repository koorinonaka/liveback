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

use std::cell::RefCell;
use std::rc::Rc;

use global_hotkey::{GlobalHotKeyEvent, HotKeyState};
use livia::capture::CaptureController;
use livia::ui_state::lifecycle::NoticeSinks;
use livia::ui_state::playback as pb;
use livia::ui_state::settings as settings_ui;
use livia::ui_state::timeline as tl;
use livia::ui_state::toast;
use windows::Win32::Foundation::HWND;

use super::desktop::{self, HotkeyRegistrar};
use super::{publish_toast, tr_locale, AppWindow};

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

    pub(super) fn apply(&mut self, marker: &str) -> Result<(), String> {
        let Some(registrar) = self.registrar.as_ref() else {
            return Err(settings_ui::hotkeys_unavailable(super::tr_locale()).into());
        };
        self.state.apply(registrar, marker)
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
    /// leaving the user with a hotkey that silently stopped working.
    pub(super) fn resume(&mut self) -> Result<(), String> {
        let Some(registrar) = self.registrar.as_ref() else {
            return Err(settings_ui::hotkeys_unavailable(super::tr_locale()).into());
        };
        self.state.resume(registrar)
    }

    /// Whether the chord is dead (task730): a machine with no registrar counts.
    pub(super) fn unavailable(&self) -> bool {
        self.registrar.is_none() || self.state.unavailable()
    }

    fn is_ours(&self, id: u32) -> bool {
        self.state.is_ours(id)
    }
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
pub(super) fn drain(
    hotkeys: &RefCell<Hotkeys>,
    controller: &CaptureController,
    capturing_chord: bool,
    displayed: Option<DisplayedReview<'_>>,
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
        if !hotkeys.borrow().is_ours(event.id) {
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
