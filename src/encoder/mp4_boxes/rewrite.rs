//! The rewriting side of the fMP4 post-processor: rebuilds `moof`/`moov` so
//! `MFCreateFMPEG4MediaSink` output becomes MSE-compliant (tfdt injection,
//! `default-base-is-moof` addressing, zero-sample-track removal). Parsing
//! primitives live in `parse`.

use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::Path,
};

use super::parse::{
    collect_track_meta, parse_tfhd, parse_tkhd_track_id, parse_trun, read_boxes, read_u32,
    Mp4BoxEntry,
};
use super::TFDT_BOX_LEN;

/// A fresh, minimal tfhd using `default-base-is-moof` (0x020000): version 0, no
/// optional fields beyond track_ID. MSE requires this addressing mode and
/// disallows `base-data-offset-present`, which is what MFCreateFMPEG4MediaSink
/// emits instead (https://www.w3.org/TR/mse-byte-stream-format-isobmff/#movie-fragment-relative-addressing).
fn build_default_base_is_moof_tfhd(track_id: u32) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&16u32.to_be_bytes());
    out[4..8].copy_from_slice(b"tfhd");
    out[8..11].copy_from_slice(&[0, 0x02, 0x00]); // version 0, flags 0x020000
    out[11] = 0x00;
    out[12..16].copy_from_slice(&track_id.to_be_bytes());
    out
}

pub(crate) fn build_tfdt_box(base_media_decode_time: u64) -> [u8; TFDT_BOX_LEN] {
    let mut out = [0u8; TFDT_BOX_LEN];
    out[0..4].copy_from_slice(&(TFDT_BOX_LEN as u32).to_be_bytes());
    out[4..8].copy_from_slice(b"tfdt");
    out[8] = 1; // version 1: 64-bit baseMediaDecodeTime
    out[12..20].copy_from_slice(&base_media_decode_time.to_be_bytes());
    out
}

/// Rebuilds one `traf`: tfhd switches from `base-data-offset-present` (absolute
/// file offset; disallowed by MSE) to `default-base-is-moof` (offset relative to
/// this moof's own start; required by MSE), each `trun.data_offset` is patched to
/// match, and a `tfdt` is inserted right after tfhd with this track's cumulative
/// duration from earlier fragments in this same file. Each `trun.data_offset` is
/// left as a placeholder (its within-mdat position is returned instead) since the
/// real value — relative to this moof's own start — depends on this whole moof's
/// final size, known only once every traf in it has been rebuilt; the caller
/// patches it in a second pass.
/// (byte offset of a pending `trun.data_offset` field, that track's sample-data
/// offset within the shared mdat's payload), collected until the enclosing
/// moof's final size is known (see `rebuild_moof_with_tfdt`).
type DataOffsetPatches = Vec<(usize, i64)>;

fn rebuild_traf_with_tfdt(
    bytes: &[u8],
    traf: &Mp4BoxEntry,
    old_mdat_payload_start: usize,
    track_cursor_by_id: &mut HashMap<u32, i64>,
) -> io::Result<(Vec<u8>, DataOffsetPatches)> {
    let children = read_boxes(bytes, traf.payload_start, traf.end)?;
    let tfhd = children
        .iter()
        .find(|entry| entry.kind == *b"tfhd")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "traf missing tfhd"))?;
    let tfhd_info = parse_tfhd(bytes, tfhd)?;
    let base_media_decode_time = *track_cursor_by_id.get(&tfhd_info.track_id).unwrap_or(&0);

    let mut body = Vec::with_capacity(traf.end - traf.payload_start + TFDT_BOX_LEN);
    body.extend_from_slice(&build_default_base_is_moof_tfhd(tfhd_info.track_id));
    body.extend_from_slice(&build_tfdt_box(base_media_decode_time as u64));

    let mut total_duration = 0i64;
    // (byte offset within `body` of a trun's data_offset field, that track's
    // sample-data offset within the shared mdat's payload).
    let mut data_offset_patches = Vec::new();
    for child in &children {
        if child.kind == *b"tfhd" {
            continue;
        }
        if child.kind != *b"trun" {
            body.extend_from_slice(&bytes[child.start..child.end]);
            continue;
        }
        let trun_info = parse_trun(bytes, child, tfhd_info.default_sample_duration)?;
        total_duration += trun_info.total_sample_duration_100ns;
        if let Some(field_at) = trun_info.data_offset_field_at {
            // The absolute file position this track's data starts at, per the
            // original addressing: tfhd.base_data_offset (if present, else 0)
            // plus this trun's own (possibly absent, then 0) data_offset. Not
            // assumed to equal the mdat's own start — it is measured against the
            // actual mdat instead, since different tracks/fragments have been
            // observed to lay out samples within the shared mdat differently.
            let old_absolute = tfhd_info.base_data_offset.unwrap_or(0) as i64
                + read_u32(bytes, field_at)? as i32 as i64;
            let within_mdat_offset = old_absolute - old_mdat_payload_start as i64;
            data_offset_patches.push((body.len() + (field_at - child.start), within_mdat_offset));
        }
        body.extend_from_slice(&bytes[child.start..child.end]);
    }
    track_cursor_by_id.insert(tfhd_info.track_id, base_media_decode_time + total_duration);

    let mut traf_bytes = Vec::with_capacity(8 + body.len());
    traf_bytes.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    traf_bytes.extend_from_slice(b"traf");
    traf_bytes.extend_from_slice(&body);
    let patches = data_offset_patches
        .into_iter()
        .map(|(pos, value)| (pos + 8, value))
        .collect();
    Ok((traf_bytes, patches))
}

/// `default-base-is-moof` addressing (see `inject_tfdt_if_missing`) makes
/// `trun.data_offset` self-contained: it only has to say "how far from my own
/// moof's start does my sample data begin", which depends solely on that moof's
/// own final size plus this track's fixed position within the mdat that follows
/// it (`within_mdat_offset`). Building every traf first, then patching every
/// `data_offset` once the whole moof's size is known, avoids needing any
/// cross-moof or whole-file position at all.
fn rebuild_moof_with_tfdt(
    bytes: &[u8],
    moof: &Mp4BoxEntry,
    old_mdat_payload_start: usize,
    track_cursor_by_id: &mut HashMap<u32, i64>,
    drop_track_ids: &HashSet<u32>,
) -> io::Result<Vec<u8>> {
    let children = read_boxes(bytes, moof.payload_start, moof.end)?;
    let traf_count = children
        .iter()
        .filter(|entry| entry.kind == *b"traf")
        .count();

    let mut body = Vec::with_capacity(moof.end - moof.payload_start + traf_count * TFDT_BOX_LEN);
    let mut patches: DataOffsetPatches = Vec::new();
    for child in &children {
        if child.kind != *b"traf" {
            body.extend_from_slice(&bytes[child.start..child.end]);
            continue;
        }
        if !drop_track_ids.is_empty() {
            let traf_children = read_boxes(bytes, child.payload_start, child.end)?;
            let tfhd = traf_children
                .iter()
                .find(|entry| entry.kind == *b"tfhd")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "traf missing tfhd"))?;
            if drop_track_ids.contains(&parse_tfhd(bytes, tfhd)?.track_id) {
                continue; // this track has zero samples anywhere in the file (see find_zero_sample_track_ids)
            }
        }
        let traf_offset_in_body = body.len();
        let (traf_bytes, traf_patches) =
            rebuild_traf_with_tfdt(bytes, child, old_mdat_payload_start, track_cursor_by_id)?;
        for (pos, value) in traf_patches {
            patches.push((traf_offset_in_body + pos, value));
        }
        body.extend_from_slice(&traf_bytes);
    }
    let moof_total_size = 8 + body.len();
    const MDAT_HEADER_LEN: i64 = 8;
    for (pos, within_mdat_offset) in patches {
        let new_data_offset = moof_total_size as i64 + MDAT_HEADER_LEN + within_mdat_offset;
        body[pos..pos + 4].copy_from_slice(&(new_data_offset as i32).to_be_bytes());
    }
    let mut moof_bytes = Vec::with_capacity(8 + body.len());
    moof_bytes.extend_from_slice(&(moof_total_size as u32).to_be_bytes());
    moof_bytes.extend_from_slice(b"moof");
    moof_bytes.extend_from_slice(&body);
    Ok(moof_bytes)
}

/// True when every `traf` in this `moof` already has a `tfdt` and already uses
/// `default-base-is-moof` addressing (tfhd flags bit 0x020000), i.e. nothing this
/// post-processor does is needed. Lets a future OS/driver update that fixes
/// MFCreateFMPEG4MediaSink upstream pass through unchanged instead of being
/// rewritten (and mis-detected as still needing tfdt).
pub(crate) fn moof_is_mse_compliant(bytes: &[u8], moof: &Mp4BoxEntry) -> bool {
    let Ok(children) = read_boxes(bytes, moof.payload_start, moof.end) else {
        return false;
    };
    let trafs: Vec<_> = children
        .iter()
        .filter(|entry| entry.kind == *b"traf")
        .collect();
    !trafs.is_empty()
        && trafs.iter().all(|traf| {
            let Ok(entries) = read_boxes(bytes, traf.payload_start, traf.end) else {
                return false;
            };
            let has_tfdt = entries.iter().any(|entry| entry.kind == *b"tfdt");
            let default_base_is_moof = entries
                .iter()
                .find(|entry| entry.kind == *b"tfhd")
                .and_then(|tfhd| read_u32(bytes, tfhd.payload_start).ok())
                .is_some_and(|version_flags| version_flags & 0x0002_0000 != 0);
            has_tfdt && default_base_is_moof
        })
}

/// Task085: `MFCreateFMPEG4MediaSink` finalizes a segment whose fragment(s) never
/// contained a single sample for some declared track (observed for the audio
/// track when a segment is shorter than one AAC frame, ~21.33ms, immediately
/// around a real WGC "映像フレーム中断" capture-interruption gap) by leaving that
/// track's every `traf` with a `tfhd`/`tfdt` but no `trun` at all. Confirmed by
/// direct experiment against real captured segments: `MFCreateSourceReaderFromURL`
/// accepts a file with a mid-file empty `traf` for a track that *does* have
/// samples elsewhere in the file (e.g. one fragment out of three), but rejects
/// with `MF_E_UNSUPPORTED_BYTESTREAM_TYPE` (0xC00D36C4) outright whenever a track
/// declared in `moov` has zero samples across the *entire* file. Only removing
/// that track's `trak`/`trex` from `moov` (not merely dropping the empty `traf`
/// from `moof`, which alone was insufficient in that experiment) made the file
/// open successfully again. Returns every `track_id` from `track_ids` for which
/// no `trun` was found in any `moof` in this file. Keys on a `trun`'s mere
/// presence, not its `sample_count`: this app's own encoder never emits a
/// present-but-zero-count `trun` (confirmed against real captured data), but if
/// some other producer ever did, that track would be kept and the file would
/// still be rejected — this function would need a `sample_count` check too.
fn find_zero_sample_track_ids(
    bytes: &[u8],
    moofs: &[&Mp4BoxEntry],
    track_ids: impl Iterator<Item = u32>,
) -> io::Result<HashSet<u32>> {
    let mut seen_with_samples: HashSet<u32> = HashSet::new();
    for moof in moofs {
        for traf in read_boxes(bytes, moof.payload_start, moof.end)?
            .iter()
            .filter(|entry| entry.kind == *b"traf")
        {
            let children = read_boxes(bytes, traf.payload_start, traf.end)?;
            let has_trun = children.iter().any(|entry| entry.kind == *b"trun");
            if !has_trun {
                continue;
            }
            let tfhd = children
                .iter()
                .find(|entry| entry.kind == *b"tfhd")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "traf missing tfhd"))?;
            seen_with_samples.insert(parse_tfhd(bytes, tfhd)?.track_id);
        }
    }
    Ok(track_ids
        .filter(|track_id| !seen_with_samples.contains(track_id))
        .collect())
}

/// Task085 companion to `find_zero_sample_track_ids`: rebuilds `moov` with the
/// `trak`(s) for `drop_track_ids` removed entirely (not just emptied), along with
/// their corresponding `mvex`/`trex` entries. A `mvex` left with no `trex`
/// children is dropped too, rather than emitted empty.
fn rebuild_moov_without_tracks(
    bytes: &[u8],
    moov: &Mp4BoxEntry,
    drop_track_ids: &HashSet<u32>,
) -> io::Result<Vec<u8>> {
    let children = read_boxes(bytes, moov.payload_start, moov.end)?;
    let mut body = Vec::with_capacity(moov.end - moov.payload_start);
    for child in &children {
        if child.kind == *b"trak" {
            let trak_children = read_boxes(bytes, child.payload_start, child.end)?;
            let tkhd = trak_children
                .iter()
                .find(|entry| entry.kind == *b"tkhd")
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "trak missing tkhd"))?;
            if drop_track_ids.contains(&parse_tkhd_track_id(bytes, tkhd)?) {
                continue;
            }
            body.extend_from_slice(&bytes[child.start..child.end]);
            continue;
        }
        if child.kind == *b"mvex" {
            let mvex_children = read_boxes(bytes, child.payload_start, child.end)?;
            let mut mvex_body = Vec::new();
            for mvex_child in &mvex_children {
                if mvex_child.kind == *b"trex" {
                    let track_id = read_u32(bytes, mvex_child.payload_start + 4)?;
                    if drop_track_ids.contains(&track_id) {
                        continue;
                    }
                }
                mvex_body.extend_from_slice(&bytes[mvex_child.start..mvex_child.end]);
            }
            if mvex_body.is_empty() {
                continue; // no trex left; an empty mvex serves no purpose
            }
            let mut mvex_bytes = Vec::with_capacity(8 + mvex_body.len());
            mvex_bytes.extend_from_slice(&((8 + mvex_body.len()) as u32).to_be_bytes());
            mvex_bytes.extend_from_slice(b"mvex");
            mvex_bytes.extend_from_slice(&mvex_body);
            body.extend_from_slice(&mvex_bytes);
            continue;
        }
        body.extend_from_slice(&bytes[child.start..child.end]);
    }
    let mut moov_bytes = Vec::with_capacity(8 + body.len());
    moov_bytes.extend_from_slice(&((8 + body.len()) as u32).to_be_bytes());
    moov_bytes.extend_from_slice(b"moov");
    moov_bytes.extend_from_slice(&body);
    Ok(moov_bytes)
}

/// `MFCreateFMPEG4MediaSink` produces fragments Chromium's MSE will not accept for
/// two reasons: no `tfdt` (track fragment base media decode time) in any `traf`,
/// and `tfhd` addresses sample data with `base-data-offset-present` (an absolute
/// file offset), which the MSE ISOBMFF byte-stream spec explicitly disallows in
/// favor of `default-base-is-moof`
/// (https://www.w3.org/TR/mse-byte-stream-format-isobmff/#movie-fragment-relative-addressing).
/// This rewrites every `moof` to fix both: each `traf`'s tfhd switches addressing
/// mode (and its `trun.data_offset` is repointed to match, still exactly locating
/// the same bytes in the same shared `mdat`), and a `tfdt` is inserted with this
/// track's cumulative duration from earlier fragments in this same file, seeded
/// with `video_initial_100ns`/`audio_initial_100ns` (each track's true first
/// written sample offset within the segment, converted to that track's own
/// `mdhd.timescale`; sub-tick remainders are truncated, at most one timescale
/// tick of error). Video's initial offset is always 0 in practice (the first
/// video sample written to a segment is always its opening clean point, at the
/// segment's own start), but this is driven by the caller-supplied value like
/// audio, not hardcoded, since nothing here should assume which track is which.
/// The trailing `mfra` box is dropped: it would reference stale fragment offsets
/// once they move, and MSE `appendBuffer` does not use it.
///
/// Must be called exactly once per segment, at finalize, on the raw
/// `IMFSinkWriter` output — never on an already-processed file. Task085's
/// zero-sample-track removal (`find_zero_sample_track_ids`) forces a rebuild
/// even when every `traf` already has a `tfdt`, so the old idempotency
/// guarantee ("already-compliant input passes through unchanged") no longer
/// holds for a second call: a `traf` whose `tfhd` is already
/// `default-base-is-moof` reports no `base_data_offset`, and a second rebuild
/// pass would misread its (already moof-relative) `trun.data_offset` as an
/// absolute file offset, corrupting the sample data location.
///
/// Bytes in, bytes out (task402), `None` when the input already needed
/// nothing: the container has no path to hand this -- a segment is a range
/// inside one file there, not a file.
///
/// Callers finalizing a segment want `finalize_fragmented_mp4_bytes` below,
/// which is this plus task1930's boundary padding, in that order.
/// `audio_initial_100ns` is one entry per audio track, in **ascending track id
/// order** -- which is the order the sink hands them out, and therefore the
/// order the muxer wrote them in (task1260). A track past the end of the slice
/// falls back to 0, so a single-element slice keeps behaving exactly as the
/// single-track parameter did.
pub(crate) fn inject_tfdt_if_missing_bytes(
    bytes: &[u8],
    video_initial_100ns: i64,
    audio_initial_100ns: &[i64],
) -> io::Result<Option<Vec<u8>>> {
    let top = read_boxes(bytes, 0, bytes.len())?;
    let moofs: Vec<&Mp4BoxEntry> = top.iter().filter(|entry| entry.kind == *b"moof").collect();
    if moofs.is_empty() {
        return Ok(None);
    }
    let moov = top
        .iter()
        .find(|entry| entry.kind == *b"moov")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "fMP4 missing moov"))?;
    let track_meta = collect_track_meta(bytes, moov)?;
    // Task085: a track with zero samples anywhere in the whole file must be
    // stripped from moov/moof even when every present traf is already
    // MSE-compliant, since `MFCreateSourceReaderFromURL` rejects the file for
    // this reason regardless of tfdt/addressing-mode compliance.
    let drop_track_ids = find_zero_sample_track_ids(bytes, &moofs, track_meta.keys().copied())?;
    if drop_track_ids.is_empty() && moofs.iter().all(|moof| moof_is_mse_compliant(bytes, moof)) {
        return Ok(None);
    }

    let mut output = Vec::with_capacity(bytes.len() + moofs.len() * TFDT_BOX_LEN * 2);
    // Audio tracks are matched to the caller's list by their position among
    // the audio track ids, sorted -- the mp4 numbers them in the order they
    // were added to the sink, which is track order.
    let mut audio_track_ids: Vec<u32> = track_meta
        .iter()
        .filter(|(_, meta)| meta.is_audio)
        .map(|(&track_id, _)| track_id)
        .collect();
    audio_track_ids.sort_unstable();
    let mut track_cursor_by_id: HashMap<u32, i64> = track_meta
        .iter()
        .map(|(&track_id, meta)| {
            let initial_100ns = if meta.is_audio {
                audio_track_ids
                    .iter()
                    .position(|id| *id == track_id)
                    .and_then(|position| audio_initial_100ns.get(position))
                    .copied()
                    .unwrap_or(0)
            } else {
                video_initial_100ns
            };
            let initial_units = (initial_100ns as i128 * meta.timescale as i128) / 10_000_000i128;
            (track_id, initial_units as i64)
        })
        .collect();
    for entry in &top {
        if entry.kind == *b"mfra" {
            continue;
        }
        if entry.kind == *b"moov" {
            output.extend_from_slice(&rebuild_moov_without_tracks(bytes, entry, &drop_track_ids)?);
            continue;
        }
        if entry.kind != *b"moof" {
            output.extend_from_slice(&bytes[entry.start..entry.end]);
            continue;
        }
        let old_mdat_payload_start = top
            .iter()
            .find(|candidate| candidate.start >= entry.end && candidate.kind == *b"mdat")
            .map(|mdat| mdat.payload_start)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "moof has no following mdat")
            })?;
        output.extend_from_slice(&rebuild_moof_with_tfdt(
            bytes,
            entry,
            old_mdat_payload_start,
            &mut track_cursor_by_id,
            &drop_track_ids,
        )?);
    }
    Ok(Some(output))
}

/// Everything a finalized segment goes through before it is published: the
/// tfdt rewrite above, then the boundary padding below it. `None` when neither
/// changed anything.
///
/// **The order is load-bearing** (task1930). Injection inserts a `tfdt` into
/// every `traf`, which moves every `moof` after the first -- padding measured
/// before that would be measured against positions the published file does not
/// have. And the padding runs *even when injection returns `None`*: a segment
/// the sink already wrote MSE-compliant can still carry a `moof` across a 64KiB
/// boundary, which is an unrelated defect (task1900 found both seg#124 and its
/// healthy neighbour already `default-base-is-moof`).
///
/// This is prevention, not repair -- it keeps *new* recordings off the
/// boundary. The read-side repair stays where it is: recordings already on disk
/// are only fixed by `unstraddle_fragments` on the playback (task1900) and
/// export (task1920) read paths.
pub(crate) fn finalize_fragmented_mp4_bytes(
    bytes: &[u8],
    video_initial_100ns: i64,
    audio_initial_100ns: &[i64],
) -> io::Result<Option<Vec<u8>>> {
    let injected = inject_tfdt_if_missing_bytes(bytes, video_initial_100ns, audio_initial_100ns)?;
    let staged = injected.as_deref().unwrap_or(bytes);
    let Some(padded) = unstraddle_fragments(staged) else {
        return Ok(injected);
    };
    tracing::info!(
        target: "task1930_writer_unstraddle",
        was = staged.len(),
        now = padded.len(),
        "padded a fragment off the MP4 source's 64KiB read boundary before publishing the segment"
    );
    Ok(Some(padded))
}

/// The file form of the above, for the finalize arm that writes a loose `.mp4`.
pub(crate) fn finalize_fragmented_mp4(
    path: &Path,
    video_initial_100ns: i64,
    audio_initial_100ns: &[i64],
) -> io::Result<()> {
    let bytes = fs::read(path)?;
    match finalize_fragmented_mp4_bytes(&bytes, video_initial_100ns, audio_initial_100ns)? {
        Some(output) => fs::write(path, output),
        // Nothing to fix: leave the file alone rather than rewriting it with
        // identical content.
        None => Ok(()),
    }
}

// ---------- pushing a fragment off Media Foundation's read boundary (task1900) ----------

/// The block size Windows' MP4 source reads a fragmented file in. A `moof`
/// header that lies across one of these boundaries is the defect below.
const MF_READ_GRID: usize = 64 * 1024;

/// The smallest legal box: the 32-bit size and the four-character type.
const BOX_HEADER_LEN: usize = 8;

/// Pads a fragmented MP4 so no `moof` header straddles a 64KiB boundary, or
/// `None` when none does (the common case, and no copy is made) or when the
/// layout is not one it is safe to move boxes in.
///
/// Task1900: Windows' MP4 source **silently ends the presentation** at a `moof`
/// that crosses one of its 64KiB read boundaries. Every sample from that
/// fragment on is dropped, on *both* tracks, and `ReadSample` then reports
/// `ENDOFSTREAM` -- no error, no warning, nothing to catch. Measured on a real
/// recording: one segment played 1.2685s of its 1.9899s, both streams stopping
/// at the same fragment, while ffmpeg decoded all 112 video and 94 audio access
/// units from the same bytes. Moving that fragment 1KiB along made Media
/// Foundation read the whole segment; moving it 100 bytes (still straddling)
/// did not. 5.69% of the segments in that session (70 of 1,231) carry such a
/// `moof`, each losing the rest of its segment.
///
/// Downstream the loss reads as playback's own fault: the worker sees a segment
/// that ended, crosses early, and then honestly sleeps out the media time
/// between where the reader stopped and the next segment's start -- the ~700ms
/// `playback_tick` task1890 found next to the seam, with the audio for that
/// stretch never read.
///
/// The padding is a `free` box inserted ahead of the offending fragment, which
/// pushes its `moof` onto the next boundary. Sample data offsets survive it
/// because every `traf` here is `default-base-is-moof` -- the fragment carries
/// its own base, so moving `moof` and its `mdat` together changes nothing they
/// point at. A file that says otherwise, or that carries the absolute offsets
/// in `sidx`/`mfra`, is passed through untouched rather than guessed at.
pub(crate) fn unstraddle_fragments(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut moofs = Vec::new();
    let mut at = 0usize;
    while at + BOX_HEADER_LEN <= bytes.len() {
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().ok()?) as usize;
        let kind = &bytes[at + 4..at + BOX_HEADER_LEN];
        // `size` 0 or 1 (to end of file / 64-bit) and the offset-carrying index
        // boxes are shapes this writer never emits. Padding one would be a
        // guess, and a wrong guess here is silent corruption.
        if size < BOX_HEADER_LEN || at + size > bytes.len() || kind == b"sidx" || kind == b"mfra" {
            return None;
        }
        if kind == b"moof" {
            // A fragment bigger than the grid straddles wherever it is put.
            if size > MF_READ_GRID || !offsets_are_moof_relative(&bytes[at..at + size]) {
                return None;
            }
            moofs.push((at, size));
        }
        at += size;
    }
    let mut out = Vec::new();
    let (mut shift, mut copied) = (0usize, 0usize);
    for (start, size) in moofs {
        let placed = start + shift;
        if placed / MF_READ_GRID == (placed + size - 1) / MF_READ_GRID {
            continue;
        }
        // Onto the boundary itself: the shortest move that clears it, and one
        // that cannot straddle again since the fragment is smaller than a grid
        // block. Rounded up to a box that can actually be written.
        let mut pad = MF_READ_GRID - placed % MF_READ_GRID;
        while pad < BOX_HEADER_LEN {
            pad += MF_READ_GRID;
        }
        out.extend_from_slice(&bytes[copied..start]);
        out.extend_from_slice(&(pad as u32).to_be_bytes());
        out.extend_from_slice(b"free");
        out.resize(out.len() + pad - BOX_HEADER_LEN, 0);
        copied = start;
        shift += pad;
    }
    if shift == 0 {
        return None;
    }
    out.extend_from_slice(&bytes[copied..]);
    Some(out)
}

/// Whether every `traf` in this `moof` leaves its sample offsets relative to
/// the `moof` (`default-base-is-moof`). A `base_data_offset` in `tfhd` is an
/// absolute file position instead, which inserting padding would invalidate.
fn offsets_are_moof_relative(moof: &[u8]) -> bool {
    let mut at = BOX_HEADER_LEN;
    while at + BOX_HEADER_LEN <= moof.len() {
        let Some(size) = box_len(moof, at) else {
            return false;
        };
        if &moof[at + 4..at + BOX_HEADER_LEN] == b"traf" {
            let mut child = at + BOX_HEADER_LEN;
            while child + BOX_HEADER_LEN <= at + size {
                let Some(len) = box_len(&moof[..at + size], child) else {
                    return false;
                };
                if &moof[child + 4..child + BOX_HEADER_LEN] == b"tfhd" {
                    // version(1) + flags(3) follow the header; bit 0 of the
                    // flags is `base-data-offset-present`.
                    if child + 12 > moof.len() {
                        return false;
                    }
                    let flags = u32::from_be_bytes(moof[child + 8..child + 12].try_into().unwrap());
                    if flags & 0x01 != 0 {
                        return false;
                    }
                }
                child += len;
            }
        }
        at += size;
    }
    true
}

/// The length of the box at `at`, or `None` when it does not fit in `within`.
fn box_len(within: &[u8], at: usize) -> Option<usize> {
    let size = u32::from_be_bytes(within.get(at..at + 4)?.try_into().ok()?) as usize;
    (size >= BOX_HEADER_LEN && at + size <= within.len()).then_some(size)
}
