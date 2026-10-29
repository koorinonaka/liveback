use super::*;

/// t260930-4822: the startup notification, in both languages.
#[test]
fn the_update_notice_renders_in_both_languages() {
    assert_eq!(
        update_notice(Locale::Ja, "9.9.9"),
        (
            "Liveback 9.9.9 が出ました".to_string(),
            "設定の「更新」から入れられます".to_string()
        )
    );
    assert_eq!(
        update_notice(Locale::En, "9.9.9"),
        (
            "Liveback 9.9.9 is available".to_string(),
            "Install it from Settings → Updates".to_string()
        )
    );
}

/// The row is three chips over one string, and the only branch worth a test
/// is the sentinel: `system` has to follow the OS in *both* directions, and
/// an unknown value has to behave like the sentinel rather than like dark.
#[test]
fn the_theme_row_round_trips_and_system_follows_the_os() {
    for (index, setting) in THEME_SETTINGS.iter().enumerate() {
        assert_eq!(theme_setting(index as i32), *setting);
        assert_eq!(theme_index(setting), index as i32);
    }
    assert_eq!(theme_setting(-1), THEME_SYSTEM);
    assert_eq!(theme_setting(3), THEME_SYSTEM);
    assert_eq!(theme_index("nonesuch"), 0);

    assert!(theme_is_light(THEME_LIGHT, false));
    assert!(!theme_is_light(THEME_DARK, true));
    assert!(theme_is_light(THEME_SYSTEM, true));
    assert!(!theme_is_light(THEME_SYSTEM, false));
    assert!(theme_is_light("nonesuch", true));
    assert!(!theme_is_light("nonesuch", false));
}

#[test]
fn the_hotkey_failure_says_what_to_do_about_it() {
    // The why names the chord the way running text spells it (DS 言葉);
    // the what is the toast's 「設定を開く」, attached by the caller.
    let rejected = hotkey_failure_detail(Locale::Ja, "Ctrl+Shift+R");
    assert_eq!(rejected, "Ctrl + Shift + R は他のアプリが使っています。");
    // The title carries the "what", the detail the "why" -- a repeat of
    // the title in the second line wastes the toast's other row.
    assert!(
        !rejected.starts_with(hotkey_failure_title(Locale::Ja)),
        "{rejected}"
    );
    assert_eq!(
        hotkey_failure_detail(Locale::En, "Ctrl+Shift+R"),
        "Ctrl + Shift + R is in use by another app."
    );
}

/// t260928-7e9a: the settings that reach the next recording say so (DS
/// Switch), and the reset is 既定 (default), never 規定 (rule).
#[test]
fn the_next_recording_switches_say_so() {
    for sub in [
        ring_buffer_sub(Locale::Ja),
        include_cursor_sub(Locale::Ja),
        capture_all_audio_sub(Locale::Ja),
        codec_av1_sub(Locale::Ja),
    ] {
        assert!(sub.ends_with("次の録画から適用されます。"), "{sub}");
        assert!(sub.matches('。').count() <= 2, "{sub}");
    }
    assert_eq!(directory_reset(Locale::Ja), "既定に戻す");
    assert!(buffer_directory_reset_done(Locale::Ja).contains("既定に戻しました"));
}

/// DS 言葉: Windows key names. The stored chord keeps global-hotkey's
/// `Super`; only what is drawn or said reads `Win`.
#[test]
fn the_windows_key_is_shown_as_win() {
    let stored = compose_shortcut(
        "a",
        Modifiers {
            ctrl: true,
            meta: true,
            ..Modifiers::default()
        },
    )
    .unwrap();
    assert_eq!(stored, "Ctrl+Super+A");
    assert_eq!(hotkey_keys(&stored), ["Ctrl", "Win", "A"]);
    assert_eq!(hotkey_display(&stored), "Ctrl + Win + A");
    assert_eq!(hotkey_display("Ctrl+Shift+G"), "Ctrl + Shift + G");
}

/// Task1760: a machine that cannot decode AV1 must not be shown a lit AV1
/// switch, however the settings file got that way.
#[test]
fn the_av1_switch_follows_both_the_setting_and_the_machine() {
    assert_eq!(codec_from_switch(true), RecordingCodec::Av1);
    assert_eq!(codec_from_switch(false), RecordingCodec::H264);
    assert!(codec_switch_on(RecordingCodec::Av1, true));
    assert!(!codec_switch_on(RecordingCodec::H264, true));
    assert!(!codec_switch_on(RecordingCodec::Av1, false));
}

#[test]
fn modifier_order_matches_the_react_version() {
    // React pushes Ctrl, Alt, Shift, Super in that fixed order regardless
    // of press order.
    assert_eq!(
        compose_shortcut(
            "a",
            Modifiers {
                ctrl: true,
                alt: true,
                shift: true,
                meta: true
            }
        )
        .as_deref(),
        Some("Ctrl+Alt+Shift+Super+A")
    );
}

#[test]
fn single_characters_are_uppercased() {
    assert_eq!(
        compose_shortcut(
            "k",
            Modifiers {
                alt: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Alt+K")
    );
    assert_eq!(
        compose_shortcut(
            "5",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+5")
    );
    // The default marker hotkey.
    assert_eq!(
        compose_shortcut(
            "r",
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Shift+R")
    );
}

#[test]
fn named_keys_use_the_react_names() {
    for (text, name) in [
        (" ", "Space"),
        ("\u{0008}", "Backspace"),
        ("\u{0009}", "Tab"),
        ("\u{000a}", "Enter"),
        ("\u{007f}", "Delete"),
        ("\u{F700}", "Up"),
        ("\u{F701}", "Down"),
        ("\u{F702}", "Left"),
        ("\u{F703}", "Right"),
        ("\u{F727}", "Insert"),
        ("\u{F729}", "Home"),
        ("\u{F72B}", "End"),
        ("\u{F72C}", "PageUp"),
        ("\u{F72D}", "PageDown"),
    ] {
        assert_eq!(
            compose_shortcut(
                text,
                Modifiers {
                    ctrl: true,
                    ..Modifiers::default()
                }
            )
            .as_deref(),
            Some(format!("Ctrl+{name}").as_str()),
            "key {text:?}"
        );
    }
}

#[test]
fn function_keys_are_numbered() {
    assert_eq!(
        compose_shortcut(
            "\u{F704}",
            Modifiers {
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("F1")
    );
    assert_eq!(
        compose_shortcut(
            "\u{F70F}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+F12")
    );
    assert_eq!(
        compose_shortcut(
            "\u{F71B}",
            Modifiers {
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("F24")
    );
}

#[test]
fn modifier_only_presses_compose_to_nothing() {
    // Shift, Control, Alt, AltGr, CapsLock, ShiftR, ControlR, Meta, MetaR.
    for code in 0x10u32..=0x18 {
        let text = char::from_u32(code).unwrap().to_string();
        assert_eq!(
            compose_shortcut(
                &text,
                Modifiers {
                    ctrl: code == 0x11,
                    shift: code == 0x10,
                    ..Modifiers::default()
                }
            ),
            None,
            "modifier {code:#x}"
        );
    }
}

#[test]
fn capturing_the_chord_the_row_already_has_changes_nothing() {
    assert_eq!(
        commit_capture("Ctrl+Shift+R", "Ctrl+Shift+R"),
        HotkeyCommit::Unchanged
    );
    // The control: a fresh chord is applied.
    assert_eq!(
        commit_capture("Ctrl+Shift+R", "Ctrl+Alt+C"),
        HotkeyCommit::Apply("Ctrl+Alt+C".into())
    );
}

#[test]
fn a_refused_registration_rolls_the_display_back_to_the_previous_chord() {
    assert_eq!(
        displayed_hotkey("Ctrl+Shift+R", "Ctrl+Alt+C", true),
        "Ctrl+Alt+C"
    );
    assert_eq!(
        displayed_hotkey("Ctrl+Shift+R", "Ctrl+Alt+C", false),
        "Ctrl+Shift+R"
    );
}

#[test]
fn ctrl_letters_arrive_as_control_codes_and_are_read_back() {
    // The two defaults, which windows delivers as 0x12 and 0x13 -- the same
    // codes as Alt and AltGr. Neither modifier is held, so they are letters.
    assert_eq!(
        compose_shortcut(
            "\u{0012}",
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Shift+R")
    );
    assert_eq!(
        compose_shortcut(
            "\u{0013}",
            Modifiers {
                ctrl: true,
                shift: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Shift+S")
    );
    // Both ends of the band, and a code outside the modifier range.
    assert_eq!(
        compose_shortcut(
            "\u{0001}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+A")
    );
    assert_eq!(
        compose_shortcut(
            "\u{001a}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Z")
    );
    // CapsLock's code has no modifier flag to check, so it reads as T.
    assert_eq!(
        compose_shortcut(
            "\u{0014}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+T")
    );
    // Shift's code is only ambiguous *while Shift is held*.
    assert_eq!(
        compose_shortcut(
            "\u{0010}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+P")
    );
}

#[test]
fn pressing_a_modifier_while_ctrl_is_held_stays_armed() {
    // Greedy conversion would confirm Ctrl+Shift+P the moment Shift went
    // down during a Ctrl+Shift+... capture, which is the whole reason the
    // band is guarded.
    for (code, alt, shift, meta) in [
        (0x10u32, false, true, false), // Shift
        (0x11, false, false, false),   // Control (ctrl is held by definition)
        (0x12, true, false, false),    // Alt
        (0x13, true, false, false),    // AltGr
        (0x15, false, true, false),    // ShiftR
        (0x16, false, false, false),   // ControlR
        (0x17, false, false, true),    // Meta
        (0x18, false, false, true),    // MetaR
    ] {
        let text = char::from_u32(code).unwrap().to_string();
        assert_eq!(
            compose_shortcut(
                &text,
                Modifiers {
                    ctrl: true,
                    alt,
                    shift,
                    meta
                }
            ),
            None,
            "modifier {code:#x} held during a ctrl chord"
        );
    }
}

#[test]
fn the_chords_that_cannot_be_told_apart_stay_armed() {
    // Each of these is indistinguishable from a modifier that is provably
    // down, so it must not be saved as a chord.
    //
    // Dormant on current builds (task145 verification, 2026-08-16): slint
    // 1.17.1 on Windows hands Ctrl+letter through as the *letter*, so these
    // control codes never actually arrive and all four chords are
    // capturable. The guard stays for the layouts, IMEs and future
    // versions that do send them, and this test is what keeps it honest.
    for (text, ctrl, shift, name) in [
        ("\u{0011}", true, false, "Ctrl+Q"),
        ("\u{0016}", true, false, "Ctrl+V"),
        ("\u{0010}", true, true, "Ctrl+Shift+P"),
        ("\u{0015}", true, true, "Ctrl+Shift+U"),
    ] {
        assert_eq!(
            compose_shortcut(
                text,
                Modifiers {
                    ctrl,
                    shift,
                    ..Modifiers::default()
                }
            ),
            None,
            "{name}"
        );
    }
}

#[test]
fn ctrl_keeps_the_named_keys_that_share_a_control_code() {
    // Ctrl+H / Ctrl+I / Ctrl+J collide with Backspace / Tab / Enter. The
    // named reading already worked, so it wins and the letters join the
    // unreachable list.
    assert_eq!(
        compose_shortcut(
            "\u{0008}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Backspace")
    );
    assert_eq!(
        compose_shortcut(
            "\u{0009}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Tab")
    );
    assert_eq!(
        compose_shortcut(
            "\u{000a}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        )
        .as_deref(),
        Some("Ctrl+Enter")
    );
}

#[test]
fn invalid_keys_compose_to_nothing() {
    // Unmapped PUA (media keys etc.), raw control codes, IME strings, and
    // an empty text all keep the capture armed.
    assert_eq!(
        compose_shortcut(
            "\u{F7FF}",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        ),
        None
    );
    assert_eq!(
        compose_shortcut(
            "\u{001b}",
            Modifiers {
                ..Modifiers::default()
            }
        ),
        None
    );
    assert_eq!(
        compose_shortcut(
            "かな",
            Modifiers {
                ..Modifiers::default()
            }
        ),
        None
    );
    assert_eq!(
        compose_shortcut(
            "",
            Modifiers {
                ctrl: true,
                ..Modifiers::default()
            }
        ),
        None
    );
}

#[test]
fn path_tail_keeps_two_segments_when_they_fit() {
    for (path, head, tail) in [
        (
            "%USERPROFILE%\\Videos\\Liveback",
            "%USERPROFILE%",
            "\\Videos\\Liveback",
        ),
        // Too long for two: falls back to one segment.
        (
            "C:\\Very\\Long\\FolderNameThatIsLong\\Liveback",
            "C:\\Very\\Long\\FolderNameThatIsLong",
            "\\Liveback",
        ),
        // No separators: all tail.
        ("plain", "", "plain"),
        // Forward slashes split too.
        ("home/example/videos", "home", "/example/videos"),
    ] {
        assert_eq!(
            split_path_tail(path),
            (head.to_string(), tail.to_string()),
            "{path}"
        );
    }
}

#[test]
fn ranges_track_the_validation_module() {
    assert_eq!(RETENTION_MINUTES_RANGE, (5, 1440));
    assert_eq!(RETENTION_CAPACITY_GB_RANGE, (1, 2000));
    assert_eq!(SESSION_LIFETIME_DAYS_RANGE, (1, 365));
}

/// `PresetRow` value resolution (task168). The chips are the answer for
/// almost every value; `-1` is how the row says "none of these, look at the
/// field instead".
#[test]
fn a_value_off_the_preset_list_lights_no_chip() {
    let presets = [15, 30, 60];
    assert_eq!(preset_index(&presets, 15), 0);
    assert_eq!(preset_index(&presets, 60), 2);
    assert_eq!(preset_index(&presets, 45), -1);
    assert_eq!(preset_index(&[], 15), -1);
}

/// Round6 3's `.errrow`: the row refuses an out-of-range draft rather
/// than clamping it, so the number the user sees is always the number the
/// setting holds.
#[test]
fn an_out_of_range_draft_is_refused_rather_than_clamped() {
    let (min, max) = RETENTION_CAPACITY_GB_RANGE;
    assert_eq!(preset_draft("20", min, max), PresetDraft::Commit(20));
    assert_eq!(preset_draft(" 20 ", min, max), PresetDraft::Commit(20));
    // Both bounds are reachable, the same way the chips are.
    assert_eq!(
        preset_draft(&min.to_string(), min, max),
        PresetDraft::Commit(min)
    );
    assert_eq!(
        preset_draft(&max.to_string(), min, max),
        PresetDraft::Commit(max)
    );
    // One past either end is refused, not pulled back in.
    assert_eq!(
        preset_draft(&(max + 1).to_string(), min, max),
        PresetDraft::OutOfRange
    );
    assert_eq!(
        preset_draft(&(min - 1).to_string(), min, max),
        PresetDraft::OutOfRange
    );
    assert_eq!(preset_draft("9999", min, max), PresetDraft::OutOfRange);
    // A non-number and an empty field are neither: nothing happens and no
    // error appears.
    assert_eq!(preset_draft("", min, max), PresetDraft::Ignore);
    assert_eq!(preset_draft("abc", min, max), PresetDraft::Ignore);
}

/// The error text under the row has to name the same bounds the refusal
/// used, or it sends the user back to a number that gets refused again.
/// t260927-fde8: the DS wording, both locales.
#[test]
fn the_refusal_message_is_the_ds_sentence() {
    assert_eq!(
        numeric_out_of_range(Locale::Ja, 1, 2000),
        "1〜2000 の範囲で入力してください"
    );
    assert_eq!(
        numeric_out_of_range(Locale::En, 1, 2000),
        "Enter a value from 1 to 2000"
    );
}

/// DS PresetField: four presets at most, and the values the task fixed.
#[test]
fn the_retention_presets_are_the_four_the_ds_allows() {
    assert_eq!(RETENTION_CAPACITY_PRESETS, [10, 20, 50, 100]);
    assert_eq!(SESSION_LIFETIME_PRESETS, [1, 7, 30, 90]);
    assert_eq!(RETENTION_MINUTES_PRESETS, [20, 60, 120]);
    // A value that left the list is not refused; it just lights nothing.
    assert_eq!(preset_index(&RETENTION_CAPACITY_PRESETS, 5), -1);
    assert_eq!(preset_index(&SESSION_LIFETIME_PRESETS, 3), -1);
}

/// Round5 §7-B. Every preset has to be reachable: a chip the clamp would
/// bounce is a chip that silently does something else when pressed.
#[test]
fn every_preset_lands_inside_its_own_clamp() {
    let rows: [(&[i64], (i64, i64)); 4] = [
        (&RETENTION_MINUTES_PRESETS, RETENTION_MINUTES_RANGE),
        (&RETENTION_CAPACITY_PRESETS, RETENTION_CAPACITY_GB_RANGE),
        (&SESSION_LIFETIME_PRESETS, SESSION_LIFETIME_DAYS_RANGE),
        (&FRAME_RATE_PRESETS, (30, 120)),
    ];
    for (presets, (min, max)) in rows {
        for value in presets {
            assert!(
                (min..=max).contains(value),
                "preset {value} is outside {min}..={max}"
            );
            // And pressing it lights itself, not some neighbour.
            assert_eq!(
                preset_index(presets, *value),
                presets.iter().position(|p| p == value).unwrap() as i32
            );
        }
    }
    // The retention row's chips no longer bracket its clamp (t260913-3127):
    // 5 and 1440 are typed into the free field. The loop above still keeps
    // every chip reachable.
    // 120 fps is on the list (task164 made the engine accept it).
    assert!(FRAME_RATE_PRESETS.contains(&120));
}

#[test]
fn the_free_fields_placeholder_is_a_typable_value_off_the_chips() {
    for (example, presets, (min, max)) in [
        (
            RETENTION_MINUTES_EXAMPLE,
            &RETENTION_MINUTES_PRESETS[..],
            RETENTION_MINUTES_RANGE,
        ),
        (
            RETENTION_CAPACITY_EXAMPLE,
            &RETENTION_CAPACITY_PRESETS[..],
            RETENTION_CAPACITY_GB_RANGE,
        ),
        (
            SESSION_LIFETIME_EXAMPLE,
            &SESSION_LIFETIME_PRESETS[..],
            SESSION_LIFETIME_DAYS_RANGE,
        ),
    ] {
        assert_eq!(
            preset_draft(&example.to_string(), min, max),
            PresetDraft::Commit(example)
        );
        assert_eq!(preset_index(presets, example), -1, "{example}");
    }
}

/// Task1490: the switch is the whole of the 0 sentinel's plumbing, so it
/// has to survive a round trip -- off writes 0 and keeps the number, on
/// puts that number back. The regression it guards is off twice: the
/// second flip must not remember the 0 it just wrote.
#[test]
fn a_retention_switch_stores_zero_off_and_restores_the_last_value_on() {
    let (stored, remembered) = retention_toggle(false, 50, 20);
    assert_eq!((stored, remembered), (0, 50));
    // The dimmed row goes on showing what it went off holding.
    assert_eq!(retention_shown(stored, remembered), 50);

    let (stored, remembered) = retention_toggle(false, stored, remembered);
    assert_eq!((stored, remembered), (0, 50));

    assert_eq!(retention_toggle(true, stored, remembered), (50, 50));

    // Nothing ever set: the row comes up on the shipped default.
    assert_eq!(retention_shown(0, 30), 30);
}

mod english_labels {
    use super::*;

    /// The rest of the settings screen, for the same reason.
    #[test]
    fn no_english_settings_label_contains_japanese() {
        for (name, text) in [
            ("retention_hint", retention_hint(Locale::En)),
            ("retention_capacity_gb", retention_capacity_gb(Locale::En)),
            ("session_lifetime_days", session_lifetime_days(Locale::En)),
            ("group_retention", group_retention(Locale::En)),
            // task4140's row. The line composed *out of* two of these is
            // checked in `unregistered_features_line` instead, since it is a `String`.
            (
                "install_feature_notifications",
                install_feature_notifications(Locale::En),
            ),
            (
                "install_feature_thumbnails",
                install_feature_thumbnails(Locale::En),
            ),
            (
                "install_unregistered_reason",
                install_unregistered_reason(Locale::En),
            ),
            // t260928-7e9a's descriptions.
            ("group_hotkey_sub", group_hotkey_sub(Locale::En)),
            ("retention_minutes_sub", retention_minutes_sub(Locale::En)),
            ("include_cursor_sub", include_cursor_sub(Locale::En)),
            ("auto_start_sub", auto_start_sub(Locale::En)),
            ("buffer_directory_sub", buffer_directory_sub(Locale::En)),
            ("capture_all_audio_sub", capture_all_audio_sub(Locale::En)),
            ("mcp_server_sub", mcp_server_sub(Locale::En)),
        ] {
            assert!(
                !text.chars().any(is_japanese),
                "{name} still contains Japanese: {text}"
            );
        }
    }

    /// Kana and the CJK ideographs. Deliberately not "any non-ASCII": an
    /// English string may hold a curly quote or an en dash.
    fn is_japanese(c: char) -> bool {
        matches!(c,
            '\u{3040}'..='\u{30ff}'   // hiragana, katakana
            | '\u{3000}'..='\u{303f}' // CJK punctuation, including 「」
            | '\u{4e00}'..='\u{9fff}' // ideographs
            | '\u{ff00}'..='\u{ffef}' // fullwidth forms
        )
    }
}

mod unregistered_features_line {
    use super::*;

    /// The whole of 「登録済みの機械では出ない」. The `.slint` side wraps the
    /// row in an `if` on this string, so an empty return is what keeps it out
    /// of the form -- there is no separate "hide the row" flag to get out of
    /// step with the text.
    #[test]
    fn a_fully_registered_machine_gets_no_line_at_all() {
        for locale in [Locale::Ja, Locale::En] {
            assert_eq!(
                unregistered_features_line(locale, true, true),
                "",
                "{locale:?} produced a line for a machine with both registrations"
            );
        }
    }

    /// Each single-missing case names its own feature and, the half that
    /// actually needs asserting, *not* the other one: a line built by
    /// concatenating both names and then filtering would pass a test that only
    /// checked the name it wanted was present.
    #[test]
    fn one_missing_registration_names_only_that_feature() {
        for locale in [Locale::Ja, Locale::En] {
            let toasts = install_feature_notifications(locale);
            let thumbnails = install_feature_thumbnails(locale);

            let aumid_missing = unregistered_features_line(locale, false, true);
            assert!(
                aumid_missing.contains(toasts),
                "{locale:?} AUMID-only line is missing the notifications name: {aumid_missing}"
            );
            assert!(
                !aumid_missing.contains(thumbnails),
                "{locale:?} AUMID-only line named thumbnails too: {aumid_missing}"
            );

            let thumbnail_missing = unregistered_features_line(locale, true, false);
            assert!(
                thumbnail_missing.contains(thumbnails),
                "{locale:?} handler-only line is missing the thumbnail name: {thumbnail_missing}"
            );
            assert!(
                !thumbnail_missing.contains(toasts),
                "{locale:?} handler-only line named notifications too: {thumbnail_missing}"
            );
        }
    }

    /// Both missing lists both, and the two names are joined rather than
    /// running together -- the join is the one part of this that reads
    /// differently for one name and for two.
    #[test]
    fn both_missing_lists_both_features_joined() {
        for (locale, join) in [(Locale::Ja, "と"), (Locale::En, " and ")] {
            let line = unregistered_features_line(locale, false, false);
            let expected = format!(
                "{}{join}{}",
                install_feature_notifications(locale),
                install_feature_thumbnails(locale)
            );
            assert!(
                line.contains(&expected),
                "{locale:?} did not join the two names with {join:?}: {line}"
            );
        }
    }

    /// AC 3: the row has to carry the state, not only the feature names. The
    /// Japanese is the user's own wording from the 2026-09-09 ruling.
    #[test]
    fn every_line_states_that_the_feature_is_off_because_it_is_unregistered() {
        for (locale, state) in [
            (Locale::Ja, "未登録のため無効"),
            (Locale::En, "not registered"),
        ] {
            for (aumid, handler) in [(false, true), (true, false), (false, false)] {
                let line = unregistered_features_line(locale, aumid, handler);
                assert!(
                    line.contains(state),
                    "{locale:?} ({aumid}, {handler}) does not say {state:?}: {line}"
                );
            }
        }
    }

    /// The composed English line, for the reason `english_labels` checks the
    /// static ones: it is assembled from two other strings, so a Japanese name
    /// left in either arrives here.
    #[test]
    fn the_english_line_has_no_japanese_left_in_it() {
        for (aumid, handler) in [(false, true), (true, false), (false, false)] {
            let line = unregistered_features_line(Locale::En, aumid, handler);
            assert!(
                !line.chars().any(|c| matches!(c,
                        '\u{3040}'..='\u{30ff}'
                        | '\u{3000}'..='\u{303f}'
                        | '\u{4e00}'..='\u{9fff}'
                        | '\u{ff00}'..='\u{ffef}')),
                "English line ({aumid}, {handler}) still contains Japanese: {line}"
            );
        }
    }
}

#[test]
fn a_hotkey_chord_is_one_cap_per_key() {
    assert_eq!(split_hotkey("Ctrl+Shift+R"), ["Ctrl", "Shift", "R"]);
    assert_eq!(split_hotkey(" Ctrl + F9 "), ["Ctrl", "F9"]);
    assert_eq!(split_hotkey("F8"), ["F8"]);
    assert!(split_hotkey("").is_empty());
}
