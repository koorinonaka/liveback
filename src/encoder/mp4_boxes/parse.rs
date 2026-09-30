//! Read-only ISO BMFF box parsing: box enumeration and the specific fields
//! this crate's fMP4 post-processing and inspection need. The rewriting side
//! (MSE-compliance rebuild) lives in `rewrite`.

use std::{collections::HashMap, fs, io, path::Path};

#[derive(Debug, Clone, Copy)]
pub(crate) struct Mp4BoxEntry {
    pub(crate) kind: [u8; 4],
    pub(crate) start: usize,
    pub(crate) payload_start: usize,
    pub(crate) end: usize,
}

pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    bytes
        .get(offset..offset + 4)
        .map(|slice| u32::from_be_bytes(slice.try_into().unwrap()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 box field truncated"))
}

pub(crate) fn read_boxes(bytes: &[u8], start: usize, end: usize) -> io::Result<Vec<Mp4BoxEntry>> {
    let mut cursor = start;
    let mut boxes = Vec::new();
    while cursor + 8 <= end {
        let size32 = read_u32(bytes, cursor)? as u64;
        let kind: [u8; 4] = bytes[cursor + 4..cursor + 8].try_into().unwrap();
        let (size, header) = match size32 {
            0 => ((end - cursor) as u64, 8u64),
            1 => {
                if cursor + 16 > end {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "fMP4 extended box header truncated",
                    ));
                }
                let big = u64::from_be_bytes(bytes[cursor + 8..cursor + 16].try_into().unwrap());
                (big, 16u64)
            }
            size => (size, 8u64),
        };
        if size < header || cursor + size as usize > end {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fMP4 box size outside parent",
            ));
        }
        let box_end = cursor + size as usize;
        boxes.push(Mp4BoxEntry {
            kind,
            start: cursor,
            payload_start: cursor + header as usize,
            end: box_end,
        });
        cursor = box_end;
    }
    Ok(boxes)
}

pub(crate) struct TfhdInfo {
    pub(crate) track_id: u32,
    /// Absolute file offset this fragment's sample data is based from, per the
    /// original `base-data-offset-present` tfhd. Not assumed to equal the
    /// following mdat's payload start — that is looked up independently and the
    /// two are combined with a trun's own `data_offset` to get this track's true
    /// absolute sample position (see `rebuild_traf_with_tfdt`).
    pub(crate) base_data_offset: Option<u64>,
    pub(crate) default_sample_duration: Option<u32>,
}

/// Reads only the fixed-position fields this post-processor needs (track_ID,
/// base_data_offset, default_sample_duration). Never writes; callers rebuild a
/// fresh tfhd using `default-base-is-moof` addressing instead.
pub(crate) fn parse_tfhd(bytes: &[u8], tfhd: &Mp4BoxEntry) -> io::Result<TfhdInfo> {
    let version_flags = read_u32(bytes, tfhd.payload_start)?;
    let flags = version_flags & 0x00FF_FFFF;
    let track_id = read_u32(bytes, tfhd.payload_start + 4)?;
    let mut cursor = tfhd.payload_start + 8;
    let base_data_offset = if flags & 0x0000_0001 != 0 {
        let value = bytes
            .get(cursor..cursor + 8)
            .map(|slice| u64::from_be_bytes(slice.try_into().unwrap()))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tfhd base_data_offset truncated",
                )
            })?;
        cursor += 8;
        Some(value)
    } else {
        None
    };
    if flags & 0x0000_0002 != 0 {
        cursor += 4; // sample_description_index
    }
    let default_sample_duration = if flags & 0x0000_0008 != 0 {
        Some(read_u32(bytes, cursor)?)
    } else {
        None
    };
    Ok(TfhdInfo {
        track_id,
        base_data_offset,
        default_sample_duration,
    })
}

pub(crate) struct TrunInfo {
    /// Byte position (within `bytes`) of the `data_offset` field, if the
    /// data-offset-present flag is set. `None` means this run carries no
    /// data_offset of its own (not observed from this app's own muxer, but
    /// handled defensively).
    pub(crate) data_offset_field_at: Option<usize>,
    pub(crate) total_sample_duration_100ns: i64,
}

/// Parses only what this post-processor needs from a `trun`: where its
/// `data_offset` field lives (to be rewritten relative to the new moof start)
/// and the sum of its sample durations (to advance the per-track tfdt cursor).
pub(crate) fn parse_trun(
    bytes: &[u8],
    trun: &Mp4BoxEntry,
    default_sample_duration: Option<u32>,
) -> io::Result<TrunInfo> {
    let version_flags = read_u32(bytes, trun.payload_start)?;
    let flags = version_flags & 0x00FF_FFFF;
    let sample_count = read_u32(bytes, trun.payload_start + 4)? as usize;
    let mut cursor = trun.payload_start + 8;
    let data_offset_field_at = if flags & 0x0000_0001 != 0 {
        let at = cursor;
        cursor += 4;
        Some(at)
    } else {
        None
    };
    if flags & 0x0000_0004 != 0 {
        cursor += 4; // first_sample_flags
    }
    let has_duration = flags & 0x0000_0100 != 0;
    let has_size = flags & 0x0000_0200 != 0;
    let has_flags = flags & 0x0000_0400 != 0;
    let has_composition_offset = flags & 0x0000_0800 != 0;
    let total_sample_duration_100ns = if has_duration {
        let mut total = 0i64;
        for _ in 0..sample_count {
            total += read_u32(bytes, cursor)? as i64;
            cursor += 4;
            if has_size {
                cursor += 4;
            }
            if has_flags {
                cursor += 4;
            }
            if has_composition_offset {
                cursor += 4;
            }
        }
        total
    } else {
        default_sample_duration.unwrap_or(0) as i64 * sample_count as i64
    };
    Ok(TrunInfo {
        data_offset_field_at,
        total_sample_duration_100ns,
    })
}

pub(crate) struct TrackMeta {
    pub(crate) timescale: u32,
    pub(crate) is_audio: bool,
}

/// Reads `tkhd.track_ID`, version-aware (the field's byte offset depends on
/// whether `creation_time`/`modification_time` are 32- or 64-bit).
pub(crate) fn parse_tkhd_track_id(bytes: &[u8], tkhd: &Mp4BoxEntry) -> io::Result<u32> {
    let version = *bytes
        .get(tkhd.payload_start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tkhd truncated"))?;
    let track_id_offset = tkhd.payload_start + if version == 1 { 4 + 8 + 8 } else { 4 + 4 + 4 };
    read_u32(bytes, track_id_offset)
}

/// Reads `mdhd.timescale`, version-aware (same 32-/64-bit reasoning as tkhd).
fn parse_mdhd_timescale(bytes: &[u8], mdhd: &Mp4BoxEntry) -> io::Result<u32> {
    let version = *bytes
        .get(mdhd.payload_start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mdhd truncated"))?;
    let timescale_offset = mdhd.payload_start + if version == 1 { 4 + 8 + 8 } else { 4 + 4 + 4 };
    read_u32(bytes, timescale_offset)
}

/// Reads `hdlr.handler_type` (`"vide"` or `"soun"`) to tell tracks apart.
fn parse_hdlr_is_audio(bytes: &[u8], hdlr: &Mp4BoxEntry) -> io::Result<bool> {
    let handler_type_offset = hdlr.payload_start + 4 + 4; // version+flags, pre_defined
    let handler_type = bytes
        .get(handler_type_offset..handler_type_offset + 4)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hdlr truncated"))?;
    Ok(handler_type == b"soun")
}

/// Walks every `trak` in `moov` to learn each track's `mdhd.timescale` and
/// whether it is audio or video, keyed by `tkhd.track_ID` (the same ID a
/// fragment's `tfhd.track_ID` refers to).
pub(crate) fn collect_track_meta(
    bytes: &[u8],
    moov: &Mp4BoxEntry,
) -> io::Result<HashMap<u32, TrackMeta>> {
    let mut out = HashMap::new();
    for trak in read_boxes(bytes, moov.payload_start, moov.end)?
        .iter()
        .filter(|entry| entry.kind == *b"trak")
    {
        let trak_children = read_boxes(bytes, trak.payload_start, trak.end)?;
        let tkhd = trak_children
            .iter()
            .find(|entry| entry.kind == *b"tkhd")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "trak missing tkhd"))?;
        let track_id = parse_tkhd_track_id(bytes, tkhd)?;
        let mdia = trak_children
            .iter()
            .find(|entry| entry.kind == *b"mdia")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "trak missing mdia"))?;
        let mdia_children = read_boxes(bytes, mdia.payload_start, mdia.end)?;
        let mdhd = mdia_children
            .iter()
            .find(|entry| entry.kind == *b"mdhd")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mdia missing mdhd"))?;
        let timescale = parse_mdhd_timescale(bytes, mdhd)?;
        let hdlr = mdia_children
            .iter()
            .find(|entry| entry.kind == *b"hdlr")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "mdia missing hdlr"))?;
        let is_audio = parse_hdlr_is_audio(bytes, hdlr)?;
        out.insert(
            track_id,
            TrackMeta {
                timescale,
                is_audio,
            },
        );
    }
    Ok(out)
}

/// Per-track box-level measurement used by Task062 Step1 (offline segment
/// inspection) and reused by nothing else: reads `moov` for timescale/kind and
/// sums every `moof`'s `trun` sample counts/durations for that track, without
/// decoding any media (no `inspect_finalized_mp4`/Media Foundation involved).
/// `total_sample_duration_ticks` is in this track's own `timescale`, like
/// `trun.sample_duration` itself — not 100ns (unlike `TrunInfo`'s
/// similarly-named field, whose "_100ns" only ever described this app's own
/// pre-Task062 assumption, not what the box format actually stores).
pub(crate) struct TrackBoxSummary {
    pub(crate) track_id: u32,
    pub(crate) is_audio: bool,
    pub(crate) timescale: u32,
    pub(crate) sample_count: u64,
    pub(crate) total_sample_duration_ticks: i64,
}

/// Every `traf` in the file, handed over as its children and its parsed
/// `tfhd`. Both box walkers below want exactly this and nothing else about the
/// moof layout.
fn for_each_traf(
    bytes: &[u8],
    top: &[Mp4BoxEntry],
    mut visit: impl FnMut(&[Mp4BoxEntry], &TfhdInfo) -> io::Result<()>,
) -> io::Result<()> {
    for moof in top.iter().filter(|entry| entry.kind == *b"moof") {
        for traf in read_boxes(bytes, moof.payload_start, moof.end)?
            .iter()
            .filter(|entry| entry.kind == *b"traf")
        {
            let children = read_boxes(bytes, traf.payload_start, traf.end)?;
            let tfhd = children
                .iter()
                .find(|entry| entry.kind == *b"tfhd")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "traf missing tfhd"))?;
            let tfhd_info = parse_tfhd(bytes, tfhd)?;
            visit(&children, &tfhd_info)?;
        }
    }
    Ok(())
}

pub(crate) fn summarize_tracks(path: &Path) -> io::Result<Vec<TrackBoxSummary>> {
    let bytes = fs::read(path)?;
    let top = read_boxes(&bytes, 0, bytes.len())?;
    let moov = top
        .iter()
        .find(|entry| entry.kind == *b"moov")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 missing moov"))?;
    let track_meta = collect_track_meta(&bytes, moov)?;

    let mut sample_counts: HashMap<u32, u64> = HashMap::new();
    let mut duration_ticks: HashMap<u32, i64> = HashMap::new();
    for_each_traf(&bytes, &top, |children, tfhd_info| {
        for trun in children.iter().filter(|entry| entry.kind == *b"trun") {
            let trun_info = parse_trun(&bytes, trun, tfhd_info.default_sample_duration)?;
            let sample_count = read_u32(&bytes, trun.payload_start + 4)? as u64;
            *sample_counts.entry(tfhd_info.track_id).or_default() += sample_count;
            // `TrunInfo.total_sample_duration_100ns` is actually in this
            // trun's own track timescale ticks (see note above).
            *duration_ticks.entry(tfhd_info.track_id).or_default() +=
                trun_info.total_sample_duration_100ns;
        }
        Ok(())
    })?;
    Ok(track_meta
        .into_iter()
        .map(|(track_id, meta)| TrackBoxSummary {
            track_id,
            is_audio: meta.is_audio,
            timescale: meta.timescale,
            sample_count: *sample_counts.get(&track_id).unwrap_or(&0),
            total_sample_duration_ticks: *duration_ticks.get(&track_id).unwrap_or(&0),
        })
        .collect())
}

/// Task088 Step1: per-sample (not summed) audio-track durations, in this
/// track's own timescale ticks, across every `moof` in `path`. Tells apart
/// "samples are individually present at the nominal AAC frame length but too
/// few of them exist" from "each sample's own declared duration is inflated
/// beyond the nominal AAC frame length" -- `summarize_tracks` above only
/// returns the summed total, which cannot distinguish the two. When a trun's
/// `has_duration` flag (0x100) is unset, every sample in it uses the traf's
/// `tfhd.default_sample_duration` uniformly, which this still reports (one
/// entry per sample) so both cases render the same way to a caller.
#[cfg(test)]
pub(crate) fn dump_audio_sample_durations(path: &Path) -> io::Result<Vec<u32>> {
    dump_sample_durations(path, true)
}

/// The same for the video track (task202: shows the per-sample gap a segment
/// opens with, which the summed total cannot).
#[cfg(test)]
pub(crate) fn dump_video_sample_durations(path: &Path) -> io::Result<Vec<u32>> {
    dump_sample_durations(path, false)
}

#[cfg(test)]
fn dump_sample_durations(path: &Path, want_audio: bool) -> io::Result<Vec<u32>> {
    let bytes = fs::read(path)?;
    let top = read_boxes(&bytes, 0, bytes.len())?;
    let moov = top
        .iter()
        .find(|entry| entry.kind == *b"moov")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 missing moov"))?;
    let track_meta = collect_track_meta(&bytes, moov)?;
    let audio_track_id = track_meta
        .iter()
        .find(|(_, meta)| meta.is_audio == want_audio)
        .map(|(id, _)| *id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 has no matching track"))?;

    let mut durations = Vec::new();
    for_each_traf(&bytes, &top, |children, tfhd_info| {
        if tfhd_info.track_id != audio_track_id {
            return Ok(());
        }
        for trun in children.iter().filter(|entry| entry.kind == *b"trun") {
            let version_flags = read_u32(&bytes, trun.payload_start)?;
            let flags = version_flags & 0x00FF_FFFF;
            let sample_count = read_u32(&bytes, trun.payload_start + 4)? as usize;
            let mut cursor = trun.payload_start + 8;
            if flags & 0x0000_0001 != 0 {
                cursor += 4; // data_offset
            }
            if flags & 0x0000_0004 != 0 {
                cursor += 4; // first_sample_flags
            }
            let has_duration = flags & 0x0000_0100 != 0;
            let has_size = flags & 0x0000_0200 != 0;
            let has_flags = flags & 0x0000_0400 != 0;
            let has_composition_offset = flags & 0x0000_0800 != 0;
            for _ in 0..sample_count {
                let duration = if has_duration {
                    let value = read_u32(&bytes, cursor)?;
                    cursor += 4;
                    value
                } else {
                    tfhd_info.default_sample_duration.unwrap_or(0)
                };
                if has_size {
                    cursor += 4;
                }
                if has_flags {
                    cursor += 4;
                }
                if has_composition_offset {
                    cursor += 4;
                }
                durations.push(duration);
            }
        }
        Ok(())
    })?;
    Ok(durations)
}
