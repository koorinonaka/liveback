//! The one seam between the domain modules and whatever UI is attached.
//!
//! `capture` and `export` used to hold an `AppHandle` directly, which is the
//! only thing that tied them to Tauri at all. Everything they actually need
//! from a UI is here: the two export events and the tray tooltip. There was an
//! OS-notification method too, for the clip export that ran from a global
//! hotkey with no window in sight -- task1090 removed clip saving, so every
//! export now runs from a window that is looking at it and the corner toast
//! reports the outcome. Naming the events as methods rather than passing an event string
//! plus a JSON blob keeps the payloads typed and leaves the wire names in the
//! adapter, where the transport belongs.

use crate::export::{ExportProgress, ExportStatus};

pub trait EventSink: Send + Sync {
    fn export_status(&self, status: &ExportStatus);
    fn export_progress(&self, progress: &ExportProgress);
    fn set_tray_tooltip(&self, tooltip: &str);
}

/// Drops everything. What a headless run gets, and the default for a
/// `CaptureController` before a UI attaches itself.
pub struct NullSink;

impl EventSink for NullSink {
    fn export_status(&self, _status: &ExportStatus) {}
    fn export_progress(&self, _progress: &ExportProgress) {}
    fn set_tray_tooltip(&self, _tooltip: &str) {}
}

/// Keeps everything it was handed so tests can assert on it.
#[derive(Default)]
pub struct RecordingSink {
    pub statuses: std::sync::Mutex<Vec<ExportStatus>>,
    pub progress: std::sync::Mutex<Vec<ExportProgress>>,
    pub tooltips: std::sync::Mutex<Vec<String>>,
}

impl EventSink for RecordingSink {
    fn export_status(&self, status: &ExportStatus) {
        if let Ok(mut statuses) = self.statuses.lock() {
            statuses.push(status.clone());
        }
    }

    fn export_progress(&self, progress: &ExportProgress) {
        if let Ok(mut recorded) = self.progress.lock() {
            recorded.push(progress.clone());
        }
    }

    fn set_tray_tooltip(&self, tooltip: &str) {
        if let Ok(mut tooltips) = self.tooltips.lock() {
            tooltips.push(tooltip.to_string());
        }
    }
}
