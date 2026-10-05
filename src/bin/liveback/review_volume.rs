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

use livia::capture::targets as capture_targets;
use livia::capture::CaptureController;
use livia::ring_buffer::container::AudioTrackInfo;
use livia::settings::{AppSettings, ExtraAudioSource, TargetAudio, SCREEN_AUDIO_KEY};
use livia::ui_state::volume as vol;
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use super::review::{change_target_audio, reviewed_audio_key, set_track_volume, Review};
use super::settings_page::{commit_settings, settings_snapshot};
use super::{tr_locale, AppWindow, MenuEntry, ReviewVm, VolumeCandidate, VolumeRowVm};

#[derive(Default)]
pub(super) struct VolumeTab {
    rows: Vec<vol::VolumeRow>,
    /// The manifest's tracks: re-read while recording, since a sound switched
    /// on mid-recording adds one.
    tracks: Vec<AudioTrackInfo>,
    /// (display name, icon) per executable, lowercased.
    meta: HashMap<String, (String, Option<Image>)>,
    candidates: Vec<ExtraAudioSource>,
    /// The device Menu's rows after 既定のマイク: (id, name).
    devices: Vec<(String, String)>,
    device_extra: usize,
    /// The last render was of a recording: one more after it stops clears
    /// the 「起動していません」 it may have said.
    live: bool,
    /// The last render had a microphone row, and whether Windows denied the
    /// microphone then: a finished session is not re-rendered every second, so
    /// `poll` compares this to notice the privacy switch flipping.
    has_mic: bool,
    mic_denied: bool,
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
    let key = reviewed_audio_key(review);
    let levels = livia::export::track_levels(settings, key.as_deref(), &review.volume.tracks);
    for (track, level) in initial_track_volumes(&levels).into_iter().enumerate() {
        set_track_volume(review, track, |slot| *slot = level);
    }
    ui.global::<ReviewVm>().set_volume_adding(false);
    render(ui, review, controller, settings);
}

/// `track_levels` as the mixer's `(percent, muted)` slots.
fn initial_track_volumes(levels: &[livia::export::TrackLevel]) -> Vec<(i64, bool)> {
    levels
        .iter()
        .map(|level| (i64::from(level.volume_percent), level.muted))
        .collect()
}

pub(super) fn unload(review: &mut Review, ui: &AppWindow) {
    let meta = std::mem::take(&mut review.volume.meta);
    review.volume = VolumeTab {
        meta,
        ..VolumeTab::default()
    };
    let vm = ui.global::<ReviewVm>();
    vm.set_volume_adding(false);
    vm.set_volume_rows(ModelRc::default());
}

/// From the 1 s capture poll: while the stage session records, and once
/// after it stops.
pub(super) fn poll(
    ui: &AppWindow,
    review: &mut Review,
    controller: &CaptureController,
    settings: &Mutex<AppSettings>,
) {
    if review.recording || review.volume.live {
        render(ui, review, controller, &settings_snapshot(settings));
    } else if review.volume.has_mic && microphone_access_denied() != review.volume.mic_denied {
        // The privacy switch moved while a finished session is on the tab: its
        // warning row follows without a reopen (3 registry reads a second, only
        // while a microphone row is shown).
        render(ui, review, controller, &settings_snapshot(settings));
    }
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
        if let Ok(manifest) = controller.session_manifest(&session_id) {
            review.volume.tracks = manifest.audio_tracks;
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
    let connected = if has_mic && review.recording {
        livia::capture::audio::microphones()
    } else {
        Vec::new()
    };
    let default_present = has_mic && review.recording && default_microphone_present();
    let screen = key.as_deref() == Some(SCREEN_AUDIO_KEY);
    vol::apply_live_state(
        &mut rows,
        target,
        &vol::LiveFacts {
            recording: review.recording,
            app_running: &|name| capture_targets::first_process_id_for_executable(name).is_some(),
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
                vol::volume_whole_screen(locale).to_owned(),
                String::new(),
                None,
            ),
            (vol::RowKind::Target, _) => {
                let exe = target_executable.clone().unwrap_or_default();
                let (name, icon) =
                    meta(&mut review.volume.meta, &exe, target_path.as_deref(), None);
                (name, String::new(), icon)
            }
            (_, Some(source)) => source_label(&mut review.volume.meta, source, locale),
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
                    let (name, icon) = meta(&mut review.volume.meta, &exe, path, shown);
                    (name, String::new(), icon)
                }
            }
        };
        let (percent, muted) = row
            .track
            .and_then(|track| review.track_volumes.get(track).copied())
            .unwrap_or((100, false));
        out.push(VolumeRowVm {
            kind: row.kind as i32,
            name: name.into(),
            device: device.into(),
            caption: vol::state_text(locale, row.kind, row.state).into(),
            warning: row.state == vol::RowState::Denied,
            extra: row.extra.map_or(-1, |index| index as i32),
            on: row.on,
            locked: row.state == vol::RowState::Merged,
            track: row.track.map_or(-1, |track| track as i32),
            volume_ratio: (percent.clamp(0, 100) as f32) / 100.0,
            muted,
            icon: icon.unwrap_or_default(),
            screen: screen && row.kind == vol::RowKind::Target,
        });
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
        Some(current) if current.row_count() == out.len() => {
            for (index, row) in out.into_iter().enumerate() {
                if current.row_data(index).as_ref() != Some(&row) {
                    current.set_row_data(index, row);
                }
            }
        }
        _ => vm.set_volume_rows(ModelRc::new(VecModel::from(out))),
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
        let target = current.target_audio.entry(key).or_default();
        match extra {
            None => {
                target.volume_percent = percent;
                target.muted = muted;
            }
            Some(index) => {
                if let Some(entry) = target.extras.get_mut(index) {
                    entry.volume_percent = percent;
                    entry.muted = muted;
                }
            }
        }
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

/// The device a microphone entry records (`None` = the Windows default).
pub(super) fn set_mic_device(
    target: &mut TargetAudio,
    extra: usize,
    device: Option<(String, String)>,
) {
    if let Some(entry) = target.extras.get_mut(extra) {
        if matches!(entry.source, ExtraAudioSource::Mic { .. }) {
            entry.source = ExtraAudioSource::Mic {
                device_name: device.as_ref().map(|(_, name)| name.clone()),
                device_id: device.map(|(id, _)| id),
            };
        }
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
        .and_then(|(width, height, rgba)| super::image_from_rgba(width, height, &rgba));
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

/// The tab's own handlers. The mixer's two (`track-volume-changed`,
/// `track-mute-toggled`) stay in `review_wiring`, which calls `save_level`.
pub(super) fn wire(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    settings: &Arc<Mutex<AppSettings>>,
    controller: &CaptureController,
) {
    let vm = ui.global::<ReviewVm>();

    // One edit of the reviewed target's registration, then the tab again.
    let edit = {
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        let controller = controller.clone();
        Rc::new(move |change: &dyn Fn(&mut Review, &mut TargetAudio)| {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review.borrow_mut();
            let Some(key) = reviewed_audio_key(&review) else {
                return;
            };
            let _ = change_target_audio(&settings, &controller, &key, |target| {
                change(&mut review, target)
            });
            render(&ui, &mut review, &controller, &settings_snapshot(&settings));
        })
    };

    {
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        vm.on_volume_add_opened(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review.borrow_mut();
            let current = settings_snapshot(&settings);
            let key = reviewed_audio_key(&review);
            let target = key.as_deref().and_then(|key| current.target_audio.get(key));
            let screen = key.as_deref() == Some(SCREEN_AUDIO_KEY);
            let blocked = vol::add_blocked(current.capture_all_audio, screen);
            let vm = ui.global::<ReviewVm>();
            vm.set_volume_blocked_text(
                blocked
                    .map(|blocked| vol::blocked_text(tr_locale(), blocked))
                    .unwrap_or_default()
                    .into(),
            );
            vm.set_volume_mic_added(has_mic(target));
            let (sources, shown) = if blocked.is_none() {
                let recorded = review
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.target_executable.clone())
                    .unwrap_or_default();
                add_list(&mut review.volume.meta, target, &recorded)
            } else {
                Default::default()
            };
            review.volume.candidates = sources;
            vm.set_volume_candidates(ModelRc::new(VecModel::from(shown)));
        });
    }

    {
        let edit = edit.clone();
        let weak = ui.as_weak();
        vm.on_volume_add_app(move |index| {
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            edit(&|review, target| {
                if let Some(source) = review.volume.candidates.get(index) {
                    target.add(source.clone());
                }
            });
            if let Some(ui) = weak.upgrade() {
                ui.global::<ReviewVm>().set_volume_adding(false);
            }
        });
    }

    {
        let edit = edit.clone();
        let weak = ui.as_weak();
        vm.on_volume_add_mic(move || {
            edit(&|_, target| {
                target.add(ExtraAudioSource::Mic {
                    device_id: None,
                    device_name: None,
                })
            });
            if let Some(ui) = weak.upgrade() {
                ui.global::<ReviewVm>().set_volume_adding(false);
            }
        });
    }

    {
        let edit = edit.clone();
        vm.on_volume_switch_toggled(move |index| {
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
        let weak = ui.as_weak();
        let review = review.clone();
        let settings = settings.clone();
        vm.on_volume_device_opened(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(index) = usize::try_from(index) else {
                return;
            };
            let mut review = review.borrow_mut();
            let current_id = reviewed_audio_key(&review)
                .and_then(|key| settings_snapshot(&settings).target_audio.get(&key).cloned())
                .and_then(|target| target.extras.get(index).cloned())
                .and_then(|extra| match extra.source {
                    ExtraAudioSource::Mic { device_id, .. } => device_id,
                    ExtraAudioSource::App { .. } => None,
                });
            let (devices, entries, checked) = device_menu(current_id.as_deref());
            review.volume.devices = devices;
            review.volume.device_extra = index;
            let vm = ui.global::<ReviewVm>();
            vm.set_volume_devices(ModelRc::new(VecModel::from(entries)));
            vm.set_volume_device_index(checked);
        });
    }

    {
        let edit = edit.clone();
        vm.on_volume_device_chosen(move |choice| {
            edit(&|review, target| {
                let device = usize::try_from(choice - 1)
                    .ok()
                    .and_then(|index| review.volume.devices.get(index).cloned());
                set_mic_device(target, review.volume.device_extra, device);
            });
        });
    }

    vm.on_volume_open_settings(|| {
        super::desktop::shell_open(std::path::Path::new("ms-settings:privacy-microphone"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The saved levels become the mixer's slots one for one; a target with
    /// nothing saved leaves the mixer at full volume (`track_levels` answers
    /// empty and `set_track_volume` grows on demand).
    #[test]
    fn saved_levels_seed_the_mixer() {
        use livia::export::TrackLevel;
        assert!(initial_track_volumes(&[]).is_empty());
        let levels = [
            TrackLevel {
                volume_percent: 70,
                muted: false,
            },
            TrackLevel {
                volume_percent: 100,
                muted: true,
            },
        ];
        assert_eq!(
            initial_track_volumes(&levels),
            vec![(70, false), (100, true)]
        );
    }
}
