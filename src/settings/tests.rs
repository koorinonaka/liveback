use super::*;

fn stored(json: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(json).expect("test fixture must be valid JSON") {
        Value::Object(map) => map,
        other => panic!("test fixture must be an object, got {other}"),
    }
}

fn temp_path(name: &str) -> PathBuf {
    // The helper hands back a path that is ready to write to, so it makes the parent
    // directory here rather than leaving every caller to remember to do it first.
    let dir = std::env::temp_dir().join("livia-settings-tests");
    fs::create_dir_all(&dir).expect("test temp dir must be creatable");
    let path = dir.join(name);
    let _ = fs::remove_file(&path);
    path
}

/// The exact record the shipping app wrote, captured from
/// `%APPDATA%\com.liveback.desktop\settings.json`.
const REAL_FILE: &str = r#"{
  "settings": {
    "autoStart": false,
    "clipHotkey": "Ctrl+Shift+S",
    "clipSeconds": 30,
    "diagnosticsDrawerOpen": true,
    "frameRate": 60,
    "hotkey": "Ctrl+Shift+R",
    "includeCursor": true,
    "lastCaptureExecutable": "spike_d3d_window.exe",
    "muted": false,
    "retentionCapacityGb": 20,
    "retentionMinutes": 15,
    "sessionLifetimeDays": 30,
    "sessionViewMode": "list",
    "version": 2,
    "volumePercent": 100,
    "windowState": {
      "height": 1041,
      "width": 1936,
      "x": 0,
      "y": 0
    }
  }
}"#;

#[test]
fn reads_the_record_the_frontend_actually_wrote() {
    let path = temp_path("real.json");
    fs::write(&path, REAL_FILE).unwrap();
    let settings = load(&path);
    assert_eq!(settings.version, 2);
    assert_eq!(settings.retention_minutes, 15);
    assert_eq!(settings.frame_rate, 60);
    assert!(settings.include_cursor);
    assert!(settings.diagnostics_drawer_open);
    assert_eq!(settings.hotkey, "Ctrl+Shift+R");
    // `clipHotkey` / `clipSeconds` are still in REAL_FILE above and are
    // simply not read any more (task1090) -- which is the compatibility
    // claim: an old record loads without complaint.
    assert_eq!(settings.session_view_mode, SessionViewMode::List);
    assert_eq!(settings.volume_percent, 100);
    assert_eq!(settings.retention_capacity_gb, 20);
    assert_eq!(settings.session_lifetime_days, 30);
    assert_eq!(
        settings.last_capture_executable.as_deref(),
        Some("spike_d3d_window.exe")
    );
    // The record predates task2650's `maximized` key: it has to read back as
    // `false` rather than sinking the whole `windowState` (and with it every
    // setting) to defaults.
    assert_eq!(
        settings.window_state,
        Some(WindowState {
            height: 1041,
            maximized: false,
            width: 1936,
            x: 0,
            y: 0
        })
    );
    assert_eq!(settings.export_output_directory, None);
}

/// Byte-identical apart from the two keys task1090 retired: everything the
/// build still knows about survives a load/save untouched, and the dead keys
/// drop out on the way. `REAL_FILE` stays as the shipping record wrote it --
/// it is the thing being tested against, not a fixture to keep up to date.
#[test]
fn rewriting_an_unchanged_record_only_drops_the_retired_keys() {
    let path = temp_path("roundtrip-file.json");
    fs::write(&path, REAL_FILE).unwrap();
    let settings = load(&path);
    save(&path, &settings).unwrap();
    let expected = REAL_FILE
        .lines()
        .filter(|line| !line.contains("clipHotkey") && !line.contains("clipSeconds"))
        .collect::<Vec<_>>()
        .join(
            "
",
        );
    assert_eq!(fs::read_to_string(&path).unwrap(), expected);
}

#[test]
fn absent_optional_fields_are_not_written_back_as_null() {
    let path = temp_path("no-optionals.json");
    save(&path, &AppSettings::default()).unwrap();
    let written = fs::read_to_string(&path).unwrap();
    assert!(!written.contains("exportOutputDirectory"), "{written}");
    assert!(!written.contains("lastCaptureExecutable"), "{written}");
    assert!(!written.contains("windowState"), "{written}");
}

#[test]
fn save_preserves_other_store_keys() {
    let path = temp_path("other-keys.json");
    fs::write(
        &path,
        r#"{"somethingElse":{"a":1},"settings":{"version":2}}"#,
    )
    .unwrap();
    save(&path, &AppSettings::default()).unwrap();
    let value: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(value["somethingElse"]["a"], 1);
}

#[test]
fn missing_file_yields_defaults() {
    assert_eq!(load(&temp_path("absent.json")), AppSettings::default());
}

#[test]
fn corrupt_json_yields_defaults() {
    let path = temp_path("corrupt.json");
    fs::write(&path, "{ not json at all ").unwrap();
    assert_eq!(load(&path), AppSettings::default());
}

#[test]
fn missing_settings_key_yields_defaults_without_migrating() {
    let path = temp_path("no-settings-key.json");
    fs::write(&path, r#"{"other":{}}"#).unwrap();
    let (settings, error) = load_and_migrate(&path);
    assert!(error.is_none());
    assert_eq!(settings, AppSettings::default());
    assert_eq!(fs::read_to_string(&path).unwrap(), r#"{"other":{}}"#);
}

#[test]
fn out_of_range_values_are_clamped() {
    let settings = AppSettings::from_stored(&stored(
        r#"{
            "version": 2,
            "retentionMinutes": 100000,
            "retentionCapacityGb": -5,
            "sessionLifetimeDays": 9999,
            "volumePercent": 250,
            "frameRate": 144
        }"#,
    ));
    assert_eq!(settings.retention_minutes, MAX_RETENTION_MINUTES);
    assert_eq!(settings.retention_capacity_gb, MIN_RETENTION_CAPACITY_GB);
    assert_eq!(settings.session_lifetime_days, MAX_SESSION_LIFETIME_DAYS);
    assert_eq!(settings.volume_percent, MAX_VOLUME_PERCENT);
    assert_eq!(settings.frame_rate, DEFAULT_FRAME_RATE);
}

#[test]
fn non_integer_numbers_round_like_the_frontend() {
    let settings = AppSettings::from_stored(&stored(r#"{"version":2,"retentionMinutes":15.6}"#));
    assert_eq!(settings.retention_minutes, 16);
}

#[test]
fn wrong_typed_fields_fall_back_per_field() {
    let settings = AppSettings::from_stored(&stored(
        r#"{"version":2,"retentionMinutes":"lots","muted":"yes","includeCursor":false}"#,
    ));
    assert_eq!(settings.retention_minutes, DEFAULT_RETENTION_MINUTES);
    assert!(!settings.muted);
    // The neighbouring valid field survives instead of being discarded.
    assert!(!settings.include_cursor);
}

#[test]
fn version_one_migrates_to_thumbnail_and_is_written_back() {
    let path = temp_path("v1.json");
    fs::write(
        &path,
        r#"{"settings":{"version":1,"sessionViewMode":"list"}}"#,
    )
    .unwrap();
    let (settings, error) = load_and_migrate(&path);
    assert!(error.is_none());
    assert_eq!(settings.version, SETTINGS_VERSION);
    assert_eq!(settings.session_view_mode, SessionViewMode::Thumbnail);
    let reloaded = load(&path);
    assert_eq!(reloaded, settings);
    assert_eq!(reloaded.version, SETTINGS_VERSION);
}

/// A stored record with no `version` key at all is still a migration
/// (`from_stored` treats missing as version 0), so it must be written back
/// like any other pre-v2 file.
#[test]
fn versionless_record_is_written_back() {
    let path = temp_path("versionless.json");
    fs::write(&path, r#"{"settings":{"sessionViewMode":"list"}}"#).unwrap();
    let (settings, error) = load_and_migrate(&path);
    assert!(error.is_none());
    assert_eq!(settings.version, SETTINGS_VERSION);
    let reloaded = load(&path);
    assert_eq!(reloaded, settings);
    assert_eq!(reloaded.version, SETTINGS_VERSION);
}

#[test]
fn missing_version_migrates_too() {
    let settings = AppSettings::from_stored(&stored(r#"{"sessionViewMode":"list"}"#));
    assert_eq!(settings.version, SETTINGS_VERSION);
    assert_eq!(settings.session_view_mode, SessionViewMode::Thumbnail);
}

#[test]
fn a_version_two_list_choice_is_not_bounced_to_thumbnail() {
    let settings = AppSettings::from_stored(&stored(r#"{"version":2,"sessionViewMode":"list"}"#));
    assert_eq!(settings.session_view_mode, SessionViewMode::List);
}

#[test]
fn an_unrecognised_view_mode_falls_back_to_the_default() {
    let settings = AppSettings::from_stored(&stored(r#"{"version":2,"sessionViewMode":"grid"}"#));
    assert_eq!(settings.session_view_mode, SessionViewMode::List);
}

#[test]
fn an_empty_marker_hotkey_falls_back() {
    let settings = AppSettings::from_stored(&stored(r#"{"version":2,"hotkey":"   "}"#));
    assert_eq!(settings.hotkey, DEFAULT_HOTKEY);
}

/// Task1090 removed clip saving. A record that still carries its two keys has
/// to load without complaint -- there is no `deny_unknown_fields` and
/// `from_stored` reads by name, so both are simply ignored.
#[test]
fn a_record_that_still_carries_the_clip_keys_loads_fine() {
    let settings = AppSettings::from_stored(&stored(
        r#"{"version":2,"hotkey":"Ctrl+Alt+M","clipHotkey":"Ctrl+Alt+M","clipSeconds":45}"#,
    ));
    assert_eq!(settings.hotkey, "Ctrl+Alt+M");

    // ...and through the typed path too, which is what `load` uses.
    let path = temp_path("legacy-clip.json");
    std::fs::write(
        &path,
        r#"{"settings":{"version":3,"hotkey":"Ctrl+Alt+M","clipHotkey":"Ctrl+Alt+M","clipSeconds":45}}"#,
    )
    .unwrap();
    assert_eq!(load(&path).hotkey, "Ctrl+Alt+M");
}

#[test]
fn save_then_load_round_trips_every_field() {
    let path = temp_path("roundtrip.json");
    let settings = AppSettings {
        // Non-empty on purpose, like the flags below: an empty list would pass
        // even if the key never landed (task1970). With metadata (task2560),
        // so the struct shape rides the same round trip.
        auto_capture_executables: vec![AutoCaptureApp {
            // Non-default (task2570): false would pass even if the key never
            // landed.
            capture_monitor: true,
            display_name: Some("サンプルゲーム".into()),
            name: "game_dx11.exe".into(),
            path: Some(r"D:\games\game_dx11.exe".into()),
        }],
        // Non-empty for the same reason (task3600), and with the flag on: a
        // default-valued entry would pass even if the key never landed.
        auto_capture_folders: vec![AutoCaptureFolder {
            capture_monitor: true,
            path: r"D:\Games\Foo".into(),
        }],
        auto_start: true,
        buffer_directory: Some(r"D:\buffer".into()),
        // Non-default on purpose, like the two flags below: `false` would pass
        // even if the key never landed (task1430).
        capture_all_audio: true,
        // Non-default for the same reason: H.264 would pass even if the key
        // never landed (task1760).
        codec: RecordingCodec::Av1,
        // Non-default for the same reason as the folders around it: `None` is
        // skipped on write and would pass even if the key never landed
        // (task2970).
        clip_directory: Some(r"D:\Clips".into()),
        diagnostics_drawer_open: true,
        export_output_directory: Some(r"D:\clips".into()),
        frame_rate: 30,
        hotkey: "Ctrl+Alt+M".into(),
        include_cursor: false,
        language: "en".into(),
        last_capture_executable: Some("game.exe".into()),
        muted: true,
        retention_capacity_gb: 200,
        retention_minutes: 45,
        // Non-default on purpose, same reason as the panel flag below:
        // would pass even if the key never landed (round8 §3-1).
        ring_buffer_enabled: false,
        // Non-default on purpose: the round trip has to prove the *closed*
        // state survives, since `true` would pass even if the key never landed.
        review_panel_open: false,
        session_lifetime_days: 7,
        session_view_mode: SessionViewMode::Thumbnail,
        // Non-default, like the flags above: `false` is skipped on the way out
        // and would pass even if the key never landed.
        clip_month_grouping: true,
        // Non-default on purpose, same reason as the flags around it: `system`
        // is skipped on write and would pass even if the key never landed
        // (round16).
        theme: "light".into(),
        // All three non-default on purpose, same reason as the flags around
        // them (task t260920-2be2): the check flag is skipped on write while it
        // is `true`, and the other two while they are `false` / `None`, so the
        // default value of each would pass even if the key never landed.
        update_check_enabled: false,
        update_check_prompted: true,
        update_last_checked: Some(1_789_000_000),
        version: SETTINGS_VERSION,
        video_stage_hint_shown: Some(2),
        volume_percent: 42,
        // Maximized on purpose, same reason as the two flags above: `false`
        // would pass even if the key never landed (task2650).
        window_state: Some(WindowState {
            height: 800,
            maximized: true,
            width: 1200,
            x: 10,
            y: 20,
        }),
    };
    save(&path, &settings).unwrap();
    assert_eq!(load(&path), settings);
}

#[test]
fn frame_rates_match_what_the_capture_throttle_accepts() {
    for rate in 0u8..=120 {
        assert_eq!(
            crate::capture::FrameThrottle::new(rate).is_ok(),
            FRAME_RATES.contains(&rate),
            "frame rate {rate} disagrees with FrameThrottle::new"
        );
    }
}

#[test]
fn retention_bounds_match_the_ring_buffer() {
    let too_small = AppSettings::from_stored(&stored(r#"{"version":2,"retentionMinutes":0}"#));
    assert_eq!(too_small.retention_minutes, MIN_RETENTION_MINUTES);
    let too_large = AppSettings::from_stored(&stored(r#"{"version":2,"retentionMinutes":1441}"#));
    assert_eq!(too_large.retention_minutes, MAX_RETENTION_MINUTES);
    assert_eq!(
        AppSettings::default().retention_minutes,
        DEFAULT_RETENTION_MINUTES
    );
}

#[test]
fn default_path_lands_under_the_bundle_identifier() {
    let Some(path) = default_path() else {
        return;
    };
    assert!(
        path.ends_with(format!("{BUNDLE_IDENTIFIER}\\{STORE_FILE}")),
        "{path:?}"
    );
}

/// Task1080 flipped the default: recording continuously is opted into, so a
/// record with no key at all comes back off. Round8 §3-1 read the same absence
/// as "on"; existing installs therefore land on off once, which is the
/// accepted cost of the flip.
#[test]
fn a_record_without_the_ring_buffer_key_reads_as_off() {
    let stored = stored(r#"{"version":3}"#);
    assert!(!AppSettings::from_stored(&stored).ring_buffer_enabled);
    assert!(!AppSettings::default().ring_buffer_enabled);

    // And the default is still the state that is not written, so a saved
    // record stays as small as it was.
    let path = temp_path("ring-buffer.json");
    save(&path, &AppSettings::default()).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("ringBufferEnabled"),
        "the default must not be written: {text}"
    );
}

/// Both explicit values are honoured, and the non-default one is written --
/// otherwise a user who turned the ring on would find it off next launch.
#[test]
fn an_explicit_ring_buffer_key_is_honoured_and_the_on_state_is_written() {
    assert!(
        AppSettings::from_stored(&stored(r#"{"version":3,"ringBufferEnabled":true}"#))
            .ring_buffer_enabled
    );
    assert!(
        !AppSettings::from_stored(&stored(r#"{"version":3,"ringBufferEnabled":false}"#))
            .ring_buffer_enabled
    );

    let path = temp_path("ring-buffer-on.json");
    let on = AppSettings {
        ring_buffer_enabled: true,
        ..Default::default()
    };
    save(&path, &on).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains(r#""ringBufferEnabled": true"#), "{text}");
}

/// round8 §3-2: 0 is 「なし」 on the two retention limits and must survive both
/// validation paths. Everything else under the range still clamps up, so a
/// corrupt record is unchanged by this.
#[test]
fn zero_is_the_only_retention_value_that_is_not_clamped_up() {
    let none = AppSettings {
        retention_capacity_gb: 0,
        session_lifetime_days: 0,
        ..AppSettings::default()
    };
    let validated = none.validated();
    assert_eq!(validated.retention_capacity_gb, 0);
    assert_eq!(validated.session_lifetime_days, 0);

    let zeroes = stored(r#"{"retentionCapacityGb":0,"sessionLifetimeDays":0}"#);
    let loaded = AppSettings::from_stored(&zeroes);
    assert_eq!(loaded.retention_capacity_gb, 0);
    assert_eq!(loaded.session_lifetime_days, 0);

    // Under the range but not the sentinel: still the old behaviour.
    let negative = stored(r#"{"retentionCapacityGb":-5,"sessionLifetimeDays":-1}"#);
    let loaded = AppSettings::from_stored(&negative);
    assert_eq!(loaded.retention_capacity_gb, MIN_RETENTION_CAPACITY_GB);
    assert_eq!(loaded.session_lifetime_days, MIN_SESSION_LIFETIME_DAYS);

    let too_small = AppSettings {
        retention_capacity_gb: -5,
        session_lifetime_days: -1,
        ..AppSettings::default()
    }
    .validated();
    assert_eq!(too_small.retention_capacity_gb, MIN_RETENTION_CAPACITY_GB);
    assert_eq!(too_small.session_lifetime_days, MIN_SESSION_LIFETIME_DAYS);
}

/// Task1430: the toggle that replaced the per-application track list. Both
/// readers -- serde and the stored-map path -- have to agree, and both have to
/// read an absent key as off.
#[test]
fn capture_all_audio_round_trips_and_defaults_to_off() {
    // Off is not written at all, so a record made before this task does not
    // gain the key just by being loaded and saved.
    let json = serde_json::to_string(&AppSettings::default().validated()).unwrap();
    assert!(!json.contains("captureAllAudio"), "{json}");

    let on = AppSettings {
        capture_all_audio: true,
        ..AppSettings::default()
    }
    .validated();
    let json = serde_json::to_string(&on).unwrap();
    assert!(json.contains(r#""captureAllAudio":true"#), "{json}");

    let stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"captureAllAudio": true}"#).unwrap();
    assert!(AppSettings::from_stored(&stored).capture_all_audio);

    let stored: serde_json::Map<String, serde_json::Value> = serde_json::from_str(r#"{}"#).unwrap();
    assert!(!AppSettings::from_stored(&stored).capture_all_audio);
}

/// Task1970: the list of executables whose launch records itself. Nobody has
/// registered anything on a fresh install, so the key must not appear in a
/// file that predates the feature just because it was loaded and saved -- and
/// the stored-map reader has to tolerate a hand-edited array, which is how
/// this task is verified before task1980 builds the UI for it.
#[test]
fn auto_capture_executables_round_trip_and_stay_absent_while_empty() {
    let json = serde_json::to_string(&AppSettings::default().validated()).unwrap();
    assert!(!json.contains("autoCaptureExecutables"), "{json}");

    // A metadata-less entry stays a bare string on the way out (task2560):
    // a hand-edited file must not change shape on its first save.
    let registered = AppSettings {
        auto_capture_executables: vec![AutoCaptureApp::named("game_dx11.exe")],
        ..AppSettings::default()
    }
    .validated();
    let json = serde_json::to_string(&registered).unwrap();
    assert!(
        json.contains(r#""autoCaptureExecutables":["game_dx11.exe"]"#),
        "{json}"
    );

    let stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"autoCaptureExecutables": ["a.exe", "b.exe"]}"#).unwrap();
    assert_eq!(
        AppSettings::from_stored(&stored).auto_capture_executables,
        vec![
            AutoCaptureApp::named("a.exe"),
            AutoCaptureApp::named("b.exe")
        ]
    );

    // One bad element loses that element, not the whole registration: a typo
    // in a hand-edited file must not silently switch the feature off.
    let stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"autoCaptureExecutables": ["a.exe", 7]}"#).unwrap();
    assert_eq!(
        AppSettings::from_stored(&stored).auto_capture_executables,
        vec![AutoCaptureApp::named("a.exe")]
    );

    let stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"autoCaptureExecutables": "a.exe"}"#).unwrap();
    assert!(AppSettings::from_stored(&stored)
        .auto_capture_executables
        .is_empty());

    let stored: serde_json::Map<String, serde_json::Value> = serde_json::from_str(r#"{}"#).unwrap();
    assert!(AppSettings::from_stored(&stored)
        .auto_capture_executables
        .is_empty());
}

/// Task3600: the folder rules are their own key, absent while empty for the
/// same reason `autoCaptureExecutables` is -- a file written before this task
/// must not gain the key just by being loaded and saved. Registered by hand
/// until task3610 builds the button, so the stored-map reader has to tolerate a
/// hand-edited array the same way.
#[test]
fn auto_capture_folders_round_trip_and_stay_absent_while_empty() {
    let json = serde_json::to_string(&AppSettings::default().validated()).unwrap();
    assert!(!json.contains("autoCaptureFolders"), "{json}");

    // A rule with default flags keeps the shape a person would write.
    let registered = AppSettings {
        auto_capture_folders: vec![AutoCaptureFolder::at(r"D:\Games\Foo")],
        ..AppSettings::default()
    }
    .validated();
    let json = serde_json::to_string(&registered).unwrap();
    assert!(
        json.contains(r#""autoCaptureFolders":[{"path":"D:\\Games\\Foo"}]"#),
        "{json}"
    );
    // ...and `captureMonitor` appears only once it is on.
    let json = serde_json::to_string(
        &AppSettings {
            auto_capture_folders: vec![AutoCaptureFolder {
                capture_monitor: true,
                path: r"D:\Games\Foo".into(),
            }],
            ..AppSettings::default()
        }
        .validated(),
    )
    .unwrap();
    assert!(
        json.contains(r#""autoCaptureFolders":[{"captureMonitor":true,"path":"D:\\Games\\Foo"}]"#),
        "{json}"
    );

    let record = stored(r#"{"autoCaptureFolders": [{"path": "D:\\Games\\Foo"}]}"#);
    assert_eq!(
        AppSettings::from_stored(&record).auto_capture_folders,
        vec![AutoCaptureFolder::at(r"D:\Games\Foo")]
    );

    // One bad element loses that element, not the whole list: a typo in a
    // hand-edited file must not silently switch the feature off. An entry with
    // no `path` is one of those -- there is nothing to match against.
    let record = stored(
        r#"{"autoCaptureFolders": [{"path": "D:\\Games\\Foo"}, 7, {"captureMonitor": true}]}"#,
    );
    assert_eq!(
        AppSettings::from_stored(&record).auto_capture_folders,
        vec![AutoCaptureFolder::at(r"D:\Games\Foo")]
    );

    // Not an array, and absent: both are "nothing registered".
    let record = stored(r#"{"autoCaptureFolders": "D:\\Games\\Foo"}"#);
    assert!(AppSettings::from_stored(&record)
        .auto_capture_folders
        .is_empty());
    let record = stored(r#"{}"#);
    assert!(AppSettings::from_stored(&record)
        .auto_capture_folders
        .is_empty());
}

/// Task2560: entries are string-or-struct, both ways round. A bare string and
/// a metadata struct can share one list, each keeping its own shape across a
/// serialize-deserialize round trip.
#[test]
fn auto_capture_entries_round_trip_string_and_struct_shapes() {
    let with_metadata = AutoCaptureApp {
        capture_monitor: false,
        display_name: Some("ペイント".into()),
        name: "mspaint.exe".into(),
        path: Some(r"C:\Windows\System32\mspaint.exe".into()),
    };
    // Struct out, alphabetical camelCase keys; string out for the plain one.
    assert_eq!(
        serde_json::to_string(&with_metadata).unwrap(),
        r#"{"displayName":"ペイント","name":"mspaint.exe","path":"C:\\Windows\\System32\\mspaint.exe"}"#
    );
    assert_eq!(
        serde_json::to_string(&AutoCaptureApp::named("game.exe")).unwrap(),
        r#""game.exe""#
    );

    // A mixed list survives a full round trip element by element.
    let mixed = vec![AutoCaptureApp::named("game.exe"), with_metadata.clone()];
    let json = serde_json::to_string(&mixed).unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<AutoCaptureApp>>(&json).unwrap(),
        mixed
    );

    // Partial metadata is still a struct -- only the all-None entry collapses
    // back to a string.
    let path_only = AutoCaptureApp {
        path: Some(r"C:\game\game.exe".into()),
        ..AutoCaptureApp::named("game.exe")
    };
    let json = serde_json::to_string(&path_only).unwrap();
    assert_eq!(json, r#"{"name":"game.exe","path":"C:\\game\\game.exe"}"#);
    assert_eq!(
        serde_json::from_str::<AutoCaptureApp>(&json).unwrap(),
        path_only
    );

    // The monitor flag (task2570) forces the struct shape even with no other
    // metadata -- a bare string has nowhere to carry it -- and stays absent
    // while false. Key first: alphabetical camelCase.
    let monitor = AutoCaptureApp {
        capture_monitor: true,
        ..AutoCaptureApp::named("editor.exe")
    };
    let json = serde_json::to_string(&monitor).unwrap();
    assert_eq!(json, r#"{"captureMonitor":true,"name":"editor.exe"}"#);
    assert_eq!(
        serde_json::from_str::<AutoCaptureApp>(&json).unwrap(),
        monitor
    );

    // A struct without a name is a bad element, not a nameless registration --
    // `auto_capture_apps` drops it the way it drops a number.
    assert!(serde_json::from_str::<AutoCaptureApp>(r#"{"path":"C:\\x.exe"}"#).is_err());
    let stored: serde_json::Map<String, serde_json::Value> = serde_json::from_str(
        r#"{"autoCaptureExecutables": [{"path":"C:\\x.exe"}, {"name":"kept.exe"}]}"#,
    )
    .unwrap();
    assert_eq!(
        AppSettings::from_stored(&stored).auto_capture_executables,
        vec![AutoCaptureApp::named("kept.exe")]
    );
}

/// Task1760: AV1 is opt-in, so the key exists only once somebody opted in.
/// Every other value -- absent, misspelled, a codec this build never had --
/// has to land on H.264, the one codec every machine can play back.
#[test]
fn codec_round_trips_and_defaults_to_h264() {
    let json = serde_json::to_string(&AppSettings::default().validated()).unwrap();
    assert!(!json.contains("codec"), "{json}");

    let av1 = AppSettings {
        codec: RecordingCodec::Av1,
        ..AppSettings::default()
    }
    .validated();
    let json = serde_json::to_string(&av1).unwrap();
    assert!(json.contains(r#""codec":"av1""#), "{json}");
    // The save path must not quietly reset a codec the user chose.
    assert_eq!(av1.codec, RecordingCodec::Av1);

    assert_eq!(
        AppSettings::from_stored(&stored(r#"{"codec":"av1"}"#)).codec,
        RecordingCodec::Av1
    );
    for fixture in [r#"{}"#, r#"{"codec":"hevc"}"#, r#"{"codec":7}"#] {
        assert_eq!(
            AppSettings::from_stored(&stored(fixture)).codec,
            RecordingCodec::H264,
            "{fixture}"
        );
    }
}

/// The per-application track list is gone (task1430), but the settings.json
/// files that hold it are not. Both readers have to walk straight past the old
/// key rather than failing over it, and land on the new toggle's default.
#[test]
fn a_settings_file_holding_the_removed_audio_track_list_still_loads() {
    let path = temp_path("legacy-audio-tracks.json");
    std::fs::write(
        &path,
        r#"{"settings":{"version":3,"hotkey":"Ctrl+Alt+M","audioTrackExecutables":["Discord.exe","chrome.exe"]}}"#,
    )
    .unwrap();
    let loaded = load(&path);
    assert_eq!(loaded.hotkey, "Ctrl+Alt+M");
    assert!(!loaded.capture_all_audio);

    let stored: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(r#"{"audioTrackExecutables": ["a.exe", 7]}"#).unwrap();
    assert!(!AppSettings::from_stored(&stored).capture_all_audio);
}
