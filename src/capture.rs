use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

mod catalog;
/// The one writer of a recording's index (task430).
pub mod indexer;
mod review;

#[cfg(test)]
use std::time::Instant;

use crate::events::EventSink;
use crate::{encoder, ring_buffer};
use crossbeam_channel::{bounded, Receiver};

#[cfg(test)]
use windows::Win32::{
    Foundation::HWND,
    Graphics::Direct3D11::{
        ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
        D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    },
};

#[cfg(test)]
use windows::Win32::Graphics::{
    Direct3D11::{D3D11_BIND_SHADER_RESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT},
    Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC},
};

const FRAME_QUEUE_CAPACITY: usize = 1;

// `pub(crate)` since t260913-2527: `playback::gpu` reuses the full-screen
// vertex shader and the two shader-compile helpers rather than copying them.
// Only those three items are `pub(crate)` inside it; the rest stays private.
pub(crate) mod gpu;
#[cfg(test)]
pub use gpu::FitRect;
pub(crate) use gpu::GpuNv12Converter;
pub use gpu::{aspect_fit, crop_or_fit, source_viewport, FrameThrottle, Inset};
use gpu::{FrameDebug, TransformPipeline};

mod types;
use types::RawCaptureFrame;

pub mod audio;
pub mod targets;
pub use types::{
    default_encoder_output_dir, CaptureColorSpace, CaptureConfig, CaptureDiagnostics,
    CaptureLifecycleStatus, CaptureSize, CaptureStopReason, CapturedFrame, ExtraTrackSource,
    AUDIO_SESSION_STAGE, MID_RECORDING_STAGE, VIDEO_ENCODER_STAGE,
};

mod session;
pub use session::CaptureSession;
use session::HeldWorker;

pub(crate) mod leak_probe;
mod worker;
pub(crate) use worker::create_d3d_device;
#[cfg(test)]
use worker::{create_d3d_device_with_flags, measured_fps_from_counts, write_segment_thumbnail};

#[derive(Clone, Debug)]
struct SessionLease {
    session_id: String,
    indexes: Vec<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReclaimReport {
    pub removed_sessions: u32,
    pub freed_bytes: u64,
    /// The capacity-driven share of the totals above (task700). Only this half
    /// is worth telling the user about: an aged-out session is the retention
    /// setting doing what it says, while one pushed out by the capacity budget
    /// is a recording disappearing for a reason nobody asked for.
    pub over_capacity_sessions: u32,
    pub over_capacity_bytes: u64,
}

/// See `CaptureController::latest_finalized_range`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FinalizedRange {
    pub session_id: String,
    pub target_title: Option<String>,
    pub oldest_start_100ns: i64,
    pub end_100ns: i64,
}

/// One running recording, keyed by its session id in `CaptureController::active`
/// (task1990). Everything here used to be a separate `Arc<Mutex<Option<..>>>`
/// field on the controller, which quietly meant "there is at most one
/// recording" in six places at once; holding one entry per capture says it once,
/// and says it somewhere a second capture can be added (task2000, which did).
struct ActiveCapture {
    // Which `start` this was, from `CaptureController::next_start_seq`. A
    // `HashMap` has no order, and two things need one (task2000):
    // `active_session_ids()` lists captures oldest-first for the UI, and every
    // no-argument getter answers for the *last started* capture. Not the session
    // id: ids are chronological only for real ones (`default_encoder_output_dir`
    // nanosecond hex), and sorting by them would make a test's arbitrary id
    // decide which capture is "current".
    sequence: u64,
    session: CaptureSession,
    // What this capture is pointed at, for `target_minimized()`. The session
    // itself keeps no handle -- it hands the config to the worker thread and
    // forgets it.
    target: (String, targets::CaptureTargetKind),
    // What to call this capture on screen, resolved once at `start` (task2140).
    // The same title the container is named with. Kept here rather than looked
    // up when needed because the mute notice has to say *which* recording lost
    // its sound, and a window can be renamed or closed before it is read.
    target_title: Option<String>,
    // Markers pressed during this recording, waiting to be folded into its
    // manifest. The hotkey handler must not touch the manifest directly: while
    // recording it belongs to the capture worker, which rewrites it on every
    // segment and would silently overwrite an outside edit (same reason renames
    // are refused mid-recording -- see `set_session_title`).
    //
    // Per entry rather than per controller since task1990, which is also why
    // `start` no longer clears it: a queue that belongs to one recording cannot
    // hold a marker carrying a previous recording's timestamp, so the drop that
    // used to happen at the next `start` now happens structurally, when the
    // entry goes.
    pending_markers: Arc<Mutex<Vec<ring_buffer::MarkerRecord>>>,
    // Note and protection edits made *while* this container is being recorded
    // (task720). They cannot be written where they are made: reopening the file
    // to append truncates it back to the last checkpoint, cutting off whatever
    // the recording appended since. Same rail as `pending_markers`, and for the
    // same reason -- only the index writer thread may touch that `.lvb`. Empty
    // for a directory session, which still edits its manifest directly.
    pending_meta: Arc<Mutex<Vec<ring_buffer::container::MetaEvent>>>,
    // This recording's index writer (task430). Joined -- always outside every
    // lock -- between the capture worker's join and `ring.close()`, which is
    // what makes "the last segment is in the manifest" true.
    indexer: Option<thread::JoinHandle<()>>,
    // Why this capture's worker stopped, when it stopped on its own. One
    // channel per entry since task1990: `CaptureStopReason` carries no session
    // id, so a shared channel could not say *which* capture ended.
    stop_events: Receiver<CaptureStopReason>,
    // The extra sounds this recording records (t261002-577c), so a change to
    // the target's registration can reach it mid-recording.
    audio: SessionAudio,
}

/// What `CaptureController::apply_target_audio` needs to fit one running
/// recording's extra tracks to a changed registration (t261002-577c).
struct SessionAudio {
    /// `settings::target_audio_key` of the target; `None` matches nothing.
    key: Option<String>,
    capture_all_audio: bool,
    monitor: bool,
    /// The extra tracks so far, track 1 onwards. Only ever appended to.
    extras: Vec<ExtraTrackSource>,
    /// The container's `AudioTracksSet`: the target's track, then `extras`.
    tracks: Vec<ring_buffer::container::AudioTrackInfo>,
    /// Track 0.s name: the target.s executable, empty for a screen.
    target_name: String,
}

/// The `AudioTracksSet` entries for `extras`, named the way a recording keeps
/// them: an application by executable name, a microphone by the name its
/// device has right now (empty when it is not connected).
fn extra_track_infos(extras: &[ExtraTrackSource]) -> Vec<ring_buffer::container::AudioTrackInfo> {
    extras
        .iter()
        .map(|source| match source {
            ExtraTrackSource::App(name) => ring_buffer::container::AudioTrackInfo {
                executable_name: name.clone(),
                microphone: None,
            },
            ExtraTrackSource::Mic(device_id) => ring_buffer::container::AudioTrackInfo {
                executable_name: audio::microphone_name(device_id.as_deref()).unwrap_or_default(),
                microphone: Some(device_id.clone().unwrap_or_default()),
            },
        })
        .collect()
}

#[derive(Clone)]
pub struct CaptureController {
    /// Every capture running right now, by session id, however many that is
    /// (task1990 changed the shape, task2000 capped it at two, task2080 dropped
    /// the cap). The no-argument getters below answer for the last-started
    /// entry; `*_for(session_id)` answers for a named one.
    ///
    /// **Lock order: `active` before `sessions`.** `start` holds `active` from
    /// its first refusal check to the insert and takes `sessions` inside that span, so
    /// nothing may take `active` while holding `sessions` -- read the id out and
    /// drop the guard first (`acquire_review_lease`, `load_active_session`,
    /// `reclaim_sessions`). Before task1990 the same rule was written against
    /// the separate `active` / `active_session` pair (task048, task430).
    active: Arc<Mutex<HashMap<String, ActiveCapture>>>,
    next_start_seq: Arc<AtomicU64>,
    last_diagnostics: Arc<Mutex<Option<CaptureDiagnostics>>>,
    last_timeline: Arc<Mutex<Option<ring_buffer::SessionManifest>>>,
    sink: Arc<Mutex<Option<Arc<dyn EventSink>>>>,
    sessions: Arc<Mutex<HashMap<String, ring_buffer::RingBuffer>>>,
    review_leases: Arc<Mutex<HashMap<String, SessionLease>>>,
    next_review_lease: Arc<AtomicU64>,
    /// The session ids whose stop is in flight -- the worker is being joined and
    /// finalized, which takes seconds on a long recording (task2170). One
    /// process-wide `AtomicBool` until then, which made a stop of A swallow
    /// every stop and start of B for that whole window: B's capsule said
    /// 停止中… while B went on recording.
    ///
    /// **Lock order: this set is a leaf.** Never take `active` or `sessions`
    /// while holding it -- lock it for the `insert`/`remove` and drop the guard
    /// before anything else.
    stopping: Arc<Mutex<HashSet<String>>>,
    lifecycle: Arc<Mutex<CaptureLifecycleStatus>>,
    /// Capture threads that finished a recording of an armed target and kept
    /// its WGC session open, by target handle (t260929-ea5e, the user's
    /// 2026-09-29 ruling C). The next `start` on the same handle runs on it.
    ///
    /// **Lock order: a leaf**, like `stopping`.
    held: Arc<Mutex<HashMap<String, HeldWorker>>>,
}

impl Default for CaptureController {
    fn default() -> Self {
        Self::new()
    }
}

/// Where recordings are written, when it is not the default (task164). Process
/// global rather than a `CaptureController` field because `buffer_root()` is an
/// associated function called from everywhere -- the catalog, the session
/// worker, the export pipeline -- none of which hold a controller.
static BUFFER_ROOT: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// The environment variable an agent or test run sets to point the buffer
/// somewhere of its own (task3910). **The only place this name is written.**
const BUFFER_ROOT_OVERRIDE_VAR: &str = "LIVEBACK_BUFFER_ROOT";

/// The override, read from the environment exactly once per process (task3910).
///
/// Motive: without it the only way to point an agent or test run's recordings
/// away from the user's own buffer is to edit the user's real `settings.json`,
/// which is itself the accident -- the 2026-09-08 sweep left four test sessions
/// and a clip in the user's buffer that way. `%TEMP%` is the folder to point
/// this at: the tool shell that runs those sweeps has `%LOCALAPPDATA%` and
/// `%APPDATA%` shadowed by a per-package mirror (task3720) that carries the
/// same size and mtime, so a read there cannot be told from a read of the real
/// path; `%TEMP%` is not shadowed, so evidence collected under it is
/// trustworthy.
///
/// `OnceLock` for the same reason `auto_capture::FolderGuard::machine()` uses
/// one: the answer cannot change while the process lives, and scattering
/// `var_os` calls would make it look as though it could.
pub fn buffer_root_override() -> Option<&'static Path> {
    static OVERRIDE: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    OVERRIDE
        .get_or_init(|| {
            buffer_root_override_from(std::env::var_os(BUFFER_ROOT_OVERRIDE_VAR).as_deref())
        })
        .as_deref()
}

/// The pure half of [`buffer_root_override`], so the blank-value rule can be
/// tested without `std::env::set_var` -- which is process-wide and would race
/// every other test through `BUFFER_ROOT_LOCK` (task3910).
///
/// An empty or whitespace-only value means *unset*, not "the current
/// directory": a launcher that computes the path and comes up empty must land
/// on today's behaviour rather than write recordings next to the exe.
pub(crate) fn buffer_root_override_from(raw: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let text = raw?.to_string_lossy().trim().to_owned();
    (!text.is_empty()).then(|| PathBuf::from(text))
}

/// `%LOCALAPPDATA%`, when the environment has one. Split out so
/// [`resolve_buffer_root`] can be handed a value instead of reading the
/// process environment.
fn local_app_data() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(PathBuf::from)
}

/// The priority itself, as a pure function (task3910): override, then the
/// configured folder, then `%LOCALAPPDATA%\Liveback\buffer`.
///
/// The override has to win *here* rather than inside
/// [`CaptureController::default_buffer_root`]: the moment `settings.json` fills
/// the `BUFFER_ROOT` static, the default is never consulted again, so an
/// override read there would lose to exactly the setting it exists to
/// outrank.
pub(crate) fn resolve_buffer_root(
    over: Option<&Path>,
    configured: Option<&Path>,
    local_app_data: Option<&Path>,
) -> PathBuf {
    if let Some(root) = over.or(configured) {
        return root.to_path_buf();
    }
    local_app_data
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir)
        .join("Liveback")
        .join("buffer")
}

/// Makes the buffer root exist, then checks there is room in it (task156).
///
/// The order is the whole point. `free_bytes` is `GetDiskFreeSpaceExW`, which
/// answers ERROR_PATH_NOT_FOUND for a path that is not there yet -- and the
/// buffer root was only ever created later, by `RingBuffer::create` and the
/// segment writer. So on a fresh install, or after the buffer was deleted by
/// hand, *every* `start` failed with `disk guard failed: ... (0x80070003)`
/// before it reached the code that would have created the directory.
///
/// `disk_root` is the output dir's parent, or the output dir itself when it has
/// no parent; both are the right thing to create and to measure.
///
/// `already_running` is how many captures are writing to that disk right now,
/// which is what makes the requirement grow (task2010): the floor is the room
/// for the capture being started, and every one already running needs its own
/// `PER_CAPTURE_RESERVE_BYTES` on top of it.
fn ensure_buffer_root_has_space(
    output_dir: &Path,
    already_running: usize,
) -> Result<(), StartError> {
    let disk_root = output_dir.parent().unwrap_or(output_dir);
    std::fs::create_dir_all(disk_root).map_err(|error| {
        StartError::BufferFolder(format!("could not create the buffer directory: {error}"))
    })?;
    let free = ring_buffer::free_bytes(disk_root)
        .map_err(|error| StartError::BufferFolder(format!("disk guard failed: {error}")))?;
    match disk_guard_refusal(free, already_running) {
        Some(refusal) => Err(StartError::NoSpace(
            crate::ui_state::lifecycle::disk_refusal(crate::ui_state::locale::active(), refusal),
        )),
        None => Ok(()),
    }
}

/// Why [`CaptureController::start`] said no (t260928-08df), grouped by what the
/// user can do about it. The `String` is the English detail for the log -- it
/// is never UI text: `ui_state::targets::start_failure_text` words each kind.
/// `NoSpace` is the exception, its sentence being the already-localized
/// `lifecycle::disk_refusal` (task2050).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartError {
    /// This process's own state refused: a poisoned lock, a reused session id,
    /// a thread that would not start. Restarting the app is the answer.
    Internal(String),
    /// The codec in the settings cannot record on this machine (task1760).
    Codec(String),
    /// The buffer folder could not be made, measured or written to.
    BufferFolder(String),
    /// Not enough room on the buffer's disk: the localized sentence.
    NoSpace(String),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Internal(detail)
            | Self::Codec(detail)
            | Self::BufferFolder(detail)
            | Self::NoSpace(detail) => f.write_str(detail),
        }
    }
}

/// Why the disk guard said no, before it is words (task2050). The numbers are
/// what the wording is made of, and keeping them apart is what lets the guard
/// stay a pure function while the sentence lives in the locale table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiskRefusal {
    pub(crate) required_bytes: u64,
    pub(crate) free_bytes: u64,
    /// How many captures were already writing to the disk. `0` is the floor
    /// case, which has its own wording.
    pub(crate) already_running: usize,
}

/// The numeric half of the guard, split out so it can be tested with made-up
/// free-space values instead of a filled disk (task2010).
///
/// `already_running == 0` keeps the exact threshold it had when one capture was
/// the only possibility, and `ui_state::lifecycle::disk_refusal` renders that
/// case with the exact Japanese wording it had then -- that path is a
/// regression test, not a design decision.
fn disk_guard_refusal(free_bytes: u64, already_running: usize) -> Option<DiskRefusal> {
    let required_bytes = ring_buffer::MIN_FREE_BYTES.saturating_add(
        ring_buffer::PER_CAPTURE_RESERVE_BYTES.saturating_mul(already_running as u64),
    );
    (free_bytes < required_bytes).then_some(DiskRefusal {
        required_bytes,
        free_bytes,
        already_running,
    })
}

impl CaptureController {
    /// Public because the slint bin lists and recovers sessions directly rather
    /// than through `#[tauri::command]` wrappers (task125).
    ///
    /// `LIVEBACK_BUFFER_ROOT` outranks the configured folder (task3910) -- see
    /// [`buffer_root_override`]. With the variable unset this is byte for byte
    /// the answer it always gave.
    pub fn buffer_root() -> PathBuf {
        let configured = BUFFER_ROOT
            .read()
            .ok()
            .and_then(|current| current.as_ref().cloned());
        resolve_buffer_root(
            buffer_root_override(),
            configured.as_deref(),
            local_app_data().as_deref(),
        )
    }

    /// Deliberately blind to the override: this is the *default*, and callers
    /// that want the effective root call [`Self::buffer_root`] (task3910).
    pub fn default_buffer_root() -> PathBuf {
        resolve_buffer_root(None, None, local_app_data().as_deref())
    }

    /// Points the buffer at a different folder (task164). Refused while
    /// recording: the capture worker holds a `RingBuffer` rooted at the old
    /// path and would keep writing there while everything else looked
    /// somewhere new -- and the catalog rebuild below would drop that very
    /// ring, which `sync_ring` needs to fold finalized segments into.
    /// Existing sessions are **not** moved -- only where the next recording is
    /// written, and where listing, editing and discarding resolve, changes.
    ///
    /// While `LIVEBACK_BUFFER_ROOT` is set the change is accepted and stored
    /// but never observed: [`Self::buffer_root`] keeps answering the override
    /// (task3910). That is the intended precedence, and it is why the settings
    /// screen's 「録画バッファの変更」 looks inert under an agent run.
    pub fn set_buffer_root(&self, root: Option<PathBuf>) -> Result<(), String> {
        if self.is_active() {
            return Err("the buffer folder cannot be changed while recording".into());
        }
        Self::apply_buffer_root(root)?;
        // Without this the catalog still holds rings opened from the old root,
        // so listing follows the setting while every edit writes behind it.
        self.reload_catalog_at(&Self::buffer_root())
    }

    /// The unguarded half, for startup: the persisted folder has to be in place
    /// before `new()` scans for closed sessions, which is before any controller
    /// exists to ask whether it is recording (nothing can be, yet).
    ///
    /// Same caveat as [`Self::set_buffer_root`]: while `LIVEBACK_BUFFER_ROOT`
    /// is set, what lands in the static here is stored and then outranked
    /// (task3910).
    pub fn apply_buffer_root(root: Option<PathBuf>) -> Result<(), String> {
        if let Some(root) = root.as_ref() {
            // Not while the override is up: the stored path is outranked and
            // nothing will be written there, so creating it would put an empty
            // folder on the user's disk for a run that was redirected away from
            // it on purpose (task3910's whole motive).
            if buffer_root_override().is_none() {
                std::fs::create_dir_all(root)
                    .map_err(|error| format!("could not create the buffer folder: {error}"))?;
            }
        }
        *BUFFER_ROOT
            .write()
            .map_err(|_| "the buffer setting lock is poisoned".to_owned())? = root;
        Ok(())
    }

    // `Default` mirrors `new` verbatim; it exists only because clippy asks for
    // it now that this type is reachable from outside the crate (task124).
    pub fn new() -> Self {
        Self::sweep_legacy_sessions(&Self::buffer_root());
        let (sessions, last_timeline) = Self::load_closed_sessions();
        Self {
            active: Arc::new(Mutex::new(HashMap::new())),
            next_start_seq: Arc::new(AtomicU64::new(1)),
            last_diagnostics: Arc::new(Mutex::new(None)),
            last_timeline: Arc::new(Mutex::new(last_timeline)),
            sink: Arc::new(Mutex::new(None)),
            sessions: Arc::new(Mutex::new(sessions)),
            review_leases: Arc::new(Mutex::new(HashMap::new())),
            next_review_lease: Arc::new(AtomicU64::new(1)),
            stopping: Arc::new(Mutex::new(HashSet::new())),
            lifecycle: Arc::new(Mutex::new(CaptureLifecycleStatus::idle())),
            held: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The id of the most recently started capture, which is what every
    /// no-argument getter answers for (task2000). While the cap was one this
    /// was simply "the one that is running" -- the four UI stop paths and the
    /// marker hotkey keep meaning what they meant because a UI that can only
    /// start one capture only ever has one to be last.
    ///
    /// Takes and releases `active` on its own so the caller can go on to lock
    /// `sessions` -- see the lock-order note on the field.
    fn last_started_session(&self) -> Option<String> {
        let active = self.active.lock().ok()?;
        Self::last_started_id(&active)
    }

    fn last_started_id(active: &HashMap<String, ActiveCapture>) -> Option<String> {
        active
            .iter()
            .max_by_key(|(_, entry)| entry.sequence)
            .map(|(session_id, _)| session_id.clone())
    }

    /// Every capture running right now, oldest start first (task2000). The
    /// order is the point: task2030's UI lists them as 「1本目/2本目」, and a
    /// `HashMap`'s own order would reshuffle that list on every poll.
    pub fn active_session_ids(&self) -> Vec<String> {
        self.stop_if_worker_ended();
        let Ok(active) = self.active.lock() else {
            return Vec::new();
        };
        let mut ids: Vec<_> = active
            .iter()
            .map(|(session_id, entry)| (entry.sequence, session_id.clone()))
            .collect();
        ids.sort_unstable();
        ids.into_iter().map(|(_, session_id)| session_id).collect()
    }

    /// The same list with each capture's target window handle beside its id,
    /// oldest start first (task2030). The picker matches its tiles on the
    /// handle: a tile id also carries the process id, which a restarted process
    /// changes, and the handle is what the entry actually keeps.
    pub fn active_targets(&self) -> Vec<(String, String)> {
        self.stop_if_worker_ended();
        let Ok(active) = self.active.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<_> = active
            .iter()
            .map(|(session_id, entry)| (entry.sequence, session_id.clone(), entry.target.0.clone()))
            .collect();
        rows.sort_unstable();
        rows.into_iter()
            .map(|(_, session_id, handle)| (session_id, handle))
            .collect()
    }

    /// The marker queue of the last-started capture, or `None` when nothing is
    /// recording -- in which case a marker has nowhere to go and is dropped,
    /// where before task1990 it sat in the shared queue until the next `start`
    /// cleared it. The hotkey refuses before it gets here anyway
    /// (`recording_position_100ns` answers `None` when idle).
    ///
    /// Last-started is the *fallback* resolution (task2190): it pairs with
    /// `recording_position_100ns`, which answers for that same capture, so the
    /// two together never file a marker at one recording's position into
    /// another recording's queue. When the review pane has a session on screen
    /// the hotkey names it instead and uses `pending_markers_for` with the
    /// matching `recording_position_100ns_for`.
    fn pending_markers(&self) -> Option<Arc<Mutex<Vec<ring_buffer::MarkerRecord>>>> {
        let active = self.active.lock().ok()?;
        Self::markers_of(&active, &Self::last_started_id(&active)?)
    }

    /// The same for a named capture (task2190), so the hotkey can queue into
    /// the session the user is looking at rather than the newest one.
    fn pending_markers_for(
        &self,
        session_id: &str,
    ) -> Option<Arc<Mutex<Vec<ring_buffer::MarkerRecord>>>> {
        let active = self.active.lock().ok()?;
        Self::markers_of(&active, session_id)
    }

    fn markers_of(
        active: &HashMap<String, ActiveCapture>,
        session_id: &str,
    ) -> Option<Arc<Mutex<Vec<ring_buffer::MarkerRecord>>>> {
        Some(active.get(session_id)?.pending_markers.clone())
    }

    /// The meta-edit queue of one running capture, by id. Only the recording
    /// session has one -- everything else edits its manifest where it stands.
    fn pending_meta(
        &self,
        session_id: &str,
    ) -> Option<Arc<Mutex<Vec<ring_buffer::container::MetaEvent>>>> {
        let active = self.active.lock().ok()?;
        Some(active.get(session_id)?.pending_meta.clone())
    }

    /// Fits every running recording of the target `key` names to its changed
    /// audio registration (t261002-577c, decisions 6 and 7): a sound switched
    /// on becomes a new track from the next segment, one switched off records
    /// silence on the track it already has. Returns how many recordings it
    /// reached. The settings are the caller's to save first -- this only
    /// carries the change to recordings already running.
    pub fn apply_target_audio(
        &self,
        key: &str,
        target: Option<&crate::settings::TargetAudio>,
    ) -> usize {
        let Ok(mut active) = self.active.lock() else {
            return 0;
        };
        let mut reached = 0;
        for (session_id, entry) in active.iter_mut() {
            let audio = &mut entry.audio;
            if audio.key.as_deref() != Some(key) {
                continue;
            }
            let desired = audio::planned_extras(target, audio.capture_all_audio, audio.monitor);
            let before = audio.extras.clone();
            let recording = audio::merge_extras(&mut audio.extras, &desired);
            // A microphone moved to another device keeps its track: rename it.
            let mut renamed = false;
            for (index, (was, now)) in before.iter().zip(&audio.extras).enumerate() {
                if was != now {
                    if let (Some(slot), Some(info)) = (
                        audio.tracks.get_mut(1 + index),
                        extra_track_infos(std::slice::from_ref(now)).pop(),
                    ) {
                        *slot = info;
                        renamed = true;
                    }
                }
            }
            let before = before.len();
            if audio.extras.len() > before || renamed {
                if audio.tracks.is_empty() {
                    // The first extra track of a recording that started
                    // without one: name the target's track too.
                    audio.tracks.push(ring_buffer::container::AudioTrackInfo {
                        executable_name: audio.target_name.clone(),
                        microphone: None,
                    });
                }
                audio
                    .tracks
                    .extend(extra_track_infos(&audio.extras[before..]));
                // Lock order: `active` (held) before `sessions`.
                let event = self.sessions.lock().ok().and_then(|mut sessions| {
                    sessions
                        .get_mut(session_id)
                        .map(|ring| ring.stage_audio_tracks(audio.tracks.clone()))
                });
                if let (Some(event), Ok(mut queue)) = (event, entry.pending_meta.lock()) {
                    queue.push(event);
                }
            }
            tracing::info!(
                event = "extra_audio_tracks_applied",
                session_id,
                tracks = ?audio.extras,
                recording = ?recording,
                "the target's added sounds changed mid-recording"
            );
            let _ = entry.session.extra_audio.send(worker::ExtraAudioUpdate {
                tracks: audio.extras.clone(),
                active: recording,
            });
            reached += 1;
        }
        reached
    }

    pub fn set_event_sink(&self, sink: Arc<dyn EventSink>) {
        if let Ok(mut current) = self.sink.lock() {
            *current = Some(sink);
        }
    }

    // Known limitation: this only runs when something calls stop_blocking/
    // stop_blocking_with_reason. An autonomous stop (target closed, etc.) is only
    // detected lazily via stop_if_worker_ended, which fires on the next
    // is_active()/lifecycle_status() call (frontend polls every 1s while a
    // window is open). If the window is hidden and nothing polls, the tray
    // tooltip can stay "録画中" until the next such call. Accepted for v1.
    ///
    /// `running` / `title` are passed in rather than read from `active` here
    /// (task2030, round9-response §1-6: the tooltip carries the count now, and
    /// names the target when there is exactly one). `start` calls this while it
    /// still holds the `active` guard, so a read inside would deadlock on the
    /// non-reentrant mutex.
    fn update_tray_tooltip(&self, running: usize, title: Option<&str>) {
        if let Ok(sink) = self.sink.lock() {
            if let Some(sink) = sink.as_ref() {
                sink.set_tray_tooltip(&crate::ui_state::lifecycle::tray_tooltip(
                    crate::ui_state::locale::active(),
                    running,
                    title,
                ));
            }
        }
    }

    /// The count and, when there is exactly one, the name for the tray tooltip.
    /// Takes the `active` guard, so never call it while holding one.
    fn tray_tooltip_state(&self) -> (usize, Option<String>) {
        let Ok(active) = self.active.lock() else {
            return (0, None);
        };
        (active.len(), Self::only_target_title(&active))
    }

    /// The one running capture's name, or `None` when it is not one capture --
    /// two or more report a count instead, because a tooltip is one short line.
    fn only_target_title(active: &HashMap<String, ActiveCapture>) -> Option<String> {
        if active.len() != 1 {
            return None;
        }
        active.values().next()?.target_title.clone()
    }

    /// Starts one more capture. **Nothing here refuses a start for being the
    /// Nth one** -- task2080 deleted the fixed cap of two that task2000 had put
    /// here.
    ///
    /// The cap was a constant, never a setting, so that raising it would mean
    /// "measure first" rather than "a user turned a knob nobody tested". Task2070
    /// did the measuring, K=1..6 real captures at 1080p60 for 60s
    /// (`.agents/tasks/evidence/2070-concurrency-threshold/`, all figures from
    /// `arrived=` -- never `measured_fps`, which double-counts held frames):
    ///
    /// - The slowest capture reads 51.65 fps at K=1, 50.89 at K=2, 49.71 at K=3,
    ///   48.79 at K=4, 46.44 at K=6. The mean falls linearly with it, -1.4%
    ///   (K=2) / -4.8% (K=4) / -8.9% (K=6) against K=1, and there is no cliff
    ///   anywhere in the range.
    /// - `queue_full` and `dropped_frames` are **0 at every K**. The first
    ///   counter to move at all is `throttled` at K=4 (~3% of frames); K<=3 is
    ///   clean. Fairness (slowest/fastest) never falls below 0.97.
    /// - Over 10 minutes there is no decay: K=3 reads 50.65 / 50.61 / 50.03,
    ///   *better* than the same K over 60s.
    /// - CPU rises linearly, ~0.15 cores at K=1 to ~0.75 at K=6.
    ///
    /// So no count in the measured range is a breaking point, which is why there
    /// is no number to refuse on. K=3 is only where the slowest capture first
    /// reaches the 50 fps line -- that is the warning threshold N task2030 shows
    /// the user, a "this may cost you frames, continue?" and not a limit.
    ///
    /// What still refuses a start is the disk guard (task2010, its reserve
    /// scales with `already_running`) -- the only count-aware check left, now
    /// that task2110 dropped task2020's audio combination rule as well: any
    /// audio setting may start beside any other, because "record other
    /// applications too" is a system-wide opt-in the user made themselves and
    /// another recording's target leaking into it is what that setting *is*.
    /// The driver wall is real but far
    /// past N -- task2070 saw the **13th** encode session machine-wide refused,
    /// 12 running -- and is left to surface as `create_with_codec` failing onto
    /// the existing capture-failure banner, because which count hits it is
    /// machine-dependent and no constant here could predict it.
    /// Creates the recording's `.lvb` and stamps its title into it.
    ///
    /// Created here rather than on the index thread so a failure to make the
    /// file is reported to the user as "recording did not start", instead of
    /// surfacing later as a recording that silently indexes nothing.
    ///
    /// Task560: the title has to go *into the file*, not just into the
    /// in-memory manifest. A directory session kept it in `manifest.json`; a
    /// container's only channel is a `TitleSet` meta record, and until this
    /// existed the sole writer of one was a user rename -- so every recording
    /// came back nameless after a restart, which showed up as timestamps in the
    /// history list and as 「ゲーム名がありません」 killing every export (found
    /// verifying task240). Written before the first segment, so a crash
    /// recovers a named session: `recover` replays meta records in order, and
    /// this is the first one. Normalized through the same helper the rename
    /// path uses, so a title can never mean two different things depending on
    /// how it arrived.
    fn create_container_writer(
        container_path: &Path,
        session_id: &str,
        retention_minutes: u16,
        target_title: Option<&str>,
        target_executable: Option<&str>,
        target_executable_path: Option<&str>,
    ) -> Result<ring_buffer::container::ContainerWriter, String> {
        let mut writer = ring_buffer::container::ContainerWriter::create(
            container_path,
            session_id,
            retention_minutes,
        )
        .map_err(|error| format!("container initialization failed: {error}"))?;
        if let Some(title) = target_title.and_then(ring_buffer::normalize_target_title) {
            writer
                .append_meta(&ring_buffer::container::MetaEvent::TitleSet { title: Some(title) })
                .map_err(|error| format!("container initialization failed: {error}"))?;
        }
        // Task2350: same shape one record down -- absent rather than `None` when
        // there is nothing to say, so a monitor recording reads exactly like
        // every container written before this record existed.
        if let Some(executable) = target_executable {
            writer
                .append_meta(&ring_buffer::container::MetaEvent::TargetExecutableSet {
                    executable: Some(executable.to_owned()),
                    path: target_executable_path.map(str::to_owned),
                })
                .map_err(|error| format!("container initialization failed: {error}"))?;
        }
        Ok(writer)
    }

    pub fn start(&self, config: CaptureConfig) -> Result<(), StartError> {
        self.start_armed(config, false)
    }

    /// `start`, for a target that is armed -- an auto-capture rule watches its
    /// executable, or the replay buffer is on (t260929-ea5e, the user's
    /// 2026-09-29 ruling C). The WGC session then stays open after the stop,
    /// so the next recording of the same target does not open another one:
    /// WGC leaves ≈4.3 B/pixel of dedicated GPU memory behind per session it
    /// closes (task4280). Unarmed recordings open and close their own, as
    /// before. Arming is read here, once: a target disarmed while held keeps
    /// its session until it closes, is recorded again, or the app exits.
    pub fn start_armed(&self, config: CaptureConfig, armed: bool) -> Result<(), StartError> {
        let target_handle = config.window_handle.clone();
        let target_kind = config.kind;
        let mut active = self
            .active
            .lock()
            .map_err(|_| StartError::Internal("the capture state lock is poisoned".to_owned()))?;
        // No "a stop is in flight" gate here since task2170. It was process-wide
        // and refused a start of *anything* while any capture was being joined;
        // `start` mints a fresh session id every time, so it can never collide
        // with one that is stopping.
        if let Ok(mut last_diagnostics) = self.last_diagnostics.lock() {
            *last_diagnostics = None;
        }
        if let Ok(mut timeline) = self.last_timeline.lock() {
            *timeline = None;
        }
        // Gated per codec since task1760. A settings file hand-edited to `av1`
        // on a machine without the decoder has to fail here, with a reason on
        // the banner -- recording something this app cannot play back is the
        // one outcome the opt-in exists to prevent.
        encoder::ensure_recording_codec(config.codec.into()).map_err(|error| {
            StartError::Codec(format!(
                "recording codec unavailable: {}",
                error.diagnostics
            ))
        })?;
        let output_dir = config.encoder_output_dir.clone();
        let session_id = output_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // Structural, not a limit (task1990). With no count cap left (task2080)
        // this is the check that actually fires for a reused output dir, and it
        // has to sit *before* `ContainerWriter::create` below, which truncates an
        // existing file back to its last checkpoint and would cut the running
        // recording in half -- which is what "two rings on one `.lvb`" would mean.
        // Ids are `default_encoder_output_dir()`'s nanosecond hex, so reaching
        // this means a caller reused an output dir, which is worth a log line.
        if active.contains_key(&session_id) {
            tracing::error!(
                event = "duplicate_capture_session",
                session_id,
                "a capture with this session id is already running"
            );
            return Err(StartError::Internal("a capture is already running".into()));
        }
        // `active.len()` is the same count the cap check above used, read under
        // the same guard: an ended-but-unreaped worker still counts for both.
        // Deliberate -- one number, one staleness story (task2010).
        ensure_buffer_root_has_space(&output_dir, active.len())?;
        // The worker has no settings in reach, so it reads the language the
        // UI last set (task1150). Only a *monitor* title is generated at all;
        // a window's comes from the window itself.
        let target_title =
            targets::title_for_handle(crate::ui_state::locale::active(), &config.window_handle);
        // Task2350: read from the same live handle, at the same moment. `None`
        // for a monitor -- an HMONITOR has no process behind it, and auto-capture
        // only ever matches window targets anyway.
        let target_executable = targets::executable_name_for_handle(&config.window_handle);
        // Its full image path, for the review title bar's icon once the app
        // has exited (2026-09-20).
        let target_executable_path = targets::executable_path_for_handle(&config.window_handle);
        let retention_minutes = config.retention_minutes;
        let monitor = target_kind == targets::CaptureTargetKind::Monitor;
        let mut audio = SessionAudio {
            key: crate::settings::target_audio_key(target_executable.as_deref(), monitor),
            capture_all_audio: config.capture_all_audio,
            monitor,
            extras: config.extra_audio.clone(),
            tracks: Vec::new(),
            target_name: target_executable.clone().unwrap_or_default(),
        };
        // **The switch (task450).** One recording is now one file. The path is
        // the directory path plus `.lvb`, so the session id (the stem) and the
        // "id is a timestamp, lexical order is chronological" property both
        // survive untouched -- see task401's naming contract.
        let container_path = output_dir.with_extension(ring_buffer::CONTAINER_EXTENSION);
        let mut config = config;
        config.container_path = Some(container_path.clone());
        // Unbounded on purpose (task430): the sender is the capture thread, and
        // a bounded channel would make it wait on a manifest write -- the shape
        // that put a 250ms hole in the recording when a thumbnail write landed
        // there. Dropping instead of waiting is worse still: that is exactly the
        // "on disk but not in the manifest" state this task exists to remove.
        // The payload is one small struct per 2s segment.
        let (finalized_segments, segments) = crossbeam_channel::unbounded();
        // One stop channel per recording (task1990), so the reason a worker
        // reports can never be attributed to a different capture.
        let (stopped, stop_events) = bounded(4);
        let held = self
            .held
            .lock()
            .ok()
            .and_then(|mut held| held.remove(&target_handle));
        let session = CaptureSession::start_on(config, stopped, finalized_segments, armed, held)
            .map_err(StartError::Internal)?;
        let mut writer = Self::create_container_writer(
            &container_path,
            &session_id,
            retention_minutes,
            target_title.as_deref(),
            target_executable.as_deref(),
            target_executable_path.as_deref(),
        )
        .map_err(StartError::BufferFolder)?;
        // The track list goes in only when there is more than the target's
        // track (task1260's rule, kept by t261002-577c): a single-track
        // container stays byte-for-byte what every recording without added
        // sounds has always been. A sound switched on later appends a new
        // list through `apply_target_audio`.
        if !audio.extras.is_empty() {
            audio.tracks = std::iter::once(ring_buffer::container::AudioTrackInfo {
                executable_name: audio.target_name.clone(),
                microphone: None,
            })
            .chain(extra_track_infos(&audio.extras))
            .collect();
            writer
                .append_meta(&ring_buffer::container::MetaEvent::AudioTracksSet {
                    tracks: audio.tracks.clone(),
                })
                .map_err(|error| {
                    StartError::BufferFolder(format!("container initialization failed: {error}"))
                })?;
        }
        let mut ring = ring_buffer::RingBuffer::create_container(
            container_path,
            session_id.clone(),
            retention_minutes,
            target_title.clone(),
            // The second wire (task560's lesson): without this the live session's
            // manifest has no executable and the review screen only sees it after
            // a close-and-reopen.
            target_executable,
        )
        .with_target_executable_path(target_executable_path);
        if !audio.tracks.is_empty() {
            // The file already has it (above); this is the live manifest's copy.
            ring.stage_audio_tracks(audio.tracks.clone());
        }
        // No `clear()` of the marker/meta queues any more (task1990): they are
        // this entry's own, created empty right here, so a marker pressed just
        // before a previous capture died can no longer be waiting in them to be
        // merged at a nonsensical position in this one.
        let pending_markers = Arc::new(Mutex::new(Vec::new()));
        let pending_meta = Arc::new(Mutex::new(Vec::new()));
        self.sessions
            .lock()
            .map_err(|_| StartError::Internal("the session catalog lock is poisoned".to_owned()))?
            .insert(session_id.clone(), ring);
        // Spawned only once the ring it writes into is in the catalog, so the
        // first segment can never arrive before there is somewhere to put it.
        let writer = indexer::SegmentIndexer {
            session_id: session_id.clone(),
            sessions: self.sessions.clone(),
            pending_markers: pending_markers.clone(),
            pending_meta: pending_meta.clone(),
            last_timeline: self.last_timeline.clone(),
            writer: Some(writer),
            pending_thumbnails: Default::default(),
        }
        .spawn(segments)
        .map_err(|_| StartError::Internal("could not start the index writer thread".to_owned()))?;
        active.insert(
            session_id,
            ActiveCapture {
                sequence: self.next_start_seq.fetch_add(1, Ordering::Relaxed),
                session,
                target: (target_handle, target_kind),
                target_title,
                pending_markers,
                pending_meta,
                indexer: Some(writer),
                stop_events,
                audio,
            },
        );
        // Read off the guard that is still held, and released before the sink
        // is touched: `update_tray_tooltip` must not take `active` itself.
        let (running, title) = (active.len(), Self::only_target_title(&active));
        drop(active);
        if let Ok(mut lifecycle) = self.lifecycle.lock() {
            *lifecycle = CaptureLifecycleStatus::recording();
        }
        self.update_tray_tooltip(running, title.as_deref());
        Ok(())
    }

    /// Stops **every** running capture (task2000). Identical to what it always
    /// did while only one could run, which is why the three UI stop paths (the
    /// picker button, the LIVE chip, the tray menu) still call it unchanged: the
    /// stop the user reaches for means "stop recording", not "stop one of the
    /// recordings", until a UI exists that can tell them apart (task2030).
    /// Since task2170 this is the ids of `active` fanned out one `stop_session_async`
    /// each, rather than one thread draining the map: every capture then goes
    /// through `settle_lifecycle`, so the tray counts down 3本→2本→1本→待機中
    /// instead of jumping straight to idle (task2030's tooltip).
    ///
    /// One semantic difference from the old drain: a capture started *while*
    /// this is stopping survives, because the ids were listed up front. The only
    /// caller is the tray's "stop everything", where stopping what was running
    /// when it was pressed is the honest reading.
    pub fn stop_async(&self) -> CaptureStopReason {
        for session_id in self.active_session_ids() {
            self.stop_session_async(&session_id);
        }
        CaptureStopReason::Requested
    }

    /// Stops and closes everything still recording, waiting at most `timeout`.
    /// `true` when the shutdown landed inside it; `false` on the timeout.
    ///
    /// **This is what writes `Closed` on the way out** (task3200, observed
    /// 2026-09-06). `MetaEvent::Closed` plus the final checkpoint are written by
    /// `Indexer::finish` alone, which only runs once the capture worker has been
    /// joined and the segment channel is `Disconnected` -- i.e. only from
    /// `finish_capture`. The tray's 「終了」 used to call `slint::quit_event_loop()`
    /// and nothing else, so the process died with its workers and the `.lvb` was
    /// left `closed=false`: the *graceful* exit left a dirtier container than
    /// `taskkill /F`, whose mess at least gets repaired on the next launch.
    ///
    /// The wait is bounded because tearing a 12GB-class session down is not
    /// instant and a quit the user cannot see progress on must not hang: past
    /// the limit the caller logs and exits anyway. That is not a loss -- a
    /// container left open is still `Recoverable`, and the next launch's
    /// `repair_sessions` -> `container::recover` handles it exactly as it does
    /// today. Bounded waiting only makes the good case cheap, it does not make
    /// the bad case worse. Ten seconds is the same order as the installer's
    /// `20 x Sleep 500`.
    ///
    /// Waits for stops already in flight too, not just what is still in the
    /// `active` map: `stop_session_async` (the tray's 「キャプチャ停止」) and
    /// `stop_if_worker_ended` both take the entry out of the map *first* and
    /// close it on their own thread, so 「停止」 followed straight away by
    /// 「終了」 would otherwise find an empty map, return instantly, and lose the
    /// same `Closed` record by a different route.
    ///
    /// Idle is the fast path by construction: `stop_blocking` finds an empty map
    /// and `is_stopping()` is false, so this returns in microseconds. The worker
    /// thread is deliberately left detached on the timeout -- the process is
    /// about to exit, and whatever it manages to flush before it does is a gain.
    pub fn shutdown(&self, timeout: Duration) -> bool {
        let (done, finished) = bounded(1);
        let controller = self.clone();
        thread::spawn(move || {
            let _ = controller.stop_blocking();
            // Polled rather than signalled: the in-flight stops belong to
            // threads spawned elsewhere, and `stopping` is the only thing that
            // already tracks them.
            while controller.is_stopping() {
                thread::sleep(Duration::from_millis(10));
            }
            let _ = done.send(());
        });
        finished.recv_timeout(timeout).is_ok()
    }

    /// Stops one named capture and leaves the others recording (task2000).
    ///
    /// Claims *this* session id in the `stopping` set (task2170), so a stop of
    /// another capture no longer swallows this one. Pressing stop twice on the
    /// same capture is still a no-op the second time -- it is the same stop, not
    /// a queued second one -- and the claim happens on the calling thread, so
    /// `is_stopping()` is true by the time this returns.
    pub fn stop_session_async(&self, session_id: &str) -> CaptureStopReason {
        if !self.claim_stopping(session_id) {
            return CaptureStopReason::Requested;
        }
        let controller = self.clone();
        let session_id = session_id.to_owned();
        thread::spawn(move || {
            let _ = controller.stop_session_blocking(&session_id);
            controller.release_stopping(&session_id);
        });
        CaptureStopReason::Requested
    }

    /// `true` when this call is the one that claimed the stop of `session_id`;
    /// `false` when a stop of it is already in flight. Holds the leaf lock for
    /// the insert alone -- see the field's lock-order note.
    fn claim_stopping(&self, session_id: &str) -> bool {
        match self.stopping.lock() {
            Ok(mut stopping) => stopping.insert(session_id.to_owned()),
            // A poisoned set would otherwise refuse every stop forever.
            Err(_) => true,
        }
    }

    fn release_stopping(&self, session_id: &str) {
        if let Ok(mut stopping) = self.stopping.lock() {
            stopping.remove(session_id);
        }
    }

    /// Takes one entry out of the map, by id. `Err` is the poisoned lock.
    ///
    /// Removal is the only thing done under the lock: `session.stop()` joins the
    /// capture worker thread, which can take an unbounded amount of time to
    /// flush/finalize on a long, multi-gap recording, and
    /// `capture_diagnostics()`/`is_active()`/`recording_position_100ns()` all
    /// lock this same mutex and must not be blocked for the entire join (Task
    /// 048, found while investigating Task 047's UI-hang report).
    fn take_active(&self, session_id: &str) -> Result<Option<ActiveCapture>, ()> {
        Ok(self.active.lock().map_err(|_| ())?.remove(session_id))
    }

    /// The same for whichever capture comes to hand, which is how "stop
    /// everything" drains the map. One lock acquisition: picking the id and
    /// removing it separately would let a stop land in between and take the
    /// entry a second time.
    fn take_any_active(&self) -> Result<Option<(String, ActiveCapture)>, ()> {
        let mut active = self.active.lock().map_err(|_| ())?;
        let Some(session_id) = active.keys().next().cloned() else {
            return Ok(None);
        };
        Ok(active.remove(&session_id).map(|entry| (session_id, entry)))
    }

    /// What the lifecycle and the tray say once a capture has been finished.
    ///
    /// **The lifecycle is always written.** Task2000 skipped it too while
    /// another capture was still running, and that killed the only path a
    /// failure has to the user: `lifecycle` is read by the toast and nothing
    /// else (`picker.rs`'s 1s poll -> `auto_stop_toast`; the LIVE chip, the
    /// recording indicator and the auto-capture gate all read `is_active()` /
    /// `is_stopping()` instead). So a capture that died beside a running one
    /// went down in silence -- no banner, no recording (task2070 hit exactly
    /// that on the NVENC wall, and which capture dies is a race).
    ///
    /// The tray used to be guarded on "is anything else still running", so that
    /// 「待機中」 could not appear while frames were still being written. Since
    /// task2030 the tooltip carries the count, so it is simply rewritten from
    /// what is left: one capture of three stopping now reads 「2本を録画中」
    /// rather than staying frozen on the old text.
    fn settle_lifecycle(&self, reason: CaptureStopReason) {
        let (running, title) = self.tray_tooltip_state();
        if let Ok(mut lifecycle) = self.lifecycle.lock() {
            *lifecycle = CaptureLifecycleStatus::stopped(reason);
            // Task3630: the head of the stop-report timeline. This runs on the
            // spawned stop thread, so it lands after the finalize the UI poll
            // is waiting on -- the gap between this timestamp and the
            // `stop_report` line the poll writes is the detection lag the
            // symptom has to be measured against, and neither existed in the
            // log before. Inside the lock so the time recorded is the time the
            // status the poll reads became visible.
            tracing::info!(
                target: "task3630_stop_report",
                event = "lifecycle_settled",
                code = ?lifecycle.diagnostic_code,
                seq = lifecycle.seq,
                still_running = running,
                "a capture stop settled its lifecycle"
            );
        }
        self.update_tray_tooltip(running, title.as_deref());
    }

    /// Joins and closes one entry already taken out of the map: worker, index
    /// writer, ring. `reason` is `None` when the caller wants whatever the
    /// worker itself reported on this entry's channel.
    fn finish_capture(
        &self,
        session_id: &str,
        mut entry: ActiveCapture,
        reason: Option<CaptureStopReason>,
    ) -> CaptureStopReason {
        if let Some(held) = entry.session.stop_holding() {
            if let Ok(mut held_workers) = self.held.lock() {
                held_workers.insert(entry.target.0.clone(), held);
            }
        }
        let mut diagnostics = entry.session.diagnostics();
        // Always after the capture worker's join -- which drops the only sender
        // and so ends the writer's loop -- and before `ring.close()`, so the
        // manifest a stopped session is closed with already has its last
        // segment. Under no lock: the writer takes `sessions` itself, and
        // task048's rule is that a join never happens underneath one.
        if let Some(indexer) = entry.indexer.take() {
            let _ = indexer.join();
        }
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(ring) = sessions.get_mut(session_id) {
                match ring.close() {
                    Ok(manifest) => {
                        if let Ok(mut timeline) = self.last_timeline.lock() {
                            *timeline = Some(manifest);
                        }
                    }
                    Err(error) => diagnostics
                        .encoder_errors
                        .push(format!("ring buffer close failed: {error}")),
                }
            }
        }
        if let Ok(mut last_diagnostics) = self.last_diagnostics.lock() {
            *last_diagnostics = Some(diagnostics);
        }
        reason.unwrap_or_else(|| {
            entry
                .stop_events
                .try_recv()
                .unwrap_or(CaptureStopReason::Requested)
        })
    }

    /// Finishes every running capture, one at a time (task2000). Each join
    /// happens outside the lock (task048), so the map is re-locked per entry
    /// rather than drained in one go -- and a `start` racing in between is
    /// picked up by the next pass, since this loops until the map is empty.
    /// `switch` is the only caller left (task2170 rebuilt `stop_async` on
    /// `stop_session_async`).
    ///
    /// The reason returned is the last capture's. With one running -- every UI
    /// path today -- that is the same value it always returned; nothing calls
    /// this expecting a reason *per* capture, and `CaptureStopReason` has no
    /// room to carry more than one.
    fn stop_blocking(&self) -> CaptureStopReason {
        let mut stopped = None;
        loop {
            match self.take_any_active() {
                Ok(Some((session_id, entry))) => {
                    stopped = Some(self.finish_capture(&session_id, entry, None));
                }
                Ok(None) => break,
                Err(()) => {
                    return CaptureStopReason::InitializationFailed {
                        stage: "controller_lock",
                    }
                }
            }
        }
        // Nothing was recording: leave the lifecycle and the tray alone, so
        // `switch` on an idle controller does not announce a stop that never
        // happened.
        let Some(reason) = stopped else {
            return CaptureStopReason::Requested;
        };
        self.settle_lifecycle(reason);
        reason
    }

    /// Finishes one named capture, leaving the rest running.
    fn stop_session_blocking(&self, session_id: &str) -> CaptureStopReason {
        match self.take_active(session_id) {
            Ok(Some(entry)) => {
                let reason = self.finish_capture(session_id, entry, None);
                self.settle_lifecycle(reason);
                reason
            }
            Ok(None) => CaptureStopReason::Requested,
            Err(()) => CaptureStopReason::InitializationFailed {
                stage: "controller_lock",
            },
        }
    }

    /// Whether the window being captured is minimized right now (round7 §2-7).
    /// A monitor cannot be, and neither can a capture that is not running.
    /// Answers for the last-started capture; `target_minimized_for` names one.
    pub fn target_minimized(&self) -> bool {
        self.stop_if_worker_ended();
        self.last_started_session()
            .is_some_and(|session_id| self.target_minimized_for(&session_id))
    }

    pub fn target_minimized_for(&self, session_id: &str) -> bool {
        let Ok(active) = self.active.lock() else {
            return false;
        };
        match active.get(session_id).map(|entry| &entry.target) {
            Some((handle, targets::CaptureTargetKind::Window)) => {
                targets::window_is_minimized(handle)
            }
            _ => false,
        }
    }

    pub fn is_active(&self) -> bool {
        self.stop_if_worker_ended();
        self.active.lock().is_ok_and(|active| !active.is_empty())
    }

    /// **Any** capture's stop is in flight: its worker is still being joined and
    /// flushed, so `is_active()` can still be true. Process-wide on purpose --
    /// `picker.rs`'s auto-capture gate wants "is anything mid-stop" (task2170
    /// kept the answer while splitting the set behind it). The live chip stopped
    /// reading it in task2030 precisely because it is not per session.
    pub fn is_stopping(&self) -> bool {
        self.stopping
            .lock()
            .is_ok_and(|stopping| !stopping.is_empty())
    }

    fn stop_if_worker_ended(&self) {
        // Each entry's own channel, since `CaptureStopReason` does not say which
        // capture it came from (task1990). Read and released before anything
        // else is taken -- the entry itself is removed by the spawned stop.
        let ended = match self.active.lock() {
            Ok(active) => active.iter().find_map(|(session_id, entry)| {
                entry
                    .stop_events
                    .try_recv()
                    .ok()
                    .map(|reason| (session_id.clone(), reason))
            }),
            Err(_) => return,
        };
        let Some((session_id, reason)) = ended else {
            return;
        };
        // Claimed per id since task2170: a user-pressed stop of *another*
        // capture used to make this whole poll return early, so a capture that
        // died on its own was left in the map until that stop finished. Losing
        // the claim means this id is already being stopped; the next poll picks
        // up whatever is left.
        if !self.claim_stopping(&session_id) {
            return;
        }
        let controller = self.clone();
        thread::spawn(move || {
            let result = controller.stop_blocking_with_reason(&session_id, reason);
            // Only if that was the last one (task2000): the other capture is
            // still recording and the tray must not claim otherwise.
            controller.settle_lifecycle(result);
            controller.release_stopping(&session_id);
        });
    }

    fn stop_blocking_with_reason(
        &self,
        session_id: &str,
        reason: CaptureStopReason,
    ) -> CaptureStopReason {
        match self.take_active(session_id) {
            Ok(Some(entry)) => self.finish_capture(session_id, entry, Some(reason)),
            Ok(None) => reason,
            Err(()) => CaptureStopReason::InitializationFailed {
                stage: "controller_lock",
            },
        }
    }

    pub fn lifecycle_status(&self) -> CaptureLifecycleStatus {
        self.stop_if_worker_ended();
        self.lifecycle
            .lock()
            .map(|value| value.clone())
            .unwrap_or_else(|_| {
                // `seq: 0` on purpose: a poisoned lock is one broken state the
                // poll keeps re-reading, not a new stop event every second, and
                // a fresh `seq` per poll would toast forever (task2090).
                CaptureLifecycleStatus {
                    seq: 0,
                    ..CaptureLifecycleStatus::stopped(CaptureStopReason::InitializationFailed {
                        stage: "controller_lock",
                    })
                }
            })
    }

    /// The last-started capture's counters, or -- when nothing is recording --
    /// the ones the previous recording ended with, which is what keeps the
    /// banner populated after a stop. `diagnostics_for` names a capture and has
    /// no such fallback: an id that is not running has no live counters, and
    /// answering with an unrelated recording's would be worse than `None`.
    pub fn diagnostics(&self) -> Option<CaptureDiagnostics> {
        self.stop_if_worker_ended();
        self.last_started_session()
            .and_then(|session_id| self.diagnostics_for(&session_id))
            .or_else(|| {
                self.last_diagnostics
                    .lock()
                    .ok()
                    .and_then(|last| last.clone())
            })
    }

    /// Every capture running right now, oldest start first: its id, what to call
    /// it, and whether its audio trial failed (task2140).
    ///
    /// `diagnostics()` above answers for the last-started capture alone, which
    /// is exactly wrong for this: a second recording that came out mute is a
    /// second recording nobody would be told about. The pump is the same one
    /// `diagnostics()` does -- draining every running session's event channel
    /// rather than only the newest is the point.
    pub fn audio_status(&self) -> Vec<(String, Option<String>, bool)> {
        self.stop_if_worker_ended();
        let Ok(mut active) = self.active.lock() else {
            return Vec::new();
        };
        let mut rows: Vec<_> = active
            .iter_mut()
            .map(|(session_id, entry)| {
                (
                    entry.sequence,
                    session_id.clone(),
                    entry.target_title.clone(),
                    entry.session.diagnostics().audio_unavailable,
                )
            })
            .collect();
        // Same order as `active_session_ids`, and for the same reason: a
        // `HashMap`'s own order would reshuffle the names in the toast.
        rows.sort_unstable_by_key(|(sequence, ..)| *sequence);
        rows.into_iter()
            .map(|(_, session_id, title, mute)| (session_id, title, mute))
            .collect()
    }

    pub fn diagnostics_for(&self, session_id: &str) -> Option<CaptureDiagnostics> {
        // Reads only, since task430: the index writer folds finalized segments
        // and markers into the manifest on its own thread, so polling for
        // diagnostics no longer writes to the session it is reporting on.
        let mut active = self.active.lock().ok()?;
        active
            .get_mut(session_id)
            .map(|entry| entry.session.diagnostics())
    }

    pub fn timeline(&self) -> Option<ring_buffer::SessionManifest> {
        self.last_timeline
            .lock()
            .ok()
            .and_then(|timeline| timeline.clone())
    }

    /// One named session's manifest, straight from the catalog (task2030).
    ///
    /// `timeline()` above answers with `last_timeline`, which every running
    /// capture's index writer overwrites with its own -- so with two recordings
    /// it flips between them, and the review screen following the live edge of
    /// the one it is showing would only be fed on the ticks that happened to
    /// land on the right session. This asks for the session by name instead.
    pub fn timeline_for(&self, session_id: &str) -> Option<ring_buffer::SessionManifest> {
        Some(
            self.sessions
                .lock()
                .ok()?
                .get(session_id)?
                .manifest()
                .clone(),
        )
    }

    /// `timeline_for` plus when the recorder last folded a segment into it
    /// (t260929-e171, `RingBuffer::last_fold`), read under the one lock so the
    /// stamp belongs to the manifest's last segment and not a later one.
    pub fn timeline_and_fold_for(
        &self,
        session_id: &str,
    ) -> Option<(ring_buffer::SessionManifest, Option<std::time::Instant>)> {
        let sessions = self.sessions.lock().ok()?;
        let ring = sessions.get(session_id)?;
        Some((ring.manifest().clone(), ring.last_fold()))
    }

    /// How far one named session's manifest reaches, without cloning it
    /// (t260913-784a): the segment count and the last segment's end. The live
    /// edge poll asks this ten times a second and only takes `timeline_for`'s
    /// full copy when one of the two moved.
    pub fn timeline_extent(&self, session_id: &str) -> Option<(usize, Option<i64>)> {
        let sessions = self.sessions.lock().ok()?;
        let segments = &sessions.get(session_id)?.manifest().segments;
        Some((segments.len(), segments.last().map(|s| s.end_100ns)))
    }

    /// What a clip export can currently cover: the recording session's id and
    /// title plus the finalized-segment span `[oldest_start, last_end)`.
    /// `None` means "not recording"; a recording whose first segment hasn't
    /// finalized yet returns `Some` with a zero-width span, so the caller can
    /// tell those two apart (task097).
    pub fn latest_finalized_range(&self) -> Option<FinalizedRange> {
        // No fold-first call any more (task430): the index writer folds each
        // segment as it finalizes, on its own thread, so the manifest read below
        // is current whether or not anything is polling. It used to have to call
        // `diagnostics()` here for `sync_ring`'s side effect, because the
        // frontend's 500ms poll is throttled while the window is hidden -- which
        // is exactly when the clip hotkey is used.
        // The id first, with `active` released before `sessions` is taken --
        // the lock order the field's comment fixes.
        self.latest_finalized_range_for(&self.last_started_session()?)
    }

    /// Whether `session_id` is the last-started capture -- the question the
    /// review transport's `refresh_live` asks at every segment crossing.
    ///
    /// `active` only (t260924-327f). It used to be `latest_finalized_range`
    /// compared by id, which builds a whole span out of the manifest under
    /// `sessions` just to read the id back -- and the index writer holds
    /// `sessions` across its container appends: on 2026-09-24 a crossing sat
    /// 9s behind `index pass locked_ms=9629 append_ms=7982` and resumed 2ms
    /// after it. The id is decided by `active` alone; a ring is put in
    /// `sessions` before its entry goes into `active` (`start`), so no answer
    /// changes.
    pub fn is_last_started(&self, session_id: &str) -> bool {
        self.last_started_session().as_deref() == Some(session_id)
    }

    /// The same for a named capture (task2000). `None` for an id that is not
    /// recording, so the clip hotkey cannot export a span from a session that
    /// stopped.
    pub fn latest_finalized_range_for(&self, session_id: &str) -> Option<FinalizedRange> {
        if !self.active.lock().ok()?.contains_key(session_id) {
            return None;
        }
        let session_id = session_id.to_owned();
        let sessions = self.sessions.lock().ok()?;
        let manifest = sessions.get(&session_id)?.manifest();
        // One lock acquisition for all three: a concurrent finalize must not be
        // able to pair an id with a span it didn't produce.
        Some(FinalizedRange {
            session_id,
            target_title: manifest.target_title.clone(),
            oldest_start_100ns: manifest
                .segments
                .first()
                .map(|segment| segment.start_100ns)
                .unwrap_or_default(),
            end_100ns: manifest
                .segments
                .last()
                .map(|segment| segment.end_100ns)
                .unwrap_or_default(),
        })
    }

    /// Where the recording currently is, in the same 100ns ticks the segments
    /// carry -- the last video PTS the worker fed the encoder. `None` means
    /// nothing is recording yet (no frame has been fed), which is what the
    /// marker hotkey turns into its 録画していません refusal.
    ///
    /// Task1680: this used to answer with the latest *preview* frame's
    /// timestamp, and preview frames come only from real WGC frames. A still
    /// window delivers none, so every marker pressed during one landed on the
    /// recording's start -- and, being the same instant every time, deduped
    /// down to a single marker at 0:00 no matter how often the hotkey was
    /// pressed (measured: three presses at +40.7s/+50.8s/+60.7s produced one
    /// marker at 0.0s). Task163's held frames keep recorded time flowing
    /// through the still stretch, so the press instant does point at recorded
    /// content. Reading the fed PTS also keeps the marker inside recorded time
    /// by construction: a segment's end is `exclusive_segment_end` = last fed
    /// PTS + one frame interval, so the value can never sit past it and be
    /// dropped by the review pane's `recorded_markers` filter (task1630).
    pub fn recording_position_100ns(&self) -> Option<i64> {
        let active = self.active.lock().ok()?;
        Self::last_started_id(&active)
            .and_then(|session_id| Self::position_of(&active, &session_id))
    }

    /// The same for a named capture (task2000).
    pub fn recording_position_100ns_for(&self, session_id: &str) -> Option<i64> {
        let active = self.active.lock().ok()?;
        Self::position_of(&active, session_id)
    }

    fn position_of(active: &HashMap<String, ActiveCapture>, session_id: &str) -> Option<i64> {
        let position = active
            .get(session_id)?
            .session
            .recording_position
            .load(Ordering::Relaxed);
        (position > 0).then_some(position)
    }
}

#[cfg(test)]
pub(crate) mod tests;
