//! The settings page (task126): settings-file plumbing shared by every
//! screen (`settings_path` / `settings_snapshot` / `commit_settings`), the
//! labels and renderer, and the callback wiring moved out of `main`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use livia::capture::CaptureController;
use livia::ring_buffer::{self, RetentionPreview};
use livia::settings::{self, AppSettings, AutoCaptureApp, ExtraAudioSource, TargetAudio};
use livia::ui_state::auto_capture;
use livia::ui_state::locale;
use livia::ui_state::sessions as sessions_ui;
use livia::ui_state::settings as settings_ui;
use livia::ui_state::toast;
use livia::ui_state::volume as vol;
use slint::ComponentHandle;

use super::desktop;
use super::history::History;
use super::hotkeys::{Hotkeys, Role};
use super::review::Review;
use super::shell::open_directory;
use super::tr_locale;
use super::{
    AppWindow, AutoCaptureEntry, Cmd, SettingsLabels, SettingsVm, Tokens, TrayIcon, VolumeRowVm,
};

/// One settings callback (t261025-0e3d): clones the named captures, upgrades
/// the window and runs the body with it -- the five lines every handler on this
/// page opened with. A body that bails before the window is back returns the
/// callback type's `Default` (`()` / `false`), which is what each did by hand.
macro_rules! settings_handler {
    ($ui:ident, ($($cap:ident),*), $setter:ident, || $body:block) => {
        settings_handler!($ui, ($($cap),*), $setter, | | $body)
    };
    ($ui:ident, ($($cap:ident),*), $setter:ident, |$($arg:pat_param),*| $body:block) => {{
        let weak = $ui.as_weak();
        $(let $cap = $cap.clone();)*
        $ui.global::<SettingsVm>().$setter(move |$($arg),*| {
            let Some($ui) = weak.upgrade() else {
                return Default::default();
            };
            $body
        });
    }};
}

/// Draws the page from the stored settings, the way a handler that did not
/// just commit anything does.
fn rerender(
    ui: &AppWindow,
    settings: &Mutex<AppSettings>,
    screen: &RefCell<SettingsScreen>,
    hotkeys: &RefCell<Hotkeys>,
) {
    render_settings(
        ui,
        settings,
        &settings_snapshot(settings),
        &screen.borrow(),
        &hotkeys.borrow(),
    );
}

/// The chip rows' field ids, as `SettingsVm`'s `preset-clicked`,
/// `preset-edited` and `retention-toggled` send them (t261025-0e3d: the
/// numbers were spelled out in each handler). The slint side keeps the ints.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PresetField {
    RetentionMinutes,
    Capacity,
    Lifetime,
    FrameRate,
    SeekSeconds,
    /// An id no row sends: every handler ignores it.
    Other,
}

impl From<i32> for PresetField {
    fn from(id: i32) -> Self {
        match id {
            0 => PresetField::RetentionMinutes,
            1 => PresetField::Capacity,
            2 => PresetField::Lifetime,
            4 => PresetField::FrameRate,
            5 => PresetField::SeekSeconds,
            _ => PresetField::Other,
        }
    }
}

/// A chord refused as it was typed: which row, which chord, and whether the
/// other Liveback hotkey holds it (t261025-3669) rather than another app.
struct Refusal {
    control: i32,
    chord: String,
    clash: bool,
}

/// The row's line for a refused or dead chord, or `""` when there is none.
fn hotkey_alert(refused: Option<&Refusal>, control: i32, dead: bool, shown: &str) -> String {
    match refused.filter(|refusal| refusal.control == control) {
        Some(refusal) if refusal.clash => {
            settings_ui::hotkey_clash(tr_locale(), settings_ui::hotkey_display(&refusal.chord))
        }
        Some(refusal) => {
            settings_ui::hotkey_taken(tr_locale(), settings_ui::hotkey_display(&refusal.chord))
        }
        None if dead => settings_ui::hotkey_taken(tr_locale(), settings_ui::hotkey_display(shown)),
        None => String::new(),
    }
}

fn hotkey_role(control: i32) -> Role {
    match control {
        2 => Role::Record,
        3 => Role::Show,
        _ => Role::Marker,
    }
}

/// UI-thread state for the settings screen (task126). Only what the pure
/// settings module cannot hold: the unsaved hotkey drafts and which capture
/// control is armed. Everything else round-trips through `settings.json`.
struct SettingsScreen {
    draft_hotkey: String,
    /// The record row's chord, empty while unset (t261025-3669).
    record_draft: String,
    /// The show-window row's, the same way (t261025-034f).
    show_draft: String,
    /// 0 = none, 1 = the marker row, 2 = the record row (t261025-3669; 2 was
    /// the clip row until task1090 removed it), 3 = the show-window row
    /// (t261025-034f). Matches the ints the .slint side uses.
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
    refused_hotkey: Option<Refusal>,
    /// The folder picker returned a folder the auto-record guard turns down.
    /// Said under the 自動録画 group until the next add.
    folder_refused: bool,
    /// A retention change that would delete sessions, held back until the
    /// band under 保存領域 confirms it (t261018-34fe). Never in the file.
    retention_pending: Option<PendingRetention>,
    retention: RetentionDeps,
}

/// The two limits as the user set them, not yet saved (t261018-34fe).
struct PendingRetention {
    capacity_gb: i64,
    lifetime_days: i64,
    /// What these limits delete, as last counted.
    preview: RetentionPreview,
    /// The rows' `*_last` from before the hold, which 取り消す puts back.
    last: (i64, i64),
}

/// What counting a held-back retention change reads, and where its confirm
/// goes (t261018-34fe).
struct RetentionDeps {
    history: Rc<RefCell<History>>,
    review: Rc<RefCell<Review>>,
    cmd_tx: Sender<Cmd>,
    /// Recounts while a change is held: a recording ending or a listing can
    /// move what the new limits delete, and neither passes through this
    /// screen. ponytail: a 1 s poll over the in-memory list, only while held;
    /// a hook on `Msg::Sessions` if that ever shows up in a profile.
    recount: Rc<slint::Timer>,
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

/// Mutate → validate → save, handing back what the file now holds: any clamp or
/// hotkey-collision fix the validation applied (task121 owns those rules) is
/// what the UI shows, not what was asked for. `save` writes `validated()` and
/// `load` ends in `validated()`, so the validated copy is the file's contents
/// without reading it back (t261025-ea25).
pub(super) fn commit_settings(
    settings: &Mutex<AppSettings>,
    mutate: impl FnOnce(&mut AppSettings),
) -> AppSettings {
    commit_settings_checked(settings, mutate).0
}

/// `commit_settings`, also answering whether the file was written -- for the
/// one caller that reports it (the picker's last-executable hint). Every write
/// of `settings.json` goes through here (t261025-0e3d).
pub(super) fn commit_settings_checked(
    settings: &Mutex<AppSettings>,
    mutate: impl FnOnce(&mut AppSettings),
) -> (AppSettings, bool) {
    let Ok(mut current) = settings.lock() else {
        return (AppSettings::default(), false);
    };
    mutate(&mut current);
    *current = current.validated();
    let saved = settings_path().is_some_and(|path| match settings::save(&path, &current) {
        Ok(()) => true,
        Err(error) => {
            tracing::warn!("failed to save settings: {error}");
            false
        }
    });
    (current.clone(), saved)
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
    labels.set_audio_group(vol::audio_group(tr_locale()).into());
    labels.set_audio_group_sub(vol::audio_group_sub(tr_locale()).into());
    labels.set_audio_open(vol::audio_open(tr_locale()).into());
    labels.set_audio_close(vol::audio_close(tr_locale()).into());
    labels.set_audio_remove(vol::audio_remove(tr_locale()).into());
    labels.set_audio_target_remove(vol::audio_target_remove(tr_locale()).into());
    labels.set_audio_mic_denied(vol::audio_mic_denied(tr_locale()).into());
    labels.set_retention_capacity(settings_ui::retention_capacity_gb(tr_locale()).into());
    labels.set_session_lifetime(settings_ui::session_lifetime_days(tr_locale()).into());
    labels.set_retention_hint(settings_ui::retention_hint(tr_locale()).into());
    labels.set_unit_minutes(settings_ui::unit_minutes(tr_locale()).into());
    labels.set_unit_gigabytes(settings_ui::unit_gigabytes(tr_locale()).into());
    labels.set_unit_days(settings_ui::unit_days(tr_locale()).into());
    let ranges = [
        settings_ui::RETENTION_MINUTES_RANGE,
        settings_ui::RETENTION_CAPACITY_GB_RANGE,
        settings_ui::SESSION_LIFETIME_DAYS_RANGE,
    ];
    let error_texts: Vec<slint::SharedString> = ranges
        .iter()
        .map(|(min, max)| settings_ui::numeric_out_of_range(tr_locale(), *min, *max).into())
        .collect();
    labels.set_retention_minutes_error(error_texts[0].clone());
    labels.set_retention_capacity_error(error_texts[1].clone());
    labels.set_session_lifetime_error(error_texts[2].clone());
    labels.set_session_lifetime_min(ranges[2].0 as f32);
    labels.set_session_lifetime_max(ranges[2].1 as f32);
    labels.set_hotkey(settings_ui::hotkey(tr_locale()).into());
    labels.set_record_hotkey(settings_ui::record_hotkey(tr_locale()).into());
    labels.set_show_hotkey(settings_ui::show_hotkey(tr_locale()).into());
    labels.set_hotkey_unset(settings_ui::hotkey_unset(tr_locale()).into());
    labels.set_capture_armed(settings_ui::hotkey_capture_armed(tr_locale()).into());
    labels.set_capture_abort(settings_ui::hotkey_capture_abort(tr_locale()).into());
    labels.set_export_change(settings_ui::export_directory_change(tr_locale()).into());
    labels.set_directory_open(settings_ui::directory_open(tr_locale()).into());
    labels.set_buffer_label(settings_ui::buffer_directory_label(tr_locale()).into());
    labels.set_buffer_sub(settings_ui::buffer_directory_sub(tr_locale()).into());
    labels.set_clip_label(settings_ui::clip_directory_label(tr_locale()).into());
    labels.set_clip_sub(settings_ui::clip_directory_sub(tr_locale()).into());
    labels.set_buffer_change_blocked(
        settings_ui::buffer_directory_change_blocked(tr_locale()).into(),
    );
    // task4140: the 「インストール」 row. Read once behind a `OnceLock` and
    // logged once, so the second caller here -- a language change re-pushes
    // every label -- re-reads nothing: the registration cannot change without
    // the installer running, which stops this process first.
    //
    // An empty line is the installed machine, and the `.slint` side wraps the
    // row in an `if` on it. The reason is pushed either way; nothing draws it
    // while the line is empty.
    let registration = desktop::installer_registration();
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
    labels.set_retention_confirm(settings_ui::retention_confirm(tr_locale()).into());
    labels.set_retention_revert(settings_ui::retention_revert(tr_locale()).into());
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
    labels.set_seek_seconds(settings_ui::seek_seconds(tr_locale()).into());
    labels.set_seek_seconds_sub(settings_ui::seek_seconds_sub(tr_locale()).into());
    labels.set_seek_seconds_presets(slint::ModelRc::new(slint::VecModel::from(
        settings::SEEK_SECONDS
            .iter()
            .map(|seconds| {
                slint::SharedString::from(livia::ui_state::timeline::seconds_text(
                    tr_locale(),
                    i64::from(*seconds),
                ))
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
    let current = settings_snapshot(settings);
    // The closed rows' warning (t261003-82d6): the privacy switch is read only
    // when some registration has a microphone (3 registry reads).
    let denied = current
        .target_audio
        .values()
        .any(|target| super::review_volume::has_mic(Some(target)))
        && super::review_volume::microphone_access_denied();
    let audio_key = |name: &str| settings::target_audio_key(Some(name), false).unwrap_or_default();
    let mut rows: Vec<AutoCaptureEntry> = entries
        .iter()
        .map(|entry| AutoCaptureEntry {
            key: audio_key(&entry.name).into(),
            mic_denied: vol::mic_warning(current.target_audio.get(&audio_key(&entry.name)), denied),
            executable: entry.name.as_str().into(),
            // DS V28 AutoCaptureRow: the exe's own icon from the stored path;
            // the row falls back to the window glyph when this stays empty.
            icon: entry
                .path
                .as_deref()
                .and_then(super::app_meta::file_icon_rgba)
                .and_then(|(width, height, rgba)| super::image_from_rgba(width, height, &rgba))
                .unwrap_or_default(),
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
    // apply -- the row draws a folder glyph.
    rows.extend(current.auto_capture_folders.iter().map(|folder| {
        // task3830: the rule is split here, not in the row -- the
        // spelling questions (trailing separator, a volume root with
        // nothing above it) belong beside the rest of the rule
        // handling. `executable` still carries the whole path: it is
        // what `folder-removed` reports; `parent_path` is for reading,
        // not for identity.
        let (_, parent) = auto_capture::folder_row_parts(&folder.path);
        AutoCaptureEntry {
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
            // DS: a folder rule has no audio -- a registration is kept
            // by executable name, with no path to match a folder by.
            key: Default::default(),
            mic_denied: false,
        }
    }));
    // t261003-82d6: the 音声 group. An app's icon is read from its running
    // process when there is one -- the registration keeps no path.
    let audio: Vec<AutoCaptureEntry> = vol::audio_group_keys(&current.target_audio, &entries)
        .into_iter()
        .map(|key| {
            let screen = key == settings::SCREEN_AUDIO_KEY;
            AutoCaptureEntry {
                executable: if screen {
                    livia::ui_state::shell::control_whole_screen(tr_locale()).into()
                } else {
                    key.as_str().into()
                },
                icon: if screen {
                    slint::Image::default()
                } else {
                    livia::capture::targets::first_process_id_for_executable(&key)
                        .and_then(livia::capture::targets::executable_path_for_process)
                        .and_then(|path| super::app_meta::file_icon_rgba(&path))
                        .and_then(|(width, height, rgba)| {
                            super::image_from_rgba(width, height, &rgba)
                        })
                        .unwrap_or_default()
                },
                kind: if screen { KIND_SCREEN } else { KIND_TARGET },
                parent_path: Default::default(),
                ignored: false,
                mic_denied: vol::mic_warning(current.target_audio.get(&key), denied),
                key: key.into(),
            }
        })
        .collect();
    let vm = ui.global::<SettingsVm>();
    // The open row went away (its last sound removed): nothing stays open.
    let open = vm.get_audio_open_key();
    if !open.is_empty()
        && !rows
            .iter()
            .chain(&audio)
            .any(|row| row.key == open && row.kind != KIND_FOLDER)
    {
        vm.set_audio_open_key(Default::default());
        vm.set_audio_adding(false);
        vm.set_audio_rows(Default::default());
    }
    replace_rows(vm.get_auto_capture_executables(), rows, |m| {
        vm.set_auto_capture_executables(m)
    });
    replace_rows(vm.get_audio_targets(), audio, |m| vm.set_audio_targets(m));
}

/// Writes `rows` into the model in place when it is already a `VecModel`, so
/// the repeater keeps its row instances: replacing the model rebuilt the open
/// row and replayed its list's enter animation on every add and remove.
fn replace_rows<T: Clone + PartialEq + 'static>(
    current: slint::ModelRc<T>,
    rows: Vec<T>,
    set: impl FnOnce(slint::ModelRc<T>),
) {
    use slint::Model;
    let Some(model) = current.as_any().downcast_ref::<slint::VecModel<T>>() else {
        set(slint::ModelRc::new(slint::VecModel::from(rows)));
        return;
    };
    let len = rows.len();
    for (index, row) in rows.into_iter().enumerate() {
        if index >= model.row_count() {
            model.push(row);
        } else if model.row_data(index).as_ref() != Some(&row) {
            model.set_row_data(index, row);
        }
    }
    while model.row_count() > len {
        model.remove(model.row_count() - 1);
    }
}

/// UI-thread state of the settings rows' audio list (t261003-82d6). One of
/// each: only one row is ever open. The open row itself is
/// `SettingsVm.audio-open-key`, so the pane can close every row on entry.
#[derive(Default)]
struct AudioList {
    /// (display name, icon) per executable, lowercased.
    meta: HashMap<String, (String, Option<slint::Image>)>,
    candidates: Vec<ExtraAudioSource>,
    /// The device Menu's rows after 既定のマイク: (id, name).
    devices: Vec<(String, String)>,
    device_extra: usize,
}

/// The open row's tracks (DS AutoCaptureRow): the 音量 tab's rows while nothing
/// records, with the settings' states (`vol::settings_rows`) and no track.
/// Empty when every row is closed.
fn push_audio_tracks(ui: &AppWindow, current: &AppSettings, list: &mut AudioList) {
    let vm = ui.global::<SettingsVm>();
    let key = vm.get_audio_open_key().to_string();
    if key.is_empty() {
        vm.set_audio_rows(Default::default());
        return;
    }
    let target = current.target_audio.get(&key);
    let has_mic = super::review_volume::has_mic(target);
    let denied = has_mic && super::review_volume::microphone_access_denied();
    let connected = if has_mic {
        livia::capture::audio::microphones()
    } else {
        Vec::new()
    };
    let screen = key == settings::SCREEN_AUDIO_KEY;
    let rows = vol::settings_rows(
        target,
        &|id| connected.iter().any(|(known, _)| known == id),
        denied,
        vol::add_blocked(current.capture_all_audio, screen),
    );
    let locale = tr_locale();
    let out: Vec<VolumeRowVm> = rows
        .iter()
        .map(|row| {
            let source = row
                .extra
                .and_then(|index| target.and_then(|target| target.extras.get(index)))
                .map(|extra| &extra.source);
            let (name, device, icon) = match source {
                Some(source) => super::review_volume::source_label(&mut list.meta, source, locale),
                None if screen => (
                    livia::ui_state::shell::control_whole_screen(locale).to_owned(),
                    String::new(),
                    None,
                ),
                // The target: named as the auto-record list has it when it is
                // there (display name, stored path), else by its key.
                None => {
                    let app = current
                        .auto_capture_executables
                        .iter()
                        .find(|app| app.name.trim().eq_ignore_ascii_case(&key));
                    let (name, icon) = super::review_volume::meta(
                        &mut list.meta,
                        app.map_or(key.as_str(), |app| app.name.as_str()),
                        app.and_then(|app| app.path.as_deref()),
                        app.and_then(|app| app.display_name.as_deref()),
                    );
                    (name, String::new(), icon)
                }
            };
            VolumeRowVm {
                kind: row.kind as i32,
                name: name.into(),
                device: device.into(),
                caption: vol::state_text(locale, row.kind, row.state).into(),
                warning: row.state == vol::RowState::Denied,
                extra: row.extra.map_or(-1, |index| index as i32),
                on: row.on,
                locked: row.state == vol::RowState::Merged,
                track: -1,
                volume_ratio: 1.0,
                muted: false,
                icon: icon.unwrap_or_default(),
                screen: screen && row.kind == vol::RowKind::Target,
            }
        })
        .collect();
    replace_rows(vm.get_audio_rows(), out, |m| vm.set_audio_rows(m));
}

/// The settings rows' accordion (t261003-82d6). Every change goes through
/// `review::change_target_audio`, the path the 音量 tab and the `track` control
/// command take, so a recording of that target hears it at once. The 音量 tab
/// follows by itself: it re-renders from the settings every second while it
/// shows a recording, and shows no registration otherwise.
fn wire_audio_rows(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    controller: &CaptureController,
) {
    let vm = ui.global::<SettingsVm>();
    let list = Rc::new(RefCell::new(AudioList::default()));

    {
        // The pane has just closed every row; the lists follow whatever the
        // 音量 tab or the control port registered meanwhile.
        let weak = ui.as_weak();
        let settings = settings.clone();
        vm.on_settings_opened(move || {
            let Some(ui) = weak.upgrade() else { return };
            push_auto_capture_list(&ui, &settings);
            ui.global::<SettingsVm>().set_audio_rows(Default::default());
        });
    }

    settings_handler!(ui, (settings, list), on_audio_row_toggled, |key| {
        let vm = ui.global::<SettingsVm>();
        let next = if vm.get_audio_open_key() == key {
            Default::default()
        } else {
            key
        };
        vm.set_audio_open_key(next);
        vm.set_audio_adding(false);
        push_audio_tracks(&ui, &settings_snapshot(&settings), &mut list.borrow_mut());
    });

    // One edit of the open row's registration, then both lists again: a row
    // in 音声 goes when its last sound does, and the closed rows' warning moves.
    let edit = {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let controller = controller.clone();
        let list = list.clone();
        Rc::new(move |change: &dyn Fn(&AudioList, &mut TargetAudio)| {
            let Some(ui) = weak.upgrade() else { return };
            let key = ui.global::<SettingsVm>().get_audio_open_key().to_string();
            if key.is_empty() {
                return;
            }
            let mut list = list.borrow_mut();
            let _ = super::review::change_target_audio(&settings, &controller, &key, |target| {
                change(&list, target)
            });
            push_auto_capture_list(&ui, &settings);
            push_audio_tracks(&ui, &settings_snapshot(&settings), &mut list);
        })
    };

    settings_handler!(ui, (settings, list), on_audio_add_opened, || {
        let vm = ui.global::<SettingsVm>();
        let key = vm.get_audio_open_key().to_string();
        let current = settings_snapshot(&settings);
        let target = current.target_audio.get(&key);
        let blocked =
            vol::add_blocked(current.capture_all_audio, key == settings::SCREEN_AUDIO_KEY);
        vm.set_audio_blocked_text(
            blocked
                .map(|blocked| vol::blocked_text(tr_locale(), blocked))
                .unwrap_or_default()
                .into(),
        );
        vm.set_audio_mic_added(super::review_volume::has_mic(target));
        let mut list = list.borrow_mut();
        // The row's own target is left out, as the tab leaves out what it
        // records.
        let (sources, shown) = if blocked.is_none() {
            super::review_volume::add_list(&mut list.meta, target, &key)
        } else {
            Default::default()
        };
        list.candidates = sources;
        vm.set_audio_candidates(slint::ModelRc::new(slint::VecModel::from(shown)));
    });

    {
        let edit = edit.clone();
        let weak = ui.as_weak();
        vm.on_audio_add_app(move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            edit(&|list, target| {
                if let Some(source) = list.candidates.get(index) {
                    target.add(source.clone());
                }
            });
            if let Some(ui) = weak.upgrade() {
                ui.global::<SettingsVm>().set_audio_adding(false);
            }
        });
    }

    {
        let edit = edit.clone();
        let weak = ui.as_weak();
        vm.on_audio_add_mic(move || {
            edit(&|_, target| {
                target.add(ExtraAudioSource::Mic {
                    device_id: None,
                    device_name: None,
                })
            });
            if let Some(ui) = weak.upgrade() {
                ui.global::<SettingsVm>().set_audio_adding(false);
            }
        });
    }

    {
        let edit = edit.clone();
        vm.on_audio_switch_toggled(move |index| {
            edit(&|_, target| {
                if let Some(entry) = usize::try_from(index)
                    .ok()
                    .and_then(|index| target.extras.get_mut(index))
                {
                    entry.enabled = !entry.enabled;
                }
            });
        });
    }

    {
        // DS: 登録から外す loses nothing (the recorded tracks stay), so no hold,
        // no confirmation, no toast -- the row going is the answer.
        let edit = edit.clone();
        vm.on_audio_removed(move |index| {
            edit(&|_, target| {
                if let Ok(index) = usize::try_from(index) {
                    if index < target.extras.len() {
                        target.extras.remove(index);
                    }
                }
            });
        });
    }

    {
        // t261003-4d0d: the × on an app's row empties its added sounds, so the
        // row goes (`audio_group_keys`) and, when it was open, the open key
        // with it (`push_auto_capture_list`). The key itself stays, as the
        // auto-capture × leaves its registration; no hold, confirm or toast.
        let weak = ui.as_weak();
        let settings = settings.clone();
        let controller = controller.clone();
        let list = list.clone();
        vm.on_audio_target_removed(move |key| {
            let Some(ui) = weak.upgrade() else { return };
            let _ = super::review::change_target_audio(&settings, &controller, &key, |target| {
                target.extras.clear()
            });
            push_auto_capture_list(&ui, &settings);
            push_audio_tracks(&ui, &settings_snapshot(&settings), &mut list.borrow_mut());
        });
    }

    settings_handler!(ui, (settings, list), on_audio_device_opened, |index| {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let vm = ui.global::<SettingsVm>();
        let key = vm.get_audio_open_key().to_string();
        let current_id = settings_snapshot(&settings)
            .target_audio
            .get(&key)
            .and_then(|target| target.extras.get(index).cloned())
            .and_then(|extra| match extra.source {
                ExtraAudioSource::Mic { device_id, .. } => device_id,
                ExtraAudioSource::App { .. } => None,
            });
        let (devices, entries, checked) = super::review_volume::device_menu(current_id.as_deref());
        let mut list = list.borrow_mut();
        list.devices = devices;
        list.device_extra = index;
        vm.set_audio_devices(slint::ModelRc::new(slint::VecModel::from(entries)));
        vm.set_audio_device_index(checked);
    });

    vm.on_audio_device_chosen(move |choice| {
        edit(&|list, target| {
            let device = usize::try_from(choice - 1)
                .ok()
                .and_then(|index| list.devices.get(index).cloned());
            super::review_volume::set_mic_device(target, list.device_extra, device);
        });
    });
}

/// `AutoCaptureEntry::kind` (task3610), matching the ints the `.slint` side
/// switches on.
const KIND_EXECUTABLE: i32 = 0;
const KIND_FOLDER: i32 = 1;
/// t261003-82d6: the 音声 group's 画面全体 row, and an app with added sounds
/// that is not in the auto-record list.
const KIND_SCREEN: i32 = 2;
const KIND_TARGET: i32 = 3;

/// Entries registered before task2560 carry no metadata. When such an entry's
/// exe happens to be running at render time, resolve and persist its path and
/// display name once -- after that the row draws from the store. An entry that
/// kept its path but lost its display name (task2590) re-resolves straight
/// from the file, no running process needed. Steady state (everything
/// upgraded, or nothing resolvable) writes nothing.
fn upgrade_auto_capture_metadata(settings: &Mutex<AppSettings>) -> Vec<AutoCaptureApp> {
    let entries = settings_snapshot(settings).auto_capture_executables;
    match livia::ui_state::auto_capture::upgrade_plan(
        &entries,
        super::app_meta::describe_executable,
        super::app_meta::resolve_entry,
    ) {
        Some(upgraded) => {
            commit_settings(settings, |current| {
                current.auto_capture_executables = upgraded.clone();
            })
            .auto_capture_executables
        }
        None => entries,
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
    // t261018-34fe: a held-back retention change is what the two rows, their
    // chips and the StorageMeter show, while the file keeps the old limits.
    let held;
    let current = match &screen.retention_pending {
        Some(pending) => {
            held = AppSettings {
                retention_capacity_gb: pending.capacity_gb,
                session_lifetime_days: pending.lifetime_days,
                ..current.clone()
            };
            &held
        }
        None => current,
    };
    let pending = screen
        .retention_pending
        .as_ref()
        .map(|pending| &pending.preview);
    vm.set_retention_pending_text(
        pending
            .map(|preview| {
                settings_ui::retention_pending_text(
                    tr_locale(),
                    preview.sessions,
                    &sessions_ui::format_bytes(preview.bytes),
                )
            })
            .unwrap_or_default()
            .into(),
    );
    vm.set_retention_pending_breakdown(
        pending
            .map(|preview| {
                settings_ui::retention_pending_breakdown(
                    tr_locale(),
                    preview.over_capacity,
                    preview.too_old,
                )
            })
            .unwrap_or_default()
            .into(),
    );
    vm.set_retention_pending_in_use(
        pending
            .map(|preview| settings_ui::retention_pending_in_use(tr_locale(), preview.in_use))
            .unwrap_or_default()
            .into(),
    );
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
    vm.set_seek_seconds_index(
        settings::SEEK_SECONDS
            .iter()
            .position(|seconds| *seconds == current.seek_seconds)
            .map_or(-1, |index| index as i32),
    );
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
    // round8 §3-3: the row draws one cap per key, and slint cannot split a
    // string -- so the split happens here, where the chord is already a string
    // this side owns.
    vm.set_marker_keys(hotkey_caps(&screen.draft_hotkey));
    vm.set_armed(screen.armed);
    // task730: another app holding the chord is worth saying on the row, and
    // is a different problem from a registrar that would not start at all.
    // t260928-7e9a: and so is a chord refused as it was typed -- the field
    // keeps the chord that still holds and names the one that did not.
    // DS HotkeyField: the refusal names the chord, spelled the way running
    // text spells it. t261025-3669: and says when the other row holds it.
    let marker_alert = hotkey_alert(
        screen.refused_hotkey.as_ref(),
        1,
        hotkeys.unavailable(Role::Marker, true),
        &screen.draft_hotkey,
    );
    vm.set_marker_taken(!marker_alert.is_empty());
    ui.global::<SettingsLabels>()
        .set_state_taken(marker_alert.into());
    // The record row (t261025-3669): empty keys draw 「未設定」.
    vm.set_record_keys(hotkey_caps(&screen.record_draft));
    vm.set_record_alert(
        hotkey_alert(
            screen.refused_hotkey.as_ref(),
            2,
            hotkeys.unavailable(Role::Record, !screen.record_draft.is_empty()),
            &screen.record_draft,
        )
        .into(),
    );
    vm.set_show_keys(hotkey_caps(&screen.show_draft));
    vm.set_show_alert(
        hotkey_alert(
            screen.refused_hotkey.as_ref(),
            3,
            hotkeys.unavailable(Role::Show, !screen.show_draft.is_empty()),
            &screen.show_draft,
        )
        .into(),
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
#[allow(clippy::too_many_arguments)]
pub(super) fn wire(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    stored: &AppSettings,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    status_line: &Rc<RefCell<toast::Toast>>,
    controller: &CaptureController,
    tray: &slint::Weak<TrayIcon>,
    (history, review, cmd_tx): (&Rc<RefCell<History>>, &Rc<RefCell<Review>>, &Sender<Cmd>),
) {
    {
        push_settings_labels(ui);
        let screen = Rc::new(RefCell::new(SettingsScreen {
            draft_hotkey: stored.hotkey.clone(),
            record_draft: stored.record_hotkey.clone(),
            show_draft: stored.show_hotkey.clone(),
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
            retention_pending: None,
            retention: RetentionDeps {
                history: history.clone(),
                review: review.clone(),
                cmd_tx: cmd_tx.clone(),
                recount: Rc::new(slint::Timer::default()),
            },
        }));
        let flash = status_line.clone();
        render_settings(ui, settings, stored, &screen.borrow(), &hotkeys.borrow());

        // One call per group, named for the callbacks it holds (t261025-0e3d:
        // the old names had drifted from their bodies, and four of them called
        // the next one in a chain).
        wire_presets_and_auto_capture(ui, settings, &screen, hotkeys);
        wire_buffer_root(ui, settings, &screen, hotkeys, &flash, controller);
        wire_typed_values_and_ring(ui, settings, &screen, hotkeys);
        wire_hotkey_fields(ui, settings, &screen, hotkeys, &flash);
        wire_language_theme_and_retention(ui, settings, &screen, hotkeys, tray);
        wire_switches_and_folders(ui, settings, &screen, hotkeys, &flash);
        wire_audio_rows(ui, settings, controller);
        wire_retention_band(ui, settings, &screen, hotkeys, &flash);
    }
}

// ---------- the held-back retention change (t261018-34fe) ----------

/// What `next` deletes against the stored `saved`, counted over the history's
/// in-memory list. `None` only while the history or review is borrowed.
fn retention_preview(
    screen: &SettingsScreen,
    saved: &AppSettings,
    next: &AppSettings,
) -> Option<RetentionPreview> {
    let history = screen.retention.history.try_borrow().ok()?;
    let review = screen.retention.review.try_borrow().ok()?;
    let mut in_use = history.state.active_ids().to_vec();
    in_use.extend(super::review::loaded_session_id(&review));
    let now_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    Some(ring_buffer::retention_preview(
        history.state.sessions(),
        saved.retention_limits(),
        next.retention_limits(),
        now_nanos,
        &in_use,
    ))
}

/// The stored settings with a held-back change laid over them.
fn retention_candidate(screen: &SettingsScreen, saved: &AppSettings) -> AppSettings {
    let mut next = saved.clone();
    if let Some(pending) = &screen.retention_pending {
        next.retention_capacity_gb = pending.capacity_gb;
        next.session_lifetime_days = pending.lifetime_days;
    }
    next
}

/// Every edit to the 容量 / 期間 rows ends here -- chip, Enter, switch. One
/// that deletes a session the stored limits keep is held back, unsaved;
/// anything else is saved as before, together with whatever was held.
fn stage_retention(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    mutate: impl FnOnce(&mut AppSettings),
) {
    let saved = settings_snapshot(settings);
    let mut next = retention_candidate(&screen.borrow(), &saved);
    mutate(&mut next);
    settle_retention(ui, settings, screen, hotkeys, &saved, next);
}

fn settle_retention(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    saved: &AppSettings,
    next: AppSettings,
) {
    let Some(preview) = retention_preview(&screen.borrow(), saved, &next) else {
        tracing::warn!("retention edit dropped: the session list was busy");
        return;
    };
    let shown = if preview.grows {
        let mut held = screen.borrow_mut();
        let last = held.retention_pending.as_ref().map_or(
            (held.retention_capacity_last, held.session_lifetime_last),
            |pending| pending.last,
        );
        held.retention_pending = Some(PendingRetention {
            capacity_gb: next.retention_capacity_gb,
            lifetime_days: next.session_lifetime_days,
            preview,
            last,
        });
        let timer = held.retention.recount.clone();
        drop(held);
        if !timer.running() {
            start_retention_recount(ui, settings, screen, hotkeys, &timer);
        }
        saved.clone()
    } else {
        let mut held = screen.borrow_mut();
        held.retention_pending = None;
        held.retention.recount.stop();
        drop(held);
        commit_settings(settings, |current| {
            current.retention_capacity_gb = next.retention_capacity_gb;
            current.session_lifetime_days = next.session_lifetime_days;
        })
    };
    render_settings(ui, settings, &shown, &screen.borrow(), &hotkeys.borrow());
}

fn start_retention_recount(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    timer: &slint::Timer,
) {
    let weak = ui.as_weak();
    let settings = settings.clone();
    // Weak: the timer lives in the screen it reads.
    let screen_weak = Rc::downgrade(screen);
    let hotkeys = hotkeys.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_secs(1),
        move || {
            let (Some(ui), Some(screen)) = (weak.upgrade(), screen_weak.upgrade()) else {
                return;
            };
            let saved = settings_snapshot(&settings);
            let (next, counted) = {
                let held = screen.borrow();
                let Some(pending) = &held.retention_pending else {
                    held.retention.recount.stop();
                    return;
                };
                (retention_candidate(&held, &saved), pending.preview.clone())
            };
            let Some(preview) = retention_preview(&screen.borrow(), &saved, &next) else {
                return;
            };
            // Nothing moved: no re-render every second.
            if preview != counted {
                settle_retention(&ui, &settings, &screen, &hotkeys, &saved, next);
            }
        },
    );
}

/// 取り消す, and leaving the pane: the held change is dropped and the rows go
/// back to what the file holds. `true` when there was one to drop.
fn revert_retention(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
) -> bool {
    {
        let mut held = screen.borrow_mut();
        let Some(pending) = held.retention_pending.take() else {
            return false;
        };
        (held.retention_capacity_last, held.session_lifetime_last) = pending.last;
        held.retention.recount.stop();
    }
    let saved = settings_snapshot(settings);
    render_settings(ui, settings, &saved, &screen.borrow(), &hotkeys.borrow());
    true
}

fn wire_retention_band(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_retention_confirmed,
        || {
            let (pending, cmd_tx, loaded) = {
                let mut held = screen.borrow_mut();
                let Some(pending) = held.retention_pending.take() else {
                    return;
                };
                held.retention.recount.stop();
                let loaded = held
                    .retention
                    .review
                    .try_borrow()
                    .ok()
                    .and_then(|review| super::review::loaded_session_id(&review));
                (pending, held.retention.cmd_tx.clone(), loaded)
            };
            // Saved first: the sweep reads the limits off the shared settings
            // when it runs, so sending it ahead would sweep with the old ones.
            let saved = commit_settings(&settings, |current| {
                current.retention_capacity_gb = pending.capacity_gb;
                current.session_lifetime_days = pending.lifetime_days;
            });
            tracing::info!(
                event = "retention_change_confirmed",
                capacity_gb = pending.capacity_gb,
                lifetime_days = pending.lifetime_days,
                sessions = pending.preview.sessions,
                bytes = pending.preview.bytes,
                in_use = pending.preview.in_use,
                "the sweep runs now"
            );
            let _ = cmd_tx.send(Cmd::ReclaimConfirmed(loaded));
            render_settings(&ui, &settings, &saved, &screen.borrow(), &hotkeys.borrow());
        }
    );
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_retention_reverted,
        || {
            revert_retention(&ui, &settings, &screen, &hotkeys);
        }
    );
    settings_handler!(
        ui,
        (settings, screen, hotkeys, flash),
        on_retention_left,
        || {
            if revert_retention(&ui, &settings, &screen, &hotkeys) {
                flash_settings_message(
                    &ui,
                    &flash,
                    settings_ui::retention_reverted(tr_locale()).into(),
                    true,
                );
            }
        }
    );
}

/// The confirmed sweep's answer (`Msg::RetentionApplied`): the band says what
/// went for a few seconds, then closes.
pub(super) fn retention_applied(ui: &AppWindow, removed: u32) {
    let text: slint::SharedString =
        settings_ui::retention_applied_text(tr_locale(), removed).into();
    ui.global::<SettingsVm>()
        .set_retention_done_text(text.clone());
    let weak = ui.as_weak();
    slint::Timer::single_shot(std::time::Duration::from_secs(4), move || {
        let Some(ui) = weak.upgrade() else { return };
        let vm = ui.global::<SettingsVm>();
        if vm.get_retention_done_text() == text {
            vm.set_retention_done_text("".into());
        }
    });
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
    if let Err((role, _)) = resumed {
        flash_hotkey_failure(ui, flash, role_hotkey(&settings_snapshot(settings), role));
    }
}

/// The stored chord for `role`.
fn role_hotkey(settings: &AppSettings, role: Role) -> &str {
    match role {
        Role::Marker => &settings.hotkey,
        Role::Record => &settings.record_hotkey,
        Role::Show => &settings.show_hotkey,
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
    control: i32,
    captured: &str,
) -> bool {
    // t261025-3669: row 2 is the record chord, which may be unset (empty).
    // t261025-034f: row 3 is the show-window chord, likewise.
    let role = hotkey_role(control);
    let current = settings_snapshot(settings);
    let previous = role_hotkey(&current, role).to_owned();
    screen.borrow_mut().refused_hotkey = None;
    let accelerator = match settings_ui::commit_capture(&previous, captured) {
        settings_ui::HotkeyCommit::Unchanged => return false,
        settings_ui::HotkeyCommit::Apply(accelerator) => accelerator,
    };

    let applied = hotkeys.borrow_mut().apply(role, &accelerator);
    // `apply` re-registers its own role only. The other row's chord was
    // suspended with it when the field armed (task160), so it goes back here.
    resume_hotkeys(ui, settings, hotkeys, flash);
    let registered = applied.is_ok();
    let shown = settings_ui::displayed_hotkey(&previous, &accelerator, registered);
    {
        let mut screen = screen.borrow_mut();
        match role {
            Role::Marker => screen.draft_hotkey = shown.clone(),
            Role::Record => screen.record_draft = shown.clone(),
            Role::Show => screen.show_draft = shown.clone(),
        }
    }
    if let Err(error) = applied {
        // t260928-7e9a: said on the field, which is in front of the user --
        // no toast (DS: one place per message). `apply` already rolled back
        // to the chord we were holding, so the OS side is settled either way.
        screen.borrow_mut().refused_hotkey = Some(Refusal {
            control,
            chord: accelerator,
            clash: error == livia::ui_state::hotkeys::CLASH,
        });
        return true;
    }
    let next = commit_settings(settings, |current| match role {
        Role::Marker => current.hotkey = shown,
        Role::Record => current.record_hotkey = shown,
        Role::Show => current.show_hotkey = shown,
    });
    // From the reload, not the draft: `resolve_hotkey` may have adjusted it.
    let saved = {
        let mut screen = screen.borrow_mut();
        match role {
            Role::Marker => {
                screen.draft_hotkey = next.hotkey.clone();
                settings_ui::hotkey_display(&next.hotkey)
            }
            Role::Record | Role::Show => {
                let (draft, stored) = match role {
                    Role::Show => (&mut screen.show_draft, &next.show_hotkey),
                    _ => (&mut screen.record_draft, &next.record_hotkey),
                };
                draft.clone_from(stored);
                if stored.is_empty() {
                    settings_ui::hotkey_unset(tr_locale()).to_owned()
                } else {
                    settings_ui::hotkey_display(stored)
                }
            }
        }
    };
    flash_settings_message(
        ui,
        flash,
        settings_ui::hotkey_saved(tr_locale(), &saved),
        true,
    );
    true
}

/// The chip rows and the auto-capture list (its apps' removal, the folder
/// rules' add and remove).
fn wire_presets_and_auto_capture(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
) {
    // The chip rows (round5 §7-B). One handler for all four: the field id
    // picks which setting a press lands on.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_preset_clicked,
        |field, index| {
            let field = PresetField::from(field);
            let index = usize::try_from(index).unwrap_or(usize::MAX);
            // t261018-34fe: the two rows that delete sessions are held
            // back when they would.
            if matches!(field, PresetField::Capacity | PresetField::Lifetime) {
                let presets: &[i64] = if field == PresetField::Capacity {
                    &settings_ui::RETENTION_CAPACITY_PRESETS
                } else {
                    &settings_ui::SESSION_LIFETIME_PRESETS
                };
                let Some(value) = presets.get(index).copied() else {
                    return;
                };
                stage_retention(&ui, &settings, &screen, &hotkeys, |current| {
                    if field == PresetField::Capacity {
                        current.retention_capacity_gb = value;
                    } else {
                        current.session_lifetime_days = value;
                    }
                });
                return;
            }
            let next = commit_settings(&settings, |current| match field {
                PresetField::RetentionMinutes => {
                    if let Some(value) = settings_ui::RETENTION_MINUTES_PRESETS.get(index) {
                        current.retention_minutes = *value as u16;
                    }
                }
                PresetField::FrameRate => {
                    if let Some(value) = settings_ui::FRAME_RATE_PRESETS.get(index) {
                        current.frame_rate = *value as u8;
                    }
                }
                PresetField::SeekSeconds => {
                    if let Some(value) = settings::SEEK_SECONDS.get(index) {
                        current.seek_seconds = *value;
                    }
                }
                PresetField::Capacity | PresetField::Lifetime | PresetField::Other => {}
            });
            if field == PresetField::SeekSeconds {
                livia::ui_state::playback::set_stage_tap_seconds(next.seek_seconds);
                super::review::push_seek_labels(&ui);
            }
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        }
    );

    // task1980. The only half of the registration this screen owns: an app
    // that is registered but not running has no picker tile to press, so
    // without this row there would be no way off the list.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_auto_capture_removed,
        |name| {
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
        }
    );

    // task3610: the 「フォルダを追加」 button. One step like the clip folder's
    // 変更 (task2970) and unlike the buffer root's pick-then-confirm -- nothing
    // already written moves, so there is nothing to warn about.
    //
    // The guard is `folder_rule_allowed`, the same pure function task3600's
    // poll reads, so a folder this screen accepts is one the poll will act on.
    // Refusing here rather than storing a rule that would be ignored: the guard
    // is invisible from a folder picker, so the danger line under the group is
    // the only place its rule can be said.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_auto_capture_folder_add,
        || {
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
                    "the picked folder is wider than this feature acts on"
                );
                // t260928-7e9a: under the group, in danger, rather than a
                // toast (DS 知らせ方: next to where it happened).
                screen.borrow_mut().folder_refused = true;
                rerender(&ui, &settings, &screen, &hotkeys);
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
        }
    );

    // The × on a folder row. It gets the `render_autorec` twin of the
    // executable handler's since task3670: the review panel's toggle used to be
    // a view of the *executable* list alone, so a folder rule never reached it
    // -- it reads `covered_by` now, and a rule removed here has to take the
    // toggle it was lighting with it. (The picker's badge needs no wiring: its
    // 1s poll re-reads both lists by itself.)
    settings_handler!(ui, (settings), on_auto_capture_folder_removed, |path| {
        let next = commit_settings(&settings, |current| {
            current.auto_capture_folders =
                auto_capture::unregister_folder(&current.auto_capture_folders, &path);
        });
        tracing::info!(
            target: "task1970_auto_capture",
            event = "auto_capture_folder_unregistered",
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

/// The buffer root: putting it back, changing it and the confirm step.
fn wire_buffer_root(
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
    settings_handler!(
        ui,
        (screen, settings, hotkeys, flash, controller),
        on_buffer_reset_clicked,
        || {
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
        }
    );

    // Moving the buffer root is two steps: pick, then confirm. The pick
    // alone changes nothing -- what the dialog explains is that existing
    // recordings stay where they are and the history follows the new root.
    settings_handler!(ui, (screen, settings, hotkeys), on_buffer_change, || {
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

    {
        // Through the controller, not the bare `apply_buffer_root`: only
        // the instance method rebuilds the session catalog, without which
        // edits and discards keep resolving against the old root.
        settings_handler!(
            ui,
            (screen, settings, hotkeys, flash, controller),
            on_buffer_confirm_accepted,
            || {
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
            }
        );
    }

    settings_handler!(
        ui,
        (screen, settings, hotkeys),
        on_buffer_confirm_cancelled,
        || {
            screen.borrow_mut().buffer_confirm = None;
            let current = settings_snapshot(&settings);
            render_settings(
                &ui,
                &settings,
                &current,
                &screen.borrow(),
                &hotkeys.borrow(),
            );
        }
    );
}

/// The typed values of the chip rows and the ring-buffer switch.
fn wire_typed_values_and_ring(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
) {
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_preset_edited,
        |field, text| {
            let (min, max) = match PresetField::from(field) {
                PresetField::RetentionMinutes => settings_ui::RETENTION_MINUTES_RANGE,
                PresetField::Capacity => settings_ui::RETENTION_CAPACITY_GB_RANGE,
                PresetField::Lifetime => settings_ui::SESSION_LIFETIME_DAYS_RANGE,
                _ => return,
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
            // carries a free field again.
            match PresetField::from(field) {
                // `preset_draft` already refused anything outside
                // RETENTION_MINUTES_RANGE (5..=1440), so the cast cannot
                // lose anything.
                PresetField::RetentionMinutes => {
                    let next = commit_settings(&settings, |current| {
                        current.retention_minutes = value as u16;
                    });
                    render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
                }
                // t261018-34fe: held back when it would delete sessions.
                PresetField::Capacity => {
                    stage_retention(&ui, &settings, &screen, &hotkeys, |current| {
                        current.retention_capacity_gb = value;
                    })
                }
                PresetField::Lifetime => {
                    stage_retention(&ui, &settings, &screen, &hotkeys, |current| {
                        current.session_lifetime_days = value;
                    })
                }
                _ => {}
            }
        }
    );

    // task1410: the switch decides whether footage older than the buffer
    // length is dropped, nothing else. It no longer stops a recording, and
    // it no longer starts or blocks one -- a session takes its retention at
    // start, so a flip only reaches the next one.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_ring_buffer_toggled,
        |enabled| {
            let next = commit_settings(&settings, |current| current.ring_buffer_enabled = enabled);
            tracing::info!(
                event = "ring_buffer_toggled",
                enabled,
                "the next recording takes this"
            );
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        }
    );
}

/// The three hotkey fields: arming, the typed chord, reset and blur.
fn wire_hotkey_fields(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    settings_handler!(ui, (settings, screen, hotkeys), on_hk_clicked, |control| {
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
        rerender(&ui, &settings, &screen, &hotkeys);
    });

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
                        let Some(shortcut) = settings_ui::compose_shortcut(
                            &text,
                            settings_ui::Modifiers {
                                ctrl,
                                alt,
                                shift,
                                meta,
                            },
                        ) else {
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
                    let applied = commit_hotkey_capture(
                        &ui, &settings, &screen, &hotkeys, &flash, control, &shortcut,
                    );
                    if !applied {
                        resume_hotkeys(&ui, &settings, &hotkeys, &flash);
                    }
                } else if escaped {
                    resume_hotkeys(&ui, &settings, &hotkeys, &flash);
                }
                rerender(&ui, &settings, &screen, &hotkeys);
                true
            });
    }

    settings_handler!(
        ui,
        (settings, screen, hotkeys, flash),
        on_hk_reset,
        |control| {
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
                control,
                // The record and show rows go back to unset, their default
                // (t261025-3669, t261025-034f).
                match hotkey_role(control) {
                    Role::Marker => livia::DEFAULT_HOTKEY,
                    Role::Record | Role::Show => "",
                },
            );
            if !applied && was_armed {
                resume_hotkeys(&ui, &settings, &hotkeys, &flash);
            }
            rerender(&ui, &settings, &screen, &hotkeys);
        }
    );

    settings_handler!(
        ui,
        (settings, screen, hotkeys, flash),
        on_hk_blurred,
        |control| {
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
                    rerender(&ui, &settings, &screen, &hotkeys);
                }
                return;
            }
            resume_hotkeys(&ui, &settings, &hotkeys, &flash);
            rerender(&ui, &settings, &screen, &hotkeys);
        }
    );
}

/// The language and theme pickers and the two retention switches.
fn wire_language_theme_and_retention(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    tray: &slint::Weak<TrayIcon>,
) {
    // task1150. Its own callback rather than a preset index: the language
    // is the one setting whose change has to re-push every label in the
    // app, and it stores a string rather than a number. The row's
    // SegmentedControl speaks `LANGUAGE_SETTINGS` indices (t260927-fde8).
    settings_handler!(
        ui,
        (settings, screen, hotkeys, tray),
        on_language_selected,
        |index| {
            let setting = locale::language_setting(index);
            apply_language(&ui, &settings, &screen, &hotkeys, &tray, setting);
        }
    );

    // round16. A plain index off `THEME_SETTINGS`, and no label re-push: unlike
    // the language, nothing in the app is worded differently in the other
    // theme. `render_settings` is what actually repaints -- see the push there.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_theme_selected,
        |index| {
            let next = commit_settings(&settings, |current| {
                current.theme = settings_ui::theme_setting(index).to_owned();
            });
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        }
    );

    // task1490, replacing round8 §3-2's 「なし」 chip. Its own callback
    // rather than a preset index for the same reason the chip had one: off
    // is not a value on the row's scale, it takes the row off the scale.
    // The number the row goes back to lives here, not in the file -- off is
    // stored as `0` and there is nowhere in `settings.json` for it.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_retention_toggled,
        |field, on| {
            let capacity = match PresetField::from(field) {
                PresetField::Capacity => true,
                PresetField::Lifetime => false,
                _ => return,
            };
            // The borrow ends before `commit_settings` runs, because
            // `render_settings` below borrows the same cell.
            let stored_value = {
                let mut screen = screen.borrow_mut();
                // What the row shows, held-back change included
                // (t261018-34fe).
                let current = retention_candidate(&screen, &settings_snapshot(&settings));
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
            stage_retention(&ui, &settings, &screen, &hotkeys, |current| {
                if capacity {
                    current.retention_capacity_gb = stored_value;
                } else {
                    current.session_lifetime_days = stored_value;
                }
            });
        }
    );
}

/// The capture switches (cursor, other apps' audio, codec, autostart), the
/// buffer folder's open and the clip folder's change, reset and open.
fn wire_switches_and_folders(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    screen: &Rc<RefCell<SettingsScreen>>,
    hotkeys: &Rc<RefCell<Hotkeys>>,
    flash: &Rc<RefCell<toast::Toast>>,
) {
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_cursor_toggled,
        |include| {
            let next = commit_settings(&settings, |current| current.include_cursor = include);
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        }
    );

    // task1430. Read when a recording starts, so like the ring switch above
    // this only reaches the next one.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_capture_all_audio_toggled,
        |capture_all| {
            let next =
                commit_settings(&settings, |current| current.capture_all_audio = capture_all);
            tracing::info!(
                event = "capture_all_audio_toggled",
                enabled = capture_all,
                "the next recording takes this"
            );
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
            // t261003-c2cb: an open row's apps go to the target's track
            // and back. The list there is only an icon cache. The add list
            // closes: its candidates and its reason were built for the
            // old value (user ruling 2026-10-03).
            ui.global::<SettingsVm>().set_audio_adding(false);
            push_audio_tracks(&ui, &next, &mut AudioList::default());
        }
    );

    // task1760. Same "reaches the next recording" contract as the switch
    // above; the row is only reachable at all where AV1 both encodes and
    // decodes, and `CaptureController::start` checks that again anyway.
    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_codec_av1_toggled,
        |av1| {
            let codec = settings_ui::codec_from_switch(av1);
            let next = commit_settings(&settings, |current| current.codec = codec);
            tracing::info!(
                event = "recording_codec_changed",
                codec = ?codec,
                "the next recording takes this"
            );
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        }
    );

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
            let flash = flash.clone();
            // t260923-916e: explorer can stall the calling thread for minutes.
            desktop::open_off_thread(
                &weak,
                move || open_directory(&root),
                move |ui| {
                    flash_settings_message(
                        ui,
                        &flash,
                        settings_ui::open_directory_error(tr_locale()).to_owned(),
                        false,
                    );
                },
            );
        });
    }

    // ---------- クリップの保存先 (task2970) ----------
    // One step, unlike the buffer root's pick-then-confirm: nothing already
    // written moves, so there is nothing to warn about. `None` is what 規定値に
    // 戻す writes, so the default stays the default even if it later changes.
    settings_handler!(ui, (settings, screen, hotkeys), on_clip_change, || {
        let picked = rfd::FileDialog::new()
            .set_parent(&ui.window().window_handle())
            .pick_folder();
        let Some(path) = picked else { return };
        let next = commit_settings(&settings, |current| {
            current.clip_directory = Some(path.to_string_lossy().into_owned());
        });
        render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
    });

    settings_handler!(
        ui,
        (settings, screen, hotkeys),
        on_clip_reset_clicked,
        || {
            let next = commit_settings(&settings, |current| current.clip_directory = None);
            render_settings(&ui, &settings, &next, &screen.borrow(), &hotkeys.borrow());
        }
    );

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        let flash = flash.clone();
        ui.global::<SettingsVm>().on_clip_open(move || {
            let current = settings_snapshot(&settings);
            let directory = livia::clips::clip_directory(&current);
            let flash = flash.clone();
            // t260923-916e: off the UI thread, like the buffer's 開く.
            desktop::open_off_thread(
                &weak,
                move || {
                    // Created on the way in: 開く on a folder no clip has been
                    // saved to yet would otherwise fail for a reason the
                    // message cannot explain.
                    directory.and_then(|directory| {
                        let _ = std::fs::create_dir_all(&directory);
                        open_directory(&directory).map_err(|_| String::new())
                    })
                },
                move |ui| {
                    flash_settings_message(
                        ui,
                        &flash,
                        settings_ui::open_directory_error(tr_locale()).to_owned(),
                        false,
                    );
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
