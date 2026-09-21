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
use livia::ui_state::settings as settings_ui;
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
/// `ui` / `status_line` / `hwnd` are here for the same reason the lifecycle
/// toasts take them (task1550): Windows drops an OS toast raised by the
/// foreground app, which is exactly when the user is watching for the result of
/// their press. So the OS toast is still sent only when we are *not* in front.
///
/// The in-window toast, though, is now raised **both ways** (task4150,
/// 2026-09-09). It used to be gated on `foreground` too, which left the press
/// with no in-app trace at all in the one situation this hotkey exists for --
/// a game in front -- because an OS toast under an unregistered AUMID is
/// dropped by Windows without an error (`desktop::toast`). The marker itself
/// never depended on any of this; only the acknowledgement did.
///
/// Raising it while the window is behind is not wasted, and the two variants
/// land differently on purpose:
///
/// - `Warning` (refused: nothing is recording) ignores `HOLD_MS`, so it is
///   still standing when the user comes back to the app. That is the heavy
///   case -- believing a marker was placed when none was -- and the one this
///   change is really for.
/// - `Success` expires after `HOLD_MS` like any other result, so a press made
///   long before the user returns leaves nothing in the corner. Accepted: the
///   durable evidence of a successful marker is the marker on the review
///   timeline, not the toast.
///
/// Both are `ui_state::toast`'s existing rules, unchanged -- the foreground
/// behaviour is exactly what it was.
///
/// `displayed` is the session the review pane is showing, or `None` when it has
/// nothing loaded (task2190). With two captures running the press has to land
/// where the user is looking -- the same session the panel's ＋ button marks --
/// rather than on whichever started last. `None` keeps the last-started
/// resolution so the tray-resident path, where the window never opens, still
/// marks the recording.
///
/// It is an id rather than a Slint call on purpose: the caller holds `review`
/// mutably borrowed across this, so invoking `ReviewVm::add_marker` from here
/// would panic the handler's own `borrow_mut`.
pub(super) fn drain(
    hotkeys: &RefCell<Hotkeys>,
    controller: &CaptureController,
    capturing_chord: bool,
    displayed: Option<&str>,
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    hwnd: Option<HWND>,
) {
    let foreground = hwnd.is_some_and(desktop::is_foreground);
    // Which surfaces a press is handed to, decided once beside `foreground`
    // because both branches route the same way (task4150). It names the calls
    // this function *makes*, not what the user ends up seeing: an OS toast can
    // be dropped by Windows (see `desktop::toast`), and an in-app toast
    // repeating a message already in the corner is refused by `Toast::publish`.
    // Anything stronger than that would be a claim this code cannot check.
    let surface = if foreground {
        "in_app"
    } else {
        "in_app+os_toast"
    };
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
        // stopped one recording nothing simply refuses, as before.
        let target = match displayed {
            Some(id) => controller
                .recording_position_100ns_for(id)
                .map(|time| (Some(id.to_owned()), time)),
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
            // `Warning`, like the lifecycle auto-stop: nothing failed, but it
            // is not the result they asked for and has to keep until they have
            // read it. Unconditional since task4150 -- pressed from behind a
            // game this is the whole acknowledgement, and because `Warning`
            // ignores `HOLD_MS` it is still there when they come back.
            publish_toast(
                ui,
                status_line,
                settings_ui::marker_hotkey_refused(tr_locale()),
                settings_ui::marker_hotkey_not_recording(tr_locale()),
                toast::ToastVariant::Warning,
                "",
            );
            if !foreground {
                // Kept as well, not replaced: on a machine that has been
                // through the installer the AUMID is registered and this is
                // the one the user sees *while* the game is still in front.
                desktop::toast(
                    settings_ui::marker_hotkey_refused(tr_locale()),
                    settings_ui::marker_hotkey_not_recording(tr_locale()),
                );
            }
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
            foreground,
            surface,
            "Marker hotkey added a marker"
        );
        // Silence would leave the user unsure the press landed at all.
        // Unconditional since task4150, same as the refusal above -- though
        // this one expires after `HOLD_MS`, so a press made from behind a game
        // may well be gone by the time they look. That is deliberate: the
        // marker on the review timeline is what proves this press landed.
        publish_toast(
            ui,
            status_line,
            settings_ui::marker_hotkey_added(tr_locale()),
            "",
            toast::ToastVariant::Success,
            "",
        );
        if !foreground {
            desktop::toast(settings_ui::marker_hotkey_added(tr_locale()), "");
        }
    }
}
