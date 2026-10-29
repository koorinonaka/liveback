//! Read-only ISO BMFF box parsing: the fields this crate's fMP4
//! post-processing and inspection need. The box walker itself, `tfhd` and the
//! `trun` head are `ring_buffer::container::fragments`' (t261025-00ad): that
//! module is std only and cannot import this one, so the one copy lives there.
//! The rewriting side (MSE-compliance rebuild) lives in `rewrite`.

use std::{collections::HashMap, fs, io, path::Path};

pub(crate) use crate::ring_buffer::container::fragments::{
    parse_tfhd, read_boxes, Mp4Box as Mp4BoxEntry,
};
use crate::ring_buffer::container::fragments::{tkhd_track_id, tracks, trun_head, u32_at};

pub(crate) fn read_u32(bytes: &[u8], offset: usize) -> io::Result<u32> {
    u32_at(bytes, offset)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 box field truncated"))
}

pub(crate) struct TrunInfo {
    /// Byte position (within `bytes`) of the `data_offset` field, if the
    /// data-offset-present flag is set. `None` means this run carries no
    /// data_offset of its own (not observed from this app's own muxer, but
    /// handled defensively).
    pub(crate) data_offset_field_at: Option<usize>,
    /// In this track's own `timescale` ticks, like `trun.sample_duration`.
    pub(crate) total_sample_duration_ticks: i64,
}

/// Parses only what this post-processor needs from a `trun`: where its
/// `data_offset` field lives (to be rewritten relative to the new moof start)
/// and the sum of its sample durations (to advance the per-track tfdt cursor).
pub(crate) fn parse_trun(
    bytes: &[u8],
    trun: &Mp4BoxEntry,
    default_sample_duration: Option<u32>,
) -> io::Result<TrunInfo> {
    let head = trun_head(bytes, trun)?;
    let sample_count = head.sample_count as usize;
    let total_sample_duration_ticks = if head.flags & 0x0000_0100 != 0 {
        let stride = sample_stride(head.flags);
        let mut total = 0i64;
        for index in 0..sample_count {
            total += read_u32(bytes, head.samples_at + index * stride)? as i64;
        }
        total
    } else {
        default_sample_duration.unwrap_or(0) as i64 * sample_count as i64
    };
    Ok(TrunInfo {
        data_offset_field_at: head.data_offset_field_at,
        total_sample_duration_ticks,
    })
}

/// Bytes per `trun` sample entry: 4 for each of duration, size, flags and
/// composition offset that the flags say are present.
fn sample_stride(flags: u32) -> usize {
    [0x100, 0x200, 0x400, 0x800]
        .iter()
        .filter(|bit| flags & **bit != 0)
        .count()
        * 4
}

pub(crate) struct TrackMeta {
    pub(crate) timescale: u32,
    pub(crate) is_audio: bool,
}

/// Reads `tkhd.track_ID`, version-aware (the field's byte offset depends on
/// whether `creation_time`/`modification_time` are 32- or 64-bit).
pub(crate) fn parse_tkhd_track_id(bytes: &[u8], tkhd: &Mp4BoxEntry) -> io::Result<u32> {
    tkhd_track_id(bytes, *tkhd)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "tkhd truncated"))
}

/// Walks every `trak` in `moov` to learn each track's `mdhd.timescale` and
/// whether it is audio or video, keyed by `tkhd.track_ID` (the same ID a
/// fragment's `tfhd.track_ID` refers to).
pub(crate) fn collect_track_meta(
    bytes: &[u8],
    moov: &Mp4BoxEntry,
) -> io::Result<HashMap<u32, TrackMeta>> {
    let tracks = tracks(bytes, *moov)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 trak unreadable"))?;
    Ok(tracks
        .into_iter()
        .map(|track| {
            (
                track.id,
                TrackMeta {
                    timescale: track.timescale,
                    is_audio: track.audio,
                },
            )
        })
        .collect())
}

/// The video track's `tkhd` width and height, read from `moov` alone: the
/// file is walked box header by box header and only `moov` is loaded, so a
/// 30MB segment on the HDD buffer costs a few kilobytes, not a full read.
/// `None` when there is no `moov` or no track with a size.
pub(crate) fn video_track_size(path: &Path) -> io::Result<Option<(u32, u32)>> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let mut position = 0u64;
    let moov = loop {
        if position + 8 > length {
            return Ok(None);
        }
        let mut header = [0u8; 8];
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut header)?;
        let size = match u32::from_be_bytes(header[..4].try_into().unwrap()) as u64 {
            0 => length - position,
            1 => {
                let mut big = [0u8; 8];
                file.read_exact(&mut big)?;
                u64::from_be_bytes(big)
            }
            size => size,
        };
        if size < 8 || position + size > length {
            return Ok(None);
        }
        if &header[4..] == b"moov" {
            let mut bytes = vec![0u8; size as usize];
            file.seek(SeekFrom::Start(position))?;
            file.read_exact(&mut bytes)?;
            break bytes;
        }
        position += size;
    };
    let top = read_boxes(&moov, 0, moov.len())?;
    let Some(moov_box) = top.first() else {
        return Ok(None);
    };
    for trak in read_boxes(&moov, moov_box.payload_start, moov_box.end)?
        .iter()
        .filter(|entry| entry.kind == *b"trak")
    {
        // The last two fields of tkhd in either version: 16.16 fixed point.
        if let Some(tkhd) = read_boxes(&moov, trak.payload_start, trak.end)?
            .iter()
            .find(|entry| entry.kind == *b"tkhd")
        {
            let width = read_u32(&moov, tkhd.end - 8)? >> 16;
            let height = read_u32(&moov, tkhd.end - 4)? >> 16;
            if width > 0 && height > 0 {
                return Ok(Some((width, height)));
            }
        }
    }
    Ok(None)
}

/// Per-track box-level measurement used by Task062 Step1 (offline segment
/// inspection) and `--liveback-inspect`-style dumps: reads `moov` for
/// timescale/kind and sums every `moof`'s `trun` sample counts/durations for
/// that track, without decoding any media (no `inspect_finalized_mp4`/Media
/// Foundation involved). `total_sample_duration_ticks` is in this track's own
/// `timescale`, like `trun.sample_duration` itself -- not 100ns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackBoxSummary {
    pub track_id: u32,
    pub is_audio: bool,
    pub timescale: u32,
    pub sample_count: u64,
    pub total_sample_duration_ticks: i64,
}

/// Every `traf` in the file, handed over as its children and its parsed
/// `tfhd`. Both box walkers below want exactly this and nothing else about the
/// moof layout.
#[cfg(test)]
fn for_each_traf(
    bytes: &[u8],
    top: &[Mp4BoxEntry],
    mut visit: impl FnMut(
        &[Mp4BoxEntry],
        &crate::ring_buffer::container::fragments::TfhdInfo,
    ) -> io::Result<()>,
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

#[cfg(test)]
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
            *duration_ticks.entry(tfhd_info.track_id).or_default() +=
                trun_info.total_sample_duration_ticks;
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
            let head = trun_head(&bytes, trun)?;
            let stride = sample_stride(head.flags);
            for index in 0..head.sample_count as usize {
                durations.push(if head.flags & 0x0000_0100 != 0 {
                    read_u32(&bytes, head.samples_at + index * stride)?
                } else {
                    tfhd_info.default_sample_duration.unwrap_or(0)
                });
            }
        }
        Ok(())
    })?;
    Ok(durations)
}
