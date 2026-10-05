//! Operations on session *directories* under the buffer root -- discovery,
//! listing, crash recovery, age/capacity reclaim and on-disk title/marker
//! edits. Split from `ring_buffer.rs`, which keeps the manifest types and the
//! live `RingBuffer` itself.

use crate::encoder;
use std::{
    fs, io,
    path::{Path, PathBuf},
};

use super::{
    container, store::SessionStore, MarkerRecord, RingBuffer, SegmentRecord, SessionManifest,
    SessionSummary, DEFAULT_RETENTION_MINUTES, MANIFEST_VERSION,
};

/// Whether a directory sitting in the buffer root is a session directory
/// (task3100).
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

/// [`is_session_dir_name`] applied to a path's final component.
pub fn is_session_dir(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| is_session_dir_name(&name.to_string_lossy()))
}

/// Read-only session catalog scan: never creates, renames, or removes anything
/// on disk. Safe to call on a poll/refresh cadence.
pub fn list_sessions(root: &Path) -> io::Result<Vec<SessionSummary>> {
    let mut out = vec![];
    if !root.exists() {
        return Ok(out);
    }
    let mut unreadable = 0u32;
    for entry in fs::read_dir(root)? {
        let Ok(entry) = entry else {
            // The only remaining silent drop: `read_dir` gave no path, so there
            // is nothing to name a row after.
            tracing::warn!("list_sessions could not read a buffer-root entry");
            continue;
        };
        let path = entry.path();
        // A buffer root holds two shapes now (task440): the session directory
        // every recording still uses, and the `.lvb` container. Anything else
        // is not a session and is skipped silently, as an unrelated file
        // always was -- including a directory that is not named like a session,
        // which is how `Clips` used to get in (task3100).
        let summary = if path.is_dir() && is_session_dir(&path) {
            summarize_session_dir(&path)
        } else if !path.is_dir() && super::is_container_path(&path) {
            summarize_container(&path)
        } else {
            continue;
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
    out.sort_by(|a, b| b.session_id.cmp(&a.session_id));
    Ok(out)
}

/// The row for a session nothing could read: the file is locked by another
/// process, the directory is unreadable, the container will not open
/// (task1520 follow-up).
///
/// `readable: false` is the existing flag for "listed but not loadable" --
/// `ui_state::sessions::is_load_disabled` already refuses to open one, and
/// `classify_session` already calls a `manifest_version: 0` / `readable: false`
/// row `Unsalvageable`, which is what this is until whoever holds it lets go.
/// Every size and time is `None`/0 rather than a guess: the point of the row is
/// that nothing in it could be read.
fn unreadable_stub(path: &Path) -> SessionSummary {
    SessionSummary {
        session_id: super::container_session_id(path),
        start_100ns: None,
        end_100ns: None,
        segment_count: 0,
        total_bytes: 0,
        closed: false,
        has_partial: false,
        recoverable: false,
        manifest_version: 0,
        readable: false,
        target_title: None,
        thumbnail_index: None,
        note: None,
        protected: false,
        target_executable: None,
        target_executable_path: None,
        marker_count: 0,
    }
}

/// The same row the history shows for a directory session, read out of a
/// container's own index instead of `manifest.json` (task440).
///
/// **`total_bytes` is the allocated size, not the logical one** (計画書 R3).
/// A container's length never shrinks -- `prune` punches holes and leaves the
/// offsets where they were -- so `metadata().len()` would report a session
/// that has given all its space back as if it still held it, and every
/// capacity decision downstream (`sessions_to_reclaim`, the history footer)
/// reads this field.
pub(super) fn summarize_container(path: &Path) -> Option<SessionSummary> {
    let reader = container::ContainerReader::open_for_reading(path).ok()?;
    let snapshot = reader.snapshot();
    let (start_100ns, end_100ns) = match (snapshot.segments.first(), snapshot.segments.last()) {
        (Some(first), Some(last)) => (Some(first.start_100ns), Some(last.end_100ns)),
        _ => (None, None),
    };
    Some(SessionSummary {
        session_id: super::container_session_id(path),
        start_100ns,
        end_100ns,
        segment_count: snapshot.segments.len(),
        total_bytes: container::allocated_bytes(path).unwrap_or(0),
        closed: snapshot.closed,
        // Neither concept survives the move: a container has no `.partial.mp4`
        // sidecar (a torn tail is truncated on open), and "was it told it was
        // finished" is the `Closed` record rather than a marker file.
        has_partial: false,
        recoverable: reader.needs_recovery(),
        // The reader refuses a container whose format it does not know, so
        // anything that opened at all is readable by this build. The version
        // reported is the manifest one the UI compares against, which a
        // container has no equivalent of.
        manifest_version: MANIFEST_VERSION,
        readable: true,
        target_title: snapshot.title.clone(),
        thumbnail_index: snapshot
            .segments
            .iter()
            .find(|segment| segment.thumbnail.is_some())
            .map(|segment| segment.index),
        note: snapshot.note.clone(),
        protected: snapshot.protected,
        target_executable: snapshot.target_executable.clone(),
        target_executable_path: snapshot.target_executable_path.clone(),
        marker_count: snapshot.markers.len(),
    })
}

/// What removing this session would give back, in whichever shape it has.
///
/// **Allocated, not logical, for a container** (計画書 R3): the history's
/// "freed N" line is measured with this, and a container that has punched its
/// holes still has the full length its `metadata()` reports.
pub fn session_disk_bytes(root: &Path, session_id: &str) -> u64 {
    match container_path(root, session_id) {
        Some(path) => container::allocated_bytes(&path).unwrap_or(0),
        None => SessionStore::new(root.join(session_id)).session_bytes(),
    }
}

/// The `.lvb` a session id names, when the session is a container. `None` for
/// the directory sessions everything still writes, which is what keeps every
/// caller below on its original path unless a container is actually there.
fn container_path(root: &Path, session_id: &str) -> Option<PathBuf> {
    if session_id.contains(['/', '\\']) || session_id.contains("..") {
        return None;
    }
    let path = root
        .join(session_id)
        .with_extension(super::CONTAINER_EXTENSION);
    path.is_file().then_some(path)
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
    let mut writer = container::ContainerWriter::reopen(path, valid_end, snapshot)?;
    writer.append_meta(&event)?;
    writer.checkpoint()
}

pub(super) fn summarize_session_dir(path: &Path) -> Option<SessionSummary> {
    let session_id = path.file_name()?.to_string_lossy().into_owned();
    let store = SessionStore::new(path.to_path_buf());
    // Not an early return: a directory that cannot even be listed still has
    // to be summarised, exactly as before.
    let has_partial = store.has_partial_work();
    let recoverable = store.has_recording_marker();
    let manifest = store.read_manifest_opt();

    if let Some(manifest) = manifest {
        let (start_100ns, end_100ns) = match (manifest.segments.first(), manifest.segments.last()) {
            (Some(first), Some(last)) => (Some(first.start_100ns), Some(last.end_100ns)),
            _ => (None, None),
        };
        return Some(SessionSummary {
            session_id,
            start_100ns,
            end_100ns,
            segment_count: manifest.segments.len(),
            total_bytes: manifest.segments.iter().map(|s| s.bytes).sum(),
            closed: manifest.closed,
            has_partial,
            recoverable,
            manifest_version: manifest.version,
            readable: manifest.version >= MANIFEST_VERSION,
            target_title: manifest.target_title,
            thumbnail_index: manifest
                .segments
                .iter()
                .find(|segment| segment.thumbnail_path.is_some())
                .map(|segment| segment.index),
            note: manifest.note,
            protected: manifest.protected,
            target_executable: manifest.target_executable,
            target_executable_path: manifest.target_executable_path,
            marker_count: manifest.markers.len(),
        });
    }

    // No manifest: describe the session from the files that are there.
    let mp4_files = store.finished_segments().unwrap_or_default();
    Some(SessionSummary {
        session_id,
        start_100ns: None,
        end_100ns: None,
        segment_count: mp4_files.len(),
        total_bytes: store.finished_segment_bytes(),
        closed: false,
        has_partial,
        recoverable,
        manifest_version: 0,
        readable: false,
        target_title: None,
        thumbnail_index: None,
        note: None,
        protected: false,
        target_executable: None,
        target_executable_path: None,
        marker_count: 0,
    })
}

/// Reads a segment mp4's real video duration for `recover_session`'s
/// timeline reconstruction. `None` on any read/parse failure or a
/// missing/zero-timescale video track -- the caller falls back to the fixed
/// per-segment estimate rather than treating this as fatal, since a
/// crash-truncated segment failing to parse is an expected case for crash
/// recovery specifically, not a bug.
/// Drops the `.partial.mp4` debris a crash left in a session being recovered
/// (task470).
///
/// Recovery rebuilds the manifest from the *finished* segments, so a partial
/// is never read -- but nothing used to delete one either, and it sat there
/// for the life of the session. Best effort by design: a partial that will
/// not go (still held open, say) is not a reason to fail a recovery that
/// otherwise worked.
fn drop_partial_debris(store: &SessionStore) {
    let removed = store.remove_partial_segments();
    if removed > 0 {
        tracing::info!(removed, "recovery deleted partial segment debris");
    }
}

pub(super) fn recovered_segment_duration_100ns(path: &Path) -> Option<i64> {
    let video = encoder::dump_segment_track_summary(path)
        .ok()?
        .into_iter()
        .find(|track| !track.is_audio)?;
    if video.timescale == 0 {
        return None;
    }
    Some(video.total_sample_duration_ticks * 10_000_000 / i64::from(video.timescale))
}

/// What the startup sweep should do with a session directory (task164).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionRepair {
    /// A normally closed recording. Left alone.
    Intact,
    /// Interrupted, but the segments are there: `recover_session` rebuilds the
    /// manifest and it becomes an ordinary recording. No user decision needed --
    /// a crash-recovered session is a real recording, and asking about it was
    /// the only reason the history ever showed a warning state.
    Recoverable,
    /// Nothing playable to recover: a manifest this build cannot read (so its
    /// segment layout is unknown), or a directory holding only `.partial.mp4`
    /// debris. Discarded rather than listed.
    Unsalvageable,
}

/// Classifies a listed session for the sweep. Pure, so the three outcomes are
/// testable without a buffer directory.
pub fn classify_session(summary: &SessionSummary) -> SessionRepair {
    if summary.closed && summary.readable {
        return SessionRepair::Intact;
    }
    // A manifest that exists but does not parse into this version: its segment
    // boundaries live in a format this build cannot read, and reconstructing
    // them from the `.mp4` files would silently discard markers and titles.
    if summary.manifest_version > 0 && !summary.readable {
        return SessionRepair::Unsalvageable;
    }
    // No finished segment (`summarize_session_dir` never counts `.partial.mp4`),
    // so there is no recording under this directory at all.
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
/// session looks exactly like a recoverable one (open manifest, live
/// `recording.marker`) and "recovering" it would close a manifest the capture
/// worker is still writing.
pub fn repair_sessions(root: &Path) -> io::Result<u32> {
    let mut changed = 0;
    for summary in list_sessions(root)? {
        let session_id = summary.session_id.as_str();
        match classify_session(&summary) {
            SessionRepair::Intact => {}
            SessionRepair::Recoverable => {
                if recover_session(root, session_id).is_err() {
                    continue;
                }
                // `recover_session` leaves `closed: false` because it existed to
                // answer a "recover this?" prompt. There is no prompt now, so
                // the session is promoted the rest of the way here.
                //
                // A container needs no second step: `container::recover` ends
                // with the `Closed` record and a checkpoint, so recovery and
                // promotion are the same write.
                if container_path(root, session_id).is_none() {
                    if let Ok(mut ring) = RingBuffer::open(root.join(session_id)) {
                        if ring.close().is_err() {
                            continue;
                        }
                    }
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

/// Rebuilds a crashed session's segment list by reading the files themselves,
/// for a directory session whose `manifest.json` never landed.
fn recovered_segments(store: &SessionStore) -> io::Result<Vec<SegmentRecord>> {
    // Already in recording order: `finished_segments` sorts, and the segment
    // stem being fixed-width and zero padded is what makes a lexicographic
    // sort a correct numeric/chronological one. The order matters here
    // because the loop below accumulates each segment's real duration into a
    // running timeline.
    let entries = store.finished_segments()?;
    let mut cumulative_100ns = 0i64;
    let segments = entries
        .into_iter()
        .enumerate()
        .map(|(i, e)| {
            let bytes = e.metadata().map(|m| m.len()).unwrap_or_default();
            // Same stem as the segment mp4 (`store::thumbnail_paths`'
            // convention), so a matching sidecar just needs its extension
            // swapped, not the (now filename-sorted) `i`.
            let thumbnail_path = e.with_extension("jpg");
            let thumbnail_path = thumbnail_path.is_file().then(|| {
                thumbnail_path
                    .file_name()
                    .expect("thumbnail path always has a file name")
                    .into()
            });
            // Best-effort: read this segment's own recorded video duration
            // from its fMP4 boxes (summed `trun` sample durations, see
            // `encoder::dump_segment_track_summary`) so a crash-recovered
            // timeline reflects real segment lengths instead of assuming
            // every segment is exactly `SEGMENT_DURATION_100NS`. A segment
            // written right before a crash is exactly the one most likely to
            // be short or malformed, so a segment this can't read falls back
            // to the fixed estimate rather than failing the whole recovery --
            // an approximate recovered timeline is more useful than none.
            let duration_100ns =
                recovered_segment_duration_100ns(&e).unwrap_or(encoder::SEGMENT_DURATION_100NS);
            let start_100ns = cumulative_100ns;
            cumulative_100ns += duration_100ns;
            SegmentRecord {
                index: i as u64,
                path: e
                    .file_name()
                    .expect("a segment path always has a file name")
                    .into(),
                start_100ns,
                end_100ns: cumulative_100ns,
                bytes,
                thumbnail_path,
                // Crash recovery reads the files, not the muxer that wrote
                // them, and the shift it applied is not recorded anywhere in
                // the mp4 (task204). Zero keeps the audio readable exactly as
                // it was before priming existed: the duplicated 21ms at each
                // head plays through instead of being dropped.
                audio_offsets_100ns: vec![0],
            }
        })
        .collect::<Vec<_>>();
    Ok(segments)
}

pub fn recover_session(root: &Path, session_id: &str) -> io::Result<SessionManifest> {
    if session_id.contains(['/', '\\']) || session_id.contains("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id",
        ));
    }
    // A container recovers itself: the tail scan truncates whatever the crash
    // left half-written, re-stamps a checkpoint and leaves a clean close
    // (計画書 §3.6). There is no timeline to rebuild by re-reading segments --
    // every start/end/title/marker is already in the records.
    if let Some(container_path) = container_path(root, session_id) {
        container::recover(&container_path)?;
        return RingBuffer::open_container(container_path).map(|ring| ring.manifest().clone());
    }
    let path = root.join(session_id);
    let store = SessionStore::new(path.clone());
    if store.has_manifest() {
        let manifest = RingBuffer::open(path.clone()).map(|ring| ring.manifest().clone())?;
        // Same rationale as the reconstructed-manifest branch below: recovery
        // is the user's explicit "nothing will write here again" -- applies
        // just the same when a periodic `persist()` before the crash already
        // left a `closed: false` manifest.json behind (the common case, since
        // most crashes happen after at least one segment has been persisted).
        store.clear_recording_marker();
        drop_partial_debris(&store);
        return Ok(manifest);
    }
    let mut segments = recovered_segments(&store)?;
    segments.sort_by_key(|x| x.index);
    let manifest = SessionManifest {
        version: MANIFEST_VERSION,
        audio_tracks: Vec::new(),
        session_id: session_id.into(),
        closed: false,
        retention_minutes: DEFAULT_RETENTION_MINUTES,
        segments,
        gaps: vec![],
        target_title: None,
        target_executable: None,
        target_executable_path: None,
        markers: vec![],
        note: None,
        protected: false,
    };
    store.write_manifest(&manifest)?;
    // Recovery is the user's explicit confirmation that nothing will write to
    // this session directory again, so it can drop the same "still open"
    // signal a clean `finalize()` clears (see the `recording.marker` removal
    // above in `RingBuffer::finalize`). This also lets `load_session` treat a
    // recovered (closed: false) session as safe to open, and lets
    // `recoverable` (which is derived from this file's presence) drop the
    // session from future recovery-candidate listings.
    store.clear_recording_marker();
    drop_partial_debris(&store);
    Ok(manifest)
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
        let too_old = lifetime_nanos.is_some_and(|lifetime| {
            session_started_nanos(&session.session_id)
                .is_some_and(|started| now_nanos.saturating_sub(started) > lifetime)
        });
        let over_capacity = capacity_bytes
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
pub const MAX_NOTE_CHARS: usize = 200;

/// Trimmed, length-capped, empty-as-unset -- `normalize_target_title`'s rule,
/// for the same reason: both write paths go through one function so an
/// in-memory edit and an on-disk edit cannot store different things.
pub fn normalize_note(note: &str) -> Option<String> {
    let trimmed = note.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_NOTE_CHARS).collect())
}

/// Writes a session's note by rewriting its manifest in place (task167). Same
/// caller contract as `set_session_title`: the actively recording session must
/// already have been ruled out, because the capture worker owns its manifest.
pub fn set_session_note(root: &Path, session_id: &str, note: Option<String>) -> io::Result<()> {
    let note = note.as_deref().and_then(normalize_note);
    if let Some(path) = container_path(root, session_id) {
        return append_meta(&path, container::MetaEvent::NoteSet { note });
    }
    update_manifest(root, session_id, |manifest| manifest.note = note)
}

pub fn set_session_protected(root: &Path, session_id: &str, protected: bool) -> io::Result<()> {
    if let Some(path) = container_path(root, session_id) {
        return append_meta(&path, container::MetaEvent::ProtectedSet { protected });
    }
    update_manifest(root, session_id, |manifest| manifest.protected = protected)
}

/// Bulk protection toggle (task167): the history's multi-select acts on many
/// rows at once. Every session is attempted -- one unwritable manifest must not
/// silently drop the rest -- and the first failure is returned afterwards so the
/// caller can say so.
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

/// Read-modify-write of one session's manifest, shared by the note and
/// protection setters. Deliberately re-reads rather than taking a manifest from
/// the caller: whatever else has been written since (a marker, a title) has to
/// survive an edit that only meant to change one field.
fn update_manifest(
    root: &Path,
    session_id: &str,
    edit: impl FnOnce(&mut SessionManifest),
) -> io::Result<()> {
    // Containers take the `Meta` route instead; the two setters that use this
    // helper branch before they get here.
    let path = session_directory(root, session_id)?;
    if !path.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "session directory not found",
        ));
    }
    let store = SessionStore::new(path);
    let mut manifest = store.read_manifest()?;
    edit(&mut manifest);
    store.write_manifest(&manifest)
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

/// Renames a session that is not held in memory, by rewriting its manifest in
/// place. Callers must have already ruled out the actively recording session:
/// that one's `RingBuffer` lives in the capture worker and rewrites
/// `manifest.json` on every segment, so a write here would be overwritten.
pub fn set_session_title(root: &Path, session_id: &str, title: Option<String>) -> io::Result<()> {
    let title = title.as_deref().and_then(normalize_target_title);
    if let Some(path) = container_path(root, session_id) {
        return append_meta(&path, container::MetaEvent::TitleSet { title });
    }
    let path = session_directory(root, session_id)?;
    if !path.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no such session"));
    }
    RingBuffer::open(path)?.set_target_title(title)
}

pub fn add_marker(root: &Path, session_id: &str, time_100ns: i64) -> io::Result<()> {
    if let Some(path) = container_path(root, session_id) {
        return append_meta(
            &path,
            container::MetaEvent::MarkerAdded {
                time_100ns,
                label: String::new(),
                // Assigned at merge time for a directory session; the
                // container's own `apply` does the same when it replays.
                palette_index: None,
            },
        );
    }
    RingBuffer::open(open_session_directory(root, session_id)?)?
        .merge_markers(vec![MarkerRecord {
            time_100ns,
            label: String::new(),
            color_index: None,
        }])
        // A directory session's manifest *is* the write; the returned events
        // matter only where `persist` writes nothing (a container), and this
        // arm is never one.
        .map(|_| ())
}

pub fn update_marker(
    root: &Path,
    session_id: &str,
    time_100ns: i64,
    label: String,
) -> io::Result<()> {
    if let Some(path) = container_path(root, session_id) {
        return append_meta(
            &path,
            container::MetaEvent::MarkerRenamed { time_100ns, label },
        );
    }
    RingBuffer::open(open_session_directory(root, session_id)?)?.update_marker(time_100ns, label)
}

pub fn delete_marker(root: &Path, session_id: &str, time_100ns: i64) -> io::Result<()> {
    if let Some(path) = container_path(root, session_id) {
        return append_meta(&path, container::MetaEvent::MarkerRemoved { time_100ns });
    }
    RingBuffer::open(open_session_directory(root, session_id)?)?.delete_marker(time_100ns)
}

fn open_session_directory(root: &Path, session_id: &str) -> io::Result<PathBuf> {
    let path = session_directory(root, session_id)?;
    if !path.is_dir() {
        return Err(io::Error::new(io::ErrorKind::NotFound, "no such session"));
    }
    Ok(path)
}

fn session_directory(root: &Path, session_id: &str) -> io::Result<PathBuf> {
    if session_id.contains(['/', '\\']) || session_id.contains("..") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid session id",
        ));
    }
    Ok(root.join(session_id))
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
pub fn removal_verdict(removed: io::Result<()>, still_listed: bool) -> io::Result<()> {
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

/// Deletes the session's file or tree, and then the check that the name really
/// left the directory.
pub fn remove_container(path: &Path, disposal: Disposal) -> io::Result<()> {
    let removed = match disposal {
        Disposal::Recycle => recycle(path),
        Disposal::Permanent => fs::remove_file(path),
    };
    removal_verdict(removed, still_listed(path))
}

/// Deletes a session *directory* that the caller has already resolved.
/// `discard_session` derives the path from an id; the catalog already holds it.
pub fn discard_session_at(root: &Path, disposal: Disposal) -> io::Result<()> {
    match disposal {
        Disposal::Recycle => recycle(root),
        Disposal::Permanent => SessionStore::new(root.to_path_buf()).remove_session(),
    }
}

pub fn discard_session(root: &Path, session_id: &str, disposal: Disposal) -> io::Result<()> {
    // One file instead of a tree, which is the whole of what discarding a
    // container session is (計画書 §3.4: `remove_dir_all` → `remove_file`).
    if let Some(path) = container_path(root, session_id) {
        return remove_container(&path, disposal);
    }
    let store = SessionStore::new(session_directory(root, session_id)?);
    if store.root().exists() {
        match disposal {
            // The shell recycles a directory as one item, so the session tree
            // comes back whole rather than as loose segments.
            Disposal::Recycle => recycle(store.root())?,
            Disposal::Permanent => store.remove_session()?,
        }
    }
    Ok(())
}
