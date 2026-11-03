//! A segment body's fragment table (t261017-3f94): where each `moof` + `mdat`
//! sits inside the body, what it hashes to, and the first video keyframe and
//! the first audio sample it carries. It is what lets a seek read the head
//! (`ftyp..moov`) and the fragments from the target's GOP to the end instead
//! of the whole record, and still hand out only bytes it verified -- the CRC
//! of the record covers all of it, so a partial read needs one per piece.
//!
//! Built from the body at write time and again when a scan replays a segment
//! record, so the segment record's own format is unchanged; it travels in the
//! checkpoint as a trailing `#[serde(default)]` field of `SegmentEntry`.
//!
//! **Building never fails a write.** A body this parser does not understand --
//! a test's filler bytes, a recording from before fragments existed, a `trak`
//! without the boxes read here -- has no table, and a segment without one is
//! read whole, as every segment was before.
//!
//! Std only: `crates/livia-thumb` includes `container.rs` by `#[path]`, so
//! `crate::encoder::mp4_boxes` is not reachable from here.

use serde::{Deserialize, Serialize};

use super::crc32;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentTable {
    /// Everything in front of the first `moof` (`ftyp`, `moov` and friends):
    /// a cut read always takes it, since MF will not open fragments alone.
    pub head_len: u32,
    pub head_crc: u32,
    /// One per `moof`, running to the next `moof` or the body's end, in order;
    /// together with the head they tile the body exactly.
    pub rows: Vec<FragmentRow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FragmentRow {
    /// Body-relative, like `head_len`.
    pub offset: u32,
    pub len: u32,
    pub crc: u32,
    /// Segment-local time of the first video sample, only when that sample is
    /// a sync sample: the one place a cut reader can start decoding.
    pub keyframe_100ns: Option<i64>,
    /// Segment-local time of the first audio sample. With several audio
    /// tracks it is the latest of their first samples, and `i64::MAX` when
    /// some track has none here -- such a row can never be a cut point, so no
    /// track loses audio to a cut.
    pub audio_100ns: Option<i64>,
}

/// How far past a keyframe's `tfdt` a cut reader is trusted to serve. The
/// table holds decode times in the track's timescale; Media Foundation hands
/// out presentation times, and the two round apart -- the 120fps H.264 probe
/// put a keyframe at `tfdt` 10_083_333 and MF at 10_083_250. A seek inside
/// that sliver of a GOP's start reads from the GOP before (or, on a reader cut
/// there, opens the segment again), so no seek lands in front of a cut
/// reader's first sample.
pub const CUT_SLACK_100NS: i64 = 10_000;

impl FragmentTable {
    /// Where a cut read serving `local_100ns` starts: the row to cut at and the
    /// earliest local time the cut reader serves -- its GOP's, plus
    /// [`CUT_SLACK_100NS`]. `None` means read the whole body.
    ///
    /// The cut is the last row whose audio starts at or before the GOP, and
    /// never past the GOP's own row: the audio for a GOP's time is muxed in
    /// front of it (t261017-2334 measured 460ms of it), so cutting at the video
    /// `moof` would drop that audio, and in the last GOP leave a file with no
    /// audio sample that MF refuses to open at all.
    pub fn cut(&self, local_100ns: i64) -> Option<(usize, i64)> {
        self.cut_or_skip(local_100ns).ok()
    }

    /// [`Self::cut`], or why it reads the whole body (t261018-fc75): the
    /// `cut_skip` a whole seek read logs -- `"first_gop"` when fewer than two
    /// keyframes lie before the target, `"no_audio"` when no audio row starts
    /// by the GOP or the cut would land on the head.
    pub fn cut_or_skip(&self, local_100ns: i64) -> Result<(usize, i64), &'static str> {
        let keyframes: Vec<(usize, i64)> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(at, row)| Some((at, row.keyframe_100ns?)))
            .filter(|(_, time)| time + CUT_SLACK_100NS <= local_100ns)
            .collect();
        // The first GOP is not cut: there is nothing in front of it to skip.
        let [.., _, (video_row, gop)] = keyframes[..] else {
            return Err("first_gop");
        };
        let audio_row = if self.rows.iter().any(|row| row.audio_100ns.is_some()) {
            self.rows
                .iter()
                .rposition(|row| row.audio_100ns.is_some_and(|time| time <= gop))
                .ok_or("no_audio")?
        } else {
            video_row
        };
        let cut = audio_row.min(video_row);
        if cut > 0 {
            Ok((cut, gop + CUT_SLACK_100NS))
        } else {
            Err("no_audio")
        }
    }
}

/// The table for one segment body, or `None` when it is not a fragmented MP4
/// this can read (see the module doc).
pub fn fragment_table(body: &[u8]) -> Option<FragmentTable> {
    let top = tiled(body, 0, body.len())?;
    let first_moof = top.iter().position(|entry| &entry.kind == b"moof")?;
    let head_len = top[first_moof].start;
    let moov = top[..first_moof]
        .iter()
        .find(|entry| &entry.kind == b"moov")?;
    let tracks = tracks(body, *moov)?;
    let audio_tracks = tracks.iter().filter(|track| track.audio).count();
    let mut rows = Vec::new();
    let mut at = first_moof;
    while at < top.len() {
        let moof = top[at];
        if &moof.kind != b"moof" {
            return None;
        }
        let next = top[at + 1..]
            .iter()
            .position(|entry| &entry.kind == b"moof")
            .map_or(top.len(), |offset| at + 1 + offset);
        let end = top[next - 1].end;
        let mut keyframe = None;
        let mut audio_firsts = Vec::new();
        for traf in tiled(body, moof.payload_start, moof.end)?
            .into_iter()
            .filter(|entry| &entry.kind == b"traf")
        {
            let Some(first) = first_sample(body, traf, &tracks)? else {
                continue;
            };
            if first.audio {
                audio_firsts.push(first.time_100ns);
            } else if first.sync && keyframe.is_none() {
                keyframe = Some(first.time_100ns);
            }
        }
        let audio = match audio_firsts.len() {
            0 => None,
            n if n < audio_tracks => Some(i64::MAX),
            _ => audio_firsts.into_iter().max(),
        };
        rows.push(FragmentRow {
            offset: u32::try_from(moof.start).ok()?,
            len: u32::try_from(end - moof.start).ok()?,
            crc: crc32(&body[moof.start..end]),
            keyframe_100ns: keyframe,
            audio_100ns: audio,
        });
        at = next;
    }
    Some(FragmentTable {
        head_len: u32::try_from(head_len).ok()?,
        head_crc: crc32(&body[..head_len]),
        rows,
    })
}

// ---------- ISO BMFF reading (the one box walker, t261025-00ad) ----------
//
// `encoder::mp4_boxes` reads its boxes through these too: the dependency can
// only point this way (the module doc), so the walker lives here and the
// encoder's post-processing imports it. The two readers fail differently --
// the encoder turns a malformed box into an `io::Error`, the table above turns
// it into "no table" -- so the walker returns the `io::Error` and the table
// drops it with `.ok()`.

#[derive(Debug, Clone, Copy)]
pub(crate) struct Mp4Box {
    pub(crate) kind: [u8; 4],
    pub(crate) start: usize,
    pub(crate) payload_start: usize,
    pub(crate) end: usize,
}

fn invalid(what: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, what)
}

pub(crate) fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) fn u64_at(bytes: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(bytes.get(at..at + 8)?.try_into().ok()?))
}

/// The boxes laid end to end in `bytes[start..end]`. A size of 0 runs to the
/// parent's end, 1 means a 64-bit size follows; a box that overruns its parent
/// is an error. Fewer than 8 bytes left over at the end are not a box and are
/// left alone -- [`tiled`] is the reading that refuses them.
pub(crate) fn read_boxes(bytes: &[u8], start: usize, end: usize) -> std::io::Result<Vec<Mp4Box>> {
    let mut cursor = start;
    let mut out = Vec::new();
    while cursor + 8 <= end {
        let size32 = u32_at(bytes, cursor).ok_or_else(|| invalid("fMP4 box field truncated"))?;
        let kind: [u8; 4] = bytes
            .get(cursor + 4..cursor + 8)
            .and_then(|kind| kind.try_into().ok())
            .ok_or_else(|| invalid("fMP4 box field truncated"))?;
        let (size, header) = match size32 {
            0 => ((end - cursor) as u64, 8u64),
            1 => {
                if cursor + 16 > end {
                    return Err(invalid("fMP4 extended box header truncated"));
                }
                let big = u64_at(bytes, cursor + 8)
                    .ok_or_else(|| invalid("fMP4 extended box header truncated"))?;
                (big, 16u64)
            }
            size => (size as u64, 8u64),
        };
        if size < header || size > (end - cursor) as u64 {
            return Err(invalid("fMP4 box size outside parent"));
        }
        let box_end = cursor + size as usize;
        out.push(Mp4Box {
            kind,
            start: cursor,
            payload_start: cursor + header as usize,
            end: box_end,
        });
        cursor = box_end;
    }
    Ok(out)
}

/// [`read_boxes`] that must tile `bytes[start..end]` exactly: `None` when it
/// does not, leftover bytes included.
fn tiled(bytes: &[u8], start: usize, end: usize) -> Option<Vec<Mp4Box>> {
    let out = read_boxes(bytes, start, end).ok()?;
    (out.last().map_or(start, |last| last.end) == end).then_some(out)
}

fn child(bytes: &[u8], parent: Mp4Box, kind: &[u8; 4]) -> Option<Mp4Box> {
    tiled(bytes, parent.payload_start, parent.end)?
        .into_iter()
        .find(|entry| &entry.kind == kind)
}

pub(crate) struct Track {
    pub(crate) id: u32,
    pub(crate) timescale: u32,
    pub(crate) audio: bool,
    /// `trex.default_sample_flags`, the last word on a sample's flags.
    pub(crate) default_flags: Option<u32>,
}

/// Every `trak` in `moov`: `tkhd.track_ID`, `mdhd.timescale` (both
/// version-aware: the field sits behind 32- or 64-bit times), the `hdlr`
/// kind, and `trex.default_sample_flags`. `None` if a `trak` lacks one of the
/// boxes or says its timescale is 0.
pub(crate) fn tracks(bytes: &[u8], moov: Mp4Box) -> Option<Vec<Track>> {
    let children = tiled(bytes, moov.payload_start, moov.end)?;
    let trex: Vec<(u32, u32)> = children
        .iter()
        .find(|entry| &entry.kind == b"mvex")
        .and_then(|mvex| tiled(bytes, mvex.payload_start, mvex.end))
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| &entry.kind == b"trex")
        .filter_map(|trex| {
            Some((
                u32_at(bytes, trex.payload_start + 4)?,
                u32_at(bytes, trex.payload_start + 20)?,
            ))
        })
        .collect();
    let mut out = Vec::new();
    for trak in children.iter().filter(|entry| &entry.kind == b"trak") {
        let tkhd = child(bytes, *trak, b"tkhd")?;
        let id = tkhd_track_id(bytes, tkhd)?;
        let mdia = child(bytes, *trak, b"mdia")?;
        let mdhd = child(bytes, mdia, b"mdhd")?;
        let version = *bytes.get(mdhd.payload_start)?;
        let timescale = u32_at(
            bytes,
            mdhd.payload_start + if version == 1 { 20 } else { 12 },
        )?;
        let hdlr = child(bytes, mdia, b"hdlr")?;
        let handler = bytes.get(hdlr.payload_start + 8..hdlr.payload_start + 12)?;
        if timescale == 0 {
            return None;
        }
        out.push(Track {
            id,
            timescale,
            audio: handler == b"soun",
            default_flags: trex
                .iter()
                .find(|(track, _)| *track == id)
                .map(|(_, flags)| *flags),
        });
    }
    Some(out)
}

/// `tkhd.track_ID`, behind version + flags and two 32- or 64-bit times.
pub(crate) fn tkhd_track_id(bytes: &[u8], tkhd: Mp4Box) -> Option<u32> {
    let version = *bytes.get(tkhd.payload_start)?;
    u32_at(
        bytes,
        tkhd.payload_start + if version == 1 { 20 } else { 12 },
    )
}

pub(crate) struct TfhdInfo {
    pub(crate) track_id: u32,
    /// Absolute file offset this fragment's sample data is based from, per the
    /// original `base-data-offset-present` tfhd. Not assumed to equal the
    /// following mdat's payload start -- that is looked up independently and
    /// the two are combined with a trun's own `data_offset` to get this
    /// track's true absolute sample position (see `rebuild_traf_with_tfdt`).
    pub(crate) base_data_offset: Option<u64>,
    pub(crate) default_sample_duration: Option<u32>,
    pub(crate) default_sample_flags: Option<u32>,
}

/// The `tfhd` fields behind its optional-field flags. Read only; the encoder
/// rebuilds a fresh tfhd using `default-base-is-moof` addressing instead.
pub(crate) fn parse_tfhd(bytes: &[u8], tfhd: &Mp4Box) -> std::io::Result<TfhdInfo> {
    let field = |at: usize| u32_at(bytes, at).ok_or_else(|| invalid("fMP4 box field truncated"));
    let flags = field(tfhd.payload_start)? & 0x00FF_FFFF;
    let track_id = field(tfhd.payload_start + 4)?;
    let mut cursor = tfhd.payload_start + 8;
    let base_data_offset = if flags & 0x1 != 0 {
        let value =
            u64_at(bytes, cursor).ok_or_else(|| invalid("tfhd base_data_offset truncated"))?;
        cursor += 8;
        Some(value)
    } else {
        None
    };
    if flags & 0x2 != 0 {
        cursor += 4; // sample_description_index
    }
    let mut optional = |bit: u32| -> std::io::Result<Option<u32>> {
        if flags & bit == 0 {
            return Ok(None);
        }
        let value = field(cursor)?;
        cursor += 4;
        Ok(Some(value))
    };
    let default_sample_duration = optional(0x8)?;
    let _default_sample_size = optional(0x10)?;
    let default_sample_flags = optional(0x20)?;
    Ok(TfhdInfo {
        track_id,
        base_data_offset,
        default_sample_duration,
        default_sample_flags,
    })
}

/// A `trun`'s fixed head: its flags, sample count, where `data_offset` sits
/// (when present), its `first_sample_flags`, and where the per-sample entries
/// start.
pub(crate) struct TrunHead {
    pub(crate) flags: u32,
    pub(crate) sample_count: u32,
    pub(crate) data_offset_field_at: Option<usize>,
    pub(crate) first_sample_flags: Option<u32>,
    pub(crate) samples_at: usize,
}

pub(crate) fn trun_head(bytes: &[u8], trun: &Mp4Box) -> std::io::Result<TrunHead> {
    let field = |at: usize| u32_at(bytes, at).ok_or_else(|| invalid("fMP4 box field truncated"));
    let flags = field(trun.payload_start)? & 0x00FF_FFFF;
    let sample_count = field(trun.payload_start + 4)?;
    let mut cursor = trun.payload_start + 8;
    let data_offset_field_at = (flags & 0x1 != 0).then(|| {
        cursor += 4;
        cursor - 4
    });
    let first_sample_flags = if flags & 0x4 != 0 {
        let value = field(cursor)?;
        cursor += 4;
        Some(value)
    } else {
        None
    };
    Ok(TrunHead {
        flags,
        sample_count,
        data_offset_field_at,
        first_sample_flags,
        samples_at: cursor,
    })
}

struct FirstSample {
    audio: bool,
    sync: bool,
    time_100ns: i64,
}

/// The first sample of one `traf`: `Some(None)` when the traf carries no
/// samples, `None` when it cannot be read.
fn first_sample(bytes: &[u8], traf: Mp4Box, tracks: &[Track]) -> Option<Option<FirstSample>> {
    let children = tiled(bytes, traf.payload_start, traf.end)?;
    let truns: Vec<&Mp4Box> = children
        .iter()
        .filter(|entry| &entry.kind == b"trun")
        .filter(|trun| u32_at(bytes, trun.payload_start + 4).is_some_and(|count| count > 0))
        .collect();
    let Some(trun) = truns.first() else {
        return Some(None);
    };
    let tfhd = children.iter().find(|entry| &entry.kind == b"tfhd")?;
    let tfhd = parse_tfhd(bytes, tfhd).ok()?;
    let track = tracks.iter().find(|track| track.id == tfhd.track_id)?;
    let tfdt = children.iter().find(|entry| &entry.kind == b"tfdt")?;
    let decode_time = if *bytes.get(tfdt.payload_start)? == 1 {
        u64_at(bytes, tfdt.payload_start + 4)?
    } else {
        u32_at(bytes, tfdt.payload_start + 4)? as u64
    };
    let head = trun_head(bytes, trun).ok()?;
    let flags = if head.first_sample_flags.is_some() {
        head.first_sample_flags
    } else if head.flags & 0x400 != 0 {
        // The first sample's own flags, behind its duration and size.
        let before = [0x100, 0x200]
            .iter()
            .filter(|bit| head.flags & **bit != 0)
            .count();
        Some(u32_at(bytes, head.samples_at + 4 * before)?)
    } else {
        tfhd.default_sample_flags.or(track.default_flags)
    };
    let time = decode_time as i128 * 10_000_000 / track.timescale as i128;
    Some(Some(FirstSample {
        audio: track.audio,
        // `sample_is_non_sync_sample` is bit 16; flags nobody wrote down are
        // not taken as a keyframe.
        sync: flags.is_some_and(|flags| flags & 0x0001_0000 == 0),
        time_100ns: i64::try_from(time).ok()?,
    }))
}
