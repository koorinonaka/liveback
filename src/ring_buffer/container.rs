//! The single-file session container, `.lvb` (task401, plan Phase 1).
//!
//! A session is a directory today: hundreds of `segment-*.mp4`, as many
//! `.jpg` sidecars, and a `manifest.json` rewritten in full every two
//! seconds. This module is the format that replaces it with one file, built
//! and tested on its own -- **nothing in the app uses it yet**. Capture,
//! playback and export are untouched; the only callers are this module's
//! tests and, later, the Phase 2 tasks.
//!
//! ## Shape
//!
//! ```text
//! [ FileHeader, 4 KiB fixed ]  magic, version, session id, retention,
//!                              checkpoint slots A and B
//! [ Record ][ Record ][ ... ]  append-only, never rewritten in place
//! ```
//!
//! A record is `type | flags | payload_len | crc32(payload)` and then the
//! payload. Records are only ever appended, so a reader that has an offset
//! keeps it forever -- which is what makes deletion a *hole punch* rather
//! than a compaction: freeing the front of the file leaves every surviving
//! record exactly where it was.
//!
//! ## Why two checkpoint slots
//!
//! A checkpoint is itself an ordinary appended record; the header only points
//! at it. Writing that pointer is the one non-append the format has, so it is
//! done to the *older* of two slots and each slot carries its own generation
//! and CRC. A crash during the pointer write can therefore corrupt at most the
//! slot that was already stale, and the reader takes the newest slot whose CRC
//! is intact. The order is fixed and load-bearing: **append the checkpoint
//! record first, then update a slot** (plan §3.2, R10).
//!
//! ## Why truncation is ordinary
//!
//! A recording ends by having the machine crash at least as often as by being
//! stopped, so a half-written record is a normal end state, not damage. The
//! reader scans forward from the last good checkpoint, checking CRCs, and
//! stops at the first record that does not verify -- everything before it is
//! intact and everything after it is discarded. There is no repair pass, and
//! no input makes the reader panic.
//!
//! Deviations from the plan document, both settled when this task was written:
//! the extension and magic are `.lvb` / `LVBF` rather than the pre-rename
//! `.rsb` / `RSBF`, and there is no `FlushFileBuffers` option -- the current
//! directory format fsyncs nothing either, so adding one here would be a new
//! guarantee rather than a preserved one.

use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

pub const MAGIC: [u8; 4] = *b"LVBF";
/// v2 (task1250) widened the segment prefix from one audio offset to a
/// per-track list, so the mp4 body no longer starts at a fixed byte. The
/// reader still understands v1 -- protected sessions outlive any retention
/// window, so old containers have to keep opening forever.
pub const FORMAT_VERSION: u32 = 2;
pub const EXTENSION: &str = "lvb";

/// Fixed, so every record offset is stable from the moment it is written --
/// a growing header would move the whole file.
pub const HEADER_LEN: u64 = 4096;

const SESSION_ID_MAX: usize = 128;
const SESSION_ID_OFFSET: usize = 14;
const SLOT_A_OFFSET: usize = 256;
const SLOT_B_OFFSET: usize = 320;
/// generation(8) + offset(8) + len(8) + crc32(4), padded.
const SLOT_LEN: usize = 32;
const SLOT_BODY_LEN: usize = 24;

/// `type(1) | flags(1) | payload_len(4) | crc32(4)`.
///
/// This and the four items below it (`Slot`, `decode_header`, `read_record_at`)
/// are `pub` only because `crates/livia-thumb` includes this file to read the
/// same format from an `IStream` (task710). Nothing in `livia` needs them
/// outside this module.
pub const RECORD_HEADER_LEN: usize = 10;

/// A payload no honest record reaches. A corrupt length field is otherwise an
/// allocation request, and this reader is pointed at files a crash wrote.
const MAX_PAYLOAD_LEN: u32 = 256 * 1024 * 1024;

// ---------- crc32 ----------

/// CRC-32/ISO-HDLC, the one zip and png use. `crc32fast` computes the same
/// polynomial on the SSE4.2 CRC instruction where the byte-at-a-time table this
/// replaced could not; it was already in the tree as a transitive dependency, so
/// taking it directly costs nothing. The check value is asserted in `tests`.
///
/// `crates/livia-thumb` includes this file by `#[path]` rather than depending on
/// `livia`, so **any dependency this file takes has to be declared in that
/// crate's manifest too** -- both are listed there next to this reason.
pub fn crc32(bytes: &[u8]) -> u32 {
    crc32fast::hash(bytes)
}

// ---------- records ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordKind {
    Segment,
    Thumbnail,
    Meta,
    Checkpoint,
}

impl RecordKind {
    fn code(self) -> u8 {
        match self {
            RecordKind::Segment => 1,
            RecordKind::Thumbnail => 2,
            RecordKind::Meta => 3,
            RecordKind::Checkpoint => 4,
        }
    }

    fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(RecordKind::Segment),
            2 => Some(RecordKind::Thumbnail),
            3 => Some(RecordKind::Meta),
            4 => Some(RecordKind::Checkpoint),
            _ => None,
        }
    }
}

/// Where a payload lives, so a reader can serve it with one seek and one read
/// instead of holding the bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Span {
    pub offset: u64,
    pub len: u64,
}

/// Where one record's bytes are and what they should hash to.
///
/// The CRC travels with the location on purpose. A checkpoint lets a reader
/// skip re-verifying everything written before it -- that is what makes
/// opening a long session cheap -- but it also means rot in an old record
/// would go unnoticed until someone read it. So the check moves to the read:
/// `body` is only ever handed out after `record` has been hashed and matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Located {
    /// The whole record payload, which is what the CRC covers.
    pub record: Span,
    /// The useful part of it, past the fixed fields.
    pub body: Span,
    pub crc: u32,
}

/// One segment as the container knows it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentEntry {
    pub index: u64,
    pub start_100ns: i64,
    pub end_100ns: i64,
    /// One priming offset per audio track, in track order: index 0 is the
    /// capture target, the rest follow the settings order (task1250). A v1
    /// container reads as a list of one, which is what it always meant.
    ///
    /// The alias is for checkpoints, not for segment records: a checkpoint is
    /// this struct as JSON, so a container written before task1250 carries the
    /// old scalar field name inside it. Without the alias every old container
    /// would fail to load its checkpoint and replay from byte one -- correct,
    /// but slower for no reason.
    #[serde(
        default,
        alias = "audio_offset_100ns",
        deserialize_with = "scalar_or_list"
    )]
    pub audio_offsets_100ns: Vec<i64>,
    pub data: Located,
    pub thumbnail: Option<Located>,
}

/// Accepts either the pre-task1250 single number or the list that replaced it.
fn scalar_or_list<'de, D>(deserializer: D) -> Result<Vec<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(i64),
        Many(Vec<i64>),
    }
    Ok(match OneOrMany::deserialize(deserializer)? {
        OneOrMany::One(offset) => vec![offset],
        OneOrMany::Many(offsets) => offsets,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarkerEntry {
    pub time_100ns: i64,
    pub label: String,
    pub palette_index: Option<usize>,
}

/// Everything that is not segment bytes: the edits a session accumulates.
/// Serialised as JSON because `serde_json` is already here and these are rare,
/// small and want to stay readable in a dump.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum MetaEvent {
    MarkerAdded {
        time_100ns: i64,
        label: String,
        palette_index: Option<usize>,
    },
    MarkerRenamed {
        time_100ns: i64,
        label: String,
    },
    MarkerRemoved {
        time_100ns: i64,
    },
    TitleSet {
        title: Option<String>,
    },
    /// The executable behind the captured window, as spelled on disk
    /// (task2350). `None`/absent for a monitor recording and for everything
    /// recorded before this record existed. Mirrors `TitleSet`: written once at
    /// recording start, last one wins.
    TargetExecutableSet {
        executable: Option<String>,
        /// The executable's full image path (2026-09-20), so the review
        /// title bar can draw the app's icon after the app has exited.
        /// Absent in every record written before it existed; an older reader
        /// ignores it (no `deny_unknown_fields`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    NoteSet {
        note: Option<String>,
    },
    ProtectedSet {
        protected: bool,
    },
    /// Retention dropped every segment before `keep_from_index`. The bytes may
    /// or may not have been punched out yet; this is the logical statement.
    Pruned {
        keep_from_index: u64,
    },
    /// Which audio tracks this session records, in track order (task1250).
    /// Index 0 is the capture target; the rest follow the settings order.
    /// Last one wins, like every other setter here.
    AudioTracksSet {
        tracks: Vec<AudioTrackInfo>,
    },
    /// The last record of a cleanly finished session. Its absence is what
    /// makes a container a recovery candidate -- the job `recording.marker`
    /// does for a session directory.
    Closed,
}

/// One audio track identity. Only the executable name: the display name is
/// whatever the picker resolved at the time, and a recording keeps what it
/// recorded rather than following a process that has since exited.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioTrackInfo {
    /// The application's executable name; for a microphone track, the
    /// device's name as it was when the recording added it.
    pub executable_name: String,
    /// Set on a microphone track (t261002-577c): the device id it records,
    /// or the empty string for the Windows default. Absent on every
    /// application track and in every container written before microphones,
    /// so those read exactly as they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<String>,
}

/// The state a reader would otherwise have to replay the whole file to know.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub segments: Vec<SegmentEntry>,
    pub markers: Vec<MarkerEntry>,
    /// Empty means the single-track shape every recording had before
    /// task1250: the capture target and nothing else.
    #[serde(default)]
    pub audio_tracks: Vec<AudioTrackInfo>,
    pub title: Option<String>,
    /// The capture target's executable name (task2350). `None` for a monitor
    /// recording and for every container written before the record existed.
    #[serde(default)]
    pub target_executable: Option<String>,
    /// That executable's full image path (2026-09-20). `None` wherever
    /// `target_executable` is, and for every container written before it.
    #[serde(default)]
    pub target_executable_path: Option<String>,
    pub note: Option<String>,
    pub protected: bool,
    pub closed: bool,
}

impl Snapshot {
    fn apply(&mut self, event: &MetaEvent) {
        match event {
            MetaEvent::MarkerAdded {
                time_100ns,
                label,
                palette_index,
            } => {
                // Same position twice is one marker, as the review screen's
                // add already behaves.
                if let Some(existing) = self
                    .markers
                    .iter_mut()
                    .find(|marker| marker.time_100ns == *time_100ns)
                {
                    existing.label.clone_from(label);
                    existing.palette_index = *palette_index;
                } else {
                    self.markers.push(MarkerEntry {
                        time_100ns: *time_100ns,
                        label: label.clone(),
                        palette_index: *palette_index,
                    });
                    self.markers.sort_by_key(|marker| marker.time_100ns);
                }
            }
            MetaEvent::MarkerRenamed { time_100ns, label } => {
                if let Some(existing) = self
                    .markers
                    .iter_mut()
                    .find(|marker| marker.time_100ns == *time_100ns)
                {
                    existing.label.clone_from(label);
                }
            }
            MetaEvent::MarkerRemoved { time_100ns } => {
                self.markers
                    .retain(|marker| marker.time_100ns != *time_100ns);
            }
            MetaEvent::TitleSet { title } => self.title.clone_from(title),
            MetaEvent::TargetExecutableSet { executable, path } => {
                self.target_executable.clone_from(executable);
                self.target_executable_path.clone_from(path);
            }
            MetaEvent::NoteSet { note } => self.note.clone_from(note),
            MetaEvent::ProtectedSet { protected } => self.protected = *protected,
            MetaEvent::Pruned { keep_from_index } => {
                self.segments
                    .retain(|segment| segment.index >= *keep_from_index);
                age_out_markers(&mut self.markers, self.segments.first());
            }
            MetaEvent::AudioTracksSet { tracks } => self.audio_tracks.clone_from(tracks),
            MetaEvent::Closed => self.closed = true,
        }
    }

    /// The byte range every dropped segment used to occupy, ready to punch,
    /// run on up to the first survivor's record header (t260913-e20f): the
    /// meta and checkpoint records a pass writes after its segment sit there,
    /// and a steady one-segment prune would otherwise never cover them.
    /// `ContainerWriter::prune` keeps the checkpoints its slots name out of it.
    /// Contiguous because segments are appended in order, so the whole of a
    /// pruned prefix is one range.
    pub fn prunable_span(&self, before: &Snapshot) -> Option<Span> {
        // Both index-ascending, so the dropped segments are `before`'s prefix
        // below the first survivor (t260913-fff8).
        let dropped = self.segments.first().map_or(before.segments.len(), |kept| {
            before
                .segments
                .partition_point(|segment| segment.index < kept.index)
        });
        Some(self.span_over(dropped_extent(&before.segments[..dropped])?))
    }

    /// `prunable_span` once the dropped records' extent is known.
    fn span_over(&self, (start, end): (u64, u64)) -> Span {
        // Never narrower than the dropped records themselves, should a
        // survivor ever sit before them.
        let end = self.segments.first().map_or(end, |kept| {
            let record = kept.thumbnail.map_or(kept.data.record.offset, |thumb| {
                thumb.record.offset.min(kept.data.record.offset)
            });
            end.max(record.saturating_sub(RECORD_HEADER_LEN as u64))
        });
        Span {
            offset: start,
            len: end.saturating_sub(start),
        }
    }
}

/// Markers age out with the video they point at, as `prune` does today: the
/// half of `Pruned` that `scan_from` also needs over its own segment map.
fn age_out_markers(markers: &mut Vec<MarkerEntry>, first: Option<&SegmentEntry>) {
    if let Some(first) = first {
        let cutoff = first.start_100ns;
        markers.retain(|marker| marker.time_100ns >= cutoff);
    }
}

/// First byte and end of the records a dropped prefix occupied, thumbnails
/// included -- all a prune needs of what it drops, so it never keeps a copy
/// of the snapshot (t260913-fff8).
fn dropped_extent(dropped: &[SegmentEntry]) -> Option<(u64, u64)> {
    let first = dropped.first()?;
    let last = dropped.last()?;
    let start = first.thumbnail.map_or(first.data.record.offset, |thumb| {
        thumb.record.offset.min(first.data.record.offset)
    });
    let end = last.data.record.offset + last.data.record.len;
    let end = last
        .thumbnail
        .map_or(end, |thumb| end.max(thumb.record.offset + thumb.record.len));
    Some((start, end))
}

// ---------- header ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Slot {
    pub generation: u64,
    pub offset: u64,
    pub len: u64,
}

impl Slot {
    fn encode(self) -> [u8; SLOT_LEN] {
        let mut out = [0u8; SLOT_LEN];
        out[0..8].copy_from_slice(&self.generation.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.len.to_le_bytes());
        let crc = crc32(&out[0..SLOT_BODY_LEN]);
        out[24..28].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// `None` for a slot that was never written or whose write was cut in
    /// half -- which is exactly the case the second slot exists to cover.
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < SLOT_LEN {
            return None;
        }
        let stored = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
        if stored != crc32(&bytes[0..SLOT_BODY_LEN]) {
            return None;
        }
        let generation = u64::from_le_bytes(bytes[0..8].try_into().ok()?);
        if generation == 0 {
            return None;
        }
        Some(Self {
            generation,
            offset: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            len: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileHeader {
    pub format_version: u32,
    pub session_id: String,
    pub retention_minutes: u16,
}

fn encode_header(header: &FileHeader, slot_a: Option<Slot>, slot_b: Option<Slot>) -> Vec<u8> {
    let mut bytes = vec![0u8; HEADER_LEN as usize];
    bytes[0..4].copy_from_slice(&MAGIC);
    bytes[4..8].copy_from_slice(&header.format_version.to_le_bytes());
    bytes[8..10].copy_from_slice(&header.retention_minutes.to_le_bytes());
    let id = header.session_id.as_bytes();
    let id_len = id.len().min(SESSION_ID_MAX);
    bytes[12..14].copy_from_slice(&(id_len as u16).to_le_bytes());
    bytes[SESSION_ID_OFFSET..SESSION_ID_OFFSET + id_len].copy_from_slice(&id[..id_len]);
    if let Some(slot) = slot_a {
        bytes[SLOT_A_OFFSET..SLOT_A_OFFSET + SLOT_LEN].copy_from_slice(&slot.encode());
    }
    if let Some(slot) = slot_b {
        bytes[SLOT_B_OFFSET..SLOT_B_OFFSET + SLOT_LEN].copy_from_slice(&slot.encode());
    }
    bytes
}

pub fn decode_header(bytes: &[u8]) -> io::Result<(FileHeader, Option<Slot>, Option<Slot>)> {
    if bytes.len() < HEADER_LEN as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "container header is short",
        ));
    }
    if bytes[0..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a Liveback container",
        ));
    }
    let format_version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    if format_version > FORMAT_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("container format version {format_version} is newer than {FORMAT_VERSION}"),
        ));
    }
    let retention_minutes = u16::from_le_bytes(bytes[8..10].try_into().unwrap());
    let id_len =
        usize::from(u16::from_le_bytes(bytes[12..14].try_into().unwrap())).min(SESSION_ID_MAX);
    let session_id =
        String::from_utf8_lossy(&bytes[SESSION_ID_OFFSET..SESSION_ID_OFFSET + id_len]).into_owned();
    Ok((
        FileHeader {
            format_version,
            session_id,
            retention_minutes,
        },
        Slot::decode(&bytes[SLOT_A_OFFSET..SLOT_A_OFFSET + SLOT_LEN]),
        Slot::decode(&bytes[SLOT_B_OFFSET..SLOT_B_OFFSET + SLOT_LEN]),
    ))
}

// ---------- payload encoding ----------

fn segment_payload(entry: &SegmentEntry, body: &[u8]) -> Vec<u8> {
    let tracks = entry.audio_offsets_100ns.len();
    let mut payload = Vec::with_capacity(v2_prefix_len(tracks) + body.len());
    payload.extend_from_slice(&entry.index.to_le_bytes());
    payload.extend_from_slice(&entry.start_100ns.to_le_bytes());
    payload.extend_from_slice(&entry.end_100ns.to_le_bytes());
    payload.extend_from_slice(&(tracks as u64).to_le_bytes());
    for offset in &entry.audio_offsets_100ns {
        payload.extend_from_slice(&offset.to_le_bytes());
    }
    payload.extend_from_slice(body);
    payload
}

/// v1: `index | start | end | audio_offset`, body always at 32.
const V1_SEGMENT_PREFIX_LEN: usize = 32;

/// v2: the fourth field became a track count, followed by that many offsets,
/// so the body starts at `32 + 8 * tracks`. Writers only ever emit v2.
fn v2_prefix_len(tracks: usize) -> usize {
    V1_SEGMENT_PREFIX_LEN + 8 * tracks
}

/// A ceiling that exists only so a corrupt length field cannot make the reader
/// allocate: four tracks is the feature max (target plus three apps).
const MAX_AUDIO_TRACKS: u64 = 64;

/// The segment fields plus where its mp4 body starts. `None` means the payload
/// is too short to be a segment, which the caller treats as the end of the
/// intact prefix -- the same thing a short v1 payload always meant.
fn parse_segment_prefix(
    format_version: u32,
    payload: &[u8],
) -> Option<(u64, i64, i64, Vec<i64>, usize)> {
    if payload.len() < V1_SEGMENT_PREFIX_LEN {
        return None;
    }
    let index = u64::from_le_bytes(payload[0..8].try_into().ok()?);
    let start = i64::from_le_bytes(payload[8..16].try_into().ok()?);
    let end = i64::from_le_bytes(payload[16..24].try_into().ok()?);
    if format_version < 2 {
        let offset = i64::from_le_bytes(payload[24..32].try_into().ok()?);
        return Some((index, start, end, vec![offset], V1_SEGMENT_PREFIX_LEN));
    }
    let tracks = u64::from_le_bytes(payload[24..32].try_into().ok()?);
    if tracks > MAX_AUDIO_TRACKS {
        return None;
    }
    let tracks = tracks as usize;
    let prefix = v2_prefix_len(tracks);
    if payload.len() < prefix {
        return None;
    }
    let offsets = (0..tracks)
        .map(|track| {
            let at = V1_SEGMENT_PREFIX_LEN + track * 8;
            i64::from_le_bytes(payload[at..at + 8].try_into().unwrap())
        })
        .collect();
    Some((index, start, end, offsets, prefix))
}

const THUMBNAIL_PREFIX_LEN: usize = 8;

fn thumbnail_payload(index: u64, jpeg: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(THUMBNAIL_PREFIX_LEN + jpeg.len());
    payload.extend_from_slice(&index.to_le_bytes());
    payload.extend_from_slice(jpeg);
    payload
}

// ---------- writer ----------

/// Append-only writer. Holds the file open for the life of a recording, which
/// is also what stops a second process from writing the same session:
/// `FILE_SHARE_READ` lets readers in and keeps writers out (plan §3.7).
pub struct ContainerWriter {
    file: File,
    path: PathBuf,
    header: FileHeader,
    end: u64,
    slot_a: Option<Slot>,
    slot_b: Option<Slot>,
    snapshot: Snapshot,
    sparse: bool,
}

impl ContainerWriter {
    pub fn create(path: &Path, session_id: &str, retention_minutes: u16) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = open_write(path, true)?;
        // Best effort, and deliberately not checked: a volume that will not do
        // sparse files still records perfectly well, it just never gives the
        // pruned bytes back (plan §3.4).
        let sparse = set_sparse(&file).is_ok();
        let header = FileHeader {
            format_version: FORMAT_VERSION,
            session_id: session_id.to_owned(),
            retention_minutes,
        };
        let mut writer = Self {
            file,
            path: path.to_path_buf(),
            header,
            end: HEADER_LEN,
            slot_a: None,
            slot_b: None,
            snapshot: Snapshot::default(),
            sparse,
        };
        writer.write_header()?;
        writer.file.set_len(HEADER_LEN)?;
        Ok(writer)
    }

    /// Reopens a container to keep appending -- what recovery does once it has
    /// decided where the good bytes end.
    pub fn reopen(path: &Path, valid_end: u64, snapshot: Snapshot) -> io::Result<Self> {
        let file = open_write(path, false)?;
        let mut header_bytes = vec![0u8; HEADER_LEN as usize];
        (&file).seek(SeekFrom::Start(0))?;
        (&file).read_exact(&mut header_bytes)?;
        let (header, slot_a, slot_b) = decode_header(&header_bytes)?;
        // Everything past the last good record is debris from the interrupted
        // write. Cutting it now means the next append lands where the reader
        // will look for it.
        file.set_len(valid_end)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            header,
            end: valid_end,
            slot_a,
            slot_b,
            snapshot,
            sparse: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn session_id(&self) -> &str {
        &self.header.session_id
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn sparse(&self) -> bool {
        self.sparse
    }

    fn write_header(&mut self) -> io::Result<()> {
        let bytes = encode_header(&self.header, self.slot_a, self.slot_b);
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&bytes)
    }

    fn append(&mut self, kind: RecordKind, payload: &[u8]) -> io::Result<Span> {
        if payload.len() as u64 > u64::from(MAX_PAYLOAD_LEN) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "container payload too large",
            ));
        }
        let mut head = [0u8; RECORD_HEADER_LEN];
        head[0] = kind.code();
        head[2..6].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        head[6..10].copy_from_slice(&crc32(payload).to_le_bytes());
        self.file.seek(SeekFrom::Start(self.end))?;
        self.file.write_all(&head)?;
        self.file.write_all(payload)?;
        let span = Span {
            offset: self.end + RECORD_HEADER_LEN as u64,
            len: payload.len() as u64,
        };
        self.end = span.offset + span.len;
        Ok(span)
    }

    fn append_located(
        &mut self,
        kind: RecordKind,
        payload: &[u8],
        prefix_len: usize,
    ) -> io::Result<Located> {
        let record = self.append(kind, payload)?;
        Ok(Located {
            record,
            body: Span {
                offset: record.offset + prefix_len as u64,
                len: record.len - prefix_len as u64,
            },
            crc: crc32(payload),
        })
    }

    pub fn append_segment(
        &mut self,
        index: u64,
        start_100ns: i64,
        end_100ns: i64,
        audio_offsets_100ns: &[i64],
        body: &[u8],
    ) -> io::Result<Located> {
        let entry = SegmentEntry {
            index,
            start_100ns,
            end_100ns,
            audio_offsets_100ns: audio_offsets_100ns.to_vec(),
            data: Located::default(),
            thumbnail: None,
        };
        let payload = segment_payload(&entry, body);
        let located = self.append_located(
            RecordKind::Segment,
            &payload,
            v2_prefix_len(audio_offsets_100ns.len()),
        )?;
        self.snapshot.segments.push(SegmentEntry {
            data: located,
            ..entry
        });
        self.snapshot.segments.sort_by_key(|segment| segment.index);
        Ok(located)
    }

    /// A thumbnail for a segment that is not there is dropped, not an error:
    /// the sidecar write is best-effort today and a segment can be pruned
    /// between the capture and the encode.
    pub fn append_thumbnail(&mut self, index: u64, jpeg: &[u8]) -> io::Result<Option<Located>> {
        let payload = thumbnail_payload(index, jpeg);
        let located = self.append_located(RecordKind::Thumbnail, &payload, THUMBNAIL_PREFIX_LEN)?;
        match self
            .snapshot
            .segments
            .iter_mut()
            .find(|segment| segment.index == index)
        {
            Some(segment) => {
                segment.thumbnail = Some(located);
                Ok(Some(located))
            }
            None => Ok(None),
        }
    }

    pub fn append_meta(&mut self, event: &MetaEvent) -> io::Result<()> {
        let payload = serde_json::to_vec(event).map_err(io::Error::other)?;
        self.append(RecordKind::Meta, &payload)?;
        self.snapshot.apply(event);
        Ok(())
    }

    /// Appends the snapshot, *then* points a header slot at it. Never the
    /// other way round: a slot that names a record which is not fully on disk
    /// is the one state the reader cannot detect, because the CRC it would
    /// check belongs to the record it cannot find.
    pub fn checkpoint(&mut self) -> io::Result<()> {
        let payload = serde_json::to_vec(&self.snapshot).map_err(io::Error::other)?;
        let span = self.append(RecordKind::Checkpoint, &payload)?;
        let generation = self
            .slot_a
            .map(|slot| slot.generation)
            .unwrap_or(0)
            .max(self.slot_b.map(|slot| slot.generation).unwrap_or(0))
            + 1;
        let slot = Slot {
            generation,
            offset: span.offset,
            len: span.len,
        };
        // Into the older slot, so a torn write can only damage the one already
        // superseded.
        let a_generation = self.slot_a.map(|slot| slot.generation).unwrap_or(0);
        let b_generation = self.slot_b.map(|slot| slot.generation).unwrap_or(0);
        if a_generation <= b_generation {
            self.slot_a = Some(slot);
        } else {
            self.slot_b = Some(slot);
        }
        self.write_header()
    }

    /// Retention: drop every segment before `keep_from_index`, then give the
    /// bytes back if the volume will take them. The surviving records do not
    /// move, so every offset a reader already holds stays valid.
    pub fn prune(&mut self, keep_from_index: u64) -> io::Result<Option<Span>> {
        // What `Pruned` drops is the index-ascending prefix below it.
        let dropped = self
            .snapshot
            .segments
            .partition_point(|segment| segment.index < keep_from_index);
        let extent = dropped_extent(&self.snapshot.segments[..dropped]);
        self.append_meta(&MetaEvent::Pruned { keep_from_index })?;
        let mut span = extent.map(|extent| self.snapshot.span_over(extent));
        // Both slotted checkpoints and everything after the older one are
        // what the readers restore from and replay, so the hole stops short
        // of either. Only ever fires on a ring too small to have moved past
        // them; in steady state both slots sit at the tail, well past it.
        if let Some(span) = span.as_mut() {
            let slots: Vec<u64> = [self.slot_a, self.slot_b]
                .into_iter()
                .flatten()
                .map(|slot| slot.offset.saturating_sub(RECORD_HEADER_LEN as u64))
                .collect();
            for &record in &slots {
                if record >= span.offset && record < span.offset + span.len {
                    span.len = record - span.offset;
                }
            }
            // The volume frees whole allocation units only, and a steady
            // prune's hole is one segment plus a pass's debris -- often less
            // than a unit, and never lined up with one. Everything before the
            // first dropped record is dead once no slot and no survivor sits
            // there, so the hole starts on an aligned boundary instead and
            // takes in the unit the previous hole could not.
            // ponytail: fixed 1 MiB covers NTFS's 64 KiB sparse unit up to
            // 64 KiB clusters; ask the volume if one ever uses more.
            const PUNCH_ALIGN: u64 = 1 << 20;
            let survivor = self.snapshot.segments.first().map(|kept| {
                kept.thumbnail
                    .map_or(kept.data.record.offset, |thumb| {
                        thumb.record.offset.min(kept.data.record.offset)
                    })
                    .saturating_sub(RECORD_HEADER_LEN as u64)
            });
            if slots
                .iter()
                .chain(survivor.iter())
                .all(|&live| live >= span.offset)
            {
                let start = (span.offset / PUNCH_ALIGN * PUNCH_ALIGN).max(HEADER_LEN);
                span.len += span.offset - start;
                span.offset = start;
            }
        }
        if let Some(span) = span {
            if span.len > 0 {
                // Non-fatal by design: the session is already logically
                // shorter, and a volume without sparse support just keeps
                // paying for the bytes.
                if let Err(error) = punch_hole(&self.file, span.offset, span.len) {
                    tracing::warn!(
                        event = "container_hole_punch_failed",
                        offset = span.offset,
                        len = span.len,
                        %error,
                    );
                }
            }
        }
        self.checkpoint()?;
        Ok(span)
    }

    /// The clean end: the `Closed` marker and a final checkpoint. A container
    /// without this is what `needs_recovery` reports.
    pub fn close(mut self) -> io::Result<()> {
        self.append_meta(&MetaEvent::Closed)?;
        self.checkpoint()
    }
}

// ---------- reader ----------

#[cfg(test)]
thread_local! {
    /// How many times this thread went through `open_for_reading`
    /// (task t260913-66a6). Thread-local because the `ring_buffer::` tests run
    /// in parallel and a read happens on the thread that asks for it, so a
    /// test counts only its own opens.
    static OPEN_FOR_READING_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// See `OPEN_FOR_READING_CALLS`.
#[cfg(test)]
pub(crate) fn open_for_reading_calls() -> usize {
    OPEN_FOR_READING_CALLS.with(std::cell::Cell::get)
}

#[cfg(test)]
thread_local! {
    /// The largest capacity the replay's payload buffer reached on this thread
    /// (t260913-1e2e): the payload bytes a scan holds at once, which must track
    /// the largest record and never the file.
    static SCAN_BUFFER_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Reads and resets `SCAN_BUFFER_PEAK`.
#[cfg(test)]
pub(crate) fn take_scan_buffer_peak() -> usize {
    SCAN_BUFFER_PEAK.with(|peak| peak.replace(0))
}

/// The CRC check and the body slice `read_located` does, shared with
/// [`read_located_at`] so the check exists once. `bytes` is the whole record
/// payload `located.record` names.
fn verified_body(bytes: &[u8], located: Located) -> io::Result<Vec<u8>> {
    if crc32(bytes) != located.crc {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "container record at {} failed its checksum",
                located.record.offset
            ),
        ));
    }
    let body = located
        .body
        .offset
        .checked_sub(located.record.offset)
        .map(|start| start as usize)
        .and_then(|start| Some(start..start.checked_add(located.body.len as usize)?))
        .and_then(|range| bytes.get(range))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "container record body lies outside its record",
            )
        })?;
    Ok(body.to_vec())
}

/// One record's bytes, verified, read straight from a [`Located`] the caller
/// already holds (task t260913-66a6): open, seek, read, CRC. No header decode
/// and no checkpoint parse -- those are what `open_for_reading` pays on every
/// call, and while a writer was appending to the same file that cost 26-231ms
/// per read (task3590).
///
/// Safe for the same reason `open_for_reading` may take the file's length as
/// its bound: the CRC travels with the location, so a span that no longer
/// names its record -- a hole punched under it, a tail cut by recovery --
/// fails here instead of being served.
pub fn read_located_at(path: &Path, located: Located) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let valid_end = file.metadata()?.len();
    let span = located.record;
    match span.offset.checked_add(span.len) {
        Some(end) if end <= valid_end => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "span lies outside the container's valid range",
            ))
        }
    }
    let mut bytes = vec![0u8; span.len as usize];
    file.seek(SeekFrom::Start(span.offset))?;
    file.read_exact(&mut bytes)?;
    verified_body(&bytes, located)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecordLocation {
    pub kind: RecordKind,
    /// Where the record header starts.
    pub offset: u64,
    pub payload: Span,
    /// CRC32 of the whole payload, already verified against the record header.
    pub crc: u32,
}

pub struct ContainerReader {
    file: File,
    header: FileHeader,
    snapshot: Snapshot,
    records: Vec<RecordLocation>,
    valid_end: u64,
    truncated: bool,
    checkpoint_end: Option<u64>,
}

impl ContainerReader {
    pub fn open(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let scan = scan_from(&mut file)?;
        Ok(Self {
            file,
            header: scan.header,
            snapshot: scan.snapshot,
            records: scan.records,
            valid_end: scan.valid_end,
            truncated: scan.truncated,
            // The full scan replays past the checkpoint it started from, so
            // "where the checkpoint ended" says nothing about the tail here.
            checkpoint_end: None,
        })
    }

    /// Opens for **reading**, without replaying the file (task450).
    ///
    /// `open` reads the whole container to scan it, which was free while the
    /// only containers were fixtures. Once every recording is one, it stops
    /// being free: a segment read, a thumbnail hover and a history refresh each
    /// went through `open`, so playing a 15-minute session re-read hundreds of
    /// megabytes every two seconds -- while the writer was still appending to
    /// it.
    ///
    /// So this reads two things: the 4 KiB header, and the one checkpoint
    /// record a header slot names. That is the whole point of keeping a
    /// checkpoint.
    ///
    /// **`valid_end` becomes the file's length rather than a scanned bound**,
    /// which is safe because it is not what protects a reader: `read_located`
    /// verifies every record's CRC against the `Located` it was given, so a
    /// torn tail fails the checksum and errors instead of being served. The
    /// scan's stricter bound only matters to recovery, which still uses `open`.
    ///
    /// **A live session is consistent here** because the index writer holds the
    /// `sessions` lock across append → `RingBuffer::add` → `checkpoint`: no
    /// reader can resolve a `ReadTarget` for a segment whose checkpoint is not
    /// on disk yet.
    ///
    /// Anything unexpected -- no slot, a checkpoint that does not parse, a
    /// container that crashed before its first one -- falls back to the full
    /// scan, which is the path that can see past the checkpoint.
    pub fn open_for_reading(path: &Path) -> io::Result<Self> {
        #[cfg(test)]
        OPEN_FOR_READING_CALLS.with(|calls| calls.set(calls.get() + 1));
        match Self::open_at_checkpoint(path) {
            Ok(reader) => Ok(reader),
            Err(_) => Self::open(path),
        }
    }

    pub(crate) fn open_at_checkpoint(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut header_bytes = vec![0u8; HEADER_LEN as usize];
        file.read_exact(&mut header_bytes)?;
        let (header, slot_a, slot_b) = decode_header(&header_bytes)?;
        // Newest generation first, then the other -- the same order and the
        // same reason as `scan`: the newest slot routinely points past the end
        // of a container that crashed, while the older one still resolves.
        let mut candidates: Vec<Slot> = [slot_a, slot_b].into_iter().flatten().collect();
        candidates.sort_by_key(|slot| std::cmp::Reverse(slot.generation));
        let valid_end = file.metadata()?.len();
        for slot in candidates {
            let record_offset = slot.offset.saturating_sub(RECORD_HEADER_LEN as u64);
            let record_len = RECORD_HEADER_LEN as u64 + slot.len;
            if record_offset + record_len > valid_end {
                continue;
            }
            let mut bytes = vec![0u8; record_len as usize];
            if file.seek(SeekFrom::Start(record_offset)).is_err()
                || file.read_exact(&mut bytes).is_err()
            {
                continue;
            }
            // Offset 0: `bytes` starts at the record, so the location's spans
            // are slice-relative. Only `payload` is used from it.
            let Some((location, payload)) = read_record_at(&bytes, 0) else {
                continue;
            };
            if location.kind != RecordKind::Checkpoint || location.payload.len != slot.len {
                continue;
            }
            let Ok(snapshot) = serde_json::from_slice::<Snapshot>(payload) else {
                continue;
            };
            return Ok(Self {
                file: File::open(path)?,
                header,
                snapshot,
                // Nothing after the checkpoint is replayed here; a caller that
                // needs those (recovery, `--liveback-inspect --full`) opens the
                // other way.
                records: Vec::new(),
                valid_end,
                truncated: false,
                checkpoint_end: Some(record_offset + record_len),
            });
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "no usable checkpoint in the container header",
        ))
    }

    pub fn header(&self) -> &FileHeader {
        &self.header
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    pub fn records(&self) -> &[RecordLocation] {
        &self.records
    }

    /// Where the good bytes end. Anything past this was a write in flight.
    pub fn valid_end(&self) -> u64 {
        self.valid_end
    }

    /// Where the checkpoint this reader was restored from ends, when it was
    /// opened the cheap way. `None` after a full scan, which replays past its
    /// checkpoint and so has no single record to point at.
    ///
    /// A caller compares this with `valid_end` (the file's length on the cheap
    /// path) to learn whether anything at all sits behind the checkpoint. That
    /// is the one question the cheap open cannot answer for a *writer*: see
    /// `sessions::append_meta`.
    pub fn checkpoint_end(&self) -> Option<u64> {
        self.checkpoint_end
    }

    /// Whether anything was discarded -- a half-written record, or bytes that
    /// failed their CRC.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// A container that was never told it was finished. The `recording.marker`
    /// file's job, moved inside.
    pub fn needs_recovery(&self) -> bool {
        !self.snapshot.closed
    }

    /// One record's bytes, verified. The CRC covers the whole record payload,
    /// so that is what gets read and hashed before the useful part of it is
    /// sliced out and returned.
    ///
    /// This is where rot in a record older than the newest checkpoint is
    /// caught. The open scan deliberately does not re-read those -- that is
    /// the whole saving a checkpoint buys -- so without a check here a bad
    /// byte in an old segment would be served as if it were sound.
    pub fn read_located(&mut self, located: Located) -> io::Result<Vec<u8>> {
        let bytes = self.read_span(located.record)?;
        verified_body(&bytes, located)
    }

    /// A raw range, unverified. Callers holding a `Located` want
    /// `read_located` instead.
    pub fn read_span(&mut self, span: Span) -> io::Result<Vec<u8>> {
        if span.offset + span.len > self.valid_end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "span lies outside the container's valid range",
            ));
        }
        let mut out = vec![0u8; span.len as usize];
        self.file.seek(SeekFrom::Start(span.offset))?;
        self.file.read_exact(&mut out)?;
        Ok(out)
    }

    pub fn read_segment(&mut self, index: u64) -> io::Result<Vec<u8>> {
        let located = self
            .snapshot
            .segments
            .iter()
            .find(|segment| segment.index == index)
            .map(|segment| segment.data)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such segment"))?;
        self.read_located(located)
    }

    pub fn read_thumbnail(&mut self, index: u64) -> io::Result<Vec<u8>> {
        let located = self
            .snapshot
            .segments
            .iter()
            .find(|segment| segment.index == index)
            .and_then(|segment| segment.thumbnail)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no such thumbnail"))?;
        self.read_located(located)
    }
}

pub struct Scan {
    pub header: FileHeader,
    pub snapshot: Snapshot,
    pub records: Vec<RecordLocation>,
    pub valid_end: u64,
    pub truncated: bool,
}

/// Replays one segment record into `segments`. False means the record's prefix
/// did not parse, which ends the replay as a truncation.
///
/// Keyed by index so a later record for the same index replaces the earlier
/// one (a segment re-appended after a prune) in O(log n), where the `Vec` it
/// replaced paid a linear filter and a re-sort per record (t260913-1e2e).
fn apply_segment_record(
    segments: &mut BTreeMap<u64, SegmentEntry>,
    format_version: u32,
    location: &RecordLocation,
    payload: &[u8],
) -> bool {
    let Some((index, start, end, audio_offsets, prefix)) =
        parse_segment_prefix(format_version, payload)
    else {
        return false;
    };
    segments.insert(
        index,
        SegmentEntry {
            index,
            start_100ns: start,
            end_100ns: end,
            audio_offsets_100ns: audio_offsets,
            data: Located {
                record: location.payload,
                body: Span {
                    offset: location.payload.offset + prefix as u64,
                    len: location.payload.len - prefix as u64,
                },
                crc: location.crc,
            },
            thumbnail: None,
        },
    );
    true
}

/// Hangs one thumbnail record off the segment it names. A thumbnail for an
/// unknown segment is dropped, not a truncation; a payload too short to carry
/// its index is.
fn apply_thumbnail_record(
    segments: &mut BTreeMap<u64, SegmentEntry>,
    location: &RecordLocation,
    payload: &[u8],
) -> bool {
    if payload.len() < THUMBNAIL_PREFIX_LEN {
        return false;
    }
    let index = u64::from_le_bytes(payload[0..8].try_into().unwrap());
    if let Some(segment) = segments.get_mut(&index) {
        segment.thumbnail = Some(Located {
            record: location.payload,
            body: Span {
                offset: location.payload.offset + THUMBNAIL_PREFIX_LEN as u64,
                len: location.payload.len - THUMBNAIL_PREFIX_LEN as u64,
            },
            crc: location.crc,
        });
    }
    true
}

/// The newest checkpoint either header slot still resolves to, and where the
/// replay picks up from it.
///
/// Both slots are tried because a slot passing its CRC does not mean the record
/// it names survived: the header is 4 KiB at the front of the file and a
/// truncation takes the tail, so the newest slot routinely points past the end
/// of a crashed container while the older one still resolves. Falling straight
/// to a full replay would also be correct -- it re-verifies everything -- but it
/// is the expensive answer to a question the second slot already has.
///
/// The third value says a slot was unusable, which the caller reports as a
/// truncated container.
fn restore_checkpoint<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    buf: &mut Vec<u8>,
    slot_a: Option<Slot>,
    slot_b: Option<Slot>,
) -> (Snapshot, u64, bool) {
    // Newest generation first, then the other one. A slot whose own CRC failed
    // decoded to `None` and never enters the list.
    //
    // Both are tried because a slot passing its CRC does not mean the record
    // it names survived: the header is 4 KiB at the front of the file and a
    // truncation takes the tail, so the newest slot routinely points past the
    // end of a crashed container while the older one still resolves. Falling
    // straight to a full replay would also be correct -- it re-verifies
    // everything -- but it is the expensive answer to a question the second
    // slot already has.
    let mut candidates: Vec<Slot> = [slot_a, slot_b].into_iter().flatten().collect();
    candidates.sort_by_key(|slot| std::cmp::Reverse(slot.generation));

    let mut snapshot = Snapshot::default();
    let mut cursor = HEADER_LEN;
    let mut truncated = false;

    for slot in candidates {
        let offset = slot.offset.saturating_sub(RECORD_HEADER_LEN as u64);
        match read_record_into(reader, len, offset, buf) {
            Some(location)
                if location.kind == RecordKind::Checkpoint && location.payload.len == slot.len =>
            {
                match serde_json::from_slice::<Snapshot>(buf) {
                    Ok(restored) => {
                        snapshot = restored;
                        cursor = location.payload.offset + location.payload.len;
                        break;
                    }
                    // Passed its CRC but this build cannot read the JSON: try
                    // the other slot, then replay, rather than refusing to
                    // open the session at all.
                    Err(_) => truncated = true,
                }
            }
            _ => truncated = true,
        }
    }
    (snapshot, cursor, truncated)
}

/// Reads a container from bytes: header, newest intact checkpoint, then
/// forward to the first record that does not verify.
///
/// Total on every input. A file cut at an arbitrary byte, a flipped payload,
/// a length field pointing past the end -- each stops the scan and leaves
/// everything before it readable.
pub fn scan(bytes: &[u8]) -> io::Result<Scan> {
    scan_from(&mut Cursor::new(bytes))
}

/// The stream's length and its first `min(len, HEADER_LEN)` bytes -- all
/// [`decode_header`] ever looks at.
fn read_header_bytes<R: Read + Seek>(reader: &mut R) -> io::Result<(u64, Vec<u8>)> {
    let len = reader.seek(SeekFrom::End(0))?;
    let mut header_bytes = vec![0u8; len.min(HEADER_LEN) as usize];
    reader.seek(SeekFrom::Start(0))?;
    reader.read_exact(&mut header_bytes)?;
    Ok((len, header_bytes))
}

/// [`scan`] over a stream, which is what the real paths use (t260913-1e2e):
/// `ContainerReader::open` and `recover` used to read the whole `.lvb` into
/// one `Vec` first, so the peak memory of opening a session grew with its
/// length. Here one payload buffer is reused record after record, so what is
/// held at once is the largest record (at most `MAX_PAYLOAD_LEN`), not the file.
fn scan_from<R: Read + Seek>(reader: &mut R) -> io::Result<Scan> {
    let (len, header_bytes) = read_header_bytes(reader)?;
    let (header, slot_a, slot_b) = decode_header(&header_bytes)?;

    let mut buf = Vec::new();
    let (mut snapshot, mut cursor, mut truncated) =
        restore_checkpoint(reader, len, &mut buf, slot_a, slot_b);
    // Collecting keeps the last entry per index, the same answer a replay gives.
    let mut segments: BTreeMap<u64, SegmentEntry> = std::mem::take(&mut snapshot.segments)
        .into_iter()
        .map(|segment| (segment.index, segment))
        .collect();

    let mut records = Vec::new();
    let mut valid_end = cursor;
    while cursor < len {
        let Some(location) = read_record_into(reader, len, cursor, &mut buf) else {
            truncated = true;
            break;
        };
        let payload = buf.as_slice();
        match location.kind {
            RecordKind::Segment => {
                if !apply_segment_record(&mut segments, header.format_version, &location, payload) {
                    truncated = true;
                    break;
                }
            }
            RecordKind::Thumbnail => {
                if !apply_thumbnail_record(&mut segments, &location, payload) {
                    truncated = true;
                    break;
                }
            }
            RecordKind::Meta => match serde_json::from_slice::<MetaEvent>(payload) {
                // The one event that edits segments, done on the map: the
                // snapshot's own `segments` stays empty until the replay ends.
                Ok(MetaEvent::Pruned { keep_from_index }) => {
                    segments = segments.split_off(&keep_from_index);
                    age_out_markers(&mut snapshot.markers, segments.values().next());
                }
                Ok(event) => snapshot.apply(&event),
                Err(_) => {
                    truncated = true;
                    break;
                }
            },
            // Checkpoints are replayed past, not applied: the snapshot being
            // built is already at least as new as any of them.
            RecordKind::Checkpoint => {}
        }
        cursor = location.payload.offset + location.payload.len;
        valid_end = cursor;
        records.push(location);
    }

    if valid_end < len {
        truncated = true;
    }
    snapshot.segments = segments.into_values().collect();
    #[cfg(test)]
    SCAN_BUFFER_PEAK.with(|peak| peak.set(peak.get().max(buf.capacity())));

    Ok(Scan {
        header,
        snapshot,
        records,
        valid_end,
        truncated,
    })
}

/// One record at `offset`, or `None` if what is there is not a whole, intact
/// record. Every rejection path is a normal end-of-file, not an error.
pub fn read_record_at(bytes: &[u8], offset: u64) -> Option<(RecordLocation, &[u8])> {
    let start = usize::try_from(offset).ok()?;
    let head_end = start.checked_add(RECORD_HEADER_LEN)?;
    if head_end > bytes.len() {
        return None;
    }
    let (kind, payload_len, stored_crc) = parse_record_head(&bytes[start..head_end])?;
    let payload_end = head_end.checked_add(payload_len as usize)?;
    if payload_end > bytes.len() {
        return None;
    }
    let payload = &bytes[head_end..payload_end];
    let computed = crc32(payload);
    if computed != stored_crc {
        return None;
    }
    Some((
        RecordLocation {
            kind,
            offset,
            payload: Span {
                offset: head_end as u64,
                len: u64::from(payload_len),
            },
            // Same value as `stored_crc` once the check above passed; `computed`
            // is returned so the caller gets what was measured, not what was claimed.
            crc: computed,
        },
        payload,
    ))
}

/// Kind, payload length and stored CRC out of a record header, or `None` for
/// an unknown kind or a length over `MAX_PAYLOAD_LEN`. Shared by both readers
/// so the header is parsed in one place.
fn parse_record_head(head: &[u8]) -> Option<(RecordKind, u32, u32)> {
    let kind = RecordKind::from_code(*head.first()?)?;
    let payload_len = u32::from_le_bytes(head.get(2..6)?.try_into().ok()?);
    if payload_len > MAX_PAYLOAD_LEN {
        return None;
    }
    let stored_crc = u32::from_le_bytes(head.get(6..10)?.try_into().ok()?);
    Some((kind, payload_len, stored_crc))
}

/// [`read_record_at`] over a stream of `len` bytes: the payload lands in
/// `buf`, which the caller reuses for the next record (t260913-1e2e). The
/// length is checked against `MAX_PAYLOAD_LEN` and against `len` before `buf`
/// grows, so a garbage length field never allocates. Any read error is the
/// same `None` as a torn record.
fn read_record_into<R: Read + Seek>(
    reader: &mut R,
    len: u64,
    offset: u64,
    buf: &mut Vec<u8>,
) -> Option<RecordLocation> {
    let head_end = offset.checked_add(RECORD_HEADER_LEN as u64)?;
    if head_end > len {
        return None;
    }
    let mut head = [0u8; RECORD_HEADER_LEN];
    reader.seek(SeekFrom::Start(offset)).ok()?;
    reader.read_exact(&mut head).ok()?;
    let (kind, payload_len, stored_crc) = parse_record_head(&head)?;
    if head_end.checked_add(u64::from(payload_len))? > len {
        return None;
    }
    buf.clear();
    // Exact, so the capacity is the largest payload seen and not a doubling of it.
    buf.reserve_exact(payload_len as usize);
    buf.resize(payload_len as usize, 0);
    reader.read_exact(buf).ok()?;
    let computed = crc32(buf);
    if computed != stored_crc {
        return None;
    }
    Some(RecordLocation {
        kind,
        offset,
        payload: Span {
            offset: head_end,
            len: u64::from(payload_len),
        },
        crc: computed,
    })
}

/// Recovery: cut the interrupted tail, re-checkpoint what survived, and close
/// it properly. What `recover_session` does for a directory.
pub fn recover(path: &Path) -> io::Result<Snapshot> {
    // The read handle is closed before the writer opens the same file.
    let scan = scan_from(&mut File::open(path)?)?;
    let snapshot = scan.snapshot.clone();
    let mut writer = ContainerWriter::reopen(path, scan.valid_end, snapshot)?;
    if !writer.snapshot.closed {
        writer.append_meta(&MetaEvent::Closed)?;
    }
    writer.checkpoint()?;
    Ok(writer.snapshot.clone())
}

// ---------- inspect summary (task2890) ----------

/// What `--liveback-inspect` answers by default: the header, the slots, and
/// what the checkpoint already knows -- title included. Three reads at most,
/// none of them the body, so a 26 GB session answers as fast as a 300 MB one.
///
/// It deliberately does **not** fall back to the full scan when the checkpoint
/// is unusable. Falling back would put the original symptom -- reading the
/// whole file -- back on exactly the container that needed this path most. The
/// summary says so instead and names `--full`.
pub(crate) fn inspect_summary(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let file_len = file.metadata()?.len();
    let mut header_bytes = vec![0u8; HEADER_LEN as usize];
    file.read_exact(&mut header_bytes)?;
    let (header, slot_a, slot_b) = decode_header(&header_bytes)?;
    // Re-reads the 4 KiB header. Not worth factoring apart to save.
    let reader = ContainerReader::open_at_checkpoint(path).ok();
    Ok(summarize_inspect(
        &header,
        slot_a,
        slot_b,
        file_len,
        reader.as_ref().map(|reader| reader.snapshot()),
    ))
}

/// Bytes in, string out, no process and no file -- the same reason `dump` is
/// shaped that way.
pub(crate) fn summarize_inspect(
    header: &FileHeader,
    slot_a: Option<Slot>,
    slot_b: Option<Slot>,
    file_len: u64,
    snapshot: Option<&Snapshot>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "magic=LVBF version={} session={} retention={}min size={}\n",
        header.format_version, header.session_id, header.retention_minutes, file_len
    ));
    for (name, slot) in [("A", slot_a), ("B", slot_b)] {
        match slot {
            Some(slot) => out.push_str(&format!(
                "slot {name}: generation={} offset={} len={}\n",
                slot.generation, slot.offset, slot.len
            )),
            None => out.push_str(&format!("slot {name}: empty or corrupt\n")),
        }
    }
    let Some(snapshot) = snapshot else {
        out.push_str("no usable checkpoint; rerun with --full to scan\n");
        return out;
    };
    let text = |value: &Option<String>| value.clone().unwrap_or_else(|| "(none)".into());
    out.push_str(&format!("title={}\n", text(&snapshot.title)));
    out.push_str(&format!("note={}\n", text(&snapshot.note)));
    out.push_str(&format!("protected={}\n", snapshot.protected));
    let span = match (snapshot.segments.first(), snapshot.segments.last()) {
        (Some(first), Some(last)) => format!("{}..{}", first.start_100ns, last.end_100ns),
        _ => "(none)".into(),
    };
    out.push_str(&format!(
        "segments={} span={span} closed={}\n",
        snapshot.segments.len(),
        snapshot.closed
    ));
    // Empty is the single-track shape, said the same way `dump` says it.
    if snapshot.audio_tracks.is_empty() {
        out.push_str("audio tracks: (none recorded -- single track)\n");
    } else {
        out.push_str(&format!(
            "audio tracks: {}\n",
            snapshot
                .audio_tracks
                .iter()
                .enumerate()
                .map(|(index, track)| format!("{index}={}", track.executable_name))
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    out
}

// ---------- dump (plan R13) ----------

/// A human-readable listing of a container: header, which slot won, then one
/// line per record. Works on a damaged file -- it prints what it could scan
/// and says where it stopped, which is the whole point of having it.
pub fn dump(bytes: &[u8]) -> String {
    dump_from(&mut Cursor::new(bytes))
}

/// [`dump`] over a stream (t260925-e0d4): what `--liveback-inspect --full`
/// uses, so a 26 GB session is read record by record through [`scan_from`]
/// instead of into one `Vec`. Output is byte-for-byte [`dump`]'s.
pub fn dump_from<R: Read + Seek>(reader: &mut R) -> String {
    let mut out = String::new();
    let (len, header_bytes) = match read_header_bytes(reader) {
        Ok(read) => read,
        Err(error) => return format!("header unreadable: {error}\n"),
    };
    let (header, slot_a, slot_b) = match decode_header(&header_bytes) {
        Ok(parts) => parts,
        Err(error) => return format!("header unreadable: {error}\n"),
    };
    out.push_str(&format!(
        "magic=LVBF version={} session={} retention={}min size={}\n",
        header.format_version, header.session_id, header.retention_minutes, len
    ));
    for (name, slot) in [("A", slot_a), ("B", slot_b)] {
        match slot {
            Some(slot) => out.push_str(&format!(
                "slot {name}: generation={} offset={} len={}\n",
                slot.generation, slot.offset, slot.len
            )),
            None => out.push_str(&format!("slot {name}: empty or corrupt\n")),
        }
    }
    match scan_from(reader) {
        Ok(scan) => {
            out.push_str(&format!(
                "scan: {} records, valid_end={}, truncated={}, closed={}\n",
                scan.records.len(),
                scan.valid_end,
                scan.truncated,
                scan.snapshot.closed
            ));
            // Empty is the single-track shape, and saying so beats printing
            // nothing when the question is "how many audio tracks is this".
            if scan.snapshot.audio_tracks.is_empty() {
                out.push_str("audio tracks: (none recorded -- single track)\n");
            } else {
                out.push_str(&format!(
                    "audio tracks: {}\n",
                    scan.snapshot
                        .audio_tracks
                        .iter()
                        .enumerate()
                        .map(|(index, track)| format!("{index}={}", track.executable_name))
                        .collect::<Vec<_>>()
                        .join(" ")
                ));
            }
            for record in &scan.records {
                out.push_str(&format!(
                    "  {:?} offset={} payload={}@{}\n",
                    record.kind, record.offset, record.payload.len, record.payload.offset
                ));
            }
            for segment in &scan.snapshot.segments {
                out.push_str(&format!(
                    "  segment {} [{}..{}] audio_offsets={:?} body={}@{} thumbnail={}\n",
                    segment.index,
                    segment.start_100ns,
                    segment.end_100ns,
                    segment.audio_offsets_100ns,
                    segment.data.body.len,
                    segment.data.body.offset,
                    segment
                        .thumbnail
                        .map(|located| format!("{}@{}", located.body.len, located.body.offset))
                        .unwrap_or_else(|| "-".into())
                ));
            }
            for marker in &scan.snapshot.markers {
                out.push_str(&format!(
                    "  marker {} {}\n",
                    marker.time_100ns, marker.label
                ));
            }
        }
        Err(error) => out.push_str(&format!("scan failed: {error}\n")),
    }
    out
}

// ---------- windows plumbing ----------

use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
use windows::core::PCWSTR;
use windows::Win32::{
    Foundation::HANDLE,
    Storage::FileSystem::{GetCompressedFileSizeW, FILE_SHARE_READ},
    System::Ioctl::{FILE_ZERO_DATA_INFORMATION, FSCTL_SET_SPARSE, FSCTL_SET_ZERO_DATA},
    System::IO::DeviceIoControl,
};

/// `FILE_SHARE_READ` and nothing else: readers welcome, a second writer
/// refused. That refusal is the whole locking story for a session (plan §3.7).
fn open_write(path: &Path, create_new: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).share_mode(FILE_SHARE_READ.0);
    if create_new {
        options.create(true).truncate(true);
    }
    options.open(path)
}

fn handle_of(file: &File) -> HANDLE {
    HANDLE(file.as_raw_handle())
}

fn set_sparse(file: &File) -> io::Result<()> {
    unsafe {
        DeviceIoControl(
            handle_of(file),
            FSCTL_SET_SPARSE,
            None,
            0,
            None,
            0,
            None,
            None,
        )
        .map_err(|error| io::Error::other(error.to_string()))
    }
}

/// Gives a byte range back to the volume without moving anything around it.
/// The file keeps its length; only the allocation shrinks.
pub fn punch_hole(file: &File, offset: u64, len: u64) -> io::Result<()> {
    let info = FILE_ZERO_DATA_INFORMATION {
        FileOffset: offset as i64,
        BeyondFinalZero: (offset + len) as i64,
    };
    unsafe {
        DeviceIoControl(
            handle_of(file),
            FSCTL_SET_ZERO_DATA,
            Some(std::ptr::addr_of!(info).cast()),
            std::mem::size_of::<FILE_ZERO_DATA_INFORMATION>() as u32,
            None,
            0,
            None,
            None,
        )
        .map_err(|error| io::Error::other(error.to_string()))
    }
}

/// What the file actually costs on disk, which after a hole punch is not its
/// length. **The one place this distinction is measured** (plan R3): a caller
/// that wants a session's size asks here, never `metadata().len()`.
pub fn allocated_bytes(path: &Path) -> io::Result<u64> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut high = 0u32;
    let low = unsafe { GetCompressedFileSizeW(PCWSTR(wide.as_ptr()), Some(&mut high)) };
    if low == u32::MAX {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(0) {
            return Err(error);
        }
    }
    Ok((u64::from(high) << 32) | u64::from(low))
}

use std::os::windows::ffi::OsStrExt;

// ---------- fixture builder (plan R7) ----------

/// Builds a container from a described list of records, including deliberately
/// damaged ones. Every later phase's tests need a `.lvb` with known contents,
/// and hand-rolling one per test is how formats grow untested corners.
#[derive(Clone, Debug, Default)]
pub struct ContainerBuilder {
    session_id: String,
    retention_minutes: u16,
    entries: Vec<BuilderEntry>,
    checkpoint_after: Vec<usize>,
    close: bool,
    damage: Option<Damage>,
}

#[derive(Clone, Debug)]
enum BuilderEntry {
    Segment {
        index: u64,
        start_100ns: i64,
        end_100ns: i64,
        body: Vec<u8>,
    },
    Thumbnail {
        index: u64,
        jpeg: Vec<u8>,
    },
    Meta(MetaEvent),
}

/// How to break the file after writing it, so a test can name the failure it
/// is exercising instead of poking bytes itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Damage {
    /// Cut the file to this length -- an interrupted write.
    TruncateTo(u64),
    /// Flip one bit at this offset -- rot the CRC is there to catch.
    FlipByteAt(u64),
    /// Scribble over a header checkpoint slot, so the other one has to serve.
    CorruptSlot(bool),
}

impl ContainerBuilder {
    pub fn new(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_owned(),
            retention_minutes: 15,
            ..Self::default()
        }
    }

    pub fn retention_minutes(mut self, minutes: u16) -> Self {
        self.retention_minutes = minutes;
        self
    }

    pub fn segment(mut self, index: u64, start_100ns: i64, end_100ns: i64, body: &[u8]) -> Self {
        self.entries.push(BuilderEntry::Segment {
            index,
            start_100ns,
            end_100ns,
            body: body.to_vec(),
        });
        self
    }

    pub fn thumbnail(mut self, index: u64, jpeg: &[u8]) -> Self {
        self.entries.push(BuilderEntry::Thumbnail {
            index,
            jpeg: jpeg.to_vec(),
        });
        self
    }

    pub fn meta(mut self, event: MetaEvent) -> Self {
        self.entries.push(BuilderEntry::Meta(event));
        self
    }

    /// Checkpoint once every entry written so far is in.
    pub fn checkpoint(mut self) -> Self {
        self.checkpoint_after.push(self.entries.len());
        self
    }

    pub fn closed(mut self) -> Self {
        self.close = true;
        self
    }

    pub fn damaged(mut self, damage: Damage) -> Self {
        self.damage = Some(damage);
        self
    }

    pub fn build(&self, path: &Path) -> io::Result<()> {
        let mut writer = ContainerWriter::create(path, &self.session_id, self.retention_minutes)?;
        for (position, entry) in self.entries.iter().enumerate() {
            if self.checkpoint_after.contains(&position) {
                writer.checkpoint()?;
            }
            match entry {
                BuilderEntry::Segment {
                    index,
                    start_100ns,
                    end_100ns,
                    body,
                } => {
                    writer.append_segment(*index, *start_100ns, *end_100ns, &[0], body)?;
                }
                BuilderEntry::Thumbnail { index, jpeg } => {
                    writer.append_thumbnail(*index, jpeg)?;
                }
                BuilderEntry::Meta(event) => writer.append_meta(event)?,
            }
        }
        if self.checkpoint_after.contains(&self.entries.len()) {
            writer.checkpoint()?;
        }
        if self.close {
            writer.close()?;
        } else {
            drop(writer);
        }
        if let Some(damage) = self.damage {
            apply_damage(path, damage)?;
        }
        Ok(())
    }
}

fn apply_damage(path: &Path, damage: Damage) -> io::Result<()> {
    match damage {
        Damage::TruncateTo(len) => {
            let file = OpenOptions::new().write(true).open(path)?;
            file.set_len(len)
        }
        Damage::FlipByteAt(offset) => {
            let mut bytes = std::fs::read(path)?;
            let index = usize::try_from(offset).unwrap_or(usize::MAX);
            if index < bytes.len() {
                bytes[index] ^= 0x01;
            }
            std::fs::write(path, bytes)
        }
        Damage::CorruptSlot(second) => {
            let mut bytes = std::fs::read(path)?;
            let at = if second { SLOT_B_OFFSET } else { SLOT_A_OFFSET };
            for byte in bytes.iter_mut().skip(at).take(SLOT_LEN) {
                *byte ^= 0xFF;
            }
            std::fs::write(path, bytes)
        }
    }
}

// The path is spelled out because `crates/livia-thumb` includes this file with
// `#[path]` (task710), and a `#[path]` module resolves its children against the
// directory the file sits in -- a bare `mod tests;` would find
// `src/ring_buffer/tests.rs` over there. Here it means what it always did.
#[cfg(test)]
#[path = "container/tests.rs"]
mod tests;
