//! The settings switch behind the MCP server (t261009-5ebd). The server itself
//! is `livia::mcp`; this starts it at launch when the switch is on, starts and
//! stops it with the switch, and puts the outcome on the row's second line --
//! the URL, or the port another app already holds (a second Liveback, an
//! isolated test instance) without failing the app.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use livia::capture::CaptureController;
use livia::export::ExportController;
use livia::mcp::{Live, Server};
use livia::settings::AppSettings;
use livia::ui_state::settings as settings_ui;

use super::settings_page::{commit_settings, settings_snapshot};
use super::{tr_locale, AppWindow, SettingsLabels, SettingsVm};
use slint::ComponentHandle;

/// What the row's second line says.
#[derive(Clone, Copy)]
enum Line {
    Off,
    Listening(u16),
    Taken(u16),
}

thread_local! {
    /// Dropping it closes the port.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };
    static LINE: Cell<Line> = const { Cell::new(Line::Off) };
}

/// The row's labels, re-pushed on a language change like the rest.
pub(super) fn push_labels(ui: &AppWindow) {
    let labels = ui.global::<SettingsLabels>();
    let locale = tr_locale();
    labels.set_mcp_server(settings_ui::mcp_server(locale).into());
    let line = match LINE.get() {
        Line::Off => settings_ui::mcp_server_sub(locale).to_owned(),
        Line::Listening(port) => settings_ui::mcp_listening(locale, port),
        Line::Taken(port) => settings_ui::mcp_port_taken(locale, port),
    };
    labels.set_mcp_server_sub(line.into());
}

/// Brings the server in line with `settings`. Always stops first, so an
/// off-and-on retry after a taken port binds afresh.
fn apply(ui: &AppWindow, settings: &AppSettings, backend: &Arc<Live>) {
    SERVER.with(|server| server.borrow_mut().take());
    let port = settings.mcp_port;
    let line = if !settings.mcp_enabled {
        Line::Off
    } else {
        match Server::start(port, backend.clone()) {
            Ok(server) => {
                tracing::info!(port, "mcp server listening");
                SERVER.with(|slot| *slot.borrow_mut() = Some(server));
                Line::Listening(port)
            }
            Err(error) => {
                tracing::warn!(port, %error, "mcp server could not bind");
                Line::Taken(port)
            }
        }
    };
    LINE.set(line);
    push_labels(ui);
}

pub(super) fn wire(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    controller: &CaptureController,
    exporter: &ExportController,
) {
    // The app's own controllers: a second `ExportController` would run beside
    // the review screen's and delete its staging folder.
    let backend = Arc::new(Live {
        capture: controller.clone(),
        exporter: exporter.clone(),
        settings: Arc::clone(settings),
    });
    let current = settings_snapshot(settings);
    let vm = ui.global::<SettingsVm>();
    vm.set_mcp_enabled(current.mcp_enabled);
    apply(ui, &current, &backend);

    let weak = ui.as_weak();
    let settings = Arc::clone(settings);
    vm.on_mcp_toggled(move |enabled| {
        let Some(ui) = weak.upgrade() else { return };
        let next = commit_settings(&settings, |current| {
            current.mcp_enabled = enabled;
        });
        ui.global::<SettingsVm>().set_mcp_enabled(next.mcp_enabled);
        apply(&ui, &next, &backend);
    });
}
