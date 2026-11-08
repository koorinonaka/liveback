//! The review panel's 音量 tab (t261003-83d3, DS VolumePanel): the session's
//! tracks with their volume and mute, and the sounds added to its target.
//!
//! The rows are decided in `ui_state::volume`; this side reads the machine
//! (the manifest's tracks, running processes, connected microphones, the
//! microphone privacy switch) and wires the tab to the one path the `track`
//! control command also takes, `review::change_target_audio`. Volume and mute
//! are saved with `commit_settings` alone: they change nothing the recorder
//! does, so they skip the re-plan.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use livia::capture::targets::{self as capture_targets, RunningProcesses};
use livia::capture::CaptureController;
use livia::ring_buffer::container::AudioTrackInfo;
use livia::settings::{AppSettings, ExtraAudioSource, TargetAudio, SCREEN_AUDIO_KEY};
use livia::ui_state::volume as vol;
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use super::review::{change_target_audio, reviewed_audio_key, set_track_volume, Review};
use super::settings_page::{commit_settings, settings_snapshot};
use super::{tr_locale, AppWindow, MenuEntry, ReviewVm, VolumeCandidate, VolumeRowVm};

/// UI-thread state of an audio editor, the 音量 tab's and the settings rows'
/// (t261003-82d6) alike: what the add list and the device Menu last showed, so
/// the index the `.slint` side sends back maps through it.
#[derive(Default)]
pub(super) struct AudioList {
    /// (display name, icon) per executable, lowercased.
    pub(super) meta: HashMap<String, (String, Option<Image>)>,
    candidates: Vec<ExtraAudioSource>,
    /// The device Menu's rows after 既定のマイク: (id, name).
    devices: Vec<(String, String)>,
    device_extra: usize,
}

/// One change to the open registration, then that editor's lists again.
pub(super) type AudioEdit = Rc<dyn Fn(&dyn Fn(&AudioList, &mut TargetAudio))>;

#[derive(Default)]
pub(super) struct VolumeTab {
    rows: Vec<vol::VolumeRow>,
    /// The manifest's tracks: re-read while recording, since a sound switched
    /// on mid-recording adds one.
    tracks: Vec<AudioTrackInfo>,
    list: AudioList,
    /// The last render was of a recording: one more after it stops clears
    /// the 「起動していません」 it may have said.
    live: bool,
    /// The last render had a microphone row, and whether Windows denied the
    /// microphone then: a finished session is not re-rendered every second, so
    /// `poll` compares this to notice the privacy switch flipping.
    has_mic: bool,
    mic_denied: bool,
    /// The tab is on screen (`volume-tab-shown`): the 1 s poll redraws it only
    /// then (t261025-b28f). Kept across sessions like `meta`, since the pane
    /// and its tab outlive a session switch.
    shown: bool,
    /// The connected microphones and whether a default one exists, read once
    /// when the tab comes on screen or a session loads rather than every
    /// second. ponytail: a microphone plugged in while the tab stays open shows
    /// on the next open; `IMMNotificationClient` if that ever matters.
    mic_facts: Option<(Vec<(String, String)>, bool)>,
    /// The process names the target worker's last 1 s poll saw, lowercased
    /// (`Msg::Capturing`): the App rows' 「起動していません」 without a Toolhelp
    /// snapshot on this thread.
    processes: Option<Arc<RunningProcesses>>,
}

pub(super) fn apply_labels(vm: &ReviewVm<'_>) {
    let locale = tr_locale();
    vm.set_volume_overline(vol::volume_overline(locale).into());
    vm.set_volume_add_label(vol::volume_add(locale).into());
    vm.set_volume_running_label(vol::volume_running_apps(locale).into());
    vm.set_volume_tray_note(vol::volume_tray_note(locale).into());
    vm.set_volume_mic_label(vol::volume_microphone(locale).into());
    vm.set_volume_added_label(vol::volume_added(locale).into());
    vm.set_volume_open_settings_label(vol::volume_open_settings(locale).into());
}

/// A session was loaded: its saved levels become the mixer's starting point
/// (decision 11 -- the master stays where it is), then the tab draws.
pub(super) fn load(
    review: &mut Review,
    manifest: &livia::ring_buffer::SessionManifest,
    controller: &CaptureController,
    settings: &AppSettings,
    ui: &AppWindow,
) {
    review.volume.tracks = manifest.audio_tracks.clone();
    review.volume.mic_facts = None;
    let key = reviewed_audio_key(review);
    let levels = livia::export::track_levels(settings, key.as_deref(), &review.volume.tracks);
    for (track, level) in livia::ui_state::volume::initial_track_volumes(&levels)
        .into_iter()
        .enumerate()
    {
        set_track_volume(review, track, |slot| *slot = level);
    }
    ui.global::<ReviewVm>().set_volume_adding(false);
    render(ui, review, controller, settings);
}

pub(super) fn unload(review: &mut Review, ui: &AppWindow) {
    let meta = std::mem::take(&mut review.volume.list.meta);
    let processes = review.volume.processes.take();
    review.volume = VolumeTab {
        list: AudioList {
            meta,
            ..AudioList::default()
        },
        processes,
        shown: review.volume.shown,
        ..VolumeTab::default()
    };
    let vm = ui.global::<ReviewVm>();
    vm.set_volume_adding(false);
    vm.set_volume_rows(ModelRc::default());
}

/// From the 1 s capture poll, with the processes it saw: while the stage
/// session records, and once after it stops -- but only with the tab on
/// screen. Coming on screen draws it once (`wire`), so nothing is lost by
/// skipping it while hidden (t261025-b28f).
pub(super) fn poll(
    ui: &AppWindow,
    review: &mut Review,
    controller: &CaptureController,
    settings: &Mutex<AppSettings>,
    processes: Option<Arc<RunningProcesses>>,
) {
    review.volume.processes = processes;
    // The pane is mounted with `if`: leaving it says nothing to `shown`.
    let on_pane = ui.get_active_pane() == livia::ui_state::shell::Pane::Review.index();
    let tab = &review.volume;
    // The privacy switch moved while a finished session is on the tab: its
    // warning row follows without a reopen (3 registry reads a second, only
    // while a microphone row is shown).
    let mic_flipped = || tab.has_mic && microphone_access_denied() != tab.mic_denied;
    if poll_renders(
        tab.shown && on_pane,
        review.recording || tab.live,
        mic_flipped,
    ) {
        livia::insight_scope!("ui_volume_poll");
        render(ui, review, controller, &settings_snapshot(settings));
    }
}

/// `poll`'s decision: nothing at all off screen, not even the registry reads.
fn poll_renders(shown: bool, live: bool, mic_flipped: impl FnOnce() -> bool) -> bool {
    shown && (live || mic_flipped())
}

pub(super) fn render(
    ui: &AppWindow,
    review: &mut Review,
    controller: &CaptureController,
    settings: &AppSettings,
) {
    // Three fields, not the snapshot: this runs once a second while recording.
    let Some((session_id, target_executable, target_path)) =
        review.snapshot.as_ref().map(|snapshot| {
            (
                snapshot.session_id.clone(),
                snapshot.target_executable.clone(),
                snapshot.target_executable_path.clone(),
            )
        })
    else {
        return;
    };
    if review.recording {
        if let Some(tracks) = controller.audio_tracks_for(&session_id) {
            review.volume.tracks = tracks;
        }
    }
    review.volume.live = review.recording;
    let key = reviewed_audio_key(review);
    // A finished session lists the tracks it recorded, apart from the
    // registration (2026-10-03 ruling): no Switches, no 追加, no warnings.
    let registered = key
        .as_deref()
        .and_then(|key| settings.target_audio.get(key));
    let target = registered.filter(|_| review.recording);
    let mut rows = vol::volume_rows(target, &review.volume.tracks);
    let has_mic = has_mic(target);
    let denied = has_mic && microphone_access_denied();
    review.volume.has_mic = has_mic;
    review.volume.mic_denied = denied;
    let (connected, default_present) = if has_mic && review.recording {
        review
            .volume
            .mic_facts
            .get_or_insert_with(|| {
                livia::insight_scope!("ui_volume_poll_snapshot");
                (
                    livia::capture::audio::microphones(),
                    default_microphone_present(),
                )
            })
            .clone()
    } else {
        (Vec::new(), false)
    };
    let processes = review.volume.processes.clone();
    let screen = key.as_deref() == Some(SCREEN_AUDIO_KEY);
    vol::apply_live_state(
        &mut rows,
        target,
        &vol::LiveFacts {
            recording: review.recording,
            // The worker's list when it sent one; a lookup here only before
            // the first poll lands.
            app_running: &|name| match &processes {
                Some(processes) => processes.names.contains(&name.trim().to_ascii_lowercase()),
                None => capture_targets::first_process_id_for_executable(name).is_some(),
            },
            mic_connected: &|id| match id {
                None => default_present,
                Some(id) => connected.iter().any(|(known, _)| known == id),
            },
            mic_denied: denied,
        },
        vol::add_blocked(settings.capture_all_audio, screen),
    );
    let locale = tr_locale();
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let extra = row
            .extra
            .and_then(|index| target.and_then(|target| target.extras.get(index)));
        let recorded = row.track.and_then(|track| review.volume.tracks.get(track));
        let (name, device, icon) = match (row.kind, extra.map(|extra| &extra.source)) {
            (vol::RowKind::Target, _) if screen => (
                livia::ui_state::shell::control_whole_screen(locale).to_owned(),
                String::new(),
                None,
            ),
            (vol::RowKind::Target, _) => {
                let exe = target_executable.clone().unwrap_or_default();
                let (name, icon) = meta(
                    &mut review.volume.list.meta,
                    &exe,
                    target_path.as_deref(),
                    None,
                );
                (name, String::new(), icon)
            }
            (_, Some(source)) => source_label(&mut review.volume.list.meta, source, locale),
            (vol::RowKind::Mic, None) => (
                vol::volume_microphone(locale).to_owned(),
                recorded
                    .map(|t| t.executable_name.clone())
                    .unwrap_or_default(),
                None,
            ),
            (_, None) => {
                let exe = recorded
                    .map(|t| t.executable_name.trim().to_owned())
                    .unwrap_or_default();
                if exe.is_empty() {
                    let number = row.track.unwrap_or_default() + 1;
                    (
                        format!("{} {number}", vol::volume_unnamed_track(locale)),
                        String::new(),
                        None,
                    )
                } else {
                    // Named as registered when it still is: a finished
                    // session's app may not be running to be looked up.
                    let known = registered.and_then(|target| {
                        target.extras.iter().find_map(|extra| match &extra.source {
                            ExtraAudioSource::App {
                                path, display_name, ..
                            } if recorded.is_some_and(|info| extra.source.records(info)) => {
                                Some((path.as_deref(), display_name.as_deref()))
                            }
                            _ => None,
                        })
                    });
                    let (path, shown) = known.unwrap_or_default();
                    let (name, icon) = meta(&mut review.volume.list.meta, &exe, path, shown);
                    (name, String::new(), icon)
                }
            }
        };
        let level = row
            .track
            .and_then(|track| review.track_volumes.get(track).copied())
            .unwrap_or((100, false));
        out.push(volume_row_vm(
            locale,
            row,
            (name, device, icon),
            Some(level),
            screen,
        ));
    }
    review.volume.rows = rows;
    let vm = ui.global::<ReviewVm>();
    vm.set_volume_live(review.recording);
    if !review.recording {
        vm.set_volume_adding(false);
    }
    let model = vm.get_volume_rows();
    // In place when the row count holds, so a poll never rebuilds the row a
    // slider is being dragged on.
    match model.as_any().downcast_ref::<VecModel<VolumeRowVm>>() {
        Some(current) => super::sync_rows(current, out),
        None => vm.set_volume_rows(ModelRc::new(VecModel::from(out))),
    }
}

/// One row as the `.slint` side draws it, the 音量 tab's and the settings
/// rows' alike. `mixer` is the row's track level (percent, muted); `None`
/// draws no mixer at all (track -1), as the settings rows do although
/// `settings_rows` pairs the target with track 0.
pub(super) fn volume_row_vm(
    locale: livia::ui_state::locale::Locale,
    row: &vol::VolumeRow,
    (name, device, icon): (String, String, Option<Image>),
    mixer: Option<(i64, bool)>,
    screen: bool,
) -> VolumeRowVm {
    let track = mixer.and(row.track);
    let (percent, muted) = mixer.unwrap_or((100, false));
    VolumeRowVm {
        kind: row.kind as i32,
        name: name.into(),
        device: device.into(),
        caption: vol::state_text(locale, row.kind, row.state).into(),
        warning: row.state == vol::RowState::Denied,
        extra: row.extra.map_or(-1, |index| index as i32),
        on: row.on,
        locked: row.state == vol::RowState::Merged,
        track: track.map_or(-1, |track| track as i32),
        volume_ratio: (percent.clamp(0, 100) as f32) / 100.0,
        muted,
        icon: icon.unwrap_or_default(),
        screen: screen && row.kind == vol::RowKind::Target,
    }
}

/// The mixer state moved (a slider, a mute): the rows' levels follow in place.
pub(super) fn sync_levels(vm: &ReviewVm<'_>, review: &Review) {
    let model = vm.get_volume_rows();
    for index in 0..model.row_count() {
        let Some(mut row) = model.row_data(index) else {
            continue;
        };
        let Ok(track) = usize::try_from(row.track) else {
            continue;
        };
        let (percent, muted) = review
            .track_volumes
            .get(track)
            .copied()
            .unwrap_or((100, false));
        let ratio = (percent.clamp(0, 100) as f32) / 100.0;
        if row.volume_ratio != ratio || row.muted != muted {
            row.volume_ratio = ratio;
            row.muted = muted;
            model.set_row_data(index, row);
        }
    }
}

/// Saves one track's level to the registration entry that row stands for
/// (decision 11). A recorded track the registration no longer names stays
/// session-only: there is nothing to remember it by.
pub(super) fn save_level(review: &Review, settings: &Mutex<AppSettings>, track: usize) {
    let Some(key) = reviewed_audio_key(review) else {
        return;
    };
    let Some(row) = review
        .volume
        .rows
        .iter()
        .find(|row| row.track == Some(track))
    else {
        return;
    };
    if row.kind != vol::RowKind::Target && row.extra.is_none() {
        return;
    }
    let (percent, muted) = review
        .track_volumes
        .get(track)
        .copied()
        .unwrap_or((100, false));
    let percent = percent.clamp(0, 100) as u8;
    let extra = row.extra;
    commit_settings(settings, |current| {
        vol::set_level(
            current.target_audio.entry(key).or_default(),
            extra,
            percent,
            muted,
        );
    });
}

/// (name, device, icon) of an added sound's row, the 音量 tab's and the
/// settings rows' (t261003-82d6) alike.
pub(super) fn source_label(
    cache: &mut HashMap<String, (String, Option<Image>)>,
    source: &ExtraAudioSource,
    locale: livia::ui_state::locale::Locale,
) -> (String, String, Option<Image>) {
    match source {
        ExtraAudioSource::App {
            name,
            display_name,
            path,
        } => {
            let (shown, icon) = meta(cache, name, path.as_deref(), display_name.as_deref());
            (shown, String::new(), icon)
        }
        ExtraAudioSource::Mic {
            device_id,
            device_name,
        } => (
            vol::volume_microphone(locale).to_owned(),
            // 既定のマイク only when it is the default: a device chosen by id
            // whose name was not saved (a hand-written record) is looked up,
            // and shown by id if it is not connected.
            match (device_name, device_id) {
                (Some(name), _) => name.clone(),
                (None, None) => vol::volume_default_mic(locale).to_owned(),
                (None, Some(id)) => {
                    livia::capture::audio::microphone_name(Some(id)).unwrap_or_else(|| id.clone())
                }
            },
            None,
        ),
    }
}

/// The add list's applications (DS: running apps with a window): the sources
/// to add and the rows to show, one for one. `recorded` is the target's own
/// executable, left out with Liveback itself.
pub(super) fn add_list(
    cache: &mut HashMap<String, (String, Option<Image>)>,
    target: Option<&TargetAudio>,
    recorded: &str,
) -> (Vec<ExtraAudioSource>, Vec<VolumeCandidate>) {
    let windows = capture_targets::list_capture_targets().unwrap_or_default();
    let own = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let exclude = [own.as_str(), "liveback.exe", recorded];
    let mut sources = Vec::new();
    let mut shown = Vec::new();
    for candidate in vol::add_candidates(&windows, &exclude, target) {
        let name = candidate.executable_name.clone().unwrap_or_default();
        let path = capture_targets::executable_path_for_process(candidate.process_id);
        let (display, icon) = meta(cache, &name, path.as_deref(), None);
        shown.push(VolumeCandidate {
            name: display.clone().into(),
            icon: icon.unwrap_or_default(),
        });
        sources.push(ExtraAudioSource::App {
            display_name: (display != name).then_some(display),
            path,
            name,
        });
    }
    (sources, shown)
}

pub(super) fn has_mic(target: Option<&TargetAudio>) -> bool {
    target.is_some_and(|target| {
        target
            .extras
            .iter()
            .any(|extra| matches!(extra.source, ExtraAudioSource::Mic { .. }))
    })
}

/// The device Menu: the connected microphones (id, name), its rows
/// (既定のマイク first) and the checked row for `current` (`None` = default).
pub(super) fn device_menu(current: Option<&str>) -> (Vec<(String, String)>, Vec<MenuEntry>, i32) {
    let devices = livia::capture::audio::microphones();
    let mut entries = vec![MenuEntry {
        label: vol::volume_default_mic(tr_locale()).into(),
        ..Default::default()
    }];
    entries.extend(devices.iter().map(|(_, name)| MenuEntry {
        label: name.into(),
        ..Default::default()
    }));
    let checked = match current {
        None => 0,
        Some(id) => devices
            .iter()
            .position(|(known, _)| known == id)
            .map_or(-1, |found| found as i32 + 1),
    };
    (devices, entries, checked)
}

/// 追加 opened: the add list for the registration under `key`, minus
/// `recorded` (the target's own executable), remembered in `list` so the
/// index the `.slint` side sends back maps through it. Answers the blocked
/// text, whether a microphone is added, and the rows to show.
pub(super) fn add_opened(
    list: &mut AudioList,
    current: &AppSettings,
    key: Option<&str>,
    recorded: &str,
) -> (slint::SharedString, bool, ModelRc<VolumeCandidate>) {
    let target = key.and_then(|key| current.target_audio.get(key));
    let blocked = vol::add_blocked(current.capture_all_audio, key == Some(SCREEN_AUDIO_KEY));
    let (sources, shown) = if blocked.is_none() {
        add_list(&mut list.meta, target, recorded)
    } else {
        Default::default()
    };
    list.candidates = sources;
    (
        blocked
            .map(|blocked| vol::blocked_text(tr_locale(), blocked))
            .unwrap_or_default()
            .into(),
        has_mic(target),
        ModelRc::new(VecModel::from(shown)),
    )
}

/// The device Menu opened on the `index`-th added sound: its rows and the
/// checked one, the devices remembered in `list` for `device_chosen`.
pub(super) fn device_opened(
    list: &mut AudioList,
    target: Option<&TargetAudio>,
    index: usize,
) -> (ModelRc<MenuEntry>, i32) {
    let current_id = vol::mic_device_id(target, index);
    let (devices, entries, checked) = device_menu(current_id.as_deref());
    list.devices = devices;
    list.device_extra = index;
    (ModelRc::new(VecModel::from(entries)), checked)
}

/// The handlers the 音量 tab and the settings rows share, for their own
/// `on_*`: `edit` is that editor's one change, `close_add` closes its add list.
pub(super) fn add_app_handler(edit: AudioEdit, close_add: impl Fn() + 'static) -> impl Fn(i32) {
    move |index| {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        edit(&|list, target| vol::add_candidate(target, &list.candidates, index));
        close_add();
    }
}

pub(super) fn add_mic_handler(edit: AudioEdit, close_add: impl Fn() + 'static) -> impl Fn() {
    move || {
        edit(&|_, target| vol::add_mic(target));
        close_add();
    }
}

pub(super) fn switch_handler(edit: AudioEdit) -> impl Fn(i32) {
    move |index| {
        edit(&|_, target| {
            if let Ok(index) = usize::try_from(index) {
                vol::toggle(target, index);
            }
        })
    }
}

pub(super) fn device_chosen_handler(edit: AudioEdit) -> impl Fn(i32) {
    move |choice| {
        edit(&|list, target| vol::choose_device(target, list.device_extra, &list.devices, choice))
    }
}

/// (display name, icon) for an executable, cached: `FileDescription` and the
/// file's icon, as the 自動録画 list shows them.
pub(super) fn meta(
    cache: &mut HashMap<String, (String, Option<Image>)>,
    executable: &str,
    path: Option<&str>,
    display_name: Option<&str>,
) -> (String, Option<Image>) {
    let key = executable.to_ascii_lowercase();
    if let Some(found) = cache.get(&key) {
        return found.clone();
    }
    let path = path
        .filter(|path| std::path::Path::new(path).is_file())
        .map(str::to_owned)
        .or_else(|| {
            capture_targets::first_process_id_for_executable(executable)
                .and_then(capture_targets::executable_path_for_process)
        });
    let name = display_name
        .map(str::to_owned)
        .or_else(|| {
            path.as_deref()
                .and_then(super::app_meta::describe_executable)
        })
        .unwrap_or_else(|| executable.to_owned());
    let icon = path
        .as_deref()
        .and_then(super::app_meta::file_icon_rgba)
        .and_then(super::pixels_image);
    let entry = (name, icon);
    // Not cached without a path: the app may start later and have one.
    if path.is_some() {
        cache.insert(key, entry.clone());
    }
    entry
}

fn default_microphone_present() -> bool {
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, IMMDeviceEnumerator, MMDeviceEnumerator,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
    unsafe {
        CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .and_then(|enumerator| enumerator.GetDefaultAudioEndpoint(eCapture, eConsole))
            .is_ok()
    }
}

/// Windows' microphone privacy switch (Settings → Privacy → Microphone): the
/// machine-wide one, the user's, and the user's for desktop apps. Any of them
/// set to `Deny` keeps this process from the microphone.
pub(super) fn microphone_access_denied() -> bool {
    use windows::core::w;
    use windows::Win32::System::Registry::{
        RegGetValueW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ,
    };
    let denied = |hive: HKEY, subkey: windows::core::PCWSTR| {
        let mut buffer = [0u16; 16];
        let mut size = std::mem::size_of_val(&buffer) as u32;
        let status = unsafe {
            RegGetValueW(
                hive,
                subkey,
                w!("Value"),
                RRF_RT_REG_SZ,
                None,
                Some(buffer.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        let length = buffer.iter().position(|unit| *unit == 0).unwrap_or(0);
        status.is_ok() && String::from_utf16_lossy(&buffer[..length]) == "Deny"
    };
    let store = w!(
        r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone"
    );
    denied(HKEY_LOCAL_MACHINE, store)
        || denied(HKEY_CURRENT_USER, store)
        || denied(
            HKEY_CURRENT_USER,
            w!(
                r"SOFTWARE\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged"
            ),
        )
}

/// The tab's own handlers. The mixer's (`track-volume-changed`,
/// `track-volume-committed`, `track-mute-toggled`) stay in `review_wiring`;
/// the committed one and the mute toggle call `save_level`.
pub(super) fn wire(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    settings: &Arc<Mutex<AppSettings>>,
    controller: &CaptureController,
) {
    let vm = ui.global::<ReviewVm>();

    // One edit of the reviewed target's registration, then the tab again.
    let edit: AudioEdit = {
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        let controller = controller.clone();
        Rc::new(move |change: &dyn Fn(&AudioList, &mut TargetAudio)| {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review.borrow_mut();
            let Some(key) = reviewed_audio_key(&review) else {
                return;
            };
            let _ = change_target_audio(&settings, &controller, &key, |target| {
                change(&review.volume.list, target)
            });
            render(&ui, &mut review, &controller, &settings_snapshot(&settings));
        })
    };
    let close_add = || {
        let weak = ui.as_weak();
        move || {
            if let Some(ui) = weak.upgrade() {
                ui.global::<ReviewVm>().set_volume_adding(false);
            }
        }
    };

    {
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        vm.on_volume_add_opened(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review.borrow_mut();
            let key = reviewed_audio_key(&review);
            let recorded = review
                .snapshot
                .as_ref()
                .and_then(|snapshot| snapshot.target_executable.clone())
                .unwrap_or_default();
            let (blocked, mic_added, candidates) = add_opened(
                &mut review.volume.list,
                &settings_snapshot(&settings),
                key.as_deref(),
                &recorded,
            );
            let vm = ui.global::<ReviewVm>();
            vm.set_volume_blocked_text(blocked);
            vm.set_volume_mic_added(mic_added);
            vm.set_volume_candidates(candidates);
        });
    }

    vm.on_volume_add_app(add_app_handler(edit.clone(), close_add()));
    vm.on_volume_add_mic(add_mic_handler(edit.clone(), close_add()));
    vm.on_volume_switch_toggled(switch_handler(edit.clone()));

    {
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        vm.on_volume_device_opened(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let mut review = review.borrow_mut();
            let current = settings_snapshot(&settings);
            let target = reviewed_audio_key(&review).and_then(|key| current.target_audio.get(&key));
            let (entries, checked) = device_opened(&mut review.volume.list, target, index);
            let vm = ui.global::<ReviewVm>();
            vm.set_volume_devices(entries);
            vm.set_volume_device_index(checked);
        });
    }

    vm.on_volume_device_chosen(device_chosen_handler(edit));

    vm.on_volume_open_settings(|| {
        super::desktop::shell_open(std::path::Path::new("ms-settings:privacy-microphone"));
    });

    // The tab came on screen: draw it now, with the microphones read afresh,
    // since the 1 s poll skipped it while hidden (t261025-b28f).
    {
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        let controller = controller.clone();
        vm.on_volume_tab_shown(move |shown| {
            tab_shown(&weak, &review, &settings, &controller, shown);
        });
    }
}

/// `volume-tab-shown`. The pane's `init` sends it while it is being mounted,
/// which can be from inside a handler that holds `review`: then it waits for
/// the event loop rather than panic on the borrow.
fn tab_shown(
    weak: &slint::Weak<AppWindow>,
    review: &Rc<RefCell<Review>>,
    settings: &Arc<Mutex<AppSettings>>,
    controller: &CaptureController,
    shown: bool,
) {
    let Some(ui) = weak.upgrade() else {
        return;
    };
    let Ok(mut current) = review.try_borrow_mut() else {
        let (weak, review, settings, controller) = (
            weak.clone(),
            review.clone(),
            settings.clone(),
            controller.clone(),
        );
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            tab_shown(&weak, &review, &settings, &controller, shown);
        });
        return;
    };
    current.volume.shown = shown;
    if shown {
        current.volume.mic_facts = None;
        render(&ui, &mut current, controller, &settings_snapshot(settings));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Off screen the poll draws nothing and does not even read the privacy
    /// switch; on screen it draws while live, and after that only when the
    /// switch moved (t261025-b28f).
    #[test]
    fn the_poll_draws_only_a_tab_on_screen() {
        let flipped = std::cell::Cell::new(0);
        let probe = |answer: bool| {
            let flipped = &flipped;
            move || {
                flipped.set(flipped.get() + 1);
                answer
            }
        };
        assert!(!poll_renders(false, true, probe(true)));
        assert!(!poll_renders(false, false, probe(true)));
        assert_eq!(flipped.get(), 0, "hidden: no registry reads");
        assert!(poll_renders(true, true, probe(false)));
        assert!(poll_renders(true, false, probe(true)));
        assert!(!poll_renders(true, false, probe(false)));
    }
}
