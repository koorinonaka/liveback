//! The settings page (task126): settings-file plumbing shared by every
//! screen (`settings_path` / `settings_snapshot` / `commit_settings`), the
//! labels and renderer, and the callback wiring moved out of `main`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use livia::capture::CaptureController;
use livia::settings::{self, AppSettings, AutoCaptureApp};
use livia::ui_state::auto_capture;
use livia::ui_state::locale;
use livia::ui_state::sessions as sessions_ui;
use livia::ui_state::settings as settings_ui;
use livia::ui_state::toast;
use slint::ComponentHandle;

use super::desktop;
use super::hotkeys::Hotkeys;
use super::shell::open_directory;
use super::tr_locale;
use super::{AppWindow, AutoCaptureEntry, SettingsLabels, SettingsVm, Tokens, TrayIcon};

/// UI-thread state for the settings screen (task126). Only what the pure
/// settings module cannot hold: the unsaved hotkey drafts and which capture
/// control is armed. Everything else round-trips through `settings.json`.
struct SettingsScreen {
    draft_hotkey: String,
    /// 0 = none, 1 = the marker row. Matches the ints the .slint side uses;
    /// 2 was the clip row until task1090 removed it.
    armed: i32,
    /// The folder the user picked, waiting on the confirmation. `None` when
    /// the dialog is closed.
    buffer_confirm: Option<String>,
    /// What the two retention rows go back to when their switch comes on again
    /// (task1490), for the same reason: off is stored as `0`, so the number
    /// that was in force has nowhere in the file to live. Seeded from the
    /// stored value, or the shipped default when that is already off.
    retention_capacity_last: i64,
    session_lifetime_last: i64,
    /// The chord the OS just refused as it was typed (t260928-7e9a, DS
    /// HotkeyField: the field goes danger and says why, the original chord
    /// stays on it). Cleared by the next press on the field or its blur.
    refused_hotkey: Option<String>,
    /// The folder picker returned a folder the auto-record guard turns down.
    /// Said under the 自動録画 group until the next add.
    folder_refused: bool,
}

pub(super) fn settings_path() -> Option<std::path::PathBuf> {
    settings::default_path()
}

pub(super) fn settings_snapshot(settings: &Mutex<AppSettings>) -> AppSettings {
    settings
        .lock()
        .map(|current| current.clone())
        .unwrap_or_default()
}

/// Mutate → save → reload, handing back what the file now holds: any clamp or
/// hotkey-collision fix the validation applied (task121 owns those rules) is
/// what the UI shows, not what was asked for.
pub(super) fn commit_settings(
    settings: &Mutex<AppSettings>,
    mutate: impl FnOnce(&mut AppSettings),
) -> AppSettings {
    let Ok(mut current) = settings.lock() else {
        return AppSettings::default();
    };
    mutate(&mut current);
    if let Some(path) = settings_path() {
        if let Err(error) = settings::save(&path, &current) {
            eprintln!("failed to save settings: {error}");
        }
        *current = settings::load(&path);
    } else {
        // No %APPDATA%: keep the in-memory copy validated at least.
        *current = current.validated();
    }
    current.clone()
}

/// `Ctrl+Shift+R` as the three caps the row draws (round8 §3-3), with `Super`
/// shown as `Win` (`settings_ui::hotkey_keys`). The one place a chord becomes
/// Keycap rows: the settings row and the review marker note both use it.
pub(super) fn hotkey_caps(chord: &str) -> slint::ModelRc<slint::SharedString> {
    let caps: Vec<slint::SharedString> = settings_ui::hotkey_keys(chord)
        .into_iter()
        .map(slint::SharedString::from)
        .collect();
    slint::ModelRc::new(slint::VecModel::from(caps))
}

/// Static labels and ranges, set once. The ranges come from the same constants
/// task121's clamps use, so the display and the validation cannot drift.
pub(super) fn push_settings_labels(ui: &AppWindow) {
    let labels = ui.global::<SettingsLabels>();
    labels.set_group_capture(settings_ui::group_capture(tr_locale()).into());
    labels.set_group_retention(settings_ui::group_retention(tr_locale()).into());
    labels.set_group_hotkey(settings_ui::group_hotkey(tr_locale()).into());
    labels.set_group_hotkey_sub(settings_ui::group_hotkey_sub(tr_locale()).into());
    labels.set_group_where(settings_ui::group_where(tr_locale()).into());
    labels.set_group_general(settings_ui::group_general(tr_locale()).into());
    labels.set_storage_deletable(settings_ui::storage_deletable(tr_locale()).into());
    labels.set_storage_free(settings_ui::storage_free(tr_locale()).into());
    labels.set_retention_minutes(settings_ui::retention_minutes(tr_locale()).into());
    labels.set_retention_minutes_sub(settings_ui::retention_minutes_sub(tr_locale()).into());
    labels.set_frame_rate(settings_ui::frame_rate(tr_locale()).into());
    labels.set_ring_buffer(settings_ui::ring_buffer(tr_locale()).into());
    labels.set_ring_buffer_sub(settings_ui::ring_buffer_sub(tr_locale()).into());
    labels.set_include_cursor(settings_ui::include_cursor(tr_locale()).into());
    labels.set_include_cursor_sub(settings_ui::include_cursor_sub(tr_locale()).into());
    labels.set_capture_all_audio(settings_ui::capture_all_audio(tr_locale()).into());
    labels.set_capture_all_audio_sub(settings_ui::capture_all_audio_sub(tr_locale()).into());
    labels.set_codec_av1(settings_ui::codec_av1(tr_locale()).into());
    labels.set_codec_av1_sub(settings_ui::codec_av1_sub(tr_locale()).into());
    labels.set_auto_start(settings_ui::auto_start(tr_locale()).into());
    labels.set_auto_start_sub(settings_ui::auto_start_sub(tr_locale()).into());
    labels.set_auto_capture_rejected(auto_capture::settings_folder_rejected(tr_locale()).into());
    labels.set_auto_capture(auto_capture::settings_label(tr_locale()).into());
    labels.set_auto_capture_sub(auto_capture::settings_sub(tr_locale()).into());
    labels.set_auto_capture_empty(auto_capture::settings_empty(tr_locale()).into());
    labels.set_auto_capture_add_folder(auto_capture::settings_add_folder(tr_locale()).into());
    labels.set_auto_capture_ignored(auto_capture::settings_folder_ignored(tr_locale()).into());
    labels.set_auto_capture_remove_hint(auto_capture::settings_remove_hint(tr_locale()).into());
    labels.set_auto_capture_folder_all(auto_capture::settings_folder_all(tr_locale()).into());
    labels.set_auto_capture_window(auto_capture::settings_scope_window(tr_locale()).into());
    labels.set_auto_capture_screen(auto_capture::settings_scope_screen(tr_locale()).into());
    labels.set_retention_capacity(settings_ui::retention_capacity_gb(tr_locale()).into());
    labels.set_session_lifetime(settings_ui::session_lifetime_days(tr_locale()).into());
    labels.set_retention_hint(settings_ui::retention_hint(tr_locale()).into());
    labels.set_unit_minutes(settings_ui::unit_minutes(tr_locale()).into());
    labels.set_unit_gigabytes(settings_ui::unit_gigabytes(tr_locale()).into());
    labels.set_unit_days(settings_ui::unit_days(tr_locale()).into());
    labels.set_unit_seconds(settings_ui::unit_seconds(tr_locale()).into());
    let ranges = [
        settings_ui::RETENTION_MINUTES_RANGE,
        settings_ui::RETENTION_CAPACITY_GB_RANGE,
        settings_ui::SESSION_LIFETIME_DAYS_RANGE,
    ];
    let range_texts: Vec<slint::SharedString> = ranges
        .iter()
        .map(|(min, max)| settings_ui::numeric_range(*min, *max).into())
        .collect();
    let error_texts: Vec<slint::SharedString> = ranges
        .iter()
        .map(|(min, max)| settings_ui::numeric_out_of_range(tr_locale(), *min, *max).into())
        .collect();
    labels.set_retention_minutes_range(range_texts[0].clone());
    labels.set_retention_capacity_range(range_texts[1].clone());
    labels.set_session_lifetime_range(range_texts[2].clone());
    labels.set_retention_minutes_error(error_texts[0].clone());
    labels.set_retention_capacity_error(error_texts[1].clone());
    labels.set_session_lifetime_error(error_texts[2].clone());
    labels.set_retention_minutes_min(ranges[0].0 as f32);
    labels.set_retention_minutes_max(ranges[0].1 as f32);
    labels.set_retention_capacity_min(ranges[1].0 as f32);
    labels.set_retention_capacity_max(ranges[1].1 as f32);
    labels.set_session_lifetime_min(ranges[2].0 as f32);
    labels.set_session_lifetime_max(ranges[2].1 as f32);
    labels.set_hotkey(settings_ui::hotkey(tr_locale()).into());
    labels.set_capture_armed(settings_ui::hotkey_capture_armed(tr_locale()).into());
    labels.set_capture_abort(settings_ui::hotkey_capture_abort(tr_locale()).into());
    labels.set_state_failed(settings_ui::hotkey_state_failed(tr_locale()).into());
    labels.set_export_change(settings_ui::export_directory_change(tr_locale()).into());
    labels.set_directory_open(settings_ui::directory_open(tr_locale()).into());
    labels.set_buffer_label(settings_ui::buffer_directory_label(tr_locale()).into());
    labels.set_buffer_sub(settings_ui::buffer_directory_sub(tr_locale()).into());
    labels.set_clip_label(settings_ui::clip_directory_label(tr_locale()).into());
    labels.set_clip_sub(settings_ui::clip_directory_sub(tr_locale()).into());
    labels.set_buffer_change_blocked(
        settings_ui::buffer_directory_change_blocked(tr_locale()).into(),
    );
    // task4140: the 「インストール」 group. Read once behind a `OnceLock` and
    // logged once, so the second caller here -- a language change re-pushes
    // every label -- re-reads nothing: the registration cannot change without
    // the installer running, which stops this process first.
    //
    // An empty line is the installed machine, and the `.slint` side wraps the
    // group title and the row in one `if` on it. The reason and the title are
    // pushed either way; nothing draws them while the line is empty.
    let registration = desktop::installer_registration();
    labels.set_group_install(settings_ui::group_install(tr_locale()).into());
    labels.set_install_unregistered(
        settings_ui::unregistered_features_line(
            tr_locale(),
            registration.aumid,
            registration.thumbnail_handler,
        )
        .into(),
    );
    labels.set_install_unregistered_reason(
        settings_ui::install_unregistered_reason(tr_locale()).into(),
    );
    labels.set_directory_reset(settings_ui::directory_reset(tr_locale()).into());
    labels.set_confirm_ok(settings_ui::buffer_directory_confirm_ok(tr_locale()).into());
    labels.set_confirm_cancel(sessions_ui::discard_cancel(tr_locale()).into());
    // Round5 §7-B: chips carry the values people actually pick. Numbers only
    // -- the unit is stated once at the end of the row (task185), so repeating
    // it on five chips just made each chip wider than the number in it.
    labels.set_retention_minutes_presets(preset_labels(&settings_ui::RETENTION_MINUTES_PRESETS));
    // The frame rate is a SegmentedControl with no field beside it, so its
    // unit rides on each segment (DS SettingsScreen: 「30 fps」).
    labels.set_frame_rate_presets(slint::ModelRc::new(slint::VecModel::from(
        settings_ui::FRAME_RATE_PRESETS
            .iter()
            .map(|rate| {
                slint::SharedString::from(format!("{rate} {}", settings_ui::unit_fps(tr_locale())))
            })
            .collect::<Vec<_>>(),
    )));
    labels.set_language(locale::language_label(tr_locale()).into());
    // `LANGUAGE_SETTINGS` order: 「システム」, then the languages.
    let languages: Vec<slint::SharedString> = std::iter::once(locale::language_system(tr_locale()))
        .chain(locale::language_choices(tr_locale()))
        .map(slint::SharedString::from)
        .collect();
    labels.set_language_choices(slint::ModelRc::new(slint::VecModel::from(languages)));
    labels.set_theme(settings_ui::theme_label(tr_locale()).into());
    labels.set_theme_choices(slint::VecModel::from_slice(
        &settings_ui::theme_choices(tr_locale()).map(slint::SharedString::from),
    ));
    labels.set_retention_capacity_presets(preset_labels(&settings_ui::RETENTION_CAPACITY_PRESETS));
    labels.set_session_lifetime_presets(preset_labels(&settings_ui::SESSION_LIFETIME_PRESETS));
    labels.set_retention_minutes_placeholder(
        settings_ui::RETENTION_MINUTES_EXAMPLE.to_string().into(),
    );
    labels.set_retention_capacity_placeholder(
        settings_ui::RETENTION_CAPACITY_EXAMPLE.to_string().into(),
    );
    labels
        .set_session_lifetime_placeholder(settings_ui::SESSION_LIFETIME_EXAMPLE.to_string().into());
}

/// Chip captions: the bare number, on every row. The unit is the row's, not
/// the chip's (round5 §7-B as the mock draws it -- `5 / 10 / 20 / 40 / 60` then
/// one `分` at the end of the line).
fn preset_labels(presets: &[i64]) -> slint::ModelRc<slint::SharedString> {
    slint::ModelRc::new(slint::VecModel::from(
        presets
            .iter()
            .map(|value| slint::SharedString::from(value.to_string()))
            .collect::<Vec<_>>(),
    ))
}

/// The auto-capture list on the settings row (task1980).
///
/// Its own function because the settings page is not the only writer: the
/// review panel's 「起動したら自動で録画」 registers an executable (task2360) and
/// nothing re-renders this pane for it, exactly the split `picker::render`
/// already handles by pushing `buffer_change_enabled` itself.
///
/// Takes the settings store rather than a slice (task2560): rendering is also
/// when metadata-less entries get their one-time lazy upgrade, which has to be
/// persisted. Icons are re-read from the stored path on every push -- a few
/// `ExtractIconExW` calls per settings render, cheap enough to skip a cache.
pub(super) fn push_auto_capture_list(ui: &AppWindow, settings: &Mutex<AppSettings>) {
    let entries = upgrade_auto_capture_metadata(settings);
    let mut rows: Vec<AutoCaptureEntry> = entries
        .iter()
        .map(|entry| AutoCaptureEntry {
            capture_monitor: entry.capture_monitor,
            display: display_name(entry).into(),
            executable: entry.name.as_str().into(),
            // t260927-fde8: the row draws a bolt, not the app's own icon,
            // so none is extracted.
            icon: slint::Image::default(),
            kind: KIND_EXECUTABLE,
            // The row's second line: the folder the exe was seen in, when
            // metadata resolved one.
            parent_path: entry
                .path
                .as_deref()
                .and_then(|path| std::path::Path::new(path).parent())
                .map(|folder| folder.display().to_string())
                .unwrap_or_default()
                .into(),
            // task3730: the guard is a folder-rule question. An executable
            // registration names a process directly, so there is nothing for
            // it to be turned down by.
            ignored: false,
        })
        .collect();
    // task3610: the folder rules go on the end of the same model, so the two
    // kinds stay grouped -- a list that alternated them would be read as one
    // muddle rather than "these apps, then these folders". The icon does not
    // apply -- the row draws a folder glyph -- and since task3830 `display`
    // carries the rule's last folder name rather than an app name.
    rows.extend(
        settings_snapshot(settings)
            .auto_capture_folders
            .iter()
            .map(|folder| {
                // task3830: the rule is split here, not in the row -- the
                // spelling questions (trailing separator, a volume root with
                // nothing above it) belong beside the rest of the rule
                // handling. `executable` still carries the whole path: it is
                // what `folder-removed` reports, and the two halves are for
                // reading, not for identity.
                let (name, parent) = auto_capture::folder_row_parts(&folder.path);
                AutoCaptureEntry {
                    capture_monitor: folder.capture_monitor,
                    display: name.as_str().into(),
                    executable: folder.path.as_str().into(),
                    icon: slint::Image::default(),
                    kind: KIND_FOLDER,
                    parent_path: parent.as_str().into(),
                    // task3730: the row says so when `poll` is throwing this rule
                    // away. The judgement stays in `folder_rule_ignored`; this is
                    // the third *call* of it, not a third copy of it, and
                    // `FolderGuard::machine()` is why no signature here has to
                    // grow a guard parameter. The cost is one string test per
                    // rule on a list that is rebuilt every commit anyway.
                    ignored: auto_capture::folder_rule_ignored(
                        &folder.path,
                        auto_capture::FolderGuard::machine(),
                    ),
                }
            }),
    );
    ui.global::<SettingsVm>()
        .set_auto_capture_executables(slint::ModelRc::new(slint::VecModel::from(rows)));
}

/// `AutoCaptureEntry::kind` (task3610), matching the ints the `.slint` side
/// switches on.
const KIND_EXECUTABLE: i32 = 0;
const KIND_FOLDER: i32 = 1;

/// The row's app-name line. A metadata-less entry has none -- it keeps the
/// bare mono exe name, exactly as before task2560. An entry whose exe carried
/// no usable version info shows its name stem, so the row still reads as an
/// app rather than a file.
fn display_name(entry: &AutoCaptureApp) -> String {
    match (&entry.display_name, &entry.path) {
        (Some(name), _) => name.clone(),
        (None, Some(_)) => std::path::Path::new(&entry.name)
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default(),
        (None, None) => String::new(),
    }
}

/// Entries registered before task2560 carry no metadata. When such an entry's
/// exe happens to be running at render time, resolve and persist its path and
/// display name once -- after that the row draws from the store. An entry that
/// kept its path but lost its display name (task2590) re-resolves straight
/// from the file, no running process needed. Steady state (everything
/// upgraded, or nothing resolvable) writes nothing.
fn upgrade_auto_capture_metadata(settings: &Mutex<AppSettings>) -> Vec<AutoCaptureApp> {
    let entries = settings_snapshot(settings).auto_capture_executables;
    let mut changed = false;
    let upgraded: Vec<AutoCaptureApp> = entries
        .into_iter()
        .map(|entry| {
            if let Some(path) = &entry.path {
                if entry.display_name.is_some() {
                    return entry;
                }
                let Some(name) = super::app_meta::describe_executable(path) else {
                    return entry;
                };
                changed = true;
                return AutoCaptureApp {
                    display_name: Some(name),
                    ..entry
                };
            }
            let resolved = super::app_meta::resolve_entry(&entry.name);
            if resolved.path.is_none() {
                return entry;
            }
            changed = true;
            // The upgrade fills display metadata only -- the monitor flag
            // (task2570) is the user's setting and rides through.
            AutoCaptureApp {
                capture_monitor: entry.capture_monitor,
                ..resolved
            }
        })
        .collect();
    if changed {
        commit_settings(settings, |current| {
            current.auto_capture_executables = upgraded.clone();
        })
        .auto_capture_executables
    } else {
        upgraded
    }
}

/// Everything a language change does (task1470).
/// Order matters: the active language first, then the pushes that read it,
/// then this screen's own render.
fn apply_language(
    ui: &AppWindow,
    settings: &Mutex<AppSettings>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    tray: &slint::Weak<TrayIcon>,
    setting: &str,
) {
    let next = commit_settings(settings, |current| {
        current.language = setting.to_owned();
    });
    locale::set_active(super::resolve_locale(&next));
    // The tray goes with it (task1470 follow-up): its menu is outside the
    // window, so `push_all_labels` needs the handle to reach it.
    super::push_all_labels(ui, tray);
    render_settings(ui, settings, &next, &screen.borrow(), &hotkeys.borrow());
}

fn render_settings(
    ui: &AppWindow,
    settings: &Mutex<AppSettings>,
    current: &AppSettings,
    screen: &SettingsScreen,
    hotkeys: &Hotkeys,
) {
    let vm = ui.global::<SettingsVm>();
    // Anything that re-renders the pane has put a real value on the rows -- a
    // chip, a switch, a reload -- and that answers the question the `.errrow`
    // was asking. The refusal path in `preset_edited` never gets here: it
    // returns before it commits, which is what leaves the error standing.
    vm.set_numeric_error_field(-1);
    vm.set_retention_minutes(current.retention_minutes.to_string().into());
    // A switched-off row (stored `0`) keeps showing the number it goes back to
    // rather than a blank field or a `0 GB` that reads as a limit of zero
    // (task1490). The chips below are lit from the same value, so the whole
    // dimmed row says the same thing.
    let capacity_shown = settings_ui::retention_shown(
        current.retention_capacity_gb,
        screen.retention_capacity_last,
    );
    let lifetime_shown =
        settings_ui::retention_shown(current.session_lifetime_days, screen.session_lifetime_last);
    vm.set_retention_capacity(capacity_shown.to_string().into());
    vm.set_session_lifetime(lifetime_shown.to_string().into());
    vm.set_frame_rate(i32::from(current.frame_rate));
    vm.set_language_index(locale::language_index(&current.language));
    // round16. Pushed from here rather than from the theme callback alone, so
    // the one call `wire` makes at start-up is also what paints the window --
    // a theme has to be in force before the settings screen is ever opened.
    vm.set_theme_index(settings_ui::theme_index(&current.theme));
    let light = settings_ui::theme_is_light(&current.theme, desktop::apps_use_light_theme());
    ui.global::<Tokens>().set_light(light);
    // t260928-d906: the window's DWM outline follows the theme. Before the OS
    // window exists (the first render) there is no hwnd; the chrome timer in
    // `liveback.rs` paints it then.
    if let Some(hwnd) = super::shell::hwnd_of(ui.window()) {
        desktop::set_window_border_color(hwnd, light, ui.get_agent_run());
    }
    vm.set_ring_buffer_enabled(current.ring_buffer_enabled);
    vm.set_include_cursor(current.include_cursor);
    vm.set_capture_all_audio(current.capture_all_audio);
    // task1760. `av1_recording_supported` enumerates once per run and answers
    // from a cache after that, so asking it on every render is free.
    let av1_available = livia::encoder::av1_recording_supported();
    vm.set_codec_av1_available(av1_available);
    vm.set_codec_av1(settings_ui::codec_switch_on(current.codec, av1_available));
    vm.set_auto_start(current.auto_start);
    push_auto_capture_list(ui, settings);
    // No export row any more (task1050): the save dialog asks where each
    // export goes, and `export_output_directory` is kept only as the folder
    // that dialog opens on next time -- nothing the settings screen shows.
    //
    // Unset means "the default", and the default is shown resolved: the row is
    // the answer to *where are my files*, and `%USERPROFILE%\...` is not that.
    // The setting itself stays `None` (see `on_buffer_reset_clicked`) -- only
    // the caption is expanded. The `%VAR%` constant below is the clip row's
    // last resort for the case where the environment cannot resolve at all;
    // the buffer row needs none, because `buffer_root()` always answers a path
    // (`%TEMP%\Liveback\buffer` when even `%LOCALAPPDATA%` is missing).
    //
    // `buffer_root()`, not `buffer_directory` + `default_buffer_root()`: the
    // latter pair is blind to `LIVEBACK_BUFFER_ROOT`, so under an agent or test
    // run the row claimed the configured folder while every recording was going
    // to the override (2026-09-13: 23GB of the user's own capture landed in
    // `%TEMP%` while this row still read `D:\Apps\Liveback`). The clip row below
    // has resolved the override since task3910; this one was the odd one out.
    // With the variable unset the string is byte for byte what it always was.
    let buffer_path = CaptureController::buffer_root().display().to_string();
    let (head, tail) = settings_ui::split_path_tail(&buffer_path);
    vm.set_buffer_head(head.into());
    vm.set_buffer_tail(tail.into());
    // task2380: 規定値に戻す is drawn only once the folder has been moved. The
    // stored `Option`, not the path: reset writes `None`, so a path that
    // happens to equal today's default is still a folder the user picked.
    vm.set_buffer_modified(current.buffer_directory.is_some());
    // Task2970, shown resolved for the same reason as the buffer path above:
    // the row answers *where are my clips*, and an unset setting still has an
    // answer.
    let clip_path = livia::clips::clip_directory(current)
        .unwrap_or_else(|_| PathBuf::from(r"%USERPROFILE%\Videos\Liveback\Clips"));
    let (clip_head, clip_tail) = settings_ui::split_path_tail(&clip_path.display().to_string());
    vm.set_clip_head(clip_head.into());
    vm.set_clip_tail(clip_tail.into());
    vm.set_clip_modified(current.clip_directory.is_some());
    // Seeded from the window flag so a render that happens between capture
    // transitions still agrees; the 1s capture poll writes it too (see
    // `picker::render`), which is what actually keeps it fresh (round5 §7-C).
    vm.set_buffer_change_enabled(!ui.get_capturing());
    vm.set_retention_minutes_index(settings_ui::preset_index(
        &settings_ui::RETENTION_MINUTES_PRESETS,
        i64::from(current.retention_minutes),
    ));
    vm.set_frame_rate_index(settings_ui::preset_index(
        &settings_ui::FRAME_RATE_PRESETS,
        i64::from(current.frame_rate),
    ));
    // round8 §3-2's sentinel, task1490's control: 0 still means the limit is
    // not in force, and it is the switch that says so now.
    vm.set_retention_capacity_on(current.retention_capacity_gb != settings::RETENTION_NONE);
    vm.set_session_lifetime_on(current.session_lifetime_days != settings::RETENTION_NONE);
    vm.set_retention_capacity_index(settings_ui::preset_index(
        &settings_ui::RETENTION_CAPACITY_PRESETS,
        capacity_shown,
    ));
    vm.set_session_lifetime_index(settings_ui::preset_index(
        &settings_ui::SESSION_LIFETIME_PRESETS,
        lifetime_shown,
    ));
    vm.set_confirm_title(
        if screen.buffer_confirm.is_some() {
            settings_ui::buffer_directory_confirm_title(tr_locale())
        } else {
            ""
        }
        .into(),
    );
    vm.set_confirm_lead(settings_ui::buffer_directory_confirm_lead(tr_locale()).into());
    vm.set_confirm_note(settings_ui::buffer_directory_confirm_note(tr_locale()).into());
    vm.set_marker_draft(screen.draft_hotkey.as_str().into());
    // round8 §3-3: the row draws one cap per key, and slint cannot split a
    // string -- so the split happens here, where the chord is already a string
    // this side owns.
    vm.set_marker_keys(hotkey_caps(&screen.draft_hotkey));
    vm.set_armed(screen.armed);
    // task730: another app holding the chord is worth saying on the row, and
    // is a different problem from a registrar that would not start at all.
    // t260928-7e9a: and so is a chord refused as it was typed -- the field
    // keeps the chord that still holds and names the one that did not.
    vm.set_marker_taken(screen.refused_hotkey.is_some() || hotkeys.unavailable());
    // DS HotkeyField: the refusal names the chord, spelled the way running
    // text spells it.
    let named = screen
        .refused_hotkey
        .as_deref()
        .unwrap_or(&screen.draft_hotkey);
    ui.global::<SettingsLabels>().set_state_taken(
        settings_ui::hotkey_taken(tr_locale(), &settings_ui::hotkey_display(named)).into(),
    );
    vm.set_auto_capture_refused(screen.folder_refused);
}

/// Puts a settings result on the shared status line (round 4 §2-E4). The old
/// save bar had its own flash timer and its own standing message; the status
/// line's 6-second rule replaces both, and an error now stays put instead of
/// reverting after `SAVED_FLASH`.
fn flash_settings_message(
    ui: &AppWindow,
    line: &Rc<RefCell<toast::Toast>>,
    text: String,
    ok: bool,
) {
    super::publish_status(ui, line, &text, !ok);
}

/// Task195: a chord that is dead with no field in front of the user -- at
/// startup, or when a resume is refused on the way out of the field -- is said
/// on the shared toast, which reaches every screen. t260928-7e9a: in the DS
/// failure shape, the chord and why on the second line and 「設定を開く」 as the
/// next step. A chord refused as it is typed is said on the field instead
/// (`refused_hotkey`), never on both.
pub(super) fn flash_hotkey_failure(ui: &AppWindow, line: &Rc<RefCell<toast::Toast>>, chord: &str) {
    super::publish_toast(
        ui,
        line,
        settings_ui::hotkey_failure_title(tr_locale()),
        &settings_ui::hotkey_failure_detail(tr_locale(), chord),
        toast::ToastVariant::Error,
        sessions_ui::settings_action(tr_locale()),
    );
}

/// Wires the settings page's callbacks. Everything below moved verbatim out
/// of `main` (the block only borrows what it used to capture from it).
pub(super) fn wire(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    stored: &AppSettings,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    status_line: &Rc<RefCell<toast::Toast>>,
    controller: &CaptureController,
    tray: &slint::Weak<TrayIcon>,
) {
    {
        push_settings_labels(ui);
        let screen = Rc::new(RefCell::new(SettingsScreen {
            draft_hotkey: stored.hotkey.clone(),
            armed: 0,
            buffer_confirm: None,
            refused_hotkey: None,
            folder_refused: false,
            retention_capacity_last: settings_ui::retention_shown(
                stored.retention_capacity_gb,
                AppSettings::default().retention_capacity_gb,
            ),
            session_lifetime_last: settings_ui::retention_shown(
                stored.session_lifetime_days,
                AppSettings::default().session_lifetime_days,
            ),
        }));
        let flash = status_line.clone();
        render_settings(ui, settings, stored, &screen.borrow(), &hotkeys.borrow());

        wire_settings_inputs(ui, settings, &screen, hotkeys, &flash, tray);
        wire_buffer_and_capture(ui, settings, &screen, hotkeys, &flash, controller);
    }
}

/// Undoes the `suspend` that arming a hotkey field did (task160). Every way out
/// of armed -- Esc, blur, a refused capture, a capture that changed nothing --
/// ends here, so the chords are always live again by the time the user leaves
/// the settings screen. A registrar that refuses says so out loud instead of
/// leaving hotkeys quietly dead.
///
/// "Blur" is doing more work in that sentence than it looks, and task4300
/// measured what: **deactivating the window counts as one**. Only two things
/// raise `hk-blurred` -- `changed has-focus` on the field's own `FocusScope`
/// (`ui/settings_rows.slint`) and `changed active-pane` (`ui/app.slint`,
/// task160) -- and the first of them fires when the window stops being active,
/// not only when focus moves to another widget inside it. That is why hiding to
/// the tray (`stash_in_tray` -> `SW_HIDE`), Alt+F4 (`on_close_requested`, the
/// same `stash_in_tray`) and minimizing (`SW_MINIMIZE`) all leave the chords
/// live even though **none of those three touches `armed` itself**: each one
/// deactivates the window on the way out, and the disarm arrives through here.
/// Measured on the real machine 2026-09-11, all three routes with their own
/// unarmed positive controls and an armed-and-still-visible negative control
/// that stayed dead (`.agents/tasks/evidence/4300-tray-hotkey/`). So the
/// paragraph above is true, but do not read it as "the code that hides the
/// window remembers to disarm" -- it does not, and the guarantee would go with
/// that `FocusScope` if it ever stopped reporting deactivation.
fn resume_hotkeys(
    ui: &AppWindow,
    settings: &Mutex<AppSettings>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    let resumed = hotkeys.borrow_mut().resume();
    if resumed.is_err() {
        flash_hotkey_failure(ui, flash, &settings_snapshot(settings).hotkey);
    }
}

/// Register, then save -- and only if the OS accepted. Task146 dropped the save
/// button, so this is what a confirmed capture runs. The order matters: `apply`
/// rolls a refused pair back to the previous one, and saving first would leave
/// `settings.json` advertising a chord nothing answers to.
///
/// Returns whether `apply` ran: it re-registers by itself, so the caller only
/// owes a [`resume_hotkeys`] when it did not (task160).
fn commit_hotkey_capture(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
    captured: &str,
) -> bool {
    let current = settings_snapshot(settings);
    let previous = current.hotkey.clone();
    screen.borrow_mut().refused_hotkey = None;
    let marker = match settings_ui::commit_capture(&previous, captured) {
        settings_ui::HotkeyCommit::Unchanged => return false,
        settings_ui::HotkeyCommit::Apply(marker) => marker,
    };

    let registered = hotkeys.borrow_mut().apply(&marker).is_ok();
    let shown = settings_ui::displayed_hotkey(&previous, &marker, registered);
    screen.borrow_mut().draft_hotkey = shown.clone();
    if !registered {
        // t260928-7e9a: said on the field, which is in front of the user --
        // no toast (DS: one place per message). `apply` already rolled back
        // to the chord we were holding, so the OS side is settled either way.
        screen.borrow_mut().refused_hotkey = Some(marker);
        return true;
    }
    let next = commit_settings(settings, |current| {
        current.hotkey = shown;
    });
    // From the reload, not the draft: `resolve_hotkey` may have adjusted it.
    screen.borrow_mut().draft_hotkey = next.hotkey.clone();
    flash_settings_message(
        ui,
        flash,
        settings_ui::hotkey_saved(tr_locale(), &settings_ui::hotkey_display(&next.hotkey)),
        true,
    );
    true
}

/// The hotkey field, the language picker, the retention rows and the buffer
/// folder's own change dialog.
#[allow(clippy::too_many_arguments)]
fn wire_settings_inputs(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
    tray: &slint::Weak<TrayIcon>,
) {
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_numeric_committed(move |field, text| {
                let Some(ui) = weak.upgrade() else { return };
                // Parse as float, then round: the .slint side validates
                // with `is-float()`, so "12.5" arrives here and silently
                // dropping it would leave the draft disagreeing with the
                // stored value.
                let Ok(value) = text.trim().parse::<f64>() else {
                    return;
                };
                let value = value.round() as i64;
                let next = commit_settings(&settings, |current| match field {
                    // The .slint side already checked the range; the clamp
                    // in `validated()` stays the last line of defence.
                    0 => current.retention_minutes = value.clamp(0, i64::from(u16::MAX)) as u16,
                    1 => current.retention_capacity_gb = value,
                    // Field 3 was クリップ保存の長さ until task1090.
                    _ => current.session_lifetime_days = value,
                });
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    // The chip rows (round5 §7-B). One handler for all four: the field id
    // picks which setting a press lands on, exactly as `numeric-committed`
    // already did for the text rows.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_preset_clicked(move |field, index| {
                let Some(ui) = weak.upgrade() else { return };
                let index = usize::try_from(index).unwrap_or(usize::MAX);
                let next = commit_settings(&settings, |current| match field {
                    0 => {
                        if let Some(value) = settings_ui::RETENTION_MINUTES_PRESETS.get(index) {
                            current.retention_minutes = *value as u16;
                        }
                    }
                    1 => {
                        if let Some(value) = settings_ui::RETENTION_CAPACITY_PRESETS.get(index) {
                            current.retention_capacity_gb = *value;
                        }
                    }
                    2 => {
                        if let Some(value) = settings_ui::SESSION_LIFETIME_PRESETS.get(index) {
                            current.session_lifetime_days = *value;
                        }
                    }
                    4 => {
                        if let Some(value) = settings_ui::FRAME_RATE_PRESETS.get(index) {
                            current.frame_rate = *value as u8;
                        }
                    }
                    _ => {}
                });
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    // task1980. The only half of the registration this screen owns: an app
    // that is registered but not running has no picker tile to press, so
    // without this row there would be no way off the list.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_auto_capture_removed(move |name| {
                let Some(ui) = weak.upgrade() else { return };
                let next = commit_settings(&settings, |current| {
                    current.auto_capture_executables =
                        auto_capture::unregister(&current.auto_capture_executables, &name);
                });
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_unregistered",
                    executable = %name,
                    "removed from the settings list"
                );
                // round13 §1: the review panel's toggle is a view of this list,
                // and nothing re-renders that pane from here. Both lists since
                // task3670 -- the row reads `covered_by`, so it needs the
                // folder rules to answer at all; nothing about this handler's
                // behaviour changed.
                super::review::render_autorec(
                    &ui,
                    &next.auto_capture_executables,
                    &next.auto_capture_folders,
                );
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    // task2570: the row's window/monitor toggle. Commit, then re-push the
    // list so the glyph follows the store rather than a local guess.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.global::<SettingsVm>()
            .on_auto_capture_monitor_toggled(move |name| {
                let Some(ui) = weak.upgrade() else { return };
                let next = commit_settings(&settings, |current| {
                    current.auto_capture_executables =
                        auto_capture::toggle_monitor(&current.auto_capture_executables, &name);
                });
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_monitor_toggled",
                    executable = %name,
                    capture_monitor = auto_capture::wants_monitor_capture(
                        &next.auto_capture_executables,
                        &name
                    ),
                    "window/monitor mode switched on the settings row"
                );
                push_auto_capture_list(&ui, &settings);
            });
    }

    // task3610: the 「フォルダを追加」 button. One step like the clip folder's
    // 変更 (task2970) and unlike the buffer root's pick-then-confirm -- nothing
    // already written moves, so there is nothing to warn about.
    //
    // The guard is `folder_rule_allowed`, the same pure function task3600's
    // poll reads, so a folder this screen accepts is one the poll will act on.
    // Refusing here rather than storing a rule that would be ignored: the guard
    // is invisible from a folder picker, so the danger line under the group is
    // the only place its rule can be said.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_auto_capture_folder_add(move || {
                let Some(ui) = weak.upgrade() else { return };
                // Two `info!`s so a sweep can tell "the handler never ran" from
                // "the picker opened and the user (or the harness) cancelled":
                // task3610's sweep read the same silence as both. `info!`, not
                // `debug!` -- the file layer this is read back from is INFO.
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_folder_add_entered",
                    "the folder picker is about to open"
                );
                let picked = rfd::FileDialog::new()
                    .set_parent(&ui.window().window_handle())
                    .pick_folder();
                let Some(path) = picked else {
                    tracing::info!(
                        target: "task1970_auto_capture",
                        event = "auto_capture_folder_add_cancelled",
                        "the folder picker closed without a folder"
                    );
                    return;
                };
                let path = path.to_string_lossy().into_owned();
                if !auto_capture::folder_rule_allowed(&path) {
                    tracing::info!(
                        target: "task1970_auto_capture",
                        event = "auto_capture_folder_refused",
                        folder = %path,
                        "the picked folder is wider than this feature acts on"
                    );
                    // t260928-7e9a: under the group, in danger, rather than a
                    // toast (DS 知らせ方: next to where it happened).
                    screen.borrow_mut().folder_refused = true;
                    render_settings(
                        &ui,
                        &settings,
                        &settings_snapshot(&settings),
                        &screen.borrow(),
                        &hotkeys.borrow(),
                    );
                    return;
                }
                screen.borrow_mut().folder_refused = false;
                let next = commit_settings(&settings, |current| {
                    current.auto_capture_folders =
                        auto_capture::add_folder(&current.auto_capture_folders, &path);
                });
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_folder_registered",
                    folder = %path,
                    "added from the settings list"
                );
                // task3670: a rule the review's session falls under lights its
                // toggle, and nothing re-renders that pane from here.
                super::review::render_autorec(
                    &ui,
                    &next.auto_capture_executables,
                    &next.auto_capture_folders,
                );
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    // The × on a folder row. It gets the `render_autorec` twin of the
    // executable handler's since task3670: the review panel's toggle used to be
    // a view of the *executable* list alone, so a folder rule never reached it
    // -- it reads `covered_by` now, and a rule removed here has to take the
    // toggle it was lighting with it. (The picker's badge needs no wiring: its
    // 1s poll re-reads both lists by itself.)
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.global::<SettingsVm>()
            .on_auto_capture_folder_removed(move |path| {
                let Some(ui) = weak.upgrade() else { return };
                let next = commit_settings(&settings, |current| {
                    current.auto_capture_folders =
                        auto_capture::unregister_folder(&current.auto_capture_folders, &path);
                });
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_folder_unregistered",
                    folder = %path,
                    "removed from the settings list"
                );
                super::review::render_autorec(
                    &ui,
                    &next.auto_capture_executables,
                    &next.auto_capture_folders,
                );
                push_auto_capture_list(&ui, &settings);
            });
    }

    // The folder row's window/monitor toggle, the twin of the executable one
    // above and re-pushed the same way: the glyph follows the store, never a
    // local guess.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.global::<SettingsVm>()
            .on_auto_capture_folder_monitor_toggled(move |path| {
                let Some(ui) = weak.upgrade() else { return };
                let next = commit_settings(&settings, |current| {
                    current.auto_capture_folders =
                        auto_capture::toggle_folder_monitor(&current.auto_capture_folders, &path);
                });
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_folder_monitor_toggled",
                    folder = %path,
                    capture_monitor =
                        auto_capture::folder_wants_monitor(&next.auto_capture_folders, &path),
                    "window/monitor mode switched on the settings row"
                );
                // task3670, like the two handlers above: the toggle's answer is
                // derived from this list, so it is re-derived whenever the list
                // is written.
                super::review::render_autorec(
                    &ui,
                    &next.auto_capture_executables,
                    &next.auto_capture_folders,
                );
                push_auto_capture_list(&ui, &settings);
            });
    }

    wire_language_picker(ui, settings, screen, hotkeys, flash, tray);
}

/// Putting the buffer root back, the ring-buffer switches and everything
/// whose answer depends on whether a capture is running.
#[allow(clippy::too_many_arguments)]
fn wire_buffer_and_capture(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
    controller: &CaptureController,
) {
    // Putting the root back needs no picker and no confirm step: there is
    // nothing to choose, and it is undone by picking a folder again. The
    // one consequence worth stating -- recordings under the old root stay
    // there and stop being listed -- goes in the flash, which is the same
    // thing the change dialog says at more length.
    {
        let weak = ui.as_weak();
        let screen = screen.clone();
        let settings = settings.clone();
        let hotkeys = hotkeys.clone();
        let flash = flash.clone();
        let controller = controller.clone();
        ui.global::<SettingsVm>().on_buffer_reset_clicked(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_capturing() {
                return;
            }
            // `None`, not the resolved default path: pinning today's
            // `%LOCALAPPDATA%` would survive into a profile where it
            // resolves somewhere else.
            match controller.set_buffer_root(None) {
                Ok(()) => {
                    let next = commit_settings(&settings, |current| {
                        current.buffer_directory = None;
                    });
                    render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
                    flash_settings_message(
                        &ui,
                        &flash,
                        settings_ui::buffer_directory_reset_done(tr_locale()).to_owned(),
                        true,
                    );
                }
                Err(error) => flash_settings_message(&ui, &flash, error, false),
            }
        });
    }

    // Moving the buffer root is two steps: pick, then confirm. The pick
    // alone changes nothing -- what the dialog explains is that existing
    // recordings stay where they are and the history follows the new root.
    {
        let weak = ui.as_weak();
        let screen = screen.clone();
        let settings = settings.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_buffer_change(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_capturing() {
                return;
            }
            let picked = rfd::FileDialog::new()
                .set_parent(&ui.window().window_handle())
                .pick_folder();
            let Some(path) = picked else { return };
            screen.borrow_mut().buffer_confirm = Some(path.to_string_lossy().into_owned());
            let current = settings_snapshot(&settings);
            render_settings(
                &ui,
                &settings,
                &current,
                &screen.borrow(),
                &hotkeys.borrow(),
            );
        });
    }

    {
        let weak = ui.as_weak();
        let screen = screen.clone();
        let settings = settings.clone();
        let hotkeys = hotkeys.clone();
        let flash = flash.clone();
        // Through the controller, not the bare `apply_buffer_root`: only
        // the instance method rebuilds the session catalog, without which
        // edits and discards keep resolving against the old root.
        let controller = controller.clone();
        ui.global::<SettingsVm>()
            .on_buffer_confirm_accepted(move || {
                let Some(ui) = weak.upgrade() else { return };
                let Some(path) = screen.borrow_mut().buffer_confirm.take() else {
                    return;
                };
                // The engine has to accept the root before it is written: a
                // path it cannot use would leave the setting pointing at
                // nothing recordable.
                match controller.set_buffer_root(Some(path.clone().into())) {
                    Ok(()) => {
                        let next = commit_settings(&settings, |current| {
                            current.buffer_directory = Some(path);
                        });
                        render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
                    }
                    Err(_) => {
                        let current = settings_snapshot(&settings);
                        render_settings(
                            &ui,
                            &settings,
                            &current,
                            &screen.borrow(),
                            &hotkeys.borrow(),
                        );
                        flash_settings_message(
                            &ui,
                            &flash,
                            settings_ui::buffer_directory_change_error(tr_locale()).to_owned(),
                            false,
                        );
                    }
                }
            });
    }

    {
        let weak = ui.as_weak();
        let screen = screen.clone();
        let settings = settings.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_buffer_confirm_cancelled(move || {
                let Some(ui) = weak.upgrade() else { return };
                screen.borrow_mut().buffer_confirm = None;
                let current = settings_snapshot(&settings);
                render_settings(
                    &ui,
                    &settings,
                    &current,
                    &screen.borrow(),
                    &hotkeys.borrow(),
                );
            });
    }

    wire_capture_switches(ui, settings, screen, hotkeys, flash);
}

/// The retention rows and the buffer folder's own change dialog.
fn wire_retention_rows(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_preset_edited(move |field, text| {
                let Some(ui) = weak.upgrade() else { return };
                let ranges = [
                    settings_ui::RETENTION_MINUTES_RANGE,
                    settings_ui::RETENTION_CAPACITY_GB_RANGE,
                    settings_ui::SESSION_LIFETIME_DAYS_RANGE,
                ];
                let Some((min, max)) = ranges.get(field.max(0) as usize).copied() else {
                    return;
                };
                // Round6 3's `.errrow`: out of range is refused and said
                // out loud, not clamped into something the user did not
                // type. A non-number still leaves the setting alone and the
                // field showing what was typed -- same contract the numeric
                // rows have always had -- and says nothing.
                let value = match settings_ui::preset_draft(&text, min, max) {
                    settings_ui::PresetDraft::Commit(value) => value,
                    settings_ui::PresetDraft::OutOfRange => {
                        ui.global::<SettingsVm>().set_numeric_error_field(field);
                        return;
                    }
                    settings_ui::PresetDraft::Ignore => return,
                };
                // Field 0 is back (2026-09-13, the user): the 保持時間 row
                // carries a free field again. The ranges line up with the row
                // indices because `preset_clicked` shares them.
                let next = commit_settings(&settings, |current| match field {
                    // `preset_draft` already refused anything outside
                    // RETENTION_MINUTES_RANGE (5..=1440), so the cast cannot
                    // lose anything.
                    0 => current.retention_minutes = value as u16,
                    1 => current.retention_capacity_gb = value,
                    2 => current.session_lifetime_days = value,
                    _ => {}
                });
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    // task1410: the switch decides whether footage older than the buffer
    // length is dropped, nothing else. It no longer stops a recording, and
    // it no longer starts or blocks one -- a session takes its retention at
    // start, so a flip only reaches the next one.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_ring_buffer_toggled(move |enabled| {
                let Some(ui) = weak.upgrade() else { return };
                let next =
                    commit_settings(&settings, |current| current.ring_buffer_enabled = enabled);
                tracing::info!(
                    event = "ring_buffer_toggled",
                    enabled,
                    "the next recording takes this"
                );
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    wire_buffer_dialog(ui, settings, screen, hotkeys, flash);
}

/// The switches whose answer depends on whether a capture is running.
fn wire_capture_switches(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_hk_clicked(move |control| {
            let Some(ui) = weak.upgrade() else { return };
            // Arming gives the chords back to the OS: `RegisterHotKey`
            // swallows them system-wide, so a chord this app holds would
            // never reach the field (task160). `disarm` puts them back.
            hotkeys.borrow_mut().suspend();
            {
                let mut screen = screen.borrow_mut();
                screen.armed = control;
                // The next input clears a refusal (DS HotkeyField).
                screen.refused_hotkey = None;
            }
            render_settings(
                &ui,
                &settings,
                &settings_snapshot(&settings),
                &screen.borrow(),
                &hotkeys.borrow(),
            );
        });
    }

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        let flash = flash.clone();
        ui.global::<SettingsVm>()
            .on_hk_key(move |control, text, ctrl, alt, shift, meta| {
                let Some(ui) = weak.upgrade() else {
                    return false;
                };
                let mut captured = None;
                let mut escaped = false;
                {
                    let mut screen = screen.borrow_mut();
                    if screen.armed != control {
                        return false;
                    }
                    if text.as_str() == "\u{001b}" {
                        // Just disarm: arming never touched the draft, so
                        // the value already showing *is* the 直前の値.
                        screen.armed = 0;
                        escaped = true;
                    } else {
                        // Modifier-only and unnameable presses compose to
                        // nothing: stay armed and keep waiting.
                        let Some(shortcut) =
                            settings_ui::compose_shortcut(&text, ctrl, alt, shift, meta)
                        else {
                            return true;
                        };
                        screen.armed = 0;
                        captured = Some(shortcut);
                    }
                }
                // Capture *is* the commit since task146 -- there is no save
                // button to carry the decision to.
                if let Some(shortcut) = captured {
                    // `apply` re-registers on its own (including its own
                    // rollback), so only the paths that never reach it owe
                    // the OS a `resume` (task160).
                    let applied =
                        commit_hotkey_capture(&ui, &settings, &screen, &hotkeys, &flash, &shortcut);
                    if !applied {
                        resume_hotkeys(&ui, &settings, &hotkeys, &flash);
                    }
                } else if escaped {
                    resume_hotkeys(&ui, &settings, &hotkeys, &flash);
                }
                render_settings(
                    &ui,
                    &settings,
                    &settings_snapshot(&settings),
                    &screen.borrow(),
                    &hotkeys.borrow(),
                );
                true
            });
    }

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        let flash = flash.clone();
        ui.global::<SettingsVm>().on_hk_reset(move |_control| {
            let Some(ui) = weak.upgrade() else { return };
            // Pressing 戻す while the row is listening ends the listening
            // too -- the same exit `hk_blurred` takes, so the OS gets its
            // chord back on the paths that never reach `apply`.
            let was_armed = {
                let mut screen = screen.borrow_mut();
                let armed = screen.armed != 0;
                screen.armed = 0;
                armed
            };
            let applied = commit_hotkey_capture(
                &ui,
                &settings,
                &screen,
                &hotkeys,
                &flash,
                livia::DEFAULT_HOTKEY,
            );
            if !applied && was_armed {
                resume_hotkeys(&ui, &settings, &hotkeys, &flash);
            }
            render_settings(
                &ui,
                &settings,
                &settings_snapshot(&settings),
                &screen.borrow(),
                &hotkeys.borrow(),
            );
        });
    }

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        let flash = flash.clone();
        ui.global::<SettingsVm>().on_hk_blurred(move |control| {
            let Some(ui) = weak.upgrade() else { return };
            // A blur clears a refusal (DS HotkeyField), armed or not: the
            // field stays focused after the refused capture disarmed it.
            let (refused, armed) = {
                let mut screen = screen.borrow_mut();
                let refused = screen.refused_hotkey.take().is_some();
                let armed = screen.armed == control;
                if armed {
                    screen.armed = 0;
                }
                (refused, armed)
            };
            if !armed {
                if refused {
                    render_settings(
                        &ui,
                        &settings,
                        &settings_snapshot(&settings),
                        &screen.borrow(),
                        &hotkeys.borrow(),
                    );
                }
                return;
            }
            resume_hotkeys(&ui, &settings, &hotkeys, &flash);
            render_settings(
                &ui,
                &settings,
                &settings_snapshot(&settings),
                &screen.borrow(),
                &hotkeys.borrow(),
            );
        });
    }
}

/// The language picker and the rows that follow it.
fn wire_language_picker(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
    tray: &slint::Weak<TrayIcon>,
) {
    // task1150. Its own callback rather than a preset index: the language
    // is the one setting whose change has to re-push every label in the
    // app, and it stores a string rather than a number. The row's
    // SegmentedControl speaks `LANGUAGE_SETTINGS` indices (t260927-fde8).
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        let tray = tray.clone();
        ui.global::<SettingsVm>()
            .on_language_selected(move |index| {
                let Some(ui) = weak.upgrade() else { return };
                let setting = locale::language_setting(index);
                apply_language(&ui, &settings, &screen, &hotkeys, &tray, setting);
            });
    }

    // round16. A plain index off `THEME_SETTINGS`, and no label re-push: unlike
    // the language, nothing in the app is worded differently in the other
    // theme. `render_settings` is what actually repaints -- see the push there.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_theme_selected(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let next = commit_settings(&settings, |current| {
                current.theme = settings_ui::theme_setting(index).to_owned();
            });
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        });
    }

    // task1490, replacing round8 §3-2's 「なし」 chip. Its own callback
    // rather than a preset index for the same reason the chip had one: off
    // is not a value on the row's scale, it takes the row off the scale.
    // The number the row goes back to lives here, not in the file -- off is
    // stored as `0` and there is nowhere in `settings.json` for it.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_retention_toggled(move |field, on| {
                let Some(ui) = weak.upgrade() else { return };
                let capacity = field == 1;
                if !capacity && field != 2 {
                    return;
                }
                // The borrow ends before `commit_settings` runs, because
                // `render_settings` below borrows the same cell.
                let stored_value = {
                    let mut screen = screen.borrow_mut();
                    let current = settings_snapshot(&settings);
                    let (value, remembered) = if capacity {
                        settings_ui::retention_toggle(
                            on,
                            current.retention_capacity_gb,
                            screen.retention_capacity_last,
                        )
                    } else {
                        settings_ui::retention_toggle(
                            on,
                            current.session_lifetime_days,
                            screen.session_lifetime_last,
                        )
                    };
                    if capacity {
                        screen.retention_capacity_last = remembered;
                    } else {
                        screen.session_lifetime_last = remembered;
                    }
                    value
                };
                let next = commit_settings(&settings, |current| {
                    if capacity {
                        current.retention_capacity_gb = stored_value;
                    } else {
                        current.session_lifetime_days = stored_value;
                    }
                });
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    wire_retention_rows(ui, settings, screen, hotkeys, flash);
}

/// The buffer folder's change dialog.
fn wire_buffer_dialog(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_cursor_toggled(move |include| {
            let Some(ui) = weak.upgrade() else { return };
            let next = commit_settings(&settings, |current| current.include_cursor = include);
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        });
    }

    // task1430. Read when a recording starts, so like the ring switch above
    // this only reaches the next one.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>()
            .on_capture_all_audio_toggled(move |capture_all| {
                let Some(ui) = weak.upgrade() else { return };
                let next =
                    commit_settings(&settings, |current| current.capture_all_audio = capture_all);
                tracing::info!(
                    event = "capture_all_audio_toggled",
                    enabled = capture_all,
                    "the next recording takes this"
                );
                render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            });
    }

    // task1760. Same "reaches the next recording" contract as the switch
    // above; the row is only reachable at all where AV1 both encodes and
    // decodes, and `CaptureController::start` checks that again anyway.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_codec_av1_toggled(move |av1| {
            let Some(ui) = weak.upgrade() else { return };
            let codec = settings_ui::codec_from_switch(av1);
            let next = commit_settings(&settings, |current| current.codec = codec);
            tracing::info!(
                event = "recording_codec_changed",
                codec = ?codec,
                "the next recording takes this"
            );
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        });
    }

    {
        // Persist what the registry ended up holding, not what was asked
        // for: `syncAutoStart` did the same, so a refused write leaves the
        // toggle showing the truth (task130).
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_autostart_toggled(move |auto| {
            let Some(ui) = weak.upgrade() else { return };
            let registered = desktop::sync_auto_start(auto);
            let next = commit_settings(&settings, |current| current.auto_start = registered);
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        });
    }

    {
        let weak = ui.as_weak();
        let flash = flash.clone();
        ui.global::<SettingsVm>().on_buffer_open(move || {
            let root = CaptureController::buffer_root();
            let weak = weak.clone();
            let flash = flash.clone();
            // t260923-916e: explorer can stall the calling thread for minutes.
            desktop::off_ui_thread(
                move || open_directory(&root),
                move |result| {
                    let Some(ui) = weak.upgrade() else { return };
                    if result.is_err() {
                        flash_settings_message(
                            &ui,
                            &flash,
                            settings_ui::open_directory_error(tr_locale()).to_owned(),
                            false,
                        );
                    }
                },
            );
        });
    }

    // ---------- クリップの保存先 (task2970) ----------
    // One step, unlike the buffer root's pick-then-confirm: nothing already
    // written moves, so there is nothing to warn about. `None` is what 規定値に
    // 戻す writes, so the default stays the default even if it later changes.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_clip_change(move || {
            let Some(ui) = weak.upgrade() else { return };
            let picked = rfd::FileDialog::new()
                .set_parent(&ui.window().window_handle())
                .pick_folder();
            let Some(path) = picked else { return };
            let next = commit_settings(&settings, |current| {
                current.clip_directory = Some(path.to_string_lossy().into_owned());
            });
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        });
    }

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let screen = screen.clone();
        let hotkeys = hotkeys.clone();
        ui.global::<SettingsVm>().on_clip_reset_clicked(move || {
            let Some(ui) = weak.upgrade() else { return };
            let next = commit_settings(&settings, |current| current.clip_directory = None);
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        });
    }

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let flash = flash.clone();
        ui.global::<SettingsVm>().on_clip_open(move || {
            let current = settings_snapshot(&settings);
            let directory = livia::clips::clip_directory(&current);
            let weak = weak.clone();
            let flash = flash.clone();
            // t260923-916e: off the UI thread, like the buffer's 開く.
            desktop::off_ui_thread(
                move || {
                    // Created on the way in: 開く on a folder no clip has been
                    // saved to yet would otherwise fail for a reason the
                    // message cannot explain.
                    directory.and_then(|directory| {
                        let _ = std::fs::create_dir_all(&directory);
                        open_directory(&directory).map_err(|_| String::new())
                    })
                },
                move |result| {
                    let Some(ui) = weak.upgrade() else { return };
                    if result.is_err() {
                        flash_settings_message(
                            &ui,
                            &flash,
                            settings_ui::open_directory_error(tr_locale()).to_owned(),
                            false,
                        );
                    }
                },
            );
        });
    }
}

#[cfg(test)]
mod hotkey_caps_tests {
    use slint::Model;

    #[test]
    fn super_is_shown_as_win() {
        let caps: Vec<String> = super::hotkey_caps("Ctrl+Super+A")
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(caps, ["Ctrl", "Win", "A"]);
    }
}
