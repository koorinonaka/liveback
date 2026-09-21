//! The settings screen's update rows, and the check behind them
//! (task t260920-2be2).
//!
//! `livia::update` does the talking; this file decides *when*, keeps the answer
//! off the UI thread, and turns it into the one line the screen shows.
//!
//! Three rules shape it, and all three came from what this app is:
//!
//! - **No toast.** A recording is running while someone is in a game, and a
//!   toast lands on top of the game. The answer goes in the settings screen and
//!   waits there.
//! - **Nothing blocks the recording.** The request is on its own thread; only
//!   the result comes back through `upgrade_in_event_loop`.
//! - **Nothing goes out before the user says so.** The installer's last page
//!   carries the checkbox; the first launch reads what it recorded and never
//!   asks again.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use livia::settings::AppSettings;
use livia::ui_state::settings as settings_ui;
use livia::update;

use super::settings_page::{commit_settings, settings_snapshot};
use super::{tr_locale, AppWindow, SettingsLabels, SettingsVm};
use slint::ComponentHandle;

/// Every 24 hours after the one at startup. The app is tray-resident and stays
/// up for days, so a startup-only check would not run for weeks.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);

thread_local! {
    /// A `Timer` stops when it is dropped, so it has to outlive `wire`.
    static WATCH: slint::Timer = slint::Timer::default();
    /// The installer of the release the status line is currently naming. Set
    /// when a check finds one; read when the user presses 更新.
    static PENDING: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or_default()
}

/// The label set, re-pushed on a language change like the rest of the screen.
pub(super) fn push_labels(ui: &AppWindow) {
    let labels = ui.global::<SettingsLabels>();
    labels.set_group_update(settings_ui::group_update(tr_locale()).into());
    labels.set_update_check(settings_ui::update_check(tr_locale()).into());
    labels.set_update_now(settings_ui::update_now(tr_locale()).into());
}

fn set_status(ui: &AppWindow, text: String, actionable: bool) {
    ui.global::<SettingsLabels>().set_update_status(text.into());
    ui.global::<SettingsVm>().set_update_available(actionable);
}

/// What the row says when no check is in flight and none has answered yet.
fn idle_status(ui: &AppWindow, enabled: bool) {
    let text = if enabled {
        settings_ui::update_up_to_date(tr_locale(), update::current_version())
    } else {
        settings_ui::update_off(tr_locale()).to_string()
    };
    set_status(ui, text, false);
}

/// Asks GitHub on a worker thread. The stamp is written when the check
/// *starts*: an offline machine that recorded only successes would re-check on
/// every single launch.
fn run_check(ui: &AppWindow, settings: &Arc<Mutex<AppSettings>>) {
    commit_settings(settings, |current| {
        current.update_last_checked = Some(now_secs());
    });
    set_status(
        ui,
        settings_ui::update_checking(tr_locale()).to_string(),
        false,
    );
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let found = update::fetch_latest();
        let _ = weak.upgrade_in_event_loop(move |ui| {
            let current = update::current_version();
            match found {
                Ok(release) if update::is_newer(&release.version, current) => {
                    let text =
                        settings_ui::update_available(tr_locale(), &release.version, current);
                    // Only actionable when the release actually carries an
                    // installer: an 更新 button that can only fail is worse
                    // than the line alone.
                    let url = release.installer_url.clone();
                    let actionable = url.is_some();
                    PENDING.with(|pending| *pending.borrow_mut() = url);
                    set_status(&ui, text, actionable);
                }
                Ok(_) => {
                    PENDING.with(|pending| *pending.borrow_mut() = None);
                    set_status(
                        &ui,
                        settings_ui::update_up_to_date(tr_locale(), current),
                        false,
                    );
                }
                // Never "up to date" here: the check did not answer, so the app
                // does not know, and saying it does would be a guess.
                Err(error) => {
                    // Not `eprintln!`: this is a GUI-subsystem binary, so stderr
                    // goes nowhere and the screen would say "could not check"
                    // with the reason lost (measured 2026-09-21).
                    tracing::warn!(%error, "the update check did not answer");
                    PENDING.with(|pending| *pending.borrow_mut() = None);
                    set_status(
                        &ui,
                        settings_ui::update_failed(tr_locale()).to_string(),
                        false,
                    );
                }
            }
        });
    });
}

/// Downloads the installer and hands over to it. NSIS owns everything about a
/// running exe from here (task3001), so this process leaves rather than trying
/// to replace its own files.
fn run_update(ui: &AppWindow) {
    let Some(url) = PENDING.with(|pending| pending.borrow().clone()) else {
        return;
    };
    set_status(
        ui,
        settings_ui::update_downloading(tr_locale()).to_string(),
        false,
    );
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let downloaded = update::download_installer(&url);
        let _ = weak.upgrade_in_event_loop(move |ui| match downloaded {
            Ok(path) => match update::launch(&path) {
                Ok(()) => slint::quit_event_loop().unwrap_or_default(),
                Err(error) => {
                    tracing::warn!(%error, "could not start the downloaded installer");
                    set_status(
                        &ui,
                        settings_ui::update_failed(tr_locale()).to_string(),
                        true,
                    );
                }
            },
            Err(error) => {
                tracing::warn!(%error, "could not download the installer");
                set_status(
                    &ui,
                    settings_ui::update_failed(tr_locale()).to_string(),
                    true,
                );
            }
        });
    });
}

pub(super) fn wire(ui: &AppWindow, settings: &Arc<Mutex<AppSettings>>) {
    push_labels(ui);
    let mut current = settings_snapshot(settings);

    // First run after an install: take the answer the installer's checkbox
    // recorded. A build run from source has no such key and keeps the default
    // (on). This happens once -- afterwards the settings row is the only thing
    // that moves the flag, so a reinstall cannot overrule a later choice.
    if !current.update_check_prompted {
        let from_installer = livia::update::installer_preference().unwrap_or(true);
        current = commit_settings(settings, |stored| {
            stored.update_check_enabled = from_installer;
            stored.update_check_prompted = true;
        });
    }
    let vm = ui.global::<SettingsVm>();
    vm.set_update_check_enabled(current.update_check_enabled);
    idle_status(ui, current.update_check_enabled);

    {
        let ui = ui.as_weak();
        let settings = Arc::clone(settings);
        vm.on_update_check_toggled(move |enabled| {
            let Some(ui) = ui.upgrade() else { return };
            let next = commit_settings(&settings, |current| {
                current.update_check_enabled = enabled;
            });
            ui.global::<SettingsVm>()
                .set_update_check_enabled(next.update_check_enabled);
            if next.update_check_enabled {
                run_check(&ui, &settings);
            } else {
                PENDING.with(|pending| *pending.borrow_mut() = None);
                idle_status(&ui, false);
            }
        });
    }

    {
        let ui = ui.as_weak();
        vm.on_update_requested(move || {
            if let Some(ui) = ui.upgrade() {
                run_update(&ui);
            }
        });
    }

    if current.update_check_enabled {
        run_check(ui, settings);
        let weak = ui.as_weak();
        let settings = Arc::clone(settings);
        WATCH.with(|timer| {
            timer.start(slint::TimerMode::Repeated, EVERY, move || {
                if let Some(ui) = weak.upgrade() {
                    if settings_snapshot(&settings).update_check_enabled {
                        run_check(&ui, &settings);
                    }
                }
            });
        });
    }
}
