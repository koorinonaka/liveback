//! Operations on the container sessions under the buffer root -- discovery,
//! listing, crash recovery, age/capacity reclaim and on-disk title/marker
//! edits. Split from `ring_buffer.rs`, which keeps the manifest types and the
//! live `RingBuffer` itself.

use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};

use super::{container, RingBuffer, SessionManifest, SessionSummary, MANIFEST_VERSION};

/// Whether a directory sitting in the buffer root is a pre-container session
/// directory (task3100), which `sweep_legacy_sessions` removes at startup.
///
/// **Name only, deliberately.** Every session id this app has ever minted is
/// `capture-{unix_nanos:032x}` -- there is no code path that creates a session
/// directory under any other name -- so the name is a complete answer, and it
/// is the only answer that is *stable*. The tempting alternative, "does it
/// carry a `manifest.json`", is not: the buffer root also holds
/// `<buffer_root>\Clips`, the default clip output folder, and the scan used to
/// treat that folder as a manifest-less session. It "recovered" it (writing a
/// `manifest.json` into it) on one launch and `sweep_legacy_sessions` deleted
/// the whole folder on the next, taking the user's clips with it. Keeping
/// `has_manifest()` as an OR here would leave every machine that already hit
/// that defect still holding a `Clips\manifest.json` -- and still losing the
/// folder on the next launch. So the manifest never gets a vote.
///
/// The one behaviour this gives up is listing a session a user renamed by hand,
/// and giving it up errs the non-destructive way: unlisted means untouched.
/// It does not contradict task1520 ("a session that will not open still gets a
/// row"), which is about keeping *sessions* in place so a row cannot slide
/// under the pointer of a right-click menu. A `Clips` row is the opposite --
/// something that was never a session pushing every real row down one.
/// task450's legacy sweep already used this prefix as its first test.
pub fn is_session_dir_name(name: &str) -> bool {
    name.starts_with("capture-")
}

/// What a container looked like when its summary was taken (t261025-d59e).
///
/// Read through `fs::metadata` -- a handle on the file -- rather than
/// `DirEntry::metadata`: on NTFS the directory entry's size for a file that is
/// still open for writing is updated lazily, so the recording container would
/// keep matching its first key and its row would stop growing. Every write to
/// a container appends (`Meta`, `Pruned`, checkpoints), so the length moves on
/// every change; `modified` is belt and braces.
pub(crate) type ContainerKey = (u64, SystemTime);

pub(crate) fn container_key(path: &Path) -> Option<ContainerKey> {
    let metadata = fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}

/// Container summaries by path, reused while the file is unchanged
/// (t261025-d59e). A closed container never changes, yet the history relists
/// every 5 s while recording and four times at startup, and each listing used
/// to reopen and decode every `.lvb` -- on a HDD buffer that is the cost of the
/// whole listing. In memory only.
static CONTAINER_SUMMARIES: Mutex<Option<HashMap<PathBuf, (ContainerKey, SessionSummary)>>> =
    Mutex::new(None);

fn cached_summary(path: &Path, key: Option<ContainerKey>) -> Option<SessionSummary> {
    let key = key?;
    let cache = CONTAINER_SUMMARIES.lock().ok()?;
    let (cached_key, summary) = cache.as_ref()?.get(path)?;
    (*cached_key == key).then(|| summary.clone())
}

fn remember_summary(path: &Path, key: Option<ContainerKey>, summary: &SessionSummary) {
    let (Some(key), Ok(mut cache)) = (key, CONTAINER_SUMMARIES.lock()) else {
        return;
    };
    cache
        .get_or_insert_with(HashMap::new)
        .insert(path.to_path_buf(), (key, summary.clone()));
}

/// Seeds the listing cache from a container the caller has already opened
/// (t261025-d59e): `load_closed_sessions_at` opens every `.lvb` at startup, so
/// the first listings after it need not open them again. `key` must be read
/// before the open, so a write racing the open leaves a key that no longer
/// matches rather than one that hides the write.
pub(crate) fn remember_opened_container(path: &Path, key: Option<ContainerKey>, ring: &RingBuffer) {
    remember_summary(path, key, &container_summary(path, ring));
}

/// Read-only session catalog scan: never creates, renames, or removes anything
/// on disk. Safe to call on a poll/refresh cadence.
pub fn list_sessions(root: &Path) -> io::Result<Vec<SessionSummary>> {
    list_sessions_with_live(root, &HashMap::new())
}

/// `list_sessions` with the recording sessions' rows supplied by the caller,
/// keyed by container path (t261101-27d6). A recording container is appended
/// to every second or two, so its `container_key` never matches and every
/// 5 s relist reopened it from disk -- on a HDD buffer, against the recorder's
/// own writes. The recorder already holds that session's manifest in memory;
/// a row in `live` is used as it is, without a stat or an open, and is never
/// cached: the first listing after the stop opens the closed file once.
///
/// `ring_buffer` does not know `capture`, so the caller that owns the live
/// rings builds the rows (`container_summary`).
pub fn list_sessions_with_live(
    root: &Path,
    live: &HashMap<PathBuf, SessionSummary>,
) -> io::Result<Vec<SessionSummary>> {
    crate::insight_scope!("list_sessions");
    let mut out = vec![];
    if !root.exists() {
        return Ok(out);
    }
    let mut unreadable = 0u32;
    let mut containers_seen = vec![];
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else {
            // The only remaining silent drop: `read_dir` gave no path, so there
            // is nothing to name a row after.
            tracing::warn!("list_sessions could not read a buffer-root entry");
            continue;
        };
        let path = entry.path();
        // Only the `.lvb` container is a session (t261025-d118). Anything else
        // is skipped silently, as an unrelated file always was -- including a
        // directory, which is how `Clips` used to get in (task3100).
        //
        // `file_type()` comes with the directory entry (no stat, where
        // `Path::is_dir` stats every time), and the name is checked first so
        // an unrelated entry never asks (t261025-d59e). One difference from
        // `Path::is_dir`: a junction reads as a symlink and is not followed.
        if !super::is_container_path(&path) || entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        if let Some(summary) = live.get(&path) {
            out.push(summary.clone());
            continue;
        }
        let key = container_key(&path);
        containers_seen.push(path.clone());
        let summary = match cached_summary(&path, key) {
            Some(summary) => Some(summary),
            None => {
                let summary = summarize_container(&path);
                if let Some(summary) = &summary {
                    remember_summary(&path, key, summary);
                }
                summary
            }
        };
        // A session that will not open still gets a row (task1520 follow-up).
        // Dropping it was silent *and* moved every row below it up one: the
        // history is a list you right-click, so a row vanishing under the
        // pointer between the look and the click aims a destructive menu at
        // whatever slid into its place. That is how a user's recording was
        // discarded. The stub keeps the position -- `session_id` is the sort
        // key and comes off the path either way -- and says why.
        let summary = summary.unwrap_or_else(|| {
            unreadable += 1;
            unreadable_stub(&path)
        });
        out.push(summary);
    }
    if unreadable > 0 {
        tracing::warn!(unreadable, "list_sessions found unreadable sessions");
    }
    // Forget containers that left this root; other roots' entries stay.
    if let Ok(mut cache) = CONTAINER_SUMMARIES.lock() {
        if let Some(cache) = cache.as_mut() {
            cache.retain(|path, _| path.parent() != Some(root) || containers_seen.contains(path));
        }
    }
    out.sort_by(|a, b| b.session_id.cmp(&a.session_id));
    Ok(out)
}

/// The row for a session nothing could read: the file is locked by another
/// process, the container will not open (task1520 follow-up).
///
/// `readable: false` is the existing flag for "listed but not loadable" --
/// `ui_state::sessions::is_load_disabled` already refuses to open one, and
/// neither sweep touches it (t261101-6b34): the reason it would not open may be
/// a lock, a newer build's format or a failed HDD read, and only the last of
/// those is something a delete would not lose a recording to.
/// Every size and time is `None`/0 rather than a guess: the point of the row is
/// that nothing in it could be read.
fn unreadable_stub(path: &Path) -> SessionSummary {
    SessionSummary::empty(super::container_session_id(path))
}

impl SessionSummary {
    /// A row with nothing read into it: no times, no sizes, version 0,
    /// unreadable. The builders below start here and fill what they found.
    fn empty(session_id: String) -> Self {
        Self {
            session_id,
            start_100ns: None,
            end_100ns: None,
            segment_count: 0,
            total_bytes: 0,
            closed: false,
            recoverable: false,
            manifest_version: 0,
            readable: false,
            target_title: None,
            thumbnail_index: None,
            comment: None,
            protected: false,
            target_executable: None,
            target_executable_path: None,
            marker_count: 0,
        }
    }
}

/// The first segment's start and the last one's end, `None` for none.
fn span_of(manifest: &SessionManifest) -> (Option<i64>, Option<i64>) {
    match (manifest.segments.first(), manifest.segments.last()) {
        (Some(first), Some(last)) => (Some(first.start_100ns), Some(last.end_100ns)),
        _ => (None, None),
    }
}

/// See `SessionSummary::thumbnail_index`.
fn thumbnail_index_of(manifest: &SessionManifest) -> Option<u64> {
    manifest
        .segments
        .iter()
        .find(|segment| segment.thumbnail_path.is_some())
        .map(|segment| segment.index)
}

/// The history's row for a session, read out of the container's own index
/// (task440).
///
/// **`total_bytes` is the allocated size, not the logical one** (計画書 R3).
/// A container's length never shrinks -- `prune` punches holes and leaves the
/// offsets where they were -- so `metadata().len()` would report a session
/// that has given all its space back as if it still held it, and every
/// capacity decision downstream (`sessions_to_reclaim`, the history footer)
/// reads this field.
pub(super) fn summarize_container(path: &Path) -> Option<SessionSummary> {
    crate::insight_scope!("summarize_container");
    // Through `open_container`'s manifest rather than the raw snapshot, so the
    // row built here and the one `load_closed_sessions_at` seeds the cache
    // with come out of one builder (t261025-d59e).
    let ring = RingBuffer::open_container(path.to_path_buf()).ok()?;
    Some(container_summary(path, &ring))
}

pub(crate) fn container_summary(path: &Path, ring: &RingBuffer) -> SessionSummary {
    let manifest = ring.manifest();
    let (start_100ns, end_100ns) = span_of(manifest);
    SessionSummary {
        session_id: super::container_session_id(path),
        start_100ns,
        end_100ns,
        segment_count: manifest.segments.len(),
        total_bytes: container::allocated_bytes(path).unwrap_or(0),
        closed: manifest.closed,
        // A container never told it was finished (no `Closed` record).
        recoverable: !manifest.closed,
        // The reader refuses a container whose format it does not know, so
        // anything that opened at all is readable by this build. The version
        // reported is the manifest one the UI compares against, which a
        // container has no equivalent of.
        manifest_version: MANIFEST_VERSION,
        // Except a replay that stopped before its first segment
        // (t261101-6b34): no checkpoint resolved and the head is a punched
        // hole, so "no segment" is what could be read, not what was recorded.
        readable: !(ring.scan_truncated() && manifest.segments.is_empty()),
        target_title: manifest.target_title.clone(),
        // `open_container` carries a thumbnail as an empty `thumbnail_path`.
        thumbnail_index: thumbnail_index_of(manifest),
        comment: manifest.comment.clone(),
        protected: manifest.protected,
        target_executable: manifest.target_executable.clone(),
        target_executable_path: manifest.target_executable_path.clone(),
        marker_count: manifest.markers.len(),
    }
}

/// What removing this session would give back: 0 when there is none.
///
/// **Allocated, not logical** (計画書 R3): the history's "freed N" line is
/// measured with this, and a container that has punched its holes still has
/// the full length its `metadata()` reports.
pub fn session_disk_bytes(root: &Path, session_id: &str) -> u64 {
    container_path(root, session_id)
        .ok()
        .flatten()
        .and_then(|path| container::allocated_bytes(&path).ok())
        .unwrap_or(0)
}

/// The `.lvb` a session id names, `None` when there is no such file. An id
/// that would leave `root` is an `InvalidInput` error.
fn container_path(root: &Path, session_id: &str) -> io::Result<Option<PathBuf>> {
    if session_id.contains(['/', '\\']) || session_id.contains("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id",
        ));
    }
    let path = root
        .join(session_id)
        .with_extension(super::CONTAINER_EXTENSION);
    Ok(path.is_file().then_some(path))
}

/// [`container_path`], with a missing session as `NotFound`.
fn existing_container(root: &Path, session_id: &str) -> io::Result<PathBuf> {
    container_path(root, session_id)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such session"))
}

/// The one door an on-disk edit goes through (t261025-00ad): `event` lands as
/// a `Meta` record in the session's container.
fn edit(root: &Path, session_id: &str, event: container::MetaEvent) -> io::Result<()> {
    append_meta(&existing_container(root, session_id)?, event)
}

/// Applies one edit to a container by appending a `Meta` record and
/// checkpointing, which is what "rewrite the manifest in place" becomes.
///
/// A closed session stays closed: `closed` is a flag in the snapshot, not
/// "the last record is `Closed`", so an edit after the fact does not make the
/// session look interrupted again.
///
/// Reading the container to get there used to mean `ContainerReader::open`,
/// which slurps the whole file: adding one marker to a 26 GB session read
/// 26 GB on the UI thread before the app could redraw (task3140). The two
/// things this needs -- `valid_end` and `snapshot` -- are both in the
/// checkpoint, so the cheap open (4 KiB header + one checkpoint record) can
/// supply them. The catch is `valid_end`: after a full scan it is the end of
/// the last *sound* record, which is what `reopen` truncates to so a crashed
/// write's debris cannot end up in front of the record appended next; after
/// the cheap open it is merely the file's length, which would make that
/// `set_len` a no-op and leave a torn tail wedged between the checkpoint and
/// the new record, where the next full scan stops -- losing the edit. So the
/// cheap path is taken only when the checkpoint is provably the last thing in
/// the file (its end == the file's length), which means there is no debris to
/// cut and the two definitions of `valid_end` coincide. Anything else --
/// no usable checkpoint, or bytes behind it -- falls back to the full scan,
/// which is the only path that can judge those bytes.
pub(super) fn append_meta(path: &Path, event: container::MetaEvent) -> io::Result<()> {
    append_meta_checked(path, event, |_| Ok(()))
}

/// [`append_meta`], refused by `check` before anything is written. `check`
/// sees the snapshot the append already read, so it costs no extra read.
fn append_meta_checked(
    path: &Path,
    event: container::MetaEvent,
    check: impl FnOnce(&container::Snapshot) -> io::Result<()>,
) -> io::Result<()> {
    let fast = match container::ContainerReader::open_at_checkpoint(path) {
        Ok(reader) if reader.checkpoint_end() == Some(reader.valid_end()) => Some(reader),
        _ => None,
    };
    let reader = match fast {
        Some(reader) => reader,
        None => container::ContainerReader::open(path)?,
    };
    let valid_end = reader.valid_end();
    let snapshot = reader.snapshot().clone();
    drop(reader);
    check(&snapshot)?;
    let mut writer = container::ContainerWriter::reopen(path, valid_end, snapshot)?;
    writer.append_meta(&event)?;
    writer.checkpoint()
}

/// What the startup sweep should do with a session (task164).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionRepair {
    /// A normally closed recording. Left alone.
    Intact,
    /// Interrupted, but the segments are there: `recover_session` rebuilds the
    /// manifest and it becomes an ordinary recording. No user decision needed --
    /// a crash-recovered session is a real recording, and asking about it was
    /// the only reason the history ever showed a warning state.
    Recoverable,
    /// Nothing to recover: a session this build read in full and found no
    /// segment in. Discarded rather than listed.
    Unsalvageable,
}

/// Classifies a listed session for the sweep. Pure, so the three outcomes are
/// testable without a buffer directory.
pub(crate) fn classify_session(summary: &SessionSummary) -> SessionRepair {
    // What this build could not read is never discarded (t261101-6b34): a
    // newer build's header, a file someone holds, a failed HDD read all look
    // like this, and none of them means "no recording". Whether it goes is the
    // user's call (t261101-271d).
    if !summary.readable || summary.manifest_version == 0 {
        return SessionRepair::Intact;
    }
    if summary.closed {
        return SessionRepair::Intact;
    }
    // No segment, so there is no recording in this session at all.
    if summary.segment_count == 0 {
        return SessionRepair::Unsalvageable;
    }
    SessionRepair::Recoverable
}

/// The startup sweep (task164): repairs what it can, discards what it cannot,
/// and reports how many sessions it changed. Removes the whole warning
/// vocabulary from the history by making it untrue -- there is no
/// crash-interrupted row left to explain, because it has already become an
/// ordinary recording by the time the list is drawn.
///
/// The caller must not run this while a capture is in flight: the recording
/// session looks exactly like a recoverable one (no `Closed` record yet) and
/// "recovering" it would close a container the capture worker is still
/// writing.
pub fn repair_sessions(root: &Path) -> io::Result<u32> {
    crate::insight_scope!("repair_sessions");
    let mut changed = 0;
    for summary in list_sessions(root)? {
        let session_id = summary.session_id.as_str();
        match classify_session(&summary) {
            SessionRepair::Intact => {}
            SessionRepair::Recoverable => {
                // `container::recover` ends with the `Closed` record and a
                // checkpoint, so recovery and promotion are the same write.
                if recover_session(root, session_id).is_err() {
                    continue;
                }
                changed += 1;
                tracing::info!(session_id, "startup sweep repaired a session");
            }
            SessionRepair::Unsalvageable => {
                // Debris, not a recording the user chose to delete: nobody
                // wants an unsalvageable session back out of the bin.
                if discard_session(root, session_id, Disposal::Permanent).is_ok() {
                    changed += 1;
                    tracing::info!(
                        session_id,
                        "startup sweep discarded an unrepairable session"
                    );
                }
            }
        }
    }
    Ok(changed)
}

pub fn recover_session(root: &Path, session_id: &str) -> io::Result<SessionManifest> {
    // A container recovers itself: the tail scan truncates whatever the crash
    // left half-written, re-stamps a checkpoint and leaves a clean close
    // (計画書 §3.6). There is no timeline to rebuild by re-reading segments --
    // every start/end/title/marker is already in the records.
    let container_path = existing_container(root, session_id)?;
    container::recover(&container_path)?;
    RingBuffer::open_container(container_path).map(|ring| ring.manifest().clone())
}
/// Session ids are `capture-<unix nanos, 32 hex digits>` (written by
/// `capture::types::default_encoder_output_dir`), so the id itself dates the
/// session. Reading the age from the id rather than the directory's mtime
/// keeps it stable across a copy, a restore, or a manifest rewrite.
fn session_started_nanos(session_id: &str) -> Option<u128> {
    let hex = session_id.strip_prefix("capture-")?;
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(hex, 16).ok()
}

/// Chooses which sessions to delete, oldest first, when the buffer exceeds
/// either limit. The two limits are ORed: a session goes if it is older than
/// `lifetime_days`, or if the newer sessions already fill `capacity_bytes`
/// without it.
///
/// `protected` sessions are never chosen but still count against the quota —
/// they occupy the disk whether or not we are allowed to reclaim them, so
/// pretending otherwise would under-delete. A session whose id doesn't carry a
/// timestamp is only ever removed for capacity, never for age.
/// Why the sweep picked a session (task700).
///
/// The two are told apart because only one of them is a surprise: aging a
/// session out after `sessionLifetimeDays` is the setting doing exactly what it
/// says, while losing a recording because a *different* recording grew into the
/// capacity budget is something the user never asked for and, until this task,
/// was never told about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReclaimReason {
    /// Older than `sessionLifetimeDays`.
    TooOld,
    /// Kept sessions ahead of this one already fill `retentionCapacityGb`.
    OverCapacity,
}

/// One session the sweep will delete, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Doomed {
    pub session_id: String,
    pub reason: ReclaimReason,
}

pub fn sessions_to_reclaim(
    sessions: &[SessionSummary],
    capacity_bytes: u64,
    lifetime_days: u32,
    now_nanos: u128,
    protected: &[String],
) -> Vec<Doomed> {
    const NANOS_PER_DAY: u128 = 24 * 60 * 60 * 1_000_000_000;
    // round8 §3-2: 0 is 「なし」 -- that limit is not in force. Read as a
    // duration it would mean "everything is already past its lifetime", which
    // is the whole buffer deleted on the next sweep, so it is guarded here
    // rather than at the settings edge that can be bypassed.
    let lifetime_nanos = (lifetime_days > 0).then(|| u128::from(lifetime_days) * NANOS_PER_DAY);
    let capacity_bytes = (capacity_bytes > 0).then_some(capacity_bytes);
    let mut newest_first: Vec<&SessionSummary> = sessions.iter().collect();
    newest_first.sort_by(|a, b| b.session_id.cmp(&a.session_id));

    let mut kept_bytes = 0u64;
    let mut doomed = Vec::new();
    for session in newest_first {
        // Two kinds of protection meet here: `protected` the parameter is
        // "in use right now" (recording, loaded for review), `session.protected`
        // is the user's own flag (task167). Both keep their bytes in
        // `kept_bytes` -- what protection costs is part of the budget, it is
        // just never what gets deleted to make room.
        if session.protected || protected.contains(&session.session_id) {
            kept_bytes = kept_bytes.saturating_add(session.total_bytes);
            continue;
        }
        // A row nothing could read has no protection flag to honour either
        // (t261101-6b34), so neither limit may take it.
        if !session.readable {
            continue;
        }
        let too_old = lifetime_nanos.is_some_and(|lifetime| {
            session_started_nanos(&session.session_id)
                .is_some_and(|started| now_nanos.saturating_sub(started) > lifetime)
        });
        // `kept_bytes > 0`: with nothing newer kept, there is no "newer
        // sessions fill it without this one" -- the newest alone over the
        // budget stays (t261101-6b34, the user's 2026-11-01 ruling).
        let over_capacity = kept_bytes > 0
            && capacity_bytes
                .is_some_and(|capacity| kept_bytes.saturating_add(session.total_bytes) > capacity);
        // Age wins when both hold: a session past its lifetime was going to go
        // today whatever the budget looked like, and blaming capacity for it
        // would put a toast in front of the user for a deletion they configured
        // (task700).
        let reason = match (too_old, over_capacity) {
            (true, _) => Some(ReclaimReason::TooOld),
            (false, true) => Some(ReclaimReason::OverCapacity),
            (false, false) => None,
        };
        if let Some(reason) = reason {
            doomed.push(Doomed {
                session_id: session.session_id.clone(),
                reason,
            });
        } else {
            kept_bytes = kept_bytes.saturating_add(session.total_bytes);
        }
    }
    // Oldest first, so a sweep that fails partway still reclaimed the stalest
    // space rather than the freshest.
    doomed.reverse();
    doomed
}

/// What moving the retention limits from `stored` to `next` would delete
/// (t261018-34fe): the settings screen holds such a change back until the user
/// confirms it, because the sweep deletes for good.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionPreview {
    /// `next` dooms a session `stored` does not -- the only case that asks.
    pub grows: bool,
    /// Everything `next` dooms, not just the increase: the confirm runs the
    /// whole sweep, so this is what "N 件を削除します" has to promise.
    pub sessions: usize,
    pub bytes: u64,
    /// The same total split by `ReclaimReason`.
    pub too_old: usize,
    pub over_capacity: usize,
    /// Of `sessions`, the ones a capture or the review screen holds right now.
    /// The sweep skips those and a later one takes them.
    pub in_use: usize,
}

/// `(capacity_bytes, lifetime_days)` pairs, read the way `sessions_to_reclaim`
/// reads them (`0` = off). In-use sessions are judged as if released, since
/// they go the moment they are; their count is reported apart.
///
/// ponytail: while one is in use, its bytes are kept, so the sweep that runs
/// now can push out a session this count does not name. The confirm reports
/// what actually went (`ReclaimReport::removed_sessions`); exact only if
/// that ever matters.
pub fn retention_preview(
    sessions: &[SessionSummary],
    stored: (u64, u32),
    next: (u64, u32),
    now_nanos: u128,
    in_use: &[String],
) -> RetentionPreview {
    let before = sessions_to_reclaim(sessions, stored.0, stored.1, now_nanos, &[]);
    let after = sessions_to_reclaim(sessions, next.0, next.1, now_nanos, &[]);
    let mut preview = RetentionPreview {
        grows: after
            .iter()
            .any(|doomed| !before.iter().any(|old| old.session_id == doomed.session_id)),
        sessions: after.len(),
        ..RetentionPreview::default()
    };
    for doomed in &after {
        preview.bytes = preview.bytes.saturating_add(
            sessions
                .iter()
                .find(|session| session.session_id == doomed.session_id)
                .map_or(0, |session| session.total_bytes),
        );
        match doomed.reason {
            ReclaimReason::TooOld => preview.too_old += 1,
            ReclaimReason::OverCapacity => preview.over_capacity += 1,
        }
        if in_use.contains(&doomed.session_id) {
            preview.in_use += 1;
        }
    }
    preview
}

/// Whether the protected sessions alone have already used up the retention
/// capacity (task167). The sweep cannot act on this -- protection is exactly
/// the promise that it will not -- so it is reported instead, and the UI warns
/// when a recording or an export is about to need room that no longer exists.
///
/// Capacity only: a protected session outliving the lifetime is the point of
/// protecting it, and takes no room a new recording needs (design round5 §6-C
/// counts protection against capacity, not age).
pub fn protected_over_limit(sessions: &[SessionSummary], capacity_bytes: u64) -> bool {
    // 0 = 「なし」 (round8 §3-2): a limit that is not in force cannot be exceeded.
    let bytes = sessions
        .iter()
        .filter(|session| session.protected)
        .fold(0u64, |sum, session| sum.saturating_add(session.total_bytes));
    capacity_bytes > 0 && bytes > capacity_bytes
}

/// Longest note we persist. One line, per the design -- long enough for a
/// sentence about the recording, short enough that the manifest (rewritten on
/// every segment) stays small.
pub(crate) const MAX_SESSION_COMMENT_CHARS: usize = 200;

/// Trimmed, length-capped, empty-as-unset -- `normalize_target_title`'s rule,
/// for the same reason: both write paths go through one function so an
/// in-memory edit and an on-disk edit cannot store different things.
pub fn normalize_session_comment(comment: &str) -> Option<String> {
    let trimmed = comment.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_SESSION_COMMENT_CHARS).collect())
}

/// Longest marker comment we persist (t261009-0251): ten notes' worth, enough
/// for a few lines of reminder without swelling the Meta record.
pub const MAX_COMMENT_CHARS: usize = 2000;

/// `normalize_session_comment`'s rule for a marker comment, except that line breaks stay
/// (`\r\n` becomes `\n`). Empty = no comment.
pub fn normalize_comment(comment: &str) -> Option<String> {
    let unified = comment.replace("\r\n", "\n");
    let trimmed = unified.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_COMMENT_CHARS).collect())
}

/// Writes a session's note into its container (task167). Same caller contract
/// as `set_session_title`: the actively recording session must already have
/// been ruled out, because the capture worker owns its container.
pub fn set_session_comment(
    root: &Path,
    session_id: &str,
    comment: Option<String>,
) -> io::Result<()> {
    let comment = comment.as_deref().and_then(normalize_session_comment);
    edit(
        root,
        session_id,
        container::MetaEvent::CommentSet { comment },
    )
}

pub fn set_session_protected(root: &Path, session_id: &str, protected: bool) -> io::Result<()> {
    edit(
        root,
        session_id,
        container::MetaEvent::ProtectedSet { protected },
    )
}

/// Bulk protection toggle (task167): every session is attempted -- one
/// unwritable manifest must not silently drop the rest -- and the first
/// failure is returned afterwards. Only the reclaim tests call it; the
/// history's multi-select goes through `set_session_protected` per row.
#[cfg(test)]
pub fn set_sessions_protected(
    root: &Path,
    session_ids: &[String],
    protected: bool,
) -> io::Result<()> {
    let mut first_error = None;
    for session_id in session_ids {
        if let Err(error) = set_session_protected(root, session_id, protected) {
            first_error.get_or_insert(error);
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Longest title we persist. A title is a window name the user retyped, not a
/// document, and the manifest is rewritten in full on every segment.
pub const MAX_TARGET_TITLE_CHARS: usize = 120;

/// Trimmed, length-capped, empty-as-unset. Both rename paths (in-memory
/// session and on-disk session) go through this so they cannot drift.
pub fn normalize_target_title(title: &str) -> Option<String> {
    let trimmed = title.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_TARGET_TITLE_CHARS).collect())
}

/// Renames a session that is not held in memory, by appending to its
/// container. Callers must have already ruled out the actively recording
/// session: that one's container belongs to the capture worker's index writer.
pub fn set_session_title(root: &Path, session_id: &str, title: Option<String>) -> io::Result<()> {
    let title = title.as_deref().and_then(normalize_target_title);
    edit(root, session_id, container::MetaEvent::TitleSet { title })
}

pub fn add_marker(root: &Path, session_id: &str, time_100ns: i64) -> io::Result<()> {
    let event = container::MetaEvent::MarkerAdded {
        time_100ns,
        label: String::new(),
        // The container's own `apply` assigns it when it replays.
        palette_index: None,
        comment: None,
    };
    edit(root, session_id, event)
}

pub fn update_marker(
    root: &Path,
    session_id: &str,
    time_100ns: i64,
    label: String,
    comment: Option<String>,
) -> io::Result<()> {
    let event = container::MetaEvent::MarkerRenamed {
        time_100ns,
        label,
        comment,
    };
    edit_marker(root, session_id, time_100ns, event)
}

pub fn delete_marker(root: &Path, session_id: &str, time_100ns: i64) -> io::Result<()> {
    let event = container::MetaEvent::MarkerRemoved { time_100ns };
    edit_marker(root, session_id, time_100ns, event)
}

/// [`edit`] for an event that names an existing marker: an unknown time is the
/// same `NotFound` the in-memory `RingBuffer::update_marker` returns, and
/// nothing is appended (t261102-24df).
fn edit_marker(
    root: &Path,
    session_id: &str,
    time_100ns: i64,
    event: container::MetaEvent,
) -> io::Result<()> {
    let path = existing_container(root, session_id)?;
    append_meta_checked(&path, event, |snapshot| {
        if snapshot.markers.iter().any(|m| m.time_100ns == time_100ns) {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::NotFound, "no such marker"))
        }
    })
}

/// Whether the directory still lists `path`, which is not the same question as
/// whether the file exists (task1520).
///
/// On Windows a file deleted while another process holds it open with
/// `FILE_SHARE_DELETE` goes "delete pending": `remove_file` returns `Ok`,
/// `Path::exists` says **false** (the `CreateFile` behind it is refused with
/// ACCESS_DENIED), and the name stays in the directory until the last handle
/// closes. The history pane is rebuilt from a directory listing, so enumeration
/// is the only answer that agrees with what the user is about to see.
pub fn still_listed(path: &Path) -> bool {
    let (Some(directory), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    fs::read_dir(directory)
        .map(|entries| entries.flatten().any(|entry| entry.file_name() == name))
        .unwrap_or(false)
}

/// The verdict on one removal: what `remove_file` answered, against whether the
/// name is still in the directory afterwards.
///
/// `Ok` **and still listed** is the case this exists for -- a delete the OS
/// accepted but has not carried out, which reports success and leaves the row
/// exactly where it was. Measured on Windows 11 24H2 (task1520) the common
/// holder of a handle no longer produces it: `remove_file` deletes with POSIX
/// semantics, so the name leaves the directory at once even with a reader
/// open. It is still not a guarantee -- the pre-POSIX fallback path, and a
/// volume that does not support it, land back on the pending behaviour -- and
/// silently reporting that as success is the one outcome this task forbids.
pub(crate) fn removal_verdict(removed: io::Result<()>, still_listed: bool) -> io::Result<()> {
    match removed {
        Ok(()) if still_listed => Err(io::Error::other(
            "the delete is pending: a process still has this file open",
        )),
        other => other,
    }
}

/// Where a discarded session goes.
///
/// The two callers want opposite things and used to share one answer. A
/// recording the user discarded by hand is worth a second chance -- the one
/// that prompted this went straight to `remove_file`, and 16 GB of it was
/// simply gone. The retention sweep is the opposite: it exists to give disk
/// back (task1420's disk-full stop depends on it), and a sweep that filled the
/// recycle bin instead would free nothing while reporting that it had.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposal {
    /// The recycle bin, so the user can undo it.
    Recycle,
    /// Gone. For deletions the user did not ask for one at a time: the
    /// retention sweep, and the startup sweep's unsalvageable debris.
    Permanent,
}

/// Hands a path to the shell's recycle operation.
///
/// A thin wrapper over `SHFileOperationW` and deliberately untested: a test
/// would have to put something in the real recycle bin to prove anything. The
/// decision of *which* sessions come through here is
/// `ui_state`/`Disposal`, where it is testable.
///
/// Windows permanently deletes anything larger than the bin's own cap rather
/// than refusing, and reports success either way -- so this is a best effort at
/// recoverability, never a guarantee of it. The caller's own
/// `still_listed`/`exists` check is what decides whether the delete happened.
#[cfg(windows)]
fn recycle(path: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{
        SHFileOperationW, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
    };

    // Double-null terminated: the API takes a list, and a single trailing NUL
    // would leave it reading past the string for the next entry.
    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    wide.push(0);

    let mut op = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(wide.as_ptr()),
        fFlags: FOF_ALLOWUNDO | (FOF_NOCONFIRMATION.0 | FOF_SILENT.0 | FOF_NOERRORUI.0) as u16,
        ..Default::default()
    };
    // SAFETY: `pFrom` points at a double-NUL-terminated buffer that outlives
    // the call, and every other field is zeroed by `Default`.
    let code = unsafe { SHFileOperationW(&mut op) };
    if code != 0 {
        return Err(io::Error::other(format!(
            "SHFileOperation failed with 0x{code:x}"
        )));
    }
    if op.fAnyOperationsAborted.as_bool() {
        return Err(io::Error::other("the recycle operation was aborted"));
    }
    Ok(())
}

/// `FOF_ALLOWUNDO` is the flag that makes `FO_DELETE` mean the recycle bin
/// rather than gone; the `windows` crate does not re-export it.
#[cfg(windows)]
const FOF_ALLOWUNDO: u16 = 0x0040;

/// Deletes the session's file, and then the check that the name really left
/// the directory.
pub fn remove_container(path: &Path, disposal: Disposal) -> io::Result<()> {
    let removed = match disposal {
        Disposal::Recycle => recycle(path),
        Disposal::Permanent => fs::remove_file(path),
    };
    removal_verdict(removed, still_listed(path))
}

pub fn discard_session(root: &Path, session_id: &str, disposal: Disposal) -> io::Result<()> {
    // One file instead of a tree, which is the whole of what discarding a
    // container session is (計画書 §3.4: `remove_dir_all` → `remove_file`).
    // A session that is not there is already discarded.
    match container_path(root, session_id)? {
        Some(path) => remove_container(&path, disposal),
        None => Ok(()),
    }
}

/// Session ids embed their creation time as `capture-{unix_nanos:032x}`, so the
/// date comes from the id instead of a separate field. Returns the raw
/// milliseconds; turning those into a local timestamp is the caller's job (the
/// bin does it with `FileTimeToLocalFileTime`, no date crate).
pub fn session_epoch_millis(session_id: &str) -> Option<i64> {
    let hex = session_id.strip_prefix("capture-")?;
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(hex, 16)
        .ok()
        .map(|nanos| (nanos / 1_000_000) as i64)
}
