//! `%APPDATA%\com.liveback.desktop\settings.json` (Roaming, not Local).
//!
//! The layout is the one the WebView build's store plugin wrote -- a top-level
//! map of store keys, with everything this module cares about nested under
//! `"settings"` -- and is kept as-is so an existing install's preferences
//! survive the move to the native UI (task121). Other top-level keys are
//! preserved on save for the same reason.
//!
//! Reading is deliberately per-field and lenient: a single bad field falls back
//! to its own default rather than discarding every other setting alongside it.
//! Only a file that isn't parseable JSON at all falls back wholesale.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::ring_buffer::{DEFAULT_RETENTION_MINUTES, MAX_RETENTION_MINUTES, MIN_RETENTION_MINUTES};
use crate::DEFAULT_HOTKEY;

/// Store key the frontend nests `AppSettings` under.
const STORE_KEY: &str = "settings";
/// Names the config directory. Also the AUMID the installer registers for toasts.
const BUNDLE_IDENTIFIER: &str = "com.liveback.desktop";
const STORE_FILE: &str = "settings.json";

pub const SETTINGS_VERSION: u8 = 2;

/// 30 and 60 only, matching `FrameThrottle::new` in `capture/gpu.rs` and the
/// encoder, both of which reject anything else at `start_capture` time. A test
/// below asserts the two stay in step.
pub const FRAME_RATES: [u8; 3] = [30, 60, 120];
pub const DEFAULT_FRAME_RATE: u8 = 60;

pub const MIN_RETENTION_CAPACITY_GB: i64 = 1;
pub const MAX_RETENTION_CAPACITY_GB: i64 = 2000;
pub const DEFAULT_RETENTION_CAPACITY_GB: i64 = 20;

pub const MIN_SESSION_LIFETIME_DAYS: i64 = 1;
pub const MAX_SESSION_LIFETIME_DAYS: i64 = 365;
pub const DEFAULT_SESSION_LIFETIME_DAYS: i64 = 30;

pub const MIN_VOLUME_PERCENT: i64 = 0;
pub const MAX_VOLUME_PERCENT: i64 = 100;
pub const DEFAULT_VOLUME_PERCENT: i64 = 100;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionViewMode {
    /// Default since task115: most sessions have no usable thumbnail.
    #[default]
    List,
    Thumbnail,
}

/// What recording encodes with (task1760). The default is and stays H.264:
/// every machine that runs this app can encode and decode it, while AV1 needs
/// both a GPU encoder and an OS decoder that are separately installable.
///
/// Only ever written to the file when it is not the default -- see
/// `AppSettings::codec`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RecordingCodec {
    #[default]
    H264,
    Av1,
}

/// `skip_serializing_if` for the codec, whose default is H.264.
fn is_h264(value: &RecordingCodec) -> bool {
    matches!(value, RecordingCodec::H264)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowState {
    pub height: i64,
    /// Whether the window was maximized when it was last written (task2650).
    /// `default` is what keeps every already-installed record readable -- the
    /// key does not exist in files the shipping build wrote, and without it
    /// the whole `windowState` would fail to deserialize and silently fall
    /// back to no stored geometry at all. Skipped on `false` so an unchanged
    /// record still rewrites byte-identically.
    #[serde(default, skip_serializing_if = "is_not_maximized")]
    pub maximized: bool,
    pub width: i64,
    pub x: i64,
    pub y: i64,
}

/// `skip_serializing_if` for the maximized flag, whose default is `false`.
fn is_not_maximized(value: &bool) -> bool {
    !*value
}

/// One auto-capture registration (task2560). `name` -- the executable name as
/// spelled on disk -- is the identity every match uses; `display_name` and
/// `path` are display metadata only, resolved from the running process when the
/// registration is made. Stored as a bare string while both are `None`, so a
/// hand-written file keeps its shape across a load-and-save.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoCaptureApp {
    /// Record the monitor the app's first capturable window sits on instead of
    /// the window itself (task2570). For multi-window apps -- game editors --
    /// whose interesting activity spans windows. Audio becomes the system mix:
    /// a monitor target has no process to loopback.
    pub capture_monitor: bool,
    /// `FileDescription` (or `ProductName`) from the exe's version info.
    pub display_name: Option<String>,
    pub name: String,
    /// Full image path, kept so the icon can be re-read after the app exits.
    pub path: Option<String>,
}

impl AutoCaptureApp {
    /// A metadata-less entry -- what a bare string in the file becomes.
    pub fn named(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }
}

impl Serialize for AutoCaptureApp {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Both-way compatibility (task2560): an entry that never gained
        // metadata goes back out as the plain string it came in as. A set
        // monitor flag (task2570) forces the map shape -- a bare string has
        // nowhere to carry it.
        if self.display_name.is_none() && self.path.is_none() && !self.capture_monitor {
            return serializer.serialize_str(&self.name);
        }
        use serde::ser::SerializeMap;
        // Alphabetical camelCase keys, matching the rest of the file.
        let mut map = serializer.serialize_map(None)?;
        if self.capture_monitor {
            map.serialize_entry("captureMonitor", &true)?;
        }
        if let Some(display_name) = &self.display_name {
            map.serialize_entry("displayName", display_name)?;
        }
        map.serialize_entry("name", &self.name)?;
        if let Some(path) = &self.path {
            map.serialize_entry("path", path)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for AutoCaptureApp {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = AutoCaptureApp;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("an executable name or an auto-capture entry object")
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(AutoCaptureApp::named(value))
            }

            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut entry = AutoCaptureApp::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "captureMonitor" => entry.capture_monitor = map.next_value()?,
                        "displayName" => entry.display_name = map.next_value()?,
                        "name" => entry.name = map.next_value()?,
                        "path" => entry.path = map.next_value()?,
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                if entry.name.is_empty() {
                    return Err(serde::de::Error::missing_field("name"));
                }
                Ok(entry)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// A folder whose processes all auto-record (task3600). Every executable
/// launched from under `path` -- subfolders included -- arms the watch, whatever
/// it is called, which is how a game whose real binary sits behind a launcher
/// (or changes name between patches) gets recorded without naming it.
///
/// Deliberately its own list rather than a `folder` flag on `AutoCaptureApp`:
/// all four name-matching helpers there (`is_registered`, `wants_monitor_capture`,
/// `toggle_monitor`, `unregister`) would need a "skip folder rules" arm, and the
/// bare-string serialization a metadata-less entry keeps has nowhere to put a
/// path. `review_toggle` is not among them since task3670: it already takes
/// `folders: &[AutoCaptureFolder]` and reads them through
/// `auto_capture::covered_by`.
///
/// Struct-shaped only -- unlike `AutoCaptureApp` there is no legacy bare-string
/// form to stay compatible with. Alphabetical camelCase keys like the rest of
/// the file, and `captureMonitor` is skipped while false so a hand-written
/// `{"path": ...}` keeps its shape across a load-and-save.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutoCaptureFolder {
    /// Record the monitor the matched app's first capturable window sits on
    /// instead of the window itself, exactly as `AutoCaptureApp` does. An
    /// executable registered by name wins over this when both hit -- see
    /// `ui_state::auto_capture::wants_monitor_for`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_monitor: bool,
    /// The folder as it is written in the file. Never expanded: environment
    /// variables and junctions are out of scope, so `%ProgramFiles%\Foo` is a
    /// rule that matches nothing rather than one that matches Program Files.
    pub path: String,
}

impl AutoCaptureFolder {
    /// A rule with the default flags -- what a hand-written `{"path": ...}`
    /// becomes.
    pub fn at(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            ..Self::default()
        }
    }
}

/// `skip_serializing_if` for a bool whose default is `true`.
/// `skip_serializing_if` for a bool whose default is `false`.
/// `#[serde(default)]` on a `bool` yields `false`; this is for the flags whose
/// default is the other one.
fn yes() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// `skip_serializing_if` for the language, whose default is "ask the OS".
fn is_system_language(value: &String) -> bool {
    value == crate::ui_state::locale::LANGUAGE_SYSTEM
}

/// `skip_serializing_if` for the theme, whose default is "ask the OS" too
/// (round16).
fn is_system_theme(value: &String) -> bool {
    value == crate::ui_state::settings::THEME_SYSTEM
}

/// Field order is alphabetical to match what the store plugin wrote, so a
/// before/after diff of the real file stays empty when nothing changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    /// Executable names (`ffxiv_dx11.exe`) whose launch starts a recording on
    /// its own (task1970). Empty unless the user registered something, and
    /// absent from the file while it is empty -- same `skip_serializing_if`
    /// reasoning as the flags below: a record written before this task must
    /// not gain the key just by being loaded.
    ///
    /// Nothing to do with `auto_start` below, which is the Windows login item.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_capture_executables: Vec<AutoCaptureApp>,
    /// Folders (`D:\Games\Foo`) whose every process auto-records, whatever it
    /// is called (task3600) -- see `AutoCaptureFolder`. Its own key rather than
    /// a shape inside `autoCaptureExecutables`, and absent while empty for the
    /// same reason as the list above: a record written before this task must
    /// not gain the key just by being loaded.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auto_capture_folders: Vec<AutoCaptureFolder>,
    pub auto_start: bool,
    /// Where recordings are buffered, when the user moved it off the default
    /// `%LOCALAPPDATA%` path (task164). Absent unless set, like the other
    /// optional keys: the React build never wrote it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buffer_directory: Option<String>,
    /// Record every application's sound instead of only the capture target's
    /// process tree (task1430). Off by default, and still one audio track
    /// either way -- on, the mix is everything except Liveback itself, so a
    /// review played back during a recording is not recorded into it.
    ///
    /// Read when a recording starts, so a flip only reaches the next one. Same
    /// `skip_serializing_if` reasoning as the flags below: a record written
    /// before this task must not gain the key just by being loaded.
    #[serde(default, skip_serializing_if = "is_false")]
    pub capture_all_audio: bool,
    /// Which video codec a recording is encoded with (task1760). H.264 unless
    /// the user opted into AV1, and the option is only offered on a machine
    /// that has both an AV1 encoder and an AV1 decoder.
    ///
    /// Read when a recording starts, so a change only reaches the next one.
    /// Same `skip_serializing_if` reasoning as `capture_all_audio` above: a
    /// record written before this task must not gain the key just by being
    /// loaded, which is also what keeps the default machine's file untouched.
    #[serde(default, skip_serializing_if = "is_h264")]
    pub codec: RecordingCodec,
    // No `clip_hotkey` / `clip_seconds` since task1090: clip saving is gone.
    // Old records still carry `clipHotkey` / `clipSeconds`; serde ignores
    // unknown keys by default (there is no `deny_unknown_fields` here) and
    // `from_stored` reads a `Map` by name, so both routes skip them.
    /// Where 「クリップに保存」 writes (task2970). `None` means the fixed
    /// default `<default export directory>\Clips` -- deliberately *not*
    /// `export_output_directory`, which is a memory of the last save dialog and
    /// would make the clip folder drift with every export. Skipped when absent
    /// for the same reason as the fields below: a record written before this
    /// task must not gain the key just by being loaded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clip_directory: Option<String>,
    pub diagnostics_drawer_open: bool,
    // The three optional fields are absent from the real file unless set, and
    // writing them back as `null` would be a key the frontend never wrote.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_output_directory: Option<String>,
    pub frame_rate: u8,
    pub hotkey: String,
    pub include_cursor: bool,
    /// UI language (task1150): `system` (default), `ja` or `en`. Written only
    /// when it is not the default, like the two flags below -- the key did not
    /// exist before this task, and a record that predates it must not be given
    /// one.
    #[serde(skip_serializing_if = "is_system_language")]
    pub language: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_capture_executable: Option<String>,
    pub muted: bool,
    pub retention_capacity_gb: i64,
    pub retention_minutes: u16,
    /// round8 §3-1. Off means the ring stops: a running capture ends and no new
    /// one starts.
    ///
    /// Off by default since task1080, which flips what an absent key means:
    /// recording continuously is a thing to opt into, not something a fresh
    /// install starts doing on its own. Existing records that never wrote the
    /// key were relying on absent-means-on and therefore come back off -- see
    /// the task's Execution Log, this was the accepted cost of the change.
    #[serde(skip_serializing_if = "is_false")]
    pub ring_buffer_enabled: bool,
    /// Whether the review screen's right detail panel is open (round5 §4-D,
    /// task171). Open by default, and remembered: closing it is a deliberate
    /// "give the video the whole width" and should survive a restart.
    ///
    /// Written only once it is `false`, for the same reason the three
    /// `Option` fields above are skipped when absent: the key did not exist
    /// before this task, and rewriting a record that predates it must not
    /// invent one. Absent therefore means open.
    #[serde(skip_serializing_if = "is_true")]
    pub review_panel_open: bool,
    pub session_lifetime_days: i64,
    pub session_view_mode: SessionViewMode,
    /// Whether the clip screen groups by month rather than by week (the user,
    /// 2026-09-20). A bool rather than a `ClipGrouping`: the toggle, the slint
    /// property and this key all say the same thing, and a second enum to
    /// spell it in would only need translating at both ends.
    ///
    /// Skipped while it is `false` for the same reason `review_panel_open` is
    /// skipped while open: the key did not exist before this change, and
    /// rewriting an older record must not invent one. Absent means week.
    #[serde(skip_serializing_if = "is_false")]
    pub clip_month_grouping: bool,
    /// Which theme the window paints in (round16): `system` (default), `light`
    /// or `dark`. Skipped while it is the default for the same reason the
    /// language is: the key did not exist before round16, and rewriting a
    /// record that predates it must not invent one.
    #[serde(skip_serializing_if = "is_system_theme")]
    pub theme: String,
    /// Ask GitHub, at startup and every 24 hours, whether a newer release
    /// exists (task t260920-2be2). On unless the user turned it off; the first
    /// run asks before anything goes out, which is what `update_check_prompted`
    /// below remembers. Nothing is sent either way -- see `crate::update`.
    ///
    /// Written only while it is off, for the reason the flags above give: a
    /// record that predates this task must not gain the key just by being read.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub update_check_enabled: bool,
    /// Whether the one-time consent has been put to the user. Separate from the
    /// answer so that "never asked" and "asked, said no" stay distinguishable --
    /// otherwise a `false` answer would be re-asked on every launch.
    #[serde(default, skip_serializing_if = "is_false")]
    pub update_check_prompted: bool,
    /// Unix seconds of the last check *attempt*, not the last success. An
    /// offline machine would otherwise re-check on every launch for nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_last_checked: Option<i64>,
    pub version: u8,
    /// How many times the video stage's first-run hint has been shown. React
    /// kept this in `localStorage`; the slint build has no such store, so it
    /// rides along here (task128). Optional for the same reason as the three
    /// above: the React build never wrote this key, so a rewrite of a record
    /// it produced must not invent it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_stage_hint_shown: Option<i64>,
    pub volume_percent: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window_state: Option<WindowState>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            auto_capture_executables: Vec::new(),
            auto_capture_folders: Vec::new(),
            auto_start: false,
            buffer_directory: None,
            capture_all_audio: false,
            codec: RecordingCodec::H264,
            clip_directory: None,
            diagnostics_drawer_open: false,
            export_output_directory: None,
            frame_rate: DEFAULT_FRAME_RATE,
            hotkey: DEFAULT_HOTKEY.to_string(),
            include_cursor: true,
            language: crate::ui_state::locale::LANGUAGE_SYSTEM.to_string(),
            last_capture_executable: None,
            muted: false,
            retention_capacity_gb: DEFAULT_RETENTION_CAPACITY_GB,
            retention_minutes: DEFAULT_RETENTION_MINUTES,
            ring_buffer_enabled: false,
            review_panel_open: true,
            session_lifetime_days: DEFAULT_SESSION_LIFETIME_DAYS,
            session_view_mode: SessionViewMode::List,
            clip_month_grouping: false,
            theme: crate::ui_state::settings::THEME_SYSTEM.to_string(),
            update_check_enabled: true,
            update_check_prompted: false,
            update_last_checked: None,
            version: SETTINGS_VERSION,
            video_stage_hint_shown: None,
            volume_percent: DEFAULT_VOLUME_PERCENT,
            window_state: None,
        }
    }
}

/// Mirrors `clampInteger` in `settings.ts`: a non-finite or non-numeric value
/// falls back, anything else is rounded then clamped.
fn clamp_integer(value: Option<f64>, min: i64, max: i64, fallback: i64) -> i64 {
    match value {
        Some(value) if value.is_finite() => (value.round() as i64).clamp(min, max),
        _ => fallback,
    }
}

fn number(stored: &Map<String, Value>, key: &str) -> Option<f64> {
    stored.get(key).and_then(Value::as_f64)
}

fn text(stored: &Map<String, Value>, key: &str) -> Option<String> {
    stored.get(key).and_then(Value::as_str).map(str::to_string)
}

/// String-or-struct entries, skipping anything unreadable rather than failing
/// the whole key over one bad element (task1970's tolerance, kept for the
/// struct shape task2560 added).
/// One array key read per element rather than as a whole: a single unreadable
/// entry loses that entry, not the list. Used for both auto-capture lists
/// (task1970, task3600) -- a hand-edited `settings.json` is the documented way
/// to register either, so one typo must not silently switch the feature off.
fn tolerant_list<T: serde::de::DeserializeOwned>(stored: &Map<String, Value>, key: &str) -> Vec<T> {
    stored
        .get(key)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| serde_json::from_value(value.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// round8 §3-2: 0 is 「なし」 on the two retention limits -- the limit is not in
/// force, which the sweep reads as "never reclaim for this reason". It is the
/// one value below the range that is kept rather than clamped up into it; the
/// chip is the only way to enter it, since the free field still states 1-2000.
pub const RETENTION_NONE: i64 = 0;

fn clamp_or_none(value: i64, min: i64, max: i64) -> i64 {
    if value == RETENTION_NONE {
        RETENTION_NONE
    } else {
        value.clamp(min, max)
    }
}

fn clamp_integer_or_none(value: Option<f64>, min: i64, max: i64, fallback: i64) -> i64 {
    match value {
        // Exactly 0, not "anything under the range": a stored -5 is a corrupt
        // record and still lands on the default, the way it always did.
        Some(number) if number as i64 == RETENTION_NONE => RETENTION_NONE,
        other => clamp_integer(other, min, max, fallback),
    }
}

fn flag(stored: &Map<String, Value>, key: &str, fallback: bool) -> bool {
    stored.get(key).and_then(Value::as_bool).unwrap_or(fallback)
}

/// The hotkey half of `validateSettings`: an empty key falls back to the
/// default. The collision handling went with the clip hotkey (task1090) --
/// there is only one chord now, so it has nothing to collide with.
fn resolve_hotkey(hotkey: Option<String>) -> String {
    hotkey
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_HOTKEY.to_string())
}

impl AppSettings {
    /// Applies `validateSettings` to an already-typed value, for the save path.
    pub fn validated(&self) -> Self {
        let migrating = self.version != SETTINGS_VERSION;
        let hotkey = resolve_hotkey(Some(self.hotkey.clone()));
        Self {
            frame_rate: if FRAME_RATES.contains(&self.frame_rate) {
                self.frame_rate
            } else {
                DEFAULT_FRAME_RATE
            },
            hotkey,
            retention_capacity_gb: clamp_or_none(
                self.retention_capacity_gb,
                MIN_RETENTION_CAPACITY_GB,
                MAX_RETENTION_CAPACITY_GB,
            ),
            retention_minutes: self
                .retention_minutes
                .clamp(MIN_RETENTION_MINUTES, MAX_RETENTION_MINUTES),
            session_lifetime_days: clamp_or_none(
                self.session_lifetime_days,
                MIN_SESSION_LIFETIME_DAYS,
                MAX_SESSION_LIFETIME_DAYS,
            ),
            // task078's one-time migration: a pre-version-2 record lands on
            // thumbnail exactly once. It has to be decided from the *stored*
            // version, before the stamp below, or a genuine "list" choice and a
            // v1 record become indistinguishable on the next load.
            session_view_mode: if migrating {
                SessionViewMode::Thumbnail
            } else {
                self.session_view_mode
            },
            version: SETTINGS_VERSION,
            volume_percent: self
                .volume_percent
                .clamp(MIN_VOLUME_PERCENT, MAX_VOLUME_PERCENT),
            ..self.clone()
        }
    }

    /// Builds settings from the object stored under `"settings"`, recovering
    /// per field. A value of the wrong type is treated the same as a missing
    /// one, which is what the frontend's `??`/`typeof` guards do.
    pub fn from_stored(stored: &Map<String, Value>) -> Self {
        let defaults = Self::default();
        let version = number(stored, "version")
            .map(|value| value as u8)
            .unwrap_or(0);
        let hotkey = resolve_hotkey(text(stored, "hotkey"));
        let frame_rate = number(stored, "frameRate")
            .map(|value| value as u8)
            .filter(|value| FRAME_RATES.contains(value))
            .unwrap_or(DEFAULT_FRAME_RATE);
        let candidate = Self {
            // Per element rather than `from_value::<Vec<String>>`, which errors
            // on the whole array over one bad entry and would drop a list the
            // user hand-edited (task1970). Same tolerance the removed
            // `audioTrackExecutables` had.
            auto_capture_executables: tolerant_list(stored, "autoCaptureExecutables"),
            // Same tolerance, same reason (task3600): this list is registered
            // by hand until task3610 builds the button for it.
            auto_capture_folders: tolerant_list(stored, "autoCaptureFolders"),
            auto_start: flag(stored, "autoStart", defaults.auto_start),
            buffer_directory: text(stored, "bufferDirectory"),
            // Absent means off (task1430): recording applications the user did
            // not point at is something to opt into, and every record written
            // before the toggle existed predates the choice.
            capture_all_audio: flag(stored, "captureAllAudio", defaults.capture_all_audio),
            // Matched against the non-default string, like `sessionViewMode`
            // above: absent, misspelled or a codec this build does not know
            // all land on H.264, which is the one every machine can play.
            codec: match text(stored, "codec").as_deref() {
                Some("av1") => RecordingCodec::Av1,
                _ => RecordingCodec::H264,
            },
            // Absent means the key predates task1150, which is the same thing
            // it means afterwards: follow the OS.
            language: text(stored, "language")
                .unwrap_or_else(|| crate::ui_state::locale::LANGUAGE_SYSTEM.to_string()),
            clip_directory: text(stored, "clipDirectory"),
            diagnostics_drawer_open: flag(
                stored,
                "diagnosticsDrawerOpen",
                defaults.diagnostics_drawer_open,
            ),
            export_output_directory: text(stored, "exportOutputDirectory"),
            frame_rate,
            hotkey,
            include_cursor: flag(stored, "includeCursor", defaults.include_cursor),
            last_capture_executable: text(stored, "lastCaptureExecutable"),
            // `muted: candidate.muted === true` -- anything non-boolean is false.
            muted: flag(stored, "muted", false),
            retention_capacity_gb: clamp_integer_or_none(
                number(stored, "retentionCapacityGb"),
                MIN_RETENTION_CAPACITY_GB,
                MAX_RETENTION_CAPACITY_GB,
                DEFAULT_RETENTION_CAPACITY_GB,
            ),
            retention_minutes: clamp_integer(
                number(stored, "retentionMinutes"),
                i64::from(MIN_RETENTION_MINUTES),
                i64::from(MAX_RETENTION_MINUTES),
                i64::from(DEFAULT_RETENTION_MINUTES),
            ) as u16,
            // Absent means off (task1080): a record that never wrote the key
            // predates the flip and comes back with the ring stopped.
            ring_buffer_enabled: flag(stored, "ringBufferEnabled", defaults.ring_buffer_enabled),
            // Absent means "never closed it", which is the default: open.
            review_panel_open: flag(stored, "reviewPanelOpen", defaults.review_panel_open),
            session_lifetime_days: clamp_integer_or_none(
                number(stored, "sessionLifetimeDays"),
                MIN_SESSION_LIFETIME_DAYS,
                MAX_SESSION_LIFETIME_DAYS,
                DEFAULT_SESSION_LIFETIME_DAYS,
            ),
            // Testing for "thumbnail" rather than "list" is what actually moves
            // the default (task115): anything unset or unrecognised lands on the
            // default, which is now list.
            session_view_mode: match text(stored, "sessionViewMode").as_deref() {
                Some("thumbnail") => SessionViewMode::Thumbnail,
                _ => SessionViewMode::List,
            },
            // Absent means week: the default, and what every record written
            // before this key existed meant by saying nothing.
            clip_month_grouping: flag(stored, "clipMonthGrouping", defaults.clip_month_grouping),
            // Absent means the key predates round16, which is the same thing
            // it means afterwards: follow the OS.
            theme: text(stored, "theme")
                .unwrap_or_else(|| crate::ui_state::settings::THEME_SYSTEM.to_string()),
            // Absent means the key predates this task, which says the same
            // thing the default does: checking on, consent not yet put.
            update_check_enabled: flag(stored, "updateCheckEnabled", defaults.update_check_enabled),
            update_check_prompted: flag(
                stored,
                "updateCheckPrompted",
                defaults.update_check_prompted,
            ),
            update_last_checked: number(stored, "updateLastChecked").map(|value| value as i64),
            version,
            // Only ever counted up by the stage itself, so absent means "never
            // shown" and a negative hand-edit is floored rather than honoured.
            video_stage_hint_shown: number(stored, "videoStageHintShown")
                .map(|count| count.max(0.0) as i64),
            volume_percent: clamp_integer(
                number(stored, "volumePercent"),
                MIN_VOLUME_PERCENT,
                MAX_VOLUME_PERCENT,
                DEFAULT_VOLUME_PERCENT,
            ),
            window_state: stored
                .get("windowState")
                .and_then(|value| serde_json::from_value(value.clone()).ok()),
        };
        candidate.validated()
    }
}

/// `%APPDATA%\com.liveback.desktop\settings.json`.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join(BUNDLE_IDENTIFIER)
            .join(STORE_FILE)
    })
}

/// Reads settings without writing anything back. A missing file, unparseable
/// JSON, or a missing `"settings"` key all yield defaults.
pub fn load(path: &Path) -> AppSettings {
    let Ok(bytes) = fs::read(path) else {
        return AppSettings::default();
    };
    let Ok(Value::Object(store)) = serde_json::from_slice::<Value>(&bytes) else {
        // Loud, because the fallback is indistinguishable from "the user never
        // changed anything" at every layer above. The usual cause is a UTF-8
        // BOM: PowerShell 5.1's `Set-Content -Encoding UTF8` writes one and
        // `from_slice` rejects it, so a hand-written settings file silently
        // does nothing. A missing file is not this branch -- it returns above.
        tracing::warn!(
            target: "settings",
            path = %path.display(),
            bytes = bytes.len(),
            bom = bytes.starts_with(b"\xEF\xBB\xBF"),
            "settings file is not a JSON object; falling back to defaults"
        );
        return AppSettings::default();
    };
    match store.get(STORE_KEY) {
        Some(Value::Object(stored)) => AppSettings::from_stored(stored),
        // The frontend's `?? defaultSettings` path: no stored record at all is
        // not a migration, so nothing gets written back for it either.
        _ => AppSettings::default(),
    }
}

/// Reads settings and, when the stored record predates version 2, persists the
/// migrated result immediately -- the same thing `loadSettings` does, so the
/// file reflects version 2 right after the first load rather than waiting for an
/// unrelated edit. A failed write is reported but never blocks the read.
pub fn load_and_migrate(path: &Path) -> (AppSettings, Option<io::Error>) {
    let stored_record = fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get(STORE_KEY).cloned());
    // A record that exists but carries no (or a non-integer) `version` key is
    // still a migration: `from_stored` treats missing as version 0. Only "no
    // stored record at all" skips the write-back, matching `load`.
    let migrated = stored_record.as_ref().is_some_and(|record| {
        record.get("version").and_then(Value::as_u64) != Some(u64::from(SETTINGS_VERSION))
    });
    let settings = load(path);
    if migrated {
        if let Err(error) = save(path, &settings) {
            return (settings, Some(error));
        }
    }
    (settings, None)
}

/// Validates, then writes back in the shape described above,
/// preserving any other store keys the file already holds.
pub fn save(path: &Path, settings: &AppSettings) -> io::Result<()> {
    let mut store = match fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
    {
        Some(Value::Object(existing)) => existing,
        _ => Map::new(),
    };
    store.insert(
        STORE_KEY.to_string(),
        serde_json::to_value(settings.validated()).map_err(io::Error::other)?,
    );
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(&Value::Object(store)).map_err(io::Error::other)?;
    fs::write(path, json)
}

#[cfg(test)]
mod tests;
