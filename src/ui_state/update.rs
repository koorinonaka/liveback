//! The settings screen's update row: what its status line says, and when a
//! check's answer raises the OS notification. Moved from the bin's
//! `update_check` (t261025-730e); the network check and the window wiring stay
//! there.

use crate::ui_state::locale::Locale;
use crate::ui_state::settings as settings_ui;

/// The update row's status line.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Blank,
    Off,
    Checking,
    UpToDate,
    Available(String),
    Downloading,
    Failed,
}

/// The status line's wording for `status` in `locale`.
pub fn status_text(locale: Locale, status: &Status) -> String {
    match status {
        Status::Blank => String::new(),
        Status::Off => settings_ui::update_off(locale).to_string(),
        Status::Checking => settings_ui::update_checking(locale).to_string(),
        Status::UpToDate => settings_ui::update_up_to_date(locale).to_string(),
        Status::Available(version) => settings_ui::update_available(locale, version),
        Status::Downloading => settings_ui::update_downloading(locale).to_string(),
        Status::Failed => settings_ui::update_failed(locale).to_string(),
    }
}

/// Whether a check's answer raises the OS notification: only the startup check,
/// only for a newer release, and never while a capture is running.
pub fn os_notice_due(at_startup: bool, recording: bool, newer: bool) -> bool {
    at_startup && !recording && newer
}

#[cfg(test)]
mod tests {
    use super::{os_notice_due, status_text, Status};
    use crate::ui_state::locale::Locale;

    /// t260930-4822: all eight cases; only (startup, not recording, newer) fires.
    #[test]
    fn os_notice_fires_only_at_startup_when_idle_and_newer() {
        for at_startup in [false, true] {
            for recording in [false, true] {
                for newer in [false, true] {
                    assert_eq!(
                        os_notice_due(at_startup, recording, newer),
                        (at_startup, recording, newer) == (true, false, true),
                        "{at_startup} {recording} {newer}"
                    );
                }
            }
        }
    }

    /// Every state the line can be in, in both languages: a language switch
    /// re-renders from this, so each state has to map to its own wording.
    #[test]
    fn every_status_renders_in_both_languages() {
        let cases = [
            (Status::Blank, "", ""),
            (
                Status::Off,
                "自動では確かめません",
                "Not checked automatically",
            ),
            (Status::Checking, "確かめています…", "Checking…"),
            (Status::UpToDate, "最新です", "Up to date"),
            (
                Status::Available("9.9.9".into()),
                "9.9.9 が利用できます",
                "9.9.9 is available",
            ),
            (
                Status::Downloading,
                "ダウンロードしています…",
                "Downloading…",
            ),
            (
                Status::Failed,
                "更新を確かめられませんでした",
                "Could not check for updates",
            ),
        ];
        for (status, ja, en) in cases {
            assert_eq!(status_text(Locale::Ja, &status), ja, "{status:?} ja");
            assert_eq!(status_text(Locale::En, &status), en, "{status:?} en");
        }
    }
}
