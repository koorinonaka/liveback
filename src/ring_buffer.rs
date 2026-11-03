use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    time::Instant,
};
use windows::{core::PCWSTR, Win32::Storage::FileSystem::GetDiskFreeSpaceExW};

/// Bumped to 2 when segments moved from plain MP4 to fragmented MP4 (fMP4).
/// Every container session reports it (`sessions::container_summary`), and the
/// UI compares a summary's version against it.
pub const MANIFEST_VERSION: u32 = 2;
pub const DEFAULT_RETENTION_MINUTES: u16 = 20;
/// `0` is the one value below `MIN_RETENTION_MINUTES` that means something:
/// keep everything, never drop the head (task1410). It is what the ring buffer
/// switch sends while it is off, it rides in the container header for the whole
/// life of the session, and `prune` returns early on it. Everything else is
/// clamped into `MIN..=MAX` as before.
pub const NO_RETENTION_LIMIT: u16 = 0;
pub const MIN_RETENTION_MINUTES: u16 = 5;
pub const MAX_RETENTION_MINUTES: u16 = 1440;
pub const MIN_FREE_BYTES: u64 = 10 * 1024 * 1024 * 1024;
/// Headroom a *new* capture asks for on top of `MIN_FREE_BYTES`, once per
/// recording already running (task2010). Until two captures could exist, the
/// 10GB floor silently doubled as "one capture's room to write"; with a second
/// one starting, that room has to be asked for again rather than shared.
///
/// **Kept at 5GB by measurement** (task2040,
/// `.agents/tasks/evidence/2010-heavy-throughput/`), which replaced the
/// unverified 14-17 MB/s planning estimate this constant shipped with. Every
/// rate below is a recorded 1920x1080 capture at `QUALITY_LEVEL` = 60, audio
/// track included:
///
/// - Real game footage (textured masonry, small UI text, continuous
///   camera motion): **703,698 B/s at 27.9 fps**, and **1,031,944 B/s at
///   45.5 fps** when the same footage is fed 1.8x faster. Rate does follow the
///   frame rate, sublinearly and without changing order: 1.63x the frames cost
///   1.47x the bytes. task1950's 697,232 B/s
///   (`.agents/tasks/evidence/1950-quality-real-content/`) is the same
///   measurement at the low end, reproduced within 0.9%.
/// - Lighter material lands below it: 557,939 B/s for a flat synthetic 1080p
///   page (task1720), ~272,600 B/s for a 1266x753 animating window (task1960).
///   Two of those at once stayed at ~272,600 B/s *each* — the per-stream rate
///   did not rise, which is why this reserve is stacked per running capture
///   rather than measured again for pairs.
///
/// Read the frame rate off `capture_frame_debug`'s `session_end arrived=`, not
/// `CaptureDiagnostics::measured_fps`: the latter counts frames *handed to the
/// encoder*, task163's held duplicates included, and read 56 fps on a run WGC
/// delivered 27.9 to (task2040).
///
/// At the worst real-content rate measured, 5GB is ~87 minutes for one stream
/// (~72 at that run's worst 5-second burst); at 28 fps it is ~127. That is the
/// reason to keep the value: it buys a run-up far longer than any plausible
/// "the second recording just started" horizon, while a bigger number would
/// start refusing second captures on machines with room to spare. It is
/// deliberately *not* sized to hold a whole `MAX_RETENTION_MINUTES` session —
/// that would be ~89GB (83 GiB) for a whole day at the 45 fps rate.
///
/// The bitrate settings **cannot** bound any of this: `encoder::bitrate_for`
/// returns 12/20/35 Mbps, but `src/encoder/hardware/mft.rs` asks the MFT for
/// `eAVEncCommonRateControlMode_Quality` first, where the bitrate is only
/// advice, and falls back to `UnconstrainedVBR`, which has no ceiling. Measured
/// directly (task2040): per-frame uniform noise, which defeats motion
/// estimation outright, writes **48,074,914 B/s** — 385 Mbit/s against a 12
/// Mbps "target", and 5GB in under two minutes. No start-time reserve can cover
/// that without refusing every second capture on every machine, and it is not
/// meant to: steady-state disk use is capped by `retention_minutes` pruning, so
/// this guard only decides anything under `NO_RETENTION_LIMIT` (task1410),
/// where growth is unbounded and no finite reserve is ever sufficient. The
/// worker's 10s free-space poll down to `MIN_FREE_BYTES` (task1420) is the real
/// backstop; the reserve only buys the run-up before it matters.
///
/// Not measured: 1440p/2160p sources, and frame rates above 45.5. Both displays
/// on the measuring machine are 1920x1080, and the only real-game material
/// available carries 28 unique frames per second (task2040).
pub const PER_CAPTURE_RESERVE_BYTES: u64 = 5 * 1024 * 1024 * 1024;
pub const GAP_THRESHOLD_100NS: i64 = 10_000_000;
const GAP_REASON: &str = "video frame interruption";

/// `NO_RETENTION_LIMIT` passes through untouched; anything else lands in
/// `MIN..=MAX`.
fn clamp_retention(retention_minutes: u16) -> u16 {
    if retention_minutes == NO_RETENTION_LIMIT {
        NO_RETENTION_LIMIT
    } else {
        retention_minutes.clamp(MIN_RETENTION_MINUTES, MAX_RETENTION_MINUTES)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SegmentRecord {
    pub index: u64,
    pub path: PathBuf,
    pub start_100ns: i64,
    pub end_100ns: i64,
    pub bytes: u64,
    // Added after MANIFEST_VERSION 2 shipped (Task066), same precedent as
    // `SessionManifest.target_title`: `#[serde(default)]` keeps existing
    // on-disk manifests (written before this field existed, or for any
    // segment whose thumbnail generation failed) loadable as `None` instead
    // of forcing another version bump. Session-relative like `path` itself
    // (Task067), resolved the same way via `RingBuffer::resolve_segment_path`.
    #[serde(default)]
    pub thumbnail_path: Option<PathBuf>,
    /// How far each audio track was shifted to make room for the priming
    /// access unit at its head (task204), one entry per track in track order:
    /// index 0 is the capture target (task1250). `#[serde(default)]` for the
    /// same reason as `thumbnail_path`, and the alias plus
    /// `scalar_or_list` keep manifests written when this was a single number
    /// loading as a list of one -- which is exactly what they meant.
    #[serde(
        default,
        alias = "audioOffset100ns",
        deserialize_with = "container::scalar_or_list"
    )]
    pub audio_offsets_100ns: Vec<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TimelineGap {
    pub start_100ns: i64,
    pub end_100ns: i64,
    pub reason: String,
}
/// A hotkey marker on the session timeline. `time_100ns` is the absolute
/// timeline coordinate and doubles as the identity used to update/delete and to
/// deduplicate against markers the frontend already showed from the
/// `review-marker-added` event (Task098). `label` stays empty until Task099's
/// editing UI.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MarkerRecord {
    pub time_100ns: i64,
    #[serde(default)]
    pub label: String,
    /// Which of the review panel's six colours this marker wore (round5 §4-B,
    /// task171) before t260927-7d08 moved markers to the DS's single
    /// `marker` colour -- the UI no longer reads this field. Kept for the
    /// on-disk format: `MARKER_PALETTE_LEN` still assigns one on write so an
    /// older build (or a future one that brings colour back) can read a
    /// consistent value, and `None` marks a marker written before this field
    /// existed.
    #[serde(default)]
    pub color_index: Option<u8>,
    /// Multi-line comment written on the review screen (t261009-0251), empty
    /// = none. Already normalised (`sessions::normalize_comment`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub comment: String,
}

impl MarkerRecord {
    /// A fresh marker at `time_100ns`: no label, no comment, and no colour --
    /// `merge_markers` (or the manifest on reload) assigns that in creation
    /// order (task171).
    pub fn at(time_100ns: i64) -> Self {
        Self {
            time_100ns,
            label: String::new(),
            color_index: None,
            comment: String::new(),
        }
    }
}

/// Adds a fresh marker at `time_100ns` to a time-sorted list, keeping it
/// sorted. `false` = one is already at that instant and nothing changed.
pub fn insert_marker(markers: &mut Vec<MarkerRecord>, time_100ns: i64) -> bool {
    if markers.iter().any(|marker| marker.time_100ns == time_100ns) {
        return false;
    }
    markers.push(MarkerRecord::at(time_100ns));
    markers.sort_by_key(|marker| marker.time_100ns);
    true
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SessionManifest {
    pub version: u32,
    pub session_id: String,
    pub closed: bool,
    pub retention_minutes: u16,
    pub segments: Vec<SegmentRecord>,
    pub gaps: Vec<TimelineGap>,
    // Added after MANIFEST_VERSION 2 shipped; `#[serde(default)]` keeps
    // existing on-disk manifests (written without this field) loadable
    // instead of forcing another version bump that would strand them.
    #[serde(default)]
    pub target_title: Option<String>,
    /// The executable name behind the captured window (task2350), as spelled on
    /// disk. `None` for a monitor recording and for everything recorded before
    /// the `TargetExecutableSet` record existed -- same `#[serde(default)]`
    /// reason as `target_title` above.
    #[serde(default)]
    pub target_executable: Option<String>,
    /// That executable's full image path (2026-09-20), for the review title
    /// bar's app icon. Same `#[serde(default)]` reason.
    #[serde(default)]
    pub target_executable_path: Option<String>,
    // Same `#[serde(default)]` precedent as `target_title` above (Task098): a
    // manifest written before markers existed loads with an empty vec.
    #[serde(default)]
    pub markers: Vec<MarkerRecord>,
    /// One line the user wrote about this recording (task167). Same
    /// `#[serde(default)]` reason as everything above it: a manifest written
    /// before notes existed has to keep loading.
    #[serde(default, rename = "note")]
    pub comment: Option<String>,
    /// Kept out of the retention sweep's reach (task167). Its bytes still
    /// count towards the capacity budget -- hiding what protection costs would
    /// make a full buffer inexplicable -- it is only never the thing deleted.
    #[serde(default)]
    pub protected: bool,
    /// The audio tracks this session recorded, in track order (task1250).
    /// Empty is the single-track shape everything had before, so old
    /// manifests keep loading.
    #[serde(default)]
    pub audio_tracks: Vec<crate::ring_buffer::container::AudioTrackInfo>,
}
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub session_id: String,
    pub start_100ns: Option<i64>,
    pub end_100ns: Option<i64>,
    pub segment_count: usize,
    pub total_bytes: u64,
    pub closed: bool,
    pub recoverable: bool,
    pub manifest_version: u32,
    pub readable: bool,
    pub target_title: Option<String>,
    // Added after MANIFEST_VERSION 2 shipped (Task069). Not the vec position:
    // it's the `SegmentRecord.index` of the first segment (in manifest order,
    // which `RingBuffer::add`/`open` keep sorted by `start_100ns`) that has a
    // `thumbnail_path`. Index 0 cannot be assumed representative because
    // `prune` deletes the oldest segments (and their `.jpg` sidecars) first,
    // so a retention-capped session's earliest surviving thumbnail is often a
    // higher index. `None` for a manifest-less session directory and for any
    // manifest with no thumbnailed segment (pre-Task066 recording, or every
    // thumbnail write having failed).
    #[serde(default)]
    pub thumbnail_index: Option<u64>,
    /// Straight off the manifest (task167). Absent for a session directory with
    /// no readable manifest -- there is nowhere for a note to have been stored.
    #[serde(default, rename = "note")]
    pub comment: Option<String>,
    #[serde(default)]
    pub protected: bool,
    /// The recorded app, copied off the manifest (t260927-9a17): the history's
    /// app column shows its icon and its name. `None` for a monitor recording
    /// and for a session with no readable manifest.
    #[serde(default)]
    pub target_executable: Option<String>,
    /// Where that executable was on disk at capture start, which is what its
    /// icon is read from. `None` for anything recorded before 2026-09-20.
    #[serde(default)]
    pub target_executable_path: Option<String>,
    /// How many markers the session carries (t260927-9a17), for the row's flag.
    #[serde(default)]
    pub marker_count: usize,
}

pub struct RingBuffer {
    /// The folder the container file sits in, which is what the review and
    /// export paths report as the session's root.
    root: PathBuf,
    /// The `.lvb` this session lives in. Every session is one since task450;
    /// the directory shape is gone (t261025-d118).
    container: PathBuf,
    manifest: SessionManifest,
    leases: HashMap<u64, usize>,
    /// Container sessions only: each segment's resolved record location, by
    /// index (task t260913-66a6). An index missing here still reads, through
    /// the container's own index.
    spans: HashMap<u64, ContainerSpans>,
    /// When `add` last folded a segment into this session, in this process
    /// (t260929-e171): the recorder's side of a live session's segment phase,
    /// which the review screen's load reads. Not in the manifest -- an
    /// `Instant` means nothing once persisted -- and `None` for a session
    /// nothing in this process has added to.
    last_fold: Option<Instant>,
    /// The open replayed the container and stopped short of its end
    /// (`ContainerReader::truncated`). `false` for a session this process
    /// created. Read by `sessions::container_summary` (t261101-6b34).
    scan_truncated: bool,
}

/// Where one segment's (or thumbnail's) bytes actually are, resolved while the
/// catalog lock is held and read after it is released (task440).
///
/// The read paths have always split those two steps -- disk IO under the
/// `sessions` mutex would serialise every concurrent fetch behind one slow
/// read -- so the container arm splits the same way.
///
/// The container arm carries the record's resolved [`container::Located`]
/// when the session knows it (task t260913-66a6): the writer hands one back
/// for every append, and `open_container` reads them out of the checkpoint it
/// opens anyway. The read after the lock is then one seek and a CRC check
/// instead of reopening the file and parsing its newest checkpoint, which cost
/// 26-231ms per read while the writer was appending (task3590).
///
/// This used to carry only the index, on the grounds that the container's own
/// index is what says where a record is *now*. A resolved span is safe anyway:
/// records never move once written (`ContainerWriter::prune` punches a hole in
/// place and nothing compacts); a pruned segment leaves `manifest.segments`
/// under the same lock, so no new target names it; and a span read that races
/// a hole punch reads bytes that fail the CRC, which falls back to the index
/// path rather than serving them. `index` stays for that fallback.
///
/// No path to guard: an index either is in the container's index or is not.
/// `span` is `None` when the session has no resolved location for it, and the
/// read then asks the index.
pub struct ReadTarget {
    pub path: PathBuf,
    pub index: u64,
    pub span: Option<container::Located>,
}

/// Where a container segment's record and its thumbnail's are, kept beside the
/// manifest rather than in `SegmentRecord` (task t260913-66a6): a span is
/// rebuilt from the `.lvb` on every open and never belongs in a serialised
/// manifest or in the equality the manifest's users compare on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContainerSpans {
    /// The record plus its fragment table (t261017-3f94), which is what lets a
    /// seek read from its GOP on -- see [`read_target_from`].
    pub data: container::SegmentLocation,
    pub thumbnail: Option<container::Located>,
}

/// The extension a container session's file carries, and the marker that a
/// buffer-root entry is one at all (task401).
pub const CONTAINER_EXTENSION: &str = "lvb";

/// A container session's id is its file stem, which keeps the two properties
/// the directory layout had: the id is the recording's start timestamp, and
/// sorting ids lexicographically sorts sessions chronologically.
pub fn container_session_id(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}

/// Whether a buffer-root entry names a container session.
pub fn is_container_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case(CONTAINER_EXTENSION))
}

/// A segment for a reader that starts at segment-local `from_100ns`
/// (t261017-3f94): the head plus the fragments from the target's GOP on when
/// the segment has a fragment table and the GOP is not its first, else the
/// whole segment. The `i64` is the earliest local time the bytes can serve --
/// just past the GOP's start, or 0 for a whole read.
///
/// A cut read that fails -- a fragment that does not verify, a table that does
/// not match its body -- is not served; the whole read below either serves
/// the segment or fails the same way, exactly as for a span that went stale.
pub fn read_target_from(
    target: &ReadTarget,
    fragments: Option<&container::FragmentTable>,
    from_100ns: i64,
) -> Result<(Vec<u8>, i64), String> {
    // t261018-fc75: why this seek reads whole, for the `cut=false` line.
    let mut cut_skip = "no_table";
    if let (
        ReadTarget {
            path,
            index,
            span: Some(span),
        },
        Some(table),
    ) = (target, fragments)
    {
        let started = Instant::now();
        let read = {
            crate::insight_scope!("container_read_span");
            container::read_located_from(path, *span, table, from_100ns)
        };
        match read {
            Ok(Some((bytes, from))) => {
                tracing::info!(
                    target: "task128_read_span",
                    index = *index,
                    bytes = bytes.len(),
                    record_bytes = span.record.len,
                    cut = true,
                    read_us = started.elapsed().as_micros() as u64,
                    "container span read"
                );
                return Ok((bytes, from));
            }
            Ok(None) => cut_skip = table.cut_or_skip(from_100ns).err().unwrap_or("whole"),
            Err(error) => {
                cut_skip = "crc";
                tracing::debug!(
                    event = "container_cut_fallback",
                    index = *index,
                    %error,
                )
            }
        }
    }
    read_whole(target, false, cut_skip).map(|bytes| (bytes, 0))
}

/// Reads what a [`ReadTarget`] points at. Deliberately a free function: every
/// caller resolves the target under the catalog lock and calls this after
/// dropping it.
///
/// `thumbnail` picks which of the segment's two records is read.
pub fn read_target(target: &ReadTarget, thumbnail: bool) -> Result<Vec<u8>, String> {
    read_whole(target, thumbnail, "whole")
}

/// [`read_target`], with the `cut_skip` its `cut=false` line carries
/// (t261018-fc75): `"whole"` for a read that never asked to be cut -- a
/// crossing, the read-ahead, an export, a preview -- or the reason a seek's
/// [`read_target_from`] fell back to it.
fn read_whole(
    target: &ReadTarget,
    thumbnail: bool,
    cut_skip: &'static str,
) -> Result<Vec<u8>, String> {
    let ReadTarget { path, index, span } = target;
    // The resolved span first (task t260913-66a6): open + seek + read +
    // CRC, nothing else. A span that does not verify is not served --
    // it drops to the index path below, which either finds the record
    // or fails the same CRC.
    if let Some(span) = span {
        let started = Instant::now();
        let read = if thumbnail {
            crate::insight_scope!("container_read_thumbnail_span");
            container::read_located_at(path, *span)
        } else {
            crate::insight_scope!("container_read_span");
            container::read_located_at(path, *span)
        };
        match read {
            Ok(bytes) => {
                // The one number `container_read_span` cannot carry:
                // a scope reports time, not size, so a slow seek could
                // not be told apart from a big segment (t260922).
                // Segments only -- thumbnail reads are four times as
                // many and their size is not what a seek waits on.
                if !thumbnail {
                    tracing::info!(
                        target: "task128_read_span",
                        index = *index,
                        bytes = bytes.len(),
                        // t261017-3f94: what a whole read takes, so a
                        // cut read's `bytes` reads against it.
                        record_bytes = span.record.len,
                        cut = false,
                        cut_skip = %cut_skip,
                        read_us = started.elapsed().as_micros() as u64,
                        "container span read"
                    );
                }
                return Ok(bytes);
            }
            Err(error) => tracing::debug!(
                event = "container_span_fallback",
                index = *index,
                thumbnail,
                %error,
            ),
        }
    }
    // No span, or one that did not verify: reopen, header + the newest
    // checkpoint record, which was a 1.13MB JSON snapshot once a 2h
    // ring is full (about a third of that as v3's MessagePack). Scoped
    // apart from the span read so the parse and the disk are separable
    // (task3460).
    let mut reader = {
        crate::insight_scope!("container_open_for_reading");
        container::ContainerReader::open_for_reading(path).map_err(|_| "container open failed")?
    };
    let read = |reader: &mut container::ContainerReader| {
        if thumbnail {
            crate::insight_scope!("container_read_thumbnail");
            reader.read_thumbnail(*index)
        } else {
            crate::insight_scope!("container_read_segment");
            reader.read_segment(*index)
        }
    };
    let failed = if thumbnail {
        "thumbnail read failed"
    } else {
        "segment read failed"
    };
    match read(&mut reader) {
        // A recording checkpoints once per interval of media
        // (t261012-fe57), so its newest segments are not in the
        // snapshot yet. Replaying past the checkpoint finds them: at
        // most an interval of records, not the file.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound && reader.checkpoint_end().is_some() =>
        {
            let mut replayed = {
                crate::insight_scope!("container_open_replay");
                container::ContainerReader::open(path).map_err(|_| failed.to_owned())?
            };
            read(&mut replayed).map_err(|_| failed.to_owned())
        }
        result => result.map_err(|_| failed.to_owned()),
    }
}

fn derive_gaps(segments: &[SegmentRecord]) -> Vec<TimelineGap> {
    segments
        .windows(2)
        .filter_map(|pair| {
            let start_100ns = pair[0].end_100ns;
            let end_100ns = pair[1].start_100ns;
            (end_100ns - start_100ns >= GAP_THRESHOLD_100NS).then(|| TimelineGap {
                start_100ns,
                end_100ns,
                reason: GAP_REASON.into(),
            })
        })
        .collect()
}

impl RingBuffer {
    /// The index *view* of a container recording that is starting now
    /// (task450).
    ///
    /// Deliberately writes nothing: `ContainerWriter::create` already laid down
    /// the header, and from here on the only thing allowed to touch the `.lvb`
    /// is the index writer thread that owns that writer. This side exists so
    /// the catalog, the history list and LiveReview can read a recording in
    /// progress through the same `segment_target` path a closed container uses.
    pub fn create_container(
        path: PathBuf,
        session_id: String,
        retention_minutes: u16,
        target_title: Option<String>,
        target_executable: Option<String>,
    ) -> Self {
        let retention_minutes = clamp_retention(retention_minutes);
        Self {
            root: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            container: path,
            manifest: SessionManifest {
                version: MANIFEST_VERSION,
                audio_tracks: Vec::new(),
                session_id,
                closed: false,
                retention_minutes,
                segments: vec![],
                gaps: vec![],
                target_title,
                target_executable,
                target_executable_path: None,
                markers: vec![],
                comment: None,
                protected: false,
            },
            leases: HashMap::new(),
            spans: HashMap::new(),
            last_fold: None,
            scan_truncated: false,
        }
    }

    /// The live manifest's copy of the image path the container's
    /// `TargetExecutableSet` record carries (2026-09-20) -- the second wire,
    /// same reason as `target_executable` in `create_container`.
    pub fn with_target_executable_path(mut self, path: Option<String>) -> Self {
        self.manifest.target_executable_path = path;
        self
    }

    /// Opens a `.lvb` container session read-side (task440).
    ///
    /// The manifest it hands back is a **view**, built from the container's own
    /// index, and is never written anywhere: a container keeps its state in
    /// `Meta` records, so there is no `manifest.json` for this to round-trip
    /// to. That is why `SegmentRecord::path` is left empty here -- for a
    /// container the index *is* the address, and the only reader that would
    /// have followed a path (`resolve_segment_path`) is not on this arm.
    /// `thumbnail_path` stays an `Option` because callers only ask whether a
    /// thumbnail exists; `Some(empty)` says yes without inventing a filename.
    pub fn open_container(path: PathBuf) -> io::Result<Self> {
        let reader = container::ContainerReader::open_for_reading(&path)?;
        let snapshot = reader.snapshot();
        let spans = snapshot
            .segments
            .iter()
            .map(|segment| {
                (
                    segment.index,
                    ContainerSpans {
                        data: container::SegmentLocation {
                            located: segment.data,
                            fragments: segment.fragments.clone(),
                        },
                        thumbnail: segment.thumbnail,
                    },
                )
            })
            .collect();
        let mut manifest = SessionManifest {
            version: MANIFEST_VERSION,
            audio_tracks: snapshot.audio_tracks.clone(),
            session_id: container_session_id(&path),
            closed: snapshot.closed,
            retention_minutes: reader.header().retention_minutes,
            segments: snapshot
                .segments
                .iter()
                .map(|segment| SegmentRecord {
                    index: segment.index,
                    path: PathBuf::new(),
                    start_100ns: segment.start_100ns,
                    end_100ns: segment.end_100ns,
                    bytes: segment.data.body.len,
                    thumbnail_path: segment.thumbnail.map(|_| PathBuf::new()),
                    audio_offsets_100ns: segment.audio_offsets_100ns.clone(),
                })
                .collect(),
            gaps: vec![],
            target_title: snapshot.title.clone(),
            target_executable: snapshot.target_executable.clone(),
            target_executable_path: snapshot.target_executable_path.clone(),
            markers: snapshot
                .markers
                .iter()
                .map(|marker| MarkerRecord {
                    time_100ns: marker.time_100ns,
                    label: marker.label.clone(),
                    // The container stores the palette slot wide; the manifest
                    // has always kept it in a byte, and the palette is six
                    // colours, so anything that would not fit is corrupt data
                    // rather than a wider palette.
                    color_index: marker
                        .palette_index
                        .and_then(|slot| u8::try_from(slot).ok()),
                    comment: marker.comment.clone(),
                })
                .collect(),
            comment: snapshot.comment.clone(),
            protected: snapshot.protected,
        };
        manifest.gaps = derive_gaps(&manifest.segments);
        let root = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Ok(Self {
            root,
            container: path,
            manifest,
            leases: HashMap::new(),
            spans,
            last_fold: None,
            scan_truncated: reader.truncated(),
        })
    }

    /// See the field (t260929-e171).
    pub fn last_fold(&self) -> Option<Instant> {
        self.last_fold
    }

    /// See the field (t261101-6b34).
    pub(crate) fn scan_truncated(&self) -> bool {
        self.scan_truncated
    }

    pub fn manifest(&self) -> &SessionManifest {
        &self.manifest
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.lvb` backing this session, for the callers that need the file
    /// itself rather than bytes: export staging and "show in folder".
    pub fn container_path(&self) -> &Path {
        &self.container
    }

    /// Where this session's `index` segment is, for a caller that will read it
    /// after releasing the catalog lock. `None` when the index is not in the
    /// manifest at all.
    pub fn segment_target(&self, index: u64) -> Option<ReadTarget> {
        if !self.contains(index) {
            return None;
        }
        Some(ReadTarget {
            path: self.container.clone(),
            index,
            span: self.spans.get(&index).map(|spans| spans.data.located),
        })
    }

    /// The fragment table of a container segment (t261017-3f94), for a read
    /// that starts at a seek target. `None` for a segment whose body has none,
    /// or one whose location this session never resolved.
    pub fn segment_fragments(&self, index: u64) -> Option<container::FragmentTable> {
        self.spans.get(&index)?.data.fragments.clone()
    }

    /// The same for a segment's thumbnail. `None` when the segment has none.
    pub fn thumbnail_target(&self, index: u64) -> Option<ReadTarget> {
        let segment = self
            .manifest
            .segments
            .iter()
            .find(|segment| segment.index == index)?;
        segment.thumbnail_path.as_ref()?;
        Some(ReadTarget {
            path: self.container.clone(),
            index,
            span: self.spans.get(&index).and_then(|spans| spans.thumbnail),
        })
    }
    /// Resolves a `SegmentRecord.path` against this session's directory.
    ///
    /// `SegmentRecord.path` is session-directory-relative for every session
    /// written since Task067 (live recordings store `file_name()` only, same
    /// as `recover_session` always did) — this keeps a manifest portable
    /// across `%LOCALAPPDATA%` moving (user rename, different machine).
    /// Sessions written before Task067 still have an *absolute* path on disk,
    /// and `Path::join` already does the right thing for both: joining an
    /// absolute path onto any base simply returns that absolute path
    /// unchanged, so no version bump or on-disk migration is needed to keep
    /// reading them.
    pub fn resolve_segment_path(&self, record_path: &Path) -> PathBuf {
        self.root.join(record_path)
    }
    pub fn contains(&self, index: u64) -> bool {
        self.manifest
            .segments
            .iter()
            .any(|segment| segment.index == index)
    }
    pub fn add(&mut self, record: SegmentRecord) -> io::Result<()> {
        self.manifest
            .segments
            .retain(|segment| segment.index != record.index);
        self.manifest.segments.push(record);
        self.manifest.segments.sort_by_key(|s| s.start_100ns);
        self.last_fold = Some(Instant::now());
        self.refresh_gaps();
        Ok(())
    }
    /// `add` for a segment the index writer just appended to this session's
    /// container, with the locations the writer handed back (task t260913-66a6),
    /// so reads of it skip reopening the file.
    pub fn add_container_segment(
        &mut self,
        record: SegmentRecord,
        spans: ContainerSpans,
    ) -> io::Result<()> {
        self.spans.insert(record.index, spans);
        self.add(record)
    }
    pub fn lease(&mut self, index: u64) {
        *self.leases.entry(index).or_default() += 1
    }
    pub fn release(&mut self, index: u64) {
        if let Some(n) = self.leases.get_mut(&index) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.leases.remove(&index);
            }
        }
    }
    pub fn has_leases(&self) -> bool {
        !self.leases.is_empty()
    }
    /// A container keeps no `manifest.json`, so a rename has to land as a
    /// `TitleSet` record inside the file. The manifest write it once relied on
    /// wrote nothing for a container, which made this setter a **silent no-op** for
    /// every container session the catalog happened to hold in memory -- i.e.
    /// any session the running process had listed or opened (task560).
    ///
    /// Appending from here cannot collide with the index writer: the one ring
    /// that thread owns is the session being recorded, and
    /// `CaptureController::set_session_title` refuses that one before it ever
    /// reaches a `RingBuffer`.
    ///
    /// Normalising here rather than at each caller is what keeps the two doors
    /// (this one and `sessions::set_session_title`) from storing different
    /// values for the same typed name.
    pub fn set_target_title(&mut self, title: Option<String>) -> io::Result<()> {
        let title = title.as_deref().and_then(sessions::normalize_target_title);
        self.manifest.target_title = title.clone();
        self.commit(container::MetaEvent::TitleSet { title })
    }
    /// Writes an edit already applied to `manifest` (t261025-00ad) as a `Meta`
    /// record -- the setters below were once silent no-ops for want of it
    /// (task560 / task720).
    fn commit(&mut self, event: container::MetaEvent) -> io::Result<()> {
        sessions::append_meta(&self.container, event)
    }
    /// The in-memory twin of `sessions::set_session_comment` (task167). A session
    /// resident in the catalog has to be edited through its ring, or the
    /// process would keep showing the old value.
    ///
    /// Same case as `set_target_title`'s (task560): this was once a **silent
    /// no-op** for every container the catalog held -- which is every session
    /// the running process had listed or opened (task720).
    ///
    /// Unlike a rename, a note is allowed on the session being recorded, and
    /// that one must not come through here: `sessions::append_meta` reopens the
    /// file at its last checkpoint and truncates everything after it, which
    /// would cut the recording's own appends off. `CaptureController` routes
    /// the live session to the index writer's queue instead and only calls this
    /// for a session nothing is writing to.
    pub fn set_comment(&mut self, comment: Option<String>) -> io::Result<()> {
        let event = self.stage_comment(comment);
        self.commit(event)
    }
    /// See `set_comment` -- same two doors, same live-session caveat, and the same
    /// reason it matters more here: `protected` is what `sessions_to_reclaim`
    /// reads off disk to decide what the capacity sweep may delete, so a
    /// protection that never reached the file protects nothing (task720).
    pub fn set_protected(&mut self, protected: bool) -> io::Result<()> {
        let event = self.stage_protected(protected);
        self.commit(event)
    }
    /// The live-session half of the two setters above: update what the process
    /// shows without touching the file. The matching `MetaEvent` goes on
    /// `pending_meta`, and the index writer -- the only thread holding that
    /// container's writer -- appends it on its next pass (task720, same
    /// arrangement `push_marker` uses).
    pub fn stage_comment(&mut self, comment: Option<String>) -> container::MetaEvent {
        let comment = comment
            .as_deref()
            .and_then(sessions::normalize_session_comment);
        self.manifest.comment = comment.clone();
        container::MetaEvent::CommentSet { comment }
    }
    /// The same split for the recording's audio track list (t261002-577c):
    /// set at start when there are extra sounds, and again whenever one is
    /// switched on mid-recording.
    pub fn stage_audio_tracks(
        &mut self,
        tracks: Vec<container::AudioTrackInfo>,
    ) -> container::MetaEvent {
        self.manifest.audio_tracks.clone_from(&tracks);
        container::MetaEvent::AudioTracksSet { tracks }
    }
    pub fn stage_protected(&mut self, protected: bool) -> container::MetaEvent {
        self.manifest.protected = protected;
        container::MetaEvent::ProtectedSet { protected }
    }
    /// The same split for a marker edit on the session being recorded
    /// (task3960): rewrite what the process shows, hand the `MetaEvent` back
    /// for the index writer to append, and touch no file here.
    ///
    /// `None` means the manifest has no marker at that instant, which is the
    /// caller's cue to report the same `NotFound` `update_marker` /
    /// `delete_marker` report -- either the id is wrong, or the marker is still
    /// in the index writer's queue and belongs to that arm instead
    /// (`CaptureController::edit_marker_at`).
    ///
    /// `comment`: `None` leaves the marker's comment alone, `Some` replaces it
    /// (`Some("")` clears) -- `MetaEvent::MarkerRenamed`'s rule.
    pub fn stage_marker_renamed(
        &mut self,
        time_100ns: i64,
        label: String,
        comment: Option<String>,
    ) -> Option<container::MetaEvent> {
        let marker = self
            .manifest
            .markers
            .iter_mut()
            .find(|marker| marker.time_100ns == time_100ns)?;
        marker.label = label.clone();
        if let Some(comment) = &comment {
            marker.comment.clone_from(comment);
        }
        Some(container::MetaEvent::MarkerRenamed {
            time_100ns,
            label,
            comment,
        })
    }
    pub fn stage_marker_removed(&mut self, time_100ns: i64) -> Option<container::MetaEvent> {
        if !self
            .manifest
            .markers
            .iter()
            .any(|marker| marker.time_100ns == time_100ns)
        {
            return None;
        }
        self.manifest
            .markers
            .retain(|marker| marker.time_100ns != time_100ns);
        Some(container::MetaEvent::MarkerRemoved { time_100ns })
    }
    /// Folds queued markers into the manifest.
    ///
    /// Returns the `MarkerAdded` event for each marker it actually took, in
    /// creation order. A container has no `manifest.json`, so those events
    /// *are* the write -- and only the index writer thread may append them for
    /// the session it records (`stage_comment` has the same split). Ignoring the
    /// return value is what lost every hotkey marker a recording took: the
    /// manifest had them, the `.lvb` never did (task1630).
    pub fn merge_markers(
        &mut self,
        markers: Vec<MarkerRecord>,
    ) -> io::Result<Vec<container::MetaEvent>> {
        if markers.is_empty() {
            return Ok(Vec::new());
        }
        let mut added = Vec::new();
        for mut marker in markers {
            // `time_100ns` is identity: re-adding the same instant must not
            // duplicate it, and must not clobber a label already set.
            if !self
                .manifest
                .markers
                .iter()
                .any(|existing| existing.time_100ns == marker.time_100ns)
            {
                // Creation order, not timeline order: the colour is decided
                // here, before the sort below moves the marker to wherever its
                // instant belongs, and travels with it from then on (task171).
                marker.color_index = marker.color_index.or(Some(
                    (self.manifest.markers.len() % crate::MARKER_PALETTE_LEN) as u8,
                ));
                added.push(container::MetaEvent::MarkerAdded {
                    time_100ns: marker.time_100ns,
                    label: marker.label.clone(),
                    palette_index: marker.color_index.map(usize::from),
                    // A comment written while the marker was still queued
                    // (t261009-0251) reaches the container only here.
                    comment: (!marker.comment.is_empty()).then(|| marker.comment.clone()),
                });
                self.manifest.markers.push(marker);
            }
        }
        self.manifest
            .markers
            .sort_by_key(|marker| marker.time_100ns);
        Ok(added)
    }
    /// The review pane's add for a session the catalog holds in memory --
    /// which is every closed session, `load_closed_sessions` inserts them all
    /// at startup. `merge_markers` only *hands back* the `MarkerAdded` record
    /// because the recording session's copy belongs to the index writer
    /// (task1630); a closed container has no such thread, so the append
    /// happens here and the edit finally reaches the file (task1670).
    ///
    /// Single writer still holds, `set_comment`'s argument: the one ring the
    /// index writer owns is the session being recorded, and
    /// `CaptureController::edit_marker` still refuses *this* entry point for
    /// that one (adding mid-recording is task1560's). Rename and delete do
    /// reach a live `RingBuffer` since task3960, but through
    /// `stage_marker_renamed` / `stage_marker_removed`, which append nothing --
    /// the `MetaEvent` goes to the index writer.
    ///
    /// `open_write` takes the file with `FILE_SHARE_READ` only, so a second
    /// writer cannot even open it -- a collision would fail loudly rather than
    /// interleave.
    pub fn add_marker(&mut self, time_100ns: i64) -> io::Result<()> {
        let events = self.merge_markers(vec![MarkerRecord::at(time_100ns)])?;
        for event in events {
            self.commit(event)?;
        }
        Ok(())
    }
    /// `comment` as in `stage_marker_renamed`.
    pub fn update_marker(
        &mut self,
        time_100ns: i64,
        label: String,
        comment: Option<String>,
    ) -> io::Result<()> {
        let Some(index) = self
            .manifest
            .markers
            .iter()
            .position(|marker| marker.time_100ns == time_100ns)
        else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such marker"));
        };
        // Disk first, then memory: a failed append leaves the two agreeing and
        // the caller reporting an error, rather than a rename that shows in
        // the UI and is gone at the next launch (task1670).
        self.commit(container::MetaEvent::MarkerRenamed {
            time_100ns,
            label: label.clone(),
            comment: comment.clone(),
        })?;
        self.manifest.markers[index].label = label;
        if let Some(comment) = comment {
            self.manifest.markers[index].comment = comment;
        }
        Ok(())
    }
    pub fn delete_marker(&mut self, time_100ns: i64) -> io::Result<()> {
        if !self
            .manifest
            .markers
            .iter()
            .any(|marker| marker.time_100ns == time_100ns)
        {
            return Err(io::Error::new(io::ErrorKind::NotFound, "no such marker"));
        }
        // Disk first, as in `update_marker`.
        self.commit(container::MetaEvent::MarkerRemoved { time_100ns })?;
        self.manifest
            .markers
            .retain(|marker| marker.time_100ns != time_100ns);
        Ok(())
    }
    /// Drops everything older than the retention window.
    ///
    /// `retention_minutes == NO_RETENTION_LIMIT` means there is no window: the
    /// session keeps its head and grows for as long as it records (task1410).
    /// The value is fixed when the session starts, so flipping the switch
    /// mid-recording changes nothing until the next one.
    ///
    /// Returns the lowest index still wanted when anything was dropped
    /// (task450), because a container frees space by punching a hole rather
    /// than by deleting files, and only the index writer thread owns the
    /// `ContainerWriter` that can do it.
    pub fn prune(&mut self) -> io::Result<Option<u64>> {
        if self.manifest.retention_minutes == NO_RETENTION_LIMIT {
            return Ok(None);
        }
        let Some(last) = self.manifest.segments.last().map(|s| s.end_100ns) else {
            return Ok(None);
        };
        let cutoff = last - i64::from(self.manifest.retention_minutes) * 60 * 10_000_000;
        let mut kept = Vec::new();
        let mut dropped_any = false;
        for s in self.manifest.segments.drain(..) {
            // Nothing to unlink for a dropped one: the bytes live inside the
            // one file and come back to the filesystem through
            // `ContainerWriter::prune`.
            if s.end_100ns < cutoff && !self.leases.contains_key(&s.index) {
                dropped_any = true;
            } else {
                kept.push(s)
            }
        }
        let keep_from = dropped_any.then(|| kept.first().map(|s| s.index)).flatten();
        self.manifest.segments = kept;
        // Bookkeeping only (task t260913-66a6): a dropped segment's span goes
        // with it, so the table does not grow for the life of a recording.
        if dropped_any && !self.spans.is_empty() {
            let kept: std::collections::HashSet<u64> =
                self.manifest.segments.iter().map(|s| s.index).collect();
            self.spans.retain(|index, _| kept.contains(index));
        }
        // Markers age out on the same cutoff as the video they point at
        // (Task098). Keeping them would leave the timeline advertising
        // positions whose segments have been deleted, and would grow the
        // manifest without bound on a long session.
        //
        // From the first *kept* segment's start, not `cutoff`: that is what
        // the replayed `Pruned` record keeps (`container::age_out_markers`),
        // and the first kept segment can start before `cutoff`. Two bases
        // dropped a marker here that came back after a restart (t261101-6b34).
        if let Some(first) = self.manifest.segments.first() {
            let kept_from = first.start_100ns;
            self.manifest
                .markers
                .retain(|marker| marker.time_100ns >= kept_from);
        }
        self.refresh_gaps();
        Ok(keep_from)
    }
    /// The in-memory close. The `Closed` record itself is the writer thread's,
    /// through `ContainerWriter::close` (計画書 §3.6).
    pub fn close(&mut self) -> io::Result<SessionManifest> {
        self.manifest.closed = true;
        self.refresh_gaps();
        Ok(self.manifest.clone())
    }
    /// Re-derives the gaps after the segment list changed. Writes nothing: a
    /// container's own `Meta` records and checkpoint are the state, and the
    /// index writer thread that owns the `ContainerWriter` is the only thing
    /// allowed to append them (task430).
    fn refresh_gaps(&mut self) {
        self.manifest.gaps = derive_gaps(&self.manifest.segments);
    }
}
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    let mut wide = path.to_path_buf();
    wide.push("");
    let w: Vec<u16> = wide.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut free = 0;
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(w.as_ptr()), Some(&mut free), None, None)
            .map_err(|e| io::Error::other(e.to_string()))?
    };
    Ok(free)
}
mod sessions;
pub use sessions::*;

pub mod container;

pub mod store;

#[cfg(test)]
pub(crate) mod tests;
