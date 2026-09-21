//! Which language the UI speaks, and the shape every piece of wording takes
//! (task1150).
//!
//! The app was single-locale: wording lived as `&str` consts in whichever
//! `ui_state` module owned the screen, and the bin pushed them into slint's
//! label globals at startup. That push architecture is the reason this is a
//! Rust-side change only -- `.slint` still receives strings and still does not
//! care where they came from, so switching language is re-running the pushes,
//! not reloading a UI.
//!
//! Every piece of wording is a `fn(Locale) -> &'static str` written with
//! [`tr!`]. The match inside has no wildcard arm, so **a locale with a missing
//! translation does not compile** -- which is why there is no "every key is
//! filled in both languages" test to go looking for. The only thing worth
//! testing at runtime is the resolution below.

/// The languages the UI speaks. Japanese is the default because it is what the
/// app was written in and what every stored record's wording came from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Locale {
    #[default]
    Ja,
    En,
}

/// Declares localized wording: one function per key, both languages required.
///
/// ```ignore
/// tr! {
///     /// Doc comments ride along.
///     drop_accept_title { ja: "ドロップして開く", en: "Drop to open" }
/// }
/// ```
#[macro_export]
macro_rules! tr {
    ($(
        $(#[$meta:meta])*
        $name:ident { ja: $ja:expr, en: $en:expr }
    )*) => {
        $(
            $(#[$meta])*
            pub fn $name(locale: $crate::ui_state::locale::Locale) -> &'static str {
                match locale {
                    $crate::ui_state::locale::Locale::Ja => $ja,
                    $crate::ui_state::locale::Locale::En => $en,
                }
            }
        )*
    };
}

/// The language for the two places that cannot be handed one.
///
/// Every UI path resolves its own `Locale` from the settings it already holds,
/// which is why this is not a general-purpose global. Two callers have no such
/// settings in reach: the capture worker, which names a monitor session as it
/// records it, and the tray tooltip, which the same worker pushes through the
/// event sink. Both are set from the UI thread at startup and on a language
/// change, and read on a worker thread -- hence the atomic.
static ACTIVE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn set_active(locale: Locale) {
    let value = match locale {
        Locale::Ja => 0,
        Locale::En => 1,
    };
    ACTIVE.store(value, std::sync::atomic::Ordering::Relaxed);
}

pub fn active() -> Locale {
    match ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Locale::En,
        _ => Locale::Ja,
    }
}

/// What the settings file stores. `system` is the default and means "ask the
/// OS", which is the only value whose meaning can change between two launches
/// of the same build.
pub const LANGUAGE_SYSTEM: &str = "system";
pub const LANGUAGE_JA: &str = "ja";
pub const LANGUAGE_EN: &str = "en";

/// The language actually used, from the stored setting and what the OS says.
/// An unrecognised setting falls back to the system answer rather than to a
/// language: a record written by a later build that knows a third language
/// should degrade to "whatever this machine is", not to Japanese.
pub fn resolve(setting: &str, system: Locale) -> Locale {
    match setting {
        LANGUAGE_JA => Locale::Ja,
        LANGUAGE_EN => Locale::En,
        _ => system,
    }
}

/// Windows' `LANGID`, as `GetUserDefaultUILanguage` returns it. The primary
/// language is the low 10 bits; `0x11` is Japanese, and every sublanguage of
/// it (ja-JP is the only one shipping) counts. Anything else gets English --
/// there are two languages here, so "not Japanese" is the whole of the rule.
pub fn from_langid(langid: u16) -> Locale {
    const LANG_JAPANESE: u16 = 0x11;
    if langid & 0x3ff == LANG_JAPANESE {
        Locale::Ja
    } else {
        Locale::En
    }
}

/// The settings row's values, in order. Kept beside the parsing above so the
/// row and the stored value cannot disagree about which control means what.
/// Index 0 is the 「システム」 chip; the rest are the combo's entries (task1470).
pub const LANGUAGE_SETTINGS: [&str; 3] = [LANGUAGE_SYSTEM, LANGUAGE_JA, LANGUAGE_EN];

/// `LANGUAGE_SETTINGS` without the sentinel: the chip at index 0 is a mode, not
/// a language, and the combo beside it lists only languages. The offset lives
/// here rather than in `.slint` so the two index spaces are converted next to
/// the table that defines them.
const LANGUAGE_CHOICE_OFFSET: usize = 1;

pub fn language_index(setting: &str) -> i32 {
    LANGUAGE_SETTINGS
        .iter()
        .position(|candidate| *candidate == setting)
        .unwrap_or(0) as i32
}

pub fn language_setting(index: i32) -> &'static str {
    LANGUAGE_SETTINGS
        .get(usize::try_from(index).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or(LANGUAGE_SYSTEM)
}

tr! {
    /// The settings row itself.
    language_label { ja: "言語", en: "Language" }
    language_system { ja: "システム", en: "System" }
    language_japanese { ja: "日本語", en: "日本語" }
    language_english { ja: "English", en: "English" }
}

/// The combo's entries, in `LANGUAGE_SETTINGS` order minus the sentinel. The
/// language names are deliberately the same in both locales: a language picker
/// that renames the languages is a picker you cannot use to get back.
pub fn language_choices(locale: Locale) -> [&'static str; LANGUAGE_SETTINGS.len() - 1] {
    [language_japanese(locale), language_english(locale)]
}

/// Which combo entry the row shows. An explicit setting picks itself; while
/// 「システム」 is the answer the combo still shows a language -- the one the row
/// would go back to -- so the caller passes what the OS resolved to and keeps
/// this answer around as the session's last explicit pick (task1470).
pub fn language_choice_index(setting: &str, resolved: Locale) -> i32 {
    let language = match setting {
        LANGUAGE_JA => LANGUAGE_JA,
        LANGUAGE_EN => LANGUAGE_EN,
        _ => match resolved {
            Locale::Ja => LANGUAGE_JA,
            Locale::En => LANGUAGE_EN,
        },
    };
    language_index(language) - LANGUAGE_CHOICE_OFFSET as i32
}

/// The stored value a combo entry means. Out of range is the sentinel, same as
/// [`language_setting`] -- a row that cannot say which language was picked has
/// not picked one.
pub fn language_choice_setting(index: i32) -> &'static str {
    match usize::try_from(index) {
        Ok(index) => language_setting((index + LANGUAGE_CHOICE_OFFSET) as i32),
        Err(_) => LANGUAGE_SYSTEM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_worker_side_locale_round_trips() {
        set_active(Locale::En);
        assert_eq!(active(), Locale::En);
        set_active(Locale::Ja);
        assert_eq!(active(), Locale::Ja);
    }

    #[test]
    fn an_explicit_language_wins_and_system_defers_to_the_os() {
        assert_eq!(resolve(LANGUAGE_JA, Locale::En), Locale::Ja);
        assert_eq!(resolve(LANGUAGE_EN, Locale::Ja), Locale::En);
        assert_eq!(resolve(LANGUAGE_SYSTEM, Locale::Ja), Locale::Ja);
        assert_eq!(resolve(LANGUAGE_SYSTEM, Locale::En), Locale::En);
    }

    /// A value this build does not know -- a record written by a later one --
    /// lands on the machine's own language, not on a hard-coded default.
    #[test]
    fn an_unknown_setting_falls_back_to_the_system_language() {
        assert_eq!(resolve("de", Locale::En), Locale::En);
        assert_eq!(resolve("de", Locale::Ja), Locale::Ja);
        assert_eq!(resolve("", Locale::En), Locale::En);
    }

    #[test]
    fn every_japanese_sublanguage_reads_as_japanese() {
        // ja-JP, and the neutral/default sublanguage forms of the same
        // primary id -- the sublanguage lives in the high bits.
        assert_eq!(from_langid(0x0411), Locale::Ja);
        assert_eq!(from_langid(0x0011), Locale::Ja);
        assert_eq!(from_langid(0x0811), Locale::Ja);
        // en-US, en-GB, and anything else at all.
        assert_eq!(from_langid(0x0409), Locale::En);
        assert_eq!(from_langid(0x0809), Locale::En);
        assert_eq!(from_langid(0x0407), Locale::En, "de-DE is not Japanese");
    }

    #[test]
    fn the_chip_row_round_trips_through_the_stored_value() {
        for (index, setting) in LANGUAGE_SETTINGS.iter().enumerate() {
            assert_eq!(language_index(setting), index as i32);
            assert_eq!(language_setting(index as i32), *setting);
        }
        // Out of range, either way, is the default chip.
        assert_eq!(language_setting(9), LANGUAGE_SYSTEM);
        assert_eq!(language_setting(-1), LANGUAGE_SYSTEM);
        assert_eq!(language_index("de"), 0);
    }

    /// The completeness guarantee is the compiler's: `tr!` expands to a match
    /// with no wildcard, so a key missing a locale does not build. What a test
    /// *can* still catch is a key that compiles because the English arm was
    /// filled in with the Japanese -- a copy-paste, not a hole. This walks one
    /// key from every module and insists the two differ, except where they are
    /// deliberately the same word.
    #[test]
    fn the_english_locale_is_not_a_copy_of_the_japanese() {
        use crate::ui_state::{export, lifecycle, playback, sessions, settings, shell, targets};

        type Key = fn(Locale) -> &'static str;

        let translated: [(&str, Key); 10] = [
            ("shell", shell::drop_accept_title),
            ("targets", targets::tab_windows),
            ("lifecycle", lifecycle::capture_failed_title),
            ("playback", playback::stage_clamped_start),
            ("export", export::export_cancel),
            ("sessions", sessions::menu_load),
            ("sessions/notice", sessions::settings_action),
            ("timeline", crate::ui_state::timeline::export_selection),
            ("settings", settings::group_capture),
            ("locale", language_label),
        ];
        for (module, key) in translated {
            assert_ne!(
                key(Locale::Ja),
                key(Locale::En),
                "{module} looks untranslated"
            );
        }

        // The deliberate exceptions: a unit, an acronym, a key name.
        for same in [
            targets::recording as Key,
            settings::unit_fps,
            settings::unit_gigabytes,
            sessions::hint_edit_note,
            sessions::hint_discard,
            crate::ui_state::timeline::range_unset,
        ] {
            assert_eq!(same(Locale::Ja), same(Locale::En));
        }
    }

    #[test]
    fn the_language_names_are_not_themselves_translated() {
        // Someone who has landed on the wrong language has to be able to find
        // their way back, which means recognising the name of their own.
        assert_eq!(language_japanese(Locale::En), language_japanese(Locale::Ja));
        assert_eq!(language_english(Locale::Ja), language_english(Locale::En));
        assert_eq!(language_choices(Locale::En), language_choices(Locale::Ja));
        // The chip beside the combo is a mode, so it *is* translated.
        assert_eq!(language_system(Locale::En), "System");
        assert_eq!(language_system(Locale::Ja), "システム");
    }

    /// The combo's index space is `LANGUAGE_SETTINGS` minus the sentinel, and
    /// the two have to convert both ways without either learning the offset.
    #[test]
    fn the_combo_round_trips_through_the_stored_value() {
        for (choice, setting) in LANGUAGE_SETTINGS[1..].iter().enumerate() {
            let choice = choice as i32;
            assert_eq!(language_choice_setting(choice), *setting);
            assert_eq!(language_choice_index(setting, Locale::Ja), choice);
            assert_eq!(language_choice_index(setting, Locale::En), choice);
        }
        // 「システム」 -- and anything unrecognised -- shows what the OS answer
        // resolved to rather than emptying the combo.
        assert_eq!(language_choice_index(LANGUAGE_SYSTEM, Locale::Ja), 0);
        assert_eq!(language_choice_index(LANGUAGE_SYSTEM, Locale::En), 1);
        assert_eq!(language_choice_index("de", Locale::En), 1);
        // Out of range, either way, picks no language.
        assert_eq!(language_choice_setting(9), LANGUAGE_SYSTEM);
        assert_eq!(language_choice_setting(-1), LANGUAGE_SYSTEM);
    }
}
