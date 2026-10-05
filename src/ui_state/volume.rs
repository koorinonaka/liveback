//! The review panel's 音量 tab (t261003-83d3, DS VolumePanel): which rows it
//! shows, what each says, and what may be added. Pure -- the bin probes the
//! machine (running processes, connected microphones, the privacy switch) and
//! hands the answers in.

use crate::capture::targets::{CaptureTarget, CaptureTargetKind};
use crate::ring_buffer::container::AudioTrackInfo;
use crate::settings::{ExtraAudioSource, TargetAudio};
use crate::ui_state::locale::Locale;

crate::tr! {
    volume_tab { ja: "音量", en: "Volume" }
    volume_overline { ja: "音声トラック", en: "Audio tracks" }
    volume_add { ja: "トラックを追加", en: "Add track" }
    volume_target_caption { ja: "録画の対象", en: "Recording target" }
    volume_app_absent { ja: "起動していません", en: "Not running" }
    volume_mic_absent { ja: "つながっていません", en: "Not connected" }
    volume_not_in_session { ja: "このセッションには入っていません", en: "Not in this session" }
    /// t261003-c2cb: an added app while apps cannot be added (DS `blocked`).
    volume_merged { ja: "対象のトラックに入っています", en: "In the target's track" }
    volume_mic_denied {
        ja: "Windows の設定で、マイクの使用が許可されていません。",
        en: "Windows settings do not allow microphone access."
    }
    volume_open_settings { ja: "設定を開く", en: "Open settings" }
    volume_running_apps { ja: "起動中のアプリ", en: "Running apps" }
    volume_tray_note {
        ja: "トレイにだけいるアプリは、開くとここに出ます。",
        en: "Apps that sit only in the tray show up here once you open them."
    }
    volume_microphone { ja: "マイク", en: "Microphone" }
    volume_added { ja: "追加済み", en: "Added" }
    volume_default_mic { ja: "既定のマイク", en: "Default microphone" }
    volume_blocked_all_apps {
        ja: "「他のアプリの音も録音」がオンのため、ほかのアプリの音は対象のトラックに入っています。",
        en: "\u{201C}Record other applications too\u{201D} is on, so other apps are already in the target's track."
    }
    volume_blocked_display {
        ja: "画面全体の録画は、すべてのアプリの音を対象のトラックに録っています。",
        en: "A full-screen recording already records every app in the target's track."
    }
    volume_whole_screen { ja: "画面全体", en: "Entire screen" }
    /// A recorded track with no name: says so rather than an empty row.
    volume_unnamed_track { ja: "トラック", en: "Track" }
    // t261003-82d6: the settings page's 音声 group and its rows (DS V28
    // SettingsScreen / AutoCaptureRow).
    audio_group { ja: "音声", en: "Audio" }
    audio_group_sub {
        ja: "画面全体と、自動録画の一覧にないアプリについて、対象の音のほかに録る音を決めます。アプリは、録画中の確認画面で音を足すとここに出ます。",
        en: "Choose what else to record besides the target's own sound, for the entire screen and for apps not in the auto-record list. An app appears here once you add a sound to it on the review screen while it records."
    }
    audio_open { ja: "音声トラックを開く", en: "Show audio tracks" }
    audio_close { ja: "音声トラックを閉じる", en: "Hide audio tracks" }
    audio_remove { ja: "登録から外す", en: "Remove" }
    /// t261003-4d0d: the × on an app's row in the 音声 group.
    audio_target_remove { ja: "音声の登録を消す", en: "Remove audio settings" }
    audio_mic_denied { ja: "マイクが許可されていません", en: "Microphone access is not allowed" }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum RowKind {
    Target,
    App,
    Mic,
}

/// The row's one line of state (DS: only when there is something to say).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Quiet,
    Target,
    Absent,
    NotInSession,
    Denied,
    /// An added app while apps cannot be added (`AddBlocked`): its sound is in
    /// the target's track, so its Switch is held where it is (DS `blocked`).
    Merged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeRow {
    pub kind: RowKind,
    /// Index into the registration's `extras`: the row has a Switch and 外す.
    /// `None` for the target and for a recorded track the registration no
    /// longer names.
    pub extra: Option<usize>,
    /// The session's track: the row has a mute and a volume. `None` is a
    /// sound added after this session was recorded.
    pub track: Option<usize>,
    pub on: bool,
    pub state: RowState,
}

/// The rows, DS order: the target, the applications, the microphone. An
/// application in the order it was added; a recorded track the registration
/// no longer names sits with its kind, after the registered ones.
///
/// `tracks` is the manifest's `audio_tracks`, empty for a one-track session.
pub fn volume_rows(target: Option<&TargetAudio>, tracks: &[AudioTrackInfo]) -> Vec<VolumeRow> {
    let extras = target.map_or(&[][..], |target| target.extras.as_slice());
    let claimed: Vec<Option<usize>> = tracks
        .iter()
        .enumerate()
        .map(|(track, info)| {
            (track > 0)
                .then(|| extras.iter().position(|extra| extra.source.records(info)))
                .flatten()
        })
        .collect();
    let mut rows = vec![VolumeRow {
        kind: RowKind::Target,
        extra: None,
        track: Some(0),
        on: true,
        state: RowState::Target,
    }];
    for (index, extra) in extras.iter().enumerate() {
        let track = claimed.iter().position(|claim| *claim == Some(index));
        rows.push(VolumeRow {
            kind: kind_of(&extra.source),
            extra: Some(index),
            track,
            on: extra.enabled,
            state: if track.is_some() {
                RowState::Quiet
            } else {
                RowState::NotInSession
            },
        });
    }
    for (track, info) in tracks.iter().enumerate().skip(1) {
        if claimed[track].is_none() {
            rows.push(VolumeRow {
                kind: if info.microphone.is_some() {
                    RowKind::Mic
                } else {
                    RowKind::App
                },
                extra: None,
                track: Some(track),
                on: true,
                state: RowState::Quiet,
            });
        }
    }
    // Stable: within a kind, registered rows stay ahead of orphans.
    rows.sort_by_key(|row| row.kind);
    rows
}

fn kind_of(source: &ExtraAudioSource) -> RowKind {
    match source {
        ExtraAudioSource::App { .. } => RowKind::App,
        ExtraAudioSource::Mic { .. } => RowKind::Mic,
    }
}

/// The machine's answers the state line needs. Absence is only said while
/// recording (DS `absent`), and only of a sound that is switched on.
pub struct LiveFacts<'a> {
    pub recording: bool,
    pub app_running: &'a dyn Fn(&str) -> bool,
    /// `None` is the Windows default.
    pub mic_connected: &'a dyn Fn(Option<&str>) -> bool,
    pub mic_denied: bool,
}

pub fn apply_live_state(
    rows: &mut [VolumeRow],
    target: Option<&TargetAudio>,
    facts: &LiveFacts,
    blocked: Option<AddBlocked>,
) {
    mark_merged(rows, blocked);
    let Some(target) = target else { return };
    for row in rows.iter_mut() {
        let Some(extra) = row.extra.and_then(|index| target.extras.get(index)) else {
            continue;
        };
        match &extra.source {
            ExtraAudioSource::Mic { .. } if facts.mic_denied => row.state = RowState::Denied,
            _ if matches!(row.state, RowState::NotInSession | RowState::Merged)
                || !facts.recording
                || !extra.enabled => {}
            ExtraAudioSource::App { name, .. } if !(facts.app_running)(name) => {
                row.state = RowState::Absent;
            }
            ExtraAudioSource::Mic { device_id, .. }
                if !(facts.mic_connected)(device_id.as_deref()) =>
            {
                row.state = RowState::Absent;
            }
            _ => {}
        }
    }
}

/// The state line's words.
pub fn state_text(locale: Locale, kind: RowKind, state: RowState) -> &'static str {
    match (state, kind) {
        (RowState::Quiet, _) => "",
        (RowState::Target, _) => volume_target_caption(locale),
        (RowState::Absent, RowKind::Mic) => volume_mic_absent(locale),
        (RowState::Absent, _) => volume_app_absent(locale),
        (RowState::NotInSession, _) => volume_not_in_session(locale),
        (RowState::Denied, _) => volume_mic_denied(locale),
        (RowState::Merged, _) => volume_merged(locale),
    }
}

/// While apps cannot be added, an added app's sound goes to the target's
/// track (`capture::audio::planned_extras`), whether its Switch is on or not.
/// A row the session recorded on a track of its own keeps saying so: the
/// recording took `capture_all_audio` when it started, so a mid-recording
/// change reaches only the next one. The microphone can still be added.
fn mark_merged(rows: &mut [VolumeRow], blocked: Option<AddBlocked>) {
    if blocked.is_none() {
        return;
    }
    for row in rows {
        if row.kind == RowKind::App && row.extra.is_some() && row.track.is_none() {
            row.state = RowState::Merged;
        }
    }
}

/// Why applications cannot be added (DS: one sentence in the add list; the
/// microphone still can be).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddBlocked {
    AllApps,
    Display,
}

pub fn add_blocked(capture_all_audio: bool, screen: bool) -> Option<AddBlocked> {
    if screen {
        Some(AddBlocked::Display)
    } else if capture_all_audio {
        Some(AddBlocked::AllApps)
    } else {
        None
    }
}

pub fn blocked_text(locale: Locale, blocked: AddBlocked) -> &'static str {
    match blocked {
        AddBlocked::AllApps => volume_blocked_all_apps(locale),
        AddBlocked::Display => volume_blocked_display(locale),
    }
}

/// The rows a settings row opens (t261003-82d6, DS AutoCaptureRow): the
/// registration as the 音量 tab draws it while nothing records, with the
/// settings' own states -- an app never says 「起動していません」 (nothing is
/// being recorded), a microphone says 「つながっていません」 only when a chosen
/// device is missing (the default follows Windows), and a denied microphone
/// warns. `mic_connected` is asked only for a chosen device.
pub fn settings_rows(
    target: Option<&TargetAudio>,
    mic_connected: &dyn Fn(&str) -> bool,
    mic_denied: bool,
    blocked: Option<AddBlocked>,
) -> Vec<VolumeRow> {
    let mut rows = volume_rows(target, &[]);
    for row in &mut rows {
        let Some(extra) = row
            .extra
            .and_then(|index| target.and_then(|target| target.extras.get(index)))
        else {
            continue;
        };
        row.state = match &extra.source {
            ExtraAudioSource::Mic { .. } if mic_denied => RowState::Denied,
            ExtraAudioSource::Mic {
                device_id: Some(id),
                ..
            } if !mic_connected(id) => RowState::Absent,
            _ => RowState::Quiet,
        };
    }
    mark_merged(&mut rows, blocked);
    rows
}

/// A closed settings row's one word about its sounds (DS AutoCaptureRow): a
/// microphone is added and Windows denies it.
pub fn mic_warning(target: Option<&TargetAudio>, mic_denied: bool) -> bool {
    mic_denied
        && target.is_some_and(|target| {
            target
                .extras
                .iter()
                .any(|extra| matches!(extra.source, ExtraAudioSource::Mic { .. }))
        })
}

/// The settings page's 音声 group (DS SettingsScreen): 画面全体 always first,
/// then every registration with an added sound whose executable is not in the
/// auto-record list (case-insensitive: the list keeps the registered
/// spelling, the key is lowercased), in key order -- the executable names'.
pub fn audio_group_keys(
    target_audio: &std::collections::BTreeMap<String, TargetAudio>,
    auto_capture: &[crate::settings::AutoCaptureApp],
) -> Vec<String> {
    let mut keys = vec![crate::settings::SCREEN_AUDIO_KEY.to_owned()];
    keys.extend(
        target_audio
            .iter()
            .filter(|(key, target)| {
                key.as_str() != crate::settings::SCREEN_AUDIO_KEY
                    && !target.extras.is_empty()
                    && !auto_capture
                        .iter()
                        .any(|app| app.name.trim().eq_ignore_ascii_case(key))
            })
            .map(|(key, _)| key.clone()),
    );
    keys
}

/// Windows that only frame another app's content -- a UWP app's frame, a
/// terminal or console host. Nobody adds their sound, and their display names
/// ("Application Frame Host", "Windows Terminal Host") read as noise in the add
/// list (user ruling 2026-10-03, follow-up of t261003-83d3).
const HOST_EXECUTABLES: [&str; 4] = [
    "ApplicationFrameHost.exe",
    "WindowsTerminal.exe",
    "OpenConsole.exe",
    "conhost.exe",
];

/// The applications the add list offers: windows (not displays) of running
/// processes, one per executable name, minus `exclude` (Liveback itself and
/// the recording's target), the hosts above, and what the registration
/// already has.
pub fn add_candidates<'a>(
    targets: &'a [CaptureTarget],
    exclude: &[&str],
    target: Option<&TargetAudio>,
) -> Vec<&'a CaptureTarget> {
    let registered = |name: &str| {
        target.is_some_and(|target| {
            target.extras.iter().any(|extra| {
                matches!(&extra.source, ExtraAudioSource::App { name: added, .. }
                    if added.eq_ignore_ascii_case(name))
            })
        })
    };
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for candidate in targets {
        let Some(name) = candidate.executable_name.as_deref().map(str::trim) else {
            continue;
        };
        if candidate.kind != CaptureTargetKind::Window
            || name.is_empty()
            || exclude.iter().any(|skip| skip.eq_ignore_ascii_case(name))
            || HOST_EXECUTABLES
                .iter()
                .any(|host| host.eq_ignore_ascii_case(name))
            || seen.iter().any(|known| known.eq_ignore_ascii_case(name))
            || registered(name)
        {
            continue;
        }
        seen.push(name);
        out.push(candidate);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::ExtraAudio;

    fn app(name: &str, enabled: bool) -> ExtraAudio {
        ExtraAudio {
            source: ExtraAudioSource::App {
                name: name.into(),
                display_name: None,
                path: None,
            },
            enabled,
            muted: false,
            volume_percent: 100,
        }
    }

    fn mic(id: Option<&str>) -> ExtraAudio {
        ExtraAudio {
            source: ExtraAudioSource::Mic {
                device_id: id.map(Into::into),
                device_name: None,
            },
            enabled: true,
            muted: false,
            volume_percent: 100,
        }
    }

    fn track(name: &str, microphone: Option<&str>) -> AudioTrackInfo {
        AudioTrackInfo {
            executable_name: name.into(),
            microphone: microphone.map(Into::into),
        }
    }

    fn summary(rows: &[VolumeRow]) -> Vec<(RowKind, Option<usize>, Option<usize>, RowState)> {
        rows.iter()
            .map(|row| (row.kind, row.extra, row.track, row.state))
            .collect()
    }

    /// The mic registered before an app still draws last; a sound added
    /// after the recording has no track; a recorded track the registration
    /// lost keeps its row (volume, no switch).
    #[test]
    fn rows_follow_the_ds_order_and_pair_tracks_with_the_registration() {
        let target = TargetAudio {
            extras: vec![mic(None), app("Discord.exe", true), app("Late.exe", false)],
            ..TargetAudio::default()
        };
        let tracks = [
            track("Game.exe", None),
            track("Mic", Some("")),
            track("discord.exe", None),
            track("Gone.exe", None),
        ];
        let rows = volume_rows(Some(&target), &tracks);
        use RowKind::{App, Mic};
        use RowState::{NotInSession, Quiet};
        assert_eq!(
            summary(&rows),
            vec![
                (RowKind::Target, None, Some(0), RowState::Target),
                (App, Some(1), Some(2), Quiet),
                (App, Some(2), None, NotInSession),
                (App, None, Some(3), Quiet),
                (Mic, Some(0), Some(1), Quiet),
            ]
        );
        assert!(!rows[2].on);
    }

    /// One-track sessions (no `AudioTracksSet`) and unregistered targets
    /// still get the target's row; a registered sound is not in them.
    #[test]
    fn a_one_track_session_has_the_target_and_nothing_recorded_besides() {
        assert_eq!(
            summary(&volume_rows(None, &[])),
            vec![(RowKind::Target, None, Some(0), RowState::Target)]
        );
        let target = TargetAudio {
            extras: vec![mic(Some("{usb}"))],
            ..TargetAudio::default()
        };
        assert_eq!(
            summary(&volume_rows(Some(&target), &[]))[1],
            (RowKind::Mic, Some(0), None, RowState::NotInSession)
        );
    }

    #[test]
    fn absence_is_said_only_while_recording_a_sound_that_is_on() {
        let target = TargetAudio {
            extras: vec![app("a.exe", true), app("b.exe", false), mic(Some("{usb}"))],
            ..TargetAudio::default()
        };
        let tracks = [
            track("t.exe", None),
            track("a.exe", None),
            track("b.exe", None),
            track("USB", Some("{usb}")),
        ];
        let nobody = |_: &str| false;
        let unplugged = |_: Option<&str>| false;
        let states = |recording, denied| {
            let mut rows = volume_rows(Some(&target), &tracks);
            apply_live_state(
                &mut rows,
                Some(&target),
                &LiveFacts {
                    recording,
                    app_running: &nobody,
                    mic_connected: &unplugged,
                    mic_denied: denied,
                },
                None,
            );
            rows.iter().map(|row| row.state).collect::<Vec<_>>()
        };
        use RowState::{Absent, Denied, Quiet, Target};
        assert_eq!(states(true, false), vec![Target, Absent, Quiet, Absent]);
        assert_eq!(states(false, false), vec![Target, Quiet, Quiet, Quiet]);
        // Denied outranks absence, recording or not.
        assert_eq!(states(false, true), vec![Target, Quiet, Quiet, Denied]);
        let running = |_: &str| true;
        let plugged = |_: Option<&str>| true;
        let mut rows = volume_rows(Some(&target), &tracks);
        apply_live_state(
            &mut rows,
            Some(&target),
            &LiveFacts {
                recording: true,
                app_running: &running,
                mic_connected: &plugged,
                mic_denied: false,
            },
            None,
        );
        assert_eq!(
            rows.iter().map(|row| row.state).collect::<Vec<_>>(),
            vec![Target, Quiet, Quiet, Quiet]
        );
    }

    /// t261003-c2cb (DS `blocked`): with all apps or the whole screen in the
    /// target's track, an added app says so whatever its Switch -- live and in
    /// settings -- while the microphone keeps its own state. Not blocked, the
    /// rows are what they were; an app this session recorded on a track of
    /// its own keeps it (the recording took the setting when it started).
    #[test]
    fn a_blocked_add_puts_the_added_apps_in_the_targets_track() {
        let target = TargetAudio {
            extras: vec![app("a.exe", true), app("b.exe", false), mic(None)],
            ..TargetAudio::default()
        };
        let one_track = [track("t.exe", None)];
        let live = |blocked, tracks: &[AudioTrackInfo]| {
            let mut rows = volume_rows(Some(&target), tracks);
            apply_live_state(
                &mut rows,
                Some(&target),
                &LiveFacts {
                    recording: true,
                    app_running: &|_| false,
                    mic_connected: &|_| false,
                    mic_denied: false,
                },
                blocked,
            );
            rows.iter().map(|row| row.state).collect::<Vec<_>>()
        };
        let settings = |blocked| {
            settings_rows(Some(&target), &|_| true, false, blocked)
                .iter()
                .map(|row| row.state)
                .collect::<Vec<_>>()
        };
        use RowState::{Merged, NotInSession, Quiet, Target};
        for blocked in [AddBlocked::AllApps, AddBlocked::Display] {
            assert_eq!(
                live(Some(blocked), &one_track),
                vec![Target, Merged, Merged, NotInSession]
            );
            assert_eq!(settings(Some(blocked)), vec![Target, Merged, Merged, Quiet]);
        }
        assert_eq!(
            live(None, &one_track),
            vec![Target, NotInSession, NotInSession, NotInSession]
        );
        assert_eq!(settings(None), vec![Target, Quiet, Quiet, Quiet]);
        let own_track = [track("t.exe", None), track("a.exe", None)];
        assert_eq!(
            live(Some(AddBlocked::AllApps), &own_track),
            vec![Target, RowState::Absent, Merged, NotInSession]
        );
        assert_eq!(
            state_text(Locale::Ja, RowKind::App, Merged),
            "対象のトラックに入っています"
        );
        assert_eq!(
            state_text(Locale::En, RowKind::App, Merged),
            "In the target's track"
        );
    }

    #[test]
    fn apps_cannot_be_added_beside_all_apps_or_a_screen_recording() {
        assert_eq!(add_blocked(false, false), None);
        assert_eq!(add_blocked(true, false), Some(AddBlocked::AllApps));
        assert_eq!(add_blocked(false, true), Some(AddBlocked::Display));
        assert_eq!(add_blocked(true, true), Some(AddBlocked::Display));
    }

    /// A settings row: the target says 録画の対象, an app never says it is not
    /// running, the microphone is absent only for a chosen device that is
    /// missing (the default follows Windows), and a denied microphone warns
    /// whatever its device; the closed row's warning needs a microphone.
    #[test]
    fn settings_rows_say_only_what_holds_while_nothing_records() {
        let target = TargetAudio {
            extras: vec![mic(Some("{usb}")), app("a.exe", false), mic(None)],
            ..TargetAudio::default()
        };
        let states = |connected: bool, denied: bool| {
            settings_rows(Some(&target), &|_| connected, denied, None)
                .iter()
                .map(|row| (row.kind, row.extra, row.track, row.state))
                .collect::<Vec<_>>()
        };
        use RowKind::{App, Mic, Target};
        use RowState::{Absent, Denied, Quiet};
        assert_eq!(
            states(true, false),
            vec![
                (Target, None, Some(0), RowState::Target),
                (App, Some(1), None, Quiet),
                (Mic, Some(0), None, Quiet),
                (Mic, Some(2), None, Quiet),
            ]
        );
        let unplugged = states(false, false);
        assert_eq!(unplugged[2].3, Absent);
        assert_eq!(unplugged[3].3, Quiet);
        assert_eq!(unplugged[1].3, Quiet);
        let denied = states(false, true);
        assert_eq!((denied[2].3, denied[3].3), (Denied, Denied));
        assert_eq!(denied[1].3, Quiet);
        assert!(mic_warning(Some(&target), true));
        assert!(!mic_warning(Some(&target), false));
        let apps_only = TargetAudio {
            extras: vec![app("a.exe", true)],
            ..TargetAudio::default()
        };
        assert!(!mic_warning(Some(&apps_only), true));
        assert!(!mic_warning(None, true));
    }

    /// 画面全体 first and always; then the registrations with a sound, minus
    /// the auto-record list (case-insensitive), in key order.
    #[test]
    fn the_audio_group_is_the_screen_then_unlisted_apps_with_sounds() {
        use crate::settings::{AutoCaptureApp, SCREEN_AUDIO_KEY};
        let with = |extras: Vec<ExtraAudio>| TargetAudio {
            extras,
            ..TargetAudio::default()
        };
        let mut registered = std::collections::BTreeMap::new();
        registered.insert("zed.exe".to_owned(), with(vec![mic(None)]));
        registered.insert("game.exe".to_owned(), with(vec![app("chat.exe", true)]));
        registered.insert("quiet.exe".to_owned(), with(Vec::new()));
        registered.insert("alpha.exe".to_owned(), with(vec![app("x.exe", false)]));
        let listed = [AutoCaptureApp::named("Game.EXE")];
        assert_eq!(
            audio_group_keys(&registered, &listed),
            vec![SCREEN_AUDIO_KEY, "alpha.exe", "zed.exe"]
        );
        registered.insert(SCREEN_AUDIO_KEY.to_owned(), with(vec![mic(None)]));
        assert_eq!(
            audio_group_keys(&registered, &[]),
            vec![SCREEN_AUDIO_KEY, "alpha.exe", "game.exe", "zed.exe"]
        );
    }

    fn window(name: Option<&str>, kind: CaptureTargetKind) -> CaptureTarget {
        CaptureTarget {
            kind,
            primary: false,
            id: String::new(),
            window_handle: String::new(),
            process_id: 1,
            title: String::new(),
            executable_id: None,
            executable_name: name.map(Into::into),
            minimized: false,
            selectable: true,
            unavailable_reason: None,
        }
    }

    /// Liveback and the target are left out, one row per executable name
    /// (case-insensitive), displays and nameless windows never, and an app
    /// already registered is not offered twice.
    #[test]
    fn the_add_list_is_one_row_per_running_app_minus_self_target_and_registered() {
        use CaptureTargetKind::*;
        let targets = [
            window(Some("Discord.exe"), Window),
            window(Some("discord.exe"), Window),
            window(Some("liveback.exe"), Window),
            window(Some("Game.exe"), Window),
            window(None, Window),
            window(Some("chrome.exe"), Monitor),
            window(Some("Spotify.exe"), Window),
            window(Some("obs64.exe"), Window),
            window(Some("ApplicationFrameHost.exe"), Window),
            window(Some("windowsterminal.exe"), Window),
        ];
        let target = TargetAudio {
            extras: vec![app("OBS64.EXE", false)],
            ..TargetAudio::default()
        };
        let names: Vec<_> = add_candidates(&targets, &["Liveback.exe", "game.exe"], Some(&target))
            .into_iter()
            .map(|t| t.executable_name.clone().unwrap())
            .collect();
        assert_eq!(names, vec!["Discord.exe", "Spotify.exe"]);
    }
}
