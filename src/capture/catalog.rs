//! Session-catalog side of `CaptureController`: loading closed sessions from
//! disk, folding finalized segments/markers into the ring manifest, and the
//! per-session edit/discard/reclaim operations. Split out of `capture.rs`,
//! which keeps the record/stop lifecycle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::ring_buffer;

use super::{CaptureController, ReclaimReport};

/// What discarding one session actually deletes.
///
/// The two shapes are *not* interchangeable and the difference is destructive:
/// a directory session owns its folder, while a container session is one file
/// inside a folder full of other people's things. Naming them apart is what
/// stops the second from being handed to `remove_dir_all`.
enum DiscardTarget {
    Directory(PathBuf),
    Container(PathBuf),
}

/// What one marker edit does to the session being *recorded* (task3960).
///
/// `edit_marker_at` is shared by three callers that want three different
/// answers there, so the answer is a parameter rather than a blanket refusal:
/// rename and delete are routed through the index writer (the confirmation
/// page has always offered them, and every press used to fail), and since
/// task1560 so is `add_marker` -- the review pane's "+" onto the session being
/// recorded goes onto the same queue a hotkey press does.
pub(super) enum LiveMarkerEdit {
    Add { time_100ns: i64 },
    Rename { time_100ns: i64, label: String },
    Remove { time_100ns: i64 },
}

impl LiveMarkerEdit {
    /// Applies the edit to the index writer's pending queue. `false` means the
    /// marker is not in it, so the caller moves on to the next arm.
    fn apply_to_queue(&self, queue: &mut Vec<ring_buffer::MarkerRecord>) -> bool {
        match self {
            // Always taken, so an add never reaches the staging arm. The same
            // instant twice is one entry; one already in the manifest is
            // `merge_markers`' no-op to make.
            Self::Add { time_100ns } => {
                if !queue.iter().any(|marker| marker.time_100ns == *time_100ns) {
                    queue.push(ring_buffer::MarkerRecord {
                        time_100ns: *time_100ns,
                        label: String::new(),
                        // `merge_markers` assigns it in creation order (task171).
                        color_index: None,
                    });
                }
                true
            }
            Self::Rename { time_100ns, label } => queue
                .iter_mut()
                .find(|marker| marker.time_100ns == *time_100ns)
                .map(|marker| marker.label = label.clone())
                .is_some(),
            Self::Remove { time_100ns } => {
                let before = queue.len();
                queue.retain(|marker| marker.time_100ns != *time_100ns);
                queue.len() != before
            }
        }
    }

    /// Rewrites the live ring's in-memory manifest and hands back the record
    /// for the index writer. `None` = no marker at that instant.
    fn stage(
        &self,
        ring: &mut ring_buffer::RingBuffer,
    ) -> Option<ring_buffer::container::MetaEvent> {
        match self {
            // Unreachable in practice: `apply_to_queue` always takes an add.
            // Only a capture stopping between `is_live_session` and the queue
            // lookup gets here, and then it reports "no such marker".
            Self::Add { .. } => None,
            Self::Rename { time_100ns, label } => {
                ring.stage_marker_renamed(*time_100ns, label.clone())
            }
            Self::Remove { time_100ns } => ring.stage_marker_removed(*time_100ns),
        }
    }
}

impl CaptureController {
    /// Deletes the directory-shaped sessions left over from before the
    /// container (task450, user decision 2026-08-18: remove them at startup,
    /// silently, but say so in the log).
    ///
    /// Runs once per launch, before the catalog is built, so a legacy session
    /// never reaches the history at all. There is deliberately no reader for
    /// the old shape: the two formats were never meant to coexist, and a
    /// half-migrated buffer is worse than a clean one.
    ///
    /// **Directories only.** The buffer root also holds exported mp4s, the
    /// `Clips` output folder and, from now on, every `.lvb`; a sweep that
    /// walked files would eat both. A directory qualifies when it is *named*
    /// like a session ([`ring_buffer::is_session_dir_name`]), plus
    /// `quarantine`, which old recoveries left behind.
    ///
    /// Carrying a `manifest.json` used to qualify a directory too, and that is
    /// exactly what destroyed a user's clips (task3100): the listing scan wrote
    /// a manifest into `Clips` on one launch and this sweep `remove_dir_all`'d
    /// it on the next. The name is now the whole test, so a `Clips\manifest.json`
    /// left over from that defect no longer condemns the folder.
    pub(super) fn sweep_legacy_sessions(root: &Path) {
        let Ok(entries) = std::fs::read_dir(root) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let looks_like_a_session =
                ring_buffer::is_session_dir_name(&name) || name == "quarantine";
            if !looks_like_a_session {
                continue;
            }
            match std::fs::remove_dir_all(&path) {
                Ok(()) => tracing::info!(
                    event = "legacy_session_swept",
                    session = %name,
                    "removed a pre-container session directory (task450)"
                ),
                Err(error) => tracing::warn!(
                    event = "legacy_session_sweep_failed",
                    session = %name,
                    %error,
                ),
            }
        }
    }

    pub(super) fn load_closed_sessions() -> (
        HashMap<String, ring_buffer::RingBuffer>,
        Option<ring_buffer::SessionManifest>,
    ) {
        Self::load_closed_sessions_at(&Self::buffer_root())
    }

    // `root` is a parameter (rather than always `Self::buffer_root()`) for the
    // same reason `load_session_at`'s is: unit tests exercise the rebuild
    // against temp folders instead of the real %LOCALAPPDATA%\Liveback\buffer.
    pub(super) fn load_closed_sessions_at(
        root: &Path,
    ) -> (
        HashMap<String, ring_buffer::RingBuffer>,
        Option<ring_buffer::SessionManifest>,
    ) {
        let mut sessions = HashMap::new();
        let mut latest = None;
        let Ok(entries) = std::fs::read_dir(root) else {
            return (sessions, latest);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            // Both session shapes are catalogued (task440); which one a given
            // recording has is task450's switch, and a buffer root can hold
            // either while that is in flight. A directory also has to be
            // *named* like a session (task3100) -- `Clips` carrying a
            // `manifest.json` written by that defect is not one.
            let opened = if ring_buffer::is_container_path(&path) {
                ring_buffer::RingBuffer::open_container(path)
            } else if path.is_dir()
                && ring_buffer::is_session_dir(&path)
                && ring_buffer::SessionStore::new(path.clone()).has_manifest()
            {
                ring_buffer::RingBuffer::open(path)
            } else {
                continue;
            };
            let Ok(ring) = opened else {
                continue;
            };
            if !ring.manifest().closed {
                continue;
            }
            let manifest = ring.manifest().clone();
            let end = manifest
                .segments
                .last()
                .map(|segment| segment.end_100ns)
                .unwrap_or_default();
            let latest_end = latest
                .as_ref()
                .and_then(|current: &ring_buffer::SessionManifest| current.segments.last())
                .map(|segment| segment.end_100ns)
                .unwrap_or_default();
            if latest.is_none() || end >= latest_end {
                latest = Some(manifest.clone());
            }
            sessions.insert(manifest.session_id.clone(), ring);
        }
        (sessions, latest)
    }

    /// Rebuilds the session catalog from `root`, as a fresh launch would.
    ///
    /// `self.sessions` caches one `RingBuffer` per session, each rooted at the
    /// folder it was opened from, and every edit -- title, note, protect,
    /// discard, `session_root` -- resolves through that cached root rather than
    /// through `buffer_root()`. So when the buffer root moves (task164) the
    /// stale entries keep rewriting manifests under the *old* folder while the
    /// history lists the new one: a protect toggle wrote to the old copy and
    /// read back unprotected, and a discard would have deleted the old folder's
    /// session. `last_timeline` goes with them because a root change
    /// invalidates the loaded-session pointer exactly as a restart does.
    pub(super) fn reload_catalog_at(&self, root: &Path) -> Result<(), String> {
        let (sessions, latest) = Self::load_closed_sessions_at(root);
        *self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())? = sessions;
        *self
            .last_timeline
            .lock()
            .map_err(|_| "the timeline state lock is poisoned".to_owned())? = latest;
        Ok(())
    }

    /// The manifest for export/inspection use, with every `segments[].path`
    /// already resolved (via `RingBuffer::resolve_segment_path`) to an
    /// absolute path so callers never need to know a session's root just to
    /// read a segment's bytes. `timeline()`/`get_timeline` (frontend-facing)
    /// deliberately does *not* do this: the frontend only ever needs `index`,
    /// never a local filesystem path.
    pub fn session_manifest(
        &self,
        session_id: &str,
    ) -> Result<ring_buffer::SessionManifest, String> {
        self.sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?
            .get(session_id)
            .map(|ring| {
                let mut manifest = ring.manifest().clone();
                for segment in &mut manifest.segments {
                    segment.path = ring.resolve_segment_path(&segment.path);
                }
                manifest
            })
            .ok_or_else(|| "no such session".to_owned())
    }

    /// Whether this session is a single-file container (task450). Export asks
    /// because it still reads segments as files and has to stage them out
    /// first; everything else goes through `ReadTarget` and never needs to know.
    pub fn session_is_container(&self, session_id: &str) -> bool {
        self.session_container_path(session_id).is_some()
    }

    /// The `.lvb` behind a session, for the two callers that need the file
    /// itself rather than bytes: export staging and "show in folder".
    pub fn session_container_path(&self, session_id: &str) -> Option<PathBuf> {
        self.sessions
            .lock()
            .ok()?
            .get(session_id)?
            .container_path()
            .map(Path::to_path_buf)
    }

    pub fn session_root(&self, session_id: &str) -> Result<PathBuf, String> {
        self.sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?
            .get(session_id)
            .map(|ring| ring.root().to_path_buf())
            .ok_or_else(|| "no such session".to_owned())
    }

    // Loading swaps `last_timeline` so `get_timeline`/`acquire_review_lease` (when
    // called without an explicit session_id) point at the loaded session instead of
    // the most-recently-recorded one. A session already in `self.sessions` (the
    // common case: either restored at startup by `load_closed_sessions` or created
    // and closed during this run) is used as-is; one found only on disk is opened
    // read-only and inserted into `self.sessions` so a later `acquire_review_lease`
    // can resolve it (that lookup requires a `self.sessions` hit, not just a
    // `last_timeline` pointer).
    //
    // Recording does not close this door (task3700): only the session being
    // recorded right now is refused here, by id, because its disk manifest lags
    // its ring -- `load_active_session_by_id` is that one's door. Note that the
    // `last_timeline` swap below is *not* durable mid-recording: every running
    // capture's index writer overwrites it each pass (indexer.rs), so a caller
    // that must keep pointing at what it loaded asks `timeline_for(id)` rather
    // than `timeline()`.
    pub fn load_session(&self, session_id: &str) -> Result<ring_buffer::SessionManifest, String> {
        self.load_session_at(&Self::buffer_root(), session_id)
    }

    /// Opens a `.lvb` by path, wherever it sits (task570): a double-click, a
    /// drop on the window, or `liveback.exe <path>` all land here. No new
    /// reading -- a container's id *is* its file stem and `load_session_at`
    /// already takes the root to look in, so the file's folder is the root.
    ///
    /// Two consequences worth naming rather than coding around:
    /// - A session whose stem is already in `self.sessions` wins the lookup
    ///   inside `load_session_at`, so opening a *copy* of a session still in
    ///   the buffer shows the buffer's, not the copy's. Ids are random, so
    ///   only a copy can collide.
    /// - The opened session joins `self.sessions` under that stem, which means
    ///   `discard_session` could reach a file outside the buffer. Nothing calls
    ///   it with such an id: the history list comes from `list_sessions`, which
    ///   walks the buffer root only, so a foreign file never gets a row.
    pub fn load_session_file(&self, path: &Path) -> Result<ring_buffer::SessionManifest, String> {
        if !ring_buffer::is_container_path(path) {
            return Err("not a Liveback session file (.lvb)".into());
        }
        let session_id = ring_buffer::container_session_id(path);
        if session_id.is_empty() {
            return Err("could not read the session name".into());
        }
        // `parent()` is `Some("")` for a bare filename, which would join into a
        // path relative to nothing rather than to the working directory.
        let root = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        self.load_session_at(root, &session_id)
    }

    // `root` is a parameter (rather than always `Self::buffer_root()`) purely so unit
    // tests can exercise the disk-fallback branch without writing into the real
    // %LOCALAPPDATA%\Liveback\buffer -- same rationale as discard_session_from_disk's
    // `root: &Path` parameter below.
    pub(super) fn load_session_at(
        &self,
        root: &std::path::Path,
        session_id: &str,
    ) -> Result<ring_buffer::SessionManifest, String> {
        if session_id.contains(['/', '\\']) || session_id.contains("..") {
            return Err("invalid session id".into());
        }
        // Per-id, not process-wide (task3700). A *closed* session's on-disk
        // manifest is final and stays valid while some other capture runs, so
        // there is nothing to refuse; only the recording session's own manifest
        // is stale on disk, and that one has its own door
        // (`load_active_session_by_id`). `is_live_session` also runs
        // `stop_if_worker_ended` first -- which is what the old `is_active()`
        // call did -- so a dead worker still sitting in `active` does not
        // refuse its own id. The `recording.marker` / unclosed-container checks
        // below are independent of this and keep refusing crash remnants.
        if self.is_live_session(session_id) {
            return Err("open a recording session from its LIVE row".into());
        }
        let manifest = {
            let sessions = self
                .sessions
                .lock()
                .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
            if let Some(ring) = sessions.get(session_id) {
                ring.manifest().clone()
            } else {
                drop(sessions);
                // Whichever shape the id names on disk (task440). The
                // container path is tried first because a directory of the
                // same name cannot exist beside it -- the id is the file stem.
                let container = root
                    .join(session_id)
                    .with_extension(ring_buffer::CONTAINER_EXTENSION);
                let is_container = container.is_file();
                let ring = if is_container {
                    ring_buffer::RingBuffer::open_container(container)
                } else {
                    ring_buffer::RingBuffer::open(root.join(session_id))
                }
                .map_err(|error| format!("could not load the session: {error}"))?;
                if !ring.manifest().closed {
                    // A `closed: false` manifest read from disk (rather than
                    // from the in-memory `self.sessions` map, which is
                    // already excluded by this branch) is either a session a
                    // prior run left mid-recording, or one `recover_session`
                    // just reconstructed on the user's explicit "recover
                    // this" action. `recording.marker` tells them apart:
                    // recording always creates it and only a clean
                    // `finalize()` or `recover_session` ever removes it, so
                    // its absence here means recovery already vouched that
                    // nothing will write to this directory again.
                    //
                    // A container has no marker file: the `Closed` record is
                    // the same signal (計画書 §3.6), and `recover` writes it.
                    // So an unclosed container is exactly the case the marker
                    // used to catch, and is refused (task440).
                    let unfinished = if is_container {
                        true
                    } else {
                        ring_buffer::SessionStore::new(ring.root().to_path_buf())
                            .has_recording_marker()
                    };
                    if unfinished {
                        return Err("that session cannot be loaded".into());
                    }
                }
                let manifest = ring.manifest().clone();
                self.sessions
                    .lock()
                    .map_err(|_| "the session catalog lock is poisoned".to_owned())?
                    .insert(session_id.to_owned(), ring);
                manifest
            }
        };
        *self
            .last_timeline
            .lock()
            .map_err(|_| "the timeline state lock is poisoned".to_owned())? =
            Some(manifest.clone());
        Ok(manifest)
    }

    /// Loads the session that is being recorded right now (task161, LiveReview).
    /// Deliberately *not* routed through `load_session_at`: that path refuses
    /// the recording session by id, and must keep doing so. The stale manifest
    /// is *this* session's own -- disk lags the in-memory ring, which the index
    /// writer folds each finalized segment into as it lands (task430) -- so the
    /// recording session is the one that has to be served from memory, and it
    /// is the only one `load_session_at` turns away (task3700 narrowed that
    /// refusal from every load while capturing to this one id). Opening it
    /// through `load_session_at` would also load it `live: false` and park the
    /// review screen at the head instead of the live edge.
    pub fn load_active_session(&self) -> Result<ring_buffer::SessionManifest, String> {
        self.stop_if_worker_ended();
        let Some(session_id) = self.last_started_session() else {
            return Err("no session is recording".into());
        };
        self.load_running_session(&session_id)
    }

    /// The same for a *named* running capture (task2030). The history's LIVE
    /// rows are the only way to move the review screen from one recording to
    /// another, and `load_active_session` above can only ever reach the
    /// last-started one.
    pub fn load_active_session_by_id(
        &self,
        session_id: &str,
    ) -> Result<ring_buffer::SessionManifest, String> {
        self.stop_if_worker_ended();
        // Copy the answer out and drop `active` before `sessions` is taken: the
        // lock order the `active` field documents.
        let running = self
            .active
            .lock()
            .map_err(|_| "the capture state lock is poisoned".to_owned())?
            .contains_key(session_id);
        if !running {
            return Err("no session is recording".into());
        }
        self.load_running_session(session_id)
    }

    /// Reads one running session's live manifest out of the catalog and makes it
    /// the loaded one. Takes `sessions` only -- `active` must already be
    /// released by the caller (see the lock order on the field).
    fn load_running_session(
        &self,
        session_id: &str,
    ) -> Result<ring_buffer::SessionManifest, String> {
        let manifest = self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?
            .get(session_id)
            .map(|ring| ring.manifest().clone())
            .ok_or_else(|| "no such session".to_owned())?;
        // Same contract as `load_session`: whoever is loaded is what
        // `get_timeline` and a session-less `acquire_review_lease` resolve to.
        *self
            .last_timeline
            .lock()
            .map_err(|_| "the timeline state lock is poisoned".to_owned())? =
            Some(manifest.clone());
        Ok(manifest)
    }

    /// `disposal` is the caller's answer to "did a person ask for this one
    /// session to go?" (task1520 follow-up). The history's discard says
    /// `Recycle`; the retention sweep says `Permanent`, because recycling would
    /// free none of the disk it exists to free.
    pub fn discard_session(
        &self,
        session_id: &str,
        disposal: ring_buffer::Disposal,
    ) -> Result<(), String> {
        // A recording is never discarded, whoever asks (t260920-bb09). Until
        // that task the only thing standing here was the history screen
        // refusing to select a LIVE row; once the user's 2026-09-19 ruling
        // opened that selection, the refusal had to live where the deletion
        // does. The UI disables the menu entry and the Delete key above this,
        // so reaching it means something got past both -- which is precisely
        // the case CLAUDE.md's recording-loss incident is about.
        if self.is_live_session(session_id) {
            return Err("a recording session cannot be deleted".into());
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
        // A lease can only ever be acquired for a session already in
        // `self.sessions` (acquire_review_lease), so a session absent from this
        // map needs no equivalent guard -- that is the crash-recovery case,
        // which `load_closed_sessions` deliberately skips.
        // The session's own directory, straight off the ring, rather than
        // `segments.first().path.parent()`. That derivation silently stopped
        // working once manifests began recording a bare filename for `path`:
        // `parent()` returns the empty path, `remove_dir_all("")` fails, and a
        // manifest with no segments at all skipped the deletion entirely while
        // still returning `Ok`. A 283-session bulk discard reported success and
        // left 13 directories behind (task125).
        // **A container's `root()` is the buffer folder itself**, not a folder of
        // its own -- a container session *is* one file sitting in the buffer. So
        // the container path is taken here, straight off the ring, and deleting
        // it below is `remove_file`. Reaching the directory branch with it would
        // `remove_dir_all` the whole buffer root: every other session, and every
        // clip the user had exported into it. That is not hypothetical -- it is
        // what this did when task450 made containers the default, and it took
        // the exports with it.
        let target = match sessions.get(session_id) {
            Some(ring) => {
                if ring.has_leases() {
                    return Err("a session being reviewed or exported cannot be deleted".into());
                }
                let target = match ring.container_path() {
                    Some(container) => DiscardTarget::Container(container.to_path_buf()),
                    None => DiscardTarget::Directory(ring.root().to_path_buf()),
                };
                sessions.remove(session_id);
                Some(target)
            }
            None => None,
        };
        // Release the lock before touching disk.
        drop(sessions);
        let Some(target) = target else {
            // Never loaded into `self.sessions` -- `load_closed_sessions` skips
            // open/unclosed manifests, which is exactly what a crash-recovery
            // candidate is. A lease can only be acquired for a session that is
            // in the map, so this path needs no `has_leases` equivalent.
            return Self::discard_session_from_disk(&Self::buffer_root(), session_id, disposal);
        };
        match target {
            DiscardTarget::Container(path) => {
                // Enumeration, not `exists()`: a delete-pending file answers
                // "no" to `exists()` while still sitting in the listing the
                // history pane is rebuilt from, and returning `Ok` for it is
                // exactly the silent success task1520 is about.
                if !ring_buffer::still_listed(&path) {
                    return Ok(());
                }
                ring_buffer::remove_container(&path, disposal)
                    .map_err(|error| format!("could not delete the session: {error}"))
            }
            DiscardTarget::Directory(root) => {
                if !root.exists() {
                    return Ok(());
                }
                ring_buffer::discard_session_at(&root, disposal)
                    .map_err(|error| format!("could not delete the session: {error}"))
            }
        }
    }

    /// Renames a session. The actively recording one is refused rather than
    /// routed through: its `RingBuffer` lives inside the capture worker thread
    /// and rewrites `manifest.json` on every segment, so anything written from
    /// here would be silently overwritten on the next segment. The settings
    /// `PathRow` disables the title field while recording for the same reason.
    ///
    /// **Markers are not in that boat any more** (task3960). The confirmation
    /// page never gated its marker rows, so rename and delete were offered and
    /// failed on every press; they now route through the index writer instead
    /// (`edit_marker_at`). The title still has no such route, and is still
    /// refused here.
    pub fn set_session_title(&self, session_id: &str, title: Option<String>) -> Result<(), String> {
        if self.is_live_session(session_id) {
            return Err("a recording session's title cannot be changed".into());
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
        match sessions.get_mut(session_id) {
            Some(ring) => ring
                .set_target_title(title.clone())
                .map_err(|error| format!("could not update the title: {error}"))?,
            None => {
                // Not loaded in memory (a crash-recovery session, or one this
                // process never opened): rewrite the manifest on disk instead.
                drop(sessions);
                ring_buffer::set_session_title(&Self::buffer_root(), session_id, title.clone())
                    .map_err(|error| format!("could not update the title: {error}"))?;
            }
        }
        // Keep the snapshot `get_timeline` serves in sync, so the confirmation
        // page shows the new title without a reload.
        if let Ok(mut last) = self.last_timeline.lock() {
            if let Some(manifest) = last.as_mut() {
                if manifest.session_id == session_id {
                    manifest.target_title = title;
                }
            }
        }
        Ok(())
    }

    /// The one free line the user wrote about a session (task167 stored it,
    /// task172 gives the history a way to write it). Unlike the title this is
    /// allowed on the session being recorded: a note is about the recording,
    /// and the most useful moment to write one is while it is happening.
    pub fn set_session_note(&self, session_id: &str, note: &str) -> Result<(), String> {
        let normalized = ring_buffer::normalize_note(note);
        let live = self.is_live_session(session_id);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
        match sessions.get_mut(session_id) {
            // The session being recorded: its file belongs to the index writer
            // (task720). Update what the process shows and queue the record for
            // that thread, exactly as a marker pressed mid-recording is handled.
            Some(ring) if live && ring.is_container() => {
                let event = ring.stage_note(normalized);
                drop(sessions);
                self.queue_meta(session_id, event);
            }
            Some(ring) => ring
                .set_note(normalized)
                .map_err(|error| format!("could not update the note: {error}"))?,
            None => {
                // Not loaded in memory: rewrite the manifest on disk instead,
                // exactly as the title path does.
                drop(sessions);
                ring_buffer::set_session_note(&Self::buffer_root(), session_id, normalized)
                    .map_err(|error| format!("could not update the note: {error}"))?;
            }
        }
        Ok(())
    }

    /// Excludes a session from the retention sweep, or puts it back (task167 /
    /// round5 §6-C). Also allowed on the live session: protecting what is being
    /// recorded right now is the case that matters most.
    pub fn set_session_protected(&self, session_id: &str, protected: bool) -> Result<(), String> {
        let live = self.is_live_session(session_id);
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
        match sessions.get_mut(session_id) {
            // See `set_session_note`: the recording's own file is off limits
            // to everything but the index writer.
            Some(ring) if live && ring.is_container() => {
                let event = ring.stage_protected(protected);
                drop(sessions);
                self.queue_meta(session_id, event);
            }
            Some(ring) => ring
                .set_protected(protected)
                .map_err(|error| format!("could not update the protection: {error}"))?,
            None => {
                drop(sessions);
                ring_buffer::set_session_protected(&Self::buffer_root(), session_id, protected)
                    .map_err(|error| format!("could not update the protection: {error}"))?;
            }
        }
        Ok(())
    }

    /// Queues a marker for the last-started recording session. Task2190 made
    /// this the *fallback* entry: the hotkey uses it only when the review pane
    /// has nothing loaded (tray-resident use, window never opened), and names
    /// the displayed session through `push_marker_for` otherwise.
    ///
    /// The index writer folds the marker into the manifest on its next pass
    /// (task430: it wakes every 500ms even with no segment to write, so a
    /// marker never waits for one), so the frontend shows it immediately from
    /// `review-marker-added` and converges on the persisted value shortly after.
    pub fn push_marker(&self, time_100ns: i64) {
        Self::queue_marker(self.pending_markers(), time_100ns);
    }

    /// The same for a named capture (task2190) -- the session the review pane
    /// is showing, so a press lands where the user is looking even with two
    /// captures running. An id that is not recording queues nothing.
    pub fn push_marker_for(&self, session_id: &str, time_100ns: i64) {
        Self::queue_marker(self.pending_markers_for(session_id), time_100ns);
    }

    fn queue_marker(queue: Option<Arc<Mutex<Vec<ring_buffer::MarkerRecord>>>>, time_100ns: i64) {
        let Some(queue) = queue else {
            return;
        };
        if let Ok(mut pending) = queue.lock() {
            pending.push(ring_buffer::MarkerRecord {
                time_100ns,
                label: String::new(),
                // `merge_markers` assigns it in creation order (task171).
                color_index: None,
            });
        };
    }

    /// Marker edits follow `set_session_note`'s rules, not
    /// `set_session_title`'s (task3960): the recording session is *routed*
    /// rather than refused -- through the index writer's queue or through
    /// `stage_marker_renamed`, see `edit_marker_at` -- an in-memory session is
    /// edited through its `RingBuffer`, and anything else is rewritten on disk.
    pub fn update_marker(
        &self,
        session_id: &str,
        time_100ns: i64,
        label: String,
    ) -> Result<(), String> {
        self.edit_marker(
            session_id,
            LiveMarkerEdit::Rename {
                time_100ns,
                label: label.clone(),
            },
            |ring| ring.update_marker(time_100ns, label.clone()),
            |root| ring_buffer::update_marker(root, session_id, time_100ns, label.clone()),
            // `RingBuffer::update_marker`'s rule: rename the marker at that
            // instant, and leave an array that has none alone.
            |markers| {
                if let Some(marker) = markers
                    .iter_mut()
                    .find(|marker| marker.time_100ns == time_100ns)
                {
                    marker.label = label.clone();
                }
            },
        )
    }

    /// Adds a marker with an empty label at `time_100ns`. `merge_markers`
    /// dedups by identity, so re-adding an existing instant is a no-op. The
    /// review UI adds through this (task127); the global hotkey (task130)
    /// keeps using `push_marker` for the recording session.
    pub fn add_marker(&self, session_id: &str, time_100ns: i64) -> Result<(), String> {
        self.edit_marker(
            session_id,
            // The recording session takes it through the index writer's queue
            // (task1560), exactly like a hotkey press through `push_marker_for`:
            // `edit_marker_at`'s Arm 1 pushes it and stages no `MetaEvent`, so
            // the one `MarkerAdded` is `merge_markers`' to write on its next
            // pass. A stopped session is written directly by the arms below.
            LiveMarkerEdit::Add { time_100ns },
            // Not `merge_markers`: that one leaves the container's own record
            // to its caller, and dropping it here is what made every review
            // pane add vanish at the next launch (task1670).
            |ring| ring.add_marker(time_100ns),
            |root| ring_buffer::add_marker(root, session_id, time_100ns),
            // `merge_markers`' rules: `time_100ns` is identity so an instant
            // already there is a no-op, and the array stays in timeline order.
            // The colour is deliberately left `None` -- the disk write says
            // `palette_index: None` too, and the index is assigned by whichever
            // side replays the record. Writing one here would be a second copy
            // of that rule. The UI itself no longer reads `color_index` at all
            // since t260927-7d08 moved markers to the DS's single `marker`
            // colour; see `ring_buffer.rs`'s `MarkerRecord::color_index` doc.
            |markers| {
                if !markers.iter().any(|marker| marker.time_100ns == time_100ns) {
                    markers.push(ring_buffer::MarkerRecord {
                        time_100ns,
                        label: String::new(),
                        color_index: None,
                    });
                    markers.sort_by_key(|marker| marker.time_100ns);
                }
            },
        )
    }

    pub fn delete_marker(&self, session_id: &str, time_100ns: i64) -> Result<(), String> {
        self.edit_marker(
            session_id,
            LiveMarkerEdit::Remove { time_100ns },
            |ring| ring.delete_marker(time_100ns),
            |root| ring_buffer::delete_marker(root, session_id, time_100ns),
            |markers| markers.retain(|marker| marker.time_100ns != time_100ns),
        )
    }

    fn edit_marker(
        &self,
        session_id: &str,
        live: LiveMarkerEdit,
        in_memory: impl Fn(&mut ring_buffer::RingBuffer) -> std::io::Result<()>,
        on_disk: impl Fn(&std::path::Path) -> std::io::Result<()>,
        on_snapshot: impl Fn(&mut Vec<ring_buffer::MarkerRecord>),
    ) -> Result<(), String> {
        self.edit_marker_at(
            &Self::buffer_root(),
            session_id,
            live,
            in_memory,
            on_disk,
            on_snapshot,
        )
    }

    /// `root` is a parameter for the same reason `load_session_at`'s is: the
    /// disk arm has to be exercisable without writing into the user's real
    /// %LOCALAPPDATA%\Liveback\buffer.
    ///
    /// `on_snapshot` applies the same edit to the `last_timeline` snapshot that
    /// `on_disk` just applied to the file. It exists because the disk arm has
    /// no `RingBuffer` to read the result back from, and re-opening one to get
    /// it was both a wasted read and wrong: the old
    /// `RingBuffer::open(root.join(session_id))` resolves the *directory*
    /// layout only, so for a `.lvb` container -- `<root>\<id>.lvb`, a file --
    /// it always failed and the `if let Ok` swallowed it, leaving the snapshot
    /// stale (task3240). Replaying the edit instead is layout-blind and costs
    /// no I/O at all, which is also what keeps task3140's read reduction from
    /// being undone here.
    ///
    /// `live` says what the recording session gets: all three callers are
    /// routed -- rename and delete since task3960, add since task1560 (always
    /// through the queue arm).
    ///
    /// **The queue arm runs before the staging arm, and stages no `MetaEvent`
    /// at all.** That order is forced, not preferred: `Indexer::write` appends
    /// `pending_meta` to the container *before* it takes the `sessions` lock to
    /// run `merge_markers`, so a `MarkerRenamed` staged for a marker still in
    /// the queue reaches the container ahead of the `MarkerAdded` that creates
    /// it -- `Snapshot::apply` finds no target and silently drops the rename --
    /// and a staged `MarkerRemoved` is a no-op that the following `MarkerAdded`
    /// undoes, so the deleted marker comes back at the next load. Rewriting the
    /// queue entry instead makes `merge_markers` carry the final label in the
    /// one `MarkerAdded` it was going to write anyway, and a removal writes
    /// nothing. Do not "simplify" this back to a single staging arm.
    pub(super) fn edit_marker_at(
        &self,
        root: &std::path::Path,
        session_id: &str,
        live: LiveMarkerEdit,
        in_memory: impl Fn(&mut ring_buffer::RingBuffer) -> std::io::Result<()>,
        on_disk: impl Fn(&std::path::Path) -> std::io::Result<()>,
        on_snapshot: impl Fn(&mut Vec<ring_buffer::MarkerRecord>),
    ) -> Result<(), String> {
        if self.is_live_session(session_id) {
            // Arm 1 -- the marker was pressed so recently that it is still in
            // the index writer's queue and has never been in `manifest.markers`.
            // Edit the queue entry itself; do *not* stage a `MetaEvent`.
            //
            // This is not a preference, it is the only order that works
            // (task3960). `Indexer::write` appends `pending_meta` *before* it
            // takes the `sessions` lock to run `merge_markers`, so a staged
            // event for a queued marker reaches the container ahead of the
            // `MarkerAdded` that creates it: `MarkerRenamed` then finds no
            // target and `Snapshot::apply` silently drops it, and
            // `MarkerRemoved` runs before the add, so the deleted marker comes
            // back on the next load. Rewriting the queue instead makes
            // `merge_markers` emit the final label in the one `MarkerAdded` it
            // was going to write anyway, and a removal writes nothing at all.
            //
            // No `is_container()` here on purpose: the queue is the index
            // writer's, whatever layout the ring underneath happens to be.
            if let Some(queue) = self.pending_markers_for(session_id) {
                if queue
                    .lock()
                    .is_ok_and(|mut pending| live.apply_to_queue(&mut pending))
                {
                    self.sync_timeline_markers(session_id, None, on_snapshot);
                    return Ok(());
                }
            }
            // Arm 2 -- already folded into the manifest. Same shape as
            // `set_session_note`: rewrite what the process shows and let the
            // index writer, the only thread holding that container's writer,
            // append the record.
            //
            // Scoped so the guard is released before the fall-through below
            // locks `sessions` again.
            let staged = {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
                match sessions
                    .get_mut(session_id)
                    .filter(|ring| ring.is_container())
                {
                    Some(ring) => {
                        let event = live.stage(ring).ok_or_else(|| {
                            "could not update the marker: no such marker".to_owned()
                        })?;
                        Some((event, ring.manifest().markers.clone()))
                    }
                    // A live session with no container cannot happen in this
                    // build (only the recording start path builds one, always
                    // via `create_container`), so this falls through to the
                    // ordinary arms below rather than inventing a third answer.
                    None => None,
                }
            };
            if let Some((event, markers)) = staged {
                self.queue_meta(session_id, event);
                self.sync_timeline_markers(session_id, Some(markers), on_snapshot);
                return Ok(());
            }
        }
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| "the session catalog lock is poisoned".to_owned())?;
        let updated = match sessions.get_mut(session_id) {
            Some(ring) => {
                in_memory(ring).map_err(|error| format!("could not update the marker: {error}"))?;
                Some(ring.manifest().markers.clone())
            }
            None => {
                drop(sessions);
                on_disk(root).map_err(|error| format!("could not update the marker: {error}"))?;
                None
            }
        };
        self.sync_timeline_markers(session_id, updated, on_snapshot);
        Ok(())
    }

    /// Same reason as `set_session_title`: keep the snapshot `get_timeline`
    /// serves in sync so the UI reflects the edit without a reload.
    ///
    /// `updated` is the ring's own array where there is one -- it already
    /// merged the edit, colour index and all, so it is the more accurate
    /// answer. `None` (the on-disk arm, and the queue arm) replays the edit on
    /// the snapshot instead.
    fn sync_timeline_markers(
        &self,
        session_id: &str,
        updated: Option<Vec<ring_buffer::MarkerRecord>>,
        on_snapshot: impl Fn(&mut Vec<ring_buffer::MarkerRecord>),
    ) {
        if let Ok(mut last) = self.last_timeline.lock() {
            if let Some(manifest) = last.as_mut() {
                if manifest.session_id == session_id {
                    match updated {
                        Some(markers) => manifest.markers = markers,
                        None => on_snapshot(&mut manifest.markers),
                    }
                }
            }
        }
    }

    /// Deletes sessions past the configured capacity or age. Run at startup
    /// and after a recording stops (not at exit): a crash or a force-quit is
    /// exactly when orphans appear, and that's the run an exit-time sweep
    /// never gets.
    ///
    /// Two captures spend the same `capacity_bytes` budget at twice the rate,
    /// so this sweeps more often and older sessions live shorter (task2010).
    /// That is the configured capacity doing its job, not a bug -- the budget
    /// is a total, never per recording.
    pub fn reclaim_sessions(
        &self,
        capacity_bytes: u64,
        lifetime_days: u32,
        loaded_session_id: Option<String>,
    ) -> Result<ReclaimReport, String> {
        let root = Self::buffer_root();
        let sessions = ring_buffer::list_sessions(&root)
            .map_err(|error| format!("could not list the sessions: {error}"))?;
        let mut protected: Vec<String> = loaded_session_id.into_iter().collect();
        // **Every** running capture, not just one (task1990). While the cap was
        // one this read a single id and the answer happened to be complete; the
        // moment a second capture can exist, protecting one of them means this
        // sweep deletes the other one's `.lvb` out from under its writer.
        //
        // `stop_if_worker_ended` first (that is what `is_active()` used to do
        // here), then copy the ids out and release `active` before
        // `discard_session` below takes `sessions` -- the lock order the
        // `active` field documents.
        self.stop_if_worker_ended();
        if let Ok(active) = self.active.lock() {
            protected.extend(active.keys().cloned());
        }
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let doomed = ring_buffer::sessions_to_reclaim(
            &sessions,
            capacity_bytes,
            lifetime_days,
            now_nanos,
            &protected,
        );
        let mut report = ReclaimReport::default();
        for ring_buffer::Doomed { session_id, reason } in doomed {
            let bytes = sessions
                .iter()
                .find(|session| session.session_id == session_id)
                .map_or(0, |session| session.total_bytes);
            // A session held by a review/export lease is skipped, not an error:
            // it is in use right now and the next sweep will reconsider it.
            match self.discard_session(&session_id, ring_buffer::Disposal::Permanent) {
                Ok(()) => {
                    // Task700: two sessions vanished on 2026-08-19 with not one
                    // line about it anywhere -- `prune|reclaim|retention|delete`
                    // over that day's log returned nothing. Counted *after* the
                    // delete succeeded, so the record and the report can never
                    // claim a session that is still on disk.
                    tracing::info!(
                        target: "task700_reclaim",
                        session_id,
                        reason = ?reason,
                        bytes,
                        "reclaimed a session"
                    );
                    report.removed_sessions += 1;
                    report.freed_bytes = report.freed_bytes.saturating_add(bytes);
                    if reason == ring_buffer::ReclaimReason::OverCapacity {
                        report.over_capacity_sessions += 1;
                        report.over_capacity_bytes =
                            report.over_capacity_bytes.saturating_add(bytes);
                    }
                }
                Err(reason) => tracing::debug!(session_id, reason, "reclaim skipped a session"),
            }
        }
        Ok(report)
    }

    /// Is this the session a capture worker is writing right now? Same predicate
    /// a refused rename uses; note, protection (task167) and marker
    /// rename/delete (task3960) do not refuse it but must not write to its file
    /// themselves (task720).
    ///
    /// Since task1990 this is one map lookup: only a *running* capture has an
    /// entry, so the stale-after-stop case the old `active_session` had -- an id
    /// that outlived its recording and needed the recording flag to gate it --
    /// cannot exist. `stop_if_worker_ended` first, so a capture that ended on
    /// its own is reaped on the same schedule `is_active()` used to reap it on.
    ///
    /// Call it *before* the `sessions` lock: it takes `active`, and taking that
    /// while holding `sessions` is the lock-order inversion the `active` field
    /// documents.
    pub(super) fn is_live_session(&self, session_id: &str) -> bool {
        self.stop_if_worker_ended();
        self.active
            .lock()
            .is_ok_and(|active| active.contains_key(session_id))
    }

    /// Hands one meta record to the recording's index writer thread (task720).
    /// Dropped if that capture is no longer running -- its writer is gone and
    /// nothing would ever read the queue.
    fn queue_meta(&self, session_id: &str, event: ring_buffer::container::MetaEvent) {
        if let Some(queue) = self.pending_meta(session_id) {
            if let Ok(mut pending) = queue.lock() {
                pending.push(event);
            }
        }
    }

    pub(super) fn discard_session_from_disk(
        root: &std::path::Path,
        session_id: &str,
        disposal: ring_buffer::Disposal,
    ) -> Result<(), String> {
        // Only a cheap existence check, so a genuinely missing session_id gets
        // the same "no such session" error as the in-memory branch instead of
        // `ring_buffer::discard_session`'s silent `Ok(())` no-op for a missing
        // path. The actual traversal rejection and deletion always happen
        // inside `ring_buffer::discard_session`; this check never gates them.
        //
        // Both shapes count: a container is a file, and asking only `is_dir`
        // told the user "no such session" for every crash-recovered
        // container (task450).
        let directory = root.join(session_id).is_dir();
        let container = root
            .join(session_id)
            .with_extension(ring_buffer::CONTAINER_EXTENSION)
            .is_file();
        if !directory && !container {
            return Err("no such session".to_owned());
        }
        ring_buffer::discard_session(root, session_id, disposal)
            .map_err(|error| format!("could not delete the session: {error}"))
    }
}
