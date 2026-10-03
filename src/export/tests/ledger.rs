//! Raw MP4/H.264 sample-ledger passthrough verification helpers and tests.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Mp4Box {
    kind: [u8; 4],
    payload_start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct RawTrack {
    handler: [u8; 4],
    samples: Vec<Vec<u8>>,
    avcc: Option<Vec<u8>>,
    /// `stss` entries: the 1-based sample numbers this track declares as sync
    /// samples (task3710). `stss` is an *optional* box -- a track whose every
    /// sample is a sync sample may omit it, which is the normal shape of an
    /// AAC track -- so "missing" reads as an empty vector rather than an
    /// error. An empty vector on a *video* track is the defect task3480
    /// measured: MF's MP4 source then has no index and drops every seek to
    /// sample 0.
    sync_samples: Vec<u32>,
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().unwrap())
}

fn be_u64(bytes: &[u8]) -> u64 {
    u64::from_be_bytes(bytes.try_into().unwrap())
}

fn mp4_boxes(bytes: &[u8], start: usize, end: usize) -> Result<Vec<Mp4Box>, String> {
    let mut cursor = start;
    let mut boxes = Vec::new();
    while cursor < end {
        if end - cursor < 8 {
            return Err("MP4 box header truncated".into());
        }
        let size32 = be_u32(&bytes[cursor..cursor + 4]) as u64;
        let kind = bytes[cursor + 4..cursor + 8].try_into().unwrap();
        let (size, header) = match size32 {
            0 => ((end - cursor) as u64, 8),
            1 => {
                if end - cursor < 16 {
                    return Err("MP4 extended box header truncated".into());
                }
                (be_u64(&bytes[cursor + 8..cursor + 16]), 16)
            }
            size => (size, 8),
        };
        if size < header || size > (end - cursor) as u64 {
            return Err("MP4 box size outside parent".into());
        }
        let box_end = cursor + size as usize;
        boxes.push(Mp4Box {
            kind,
            payload_start: cursor + header as usize,
            end: box_end,
        });
        cursor = box_end;
    }
    Ok(boxes)
}

fn required_box(boxes: &[Mp4Box], kind: [u8; 4]) -> Result<&Mp4Box, String> {
    boxes
        .iter()
        .find(|entry| entry.kind == kind)
        .ok_or_else(|| format!("MP4 box missing: {}", String::from_utf8_lossy(&kind)))
}

fn raw_mp4_tracks(path: &Path) -> Result<Vec<RawTrack>, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let top = mp4_boxes(&bytes, 0, bytes.len())?;
    let moov = required_box(&top, *b"moov")?;
    let mut tracks = Vec::new();
    for trak in mp4_boxes(&bytes, moov.payload_start, moov.end)?
        .into_iter()
        .filter(|entry| entry.kind == *b"trak")
    {
        let trak_boxes = mp4_boxes(&bytes, trak.payload_start, trak.end)?;
        let mdia = required_box(&trak_boxes, *b"mdia")?;
        let mdia_boxes = mp4_boxes(&bytes, mdia.payload_start, mdia.end)?;
        let hdlr = required_box(&mdia_boxes, *b"hdlr")?;
        if hdlr.end - hdlr.payload_start < 12 {
            return Err("MP4 hdlr truncated".into());
        }
        let handler = bytes[hdlr.payload_start + 8..hdlr.payload_start + 12]
            .try_into()
            .unwrap();
        let minf = required_box(&mdia_boxes, *b"minf")?;
        let minf_boxes = mp4_boxes(&bytes, minf.payload_start, minf.end)?;
        let stbl = required_box(&minf_boxes, *b"stbl")?;
        let stbl_boxes = mp4_boxes(&bytes, stbl.payload_start, stbl.end)?;
        let stsz = required_box(&stbl_boxes, *b"stsz")?;
        let stsc = required_box(&stbl_boxes, *b"stsc")?;
        let stco = stbl_boxes
            .iter()
            .find(|entry| entry.kind == *b"stco" || entry.kind == *b"co64")
            .ok_or_else(|| "MP4 chunk offsets missing".to_string())?;
        if stsz.end - stsz.payload_start < 12 || stsc.end - stsc.payload_start < 8 {
            return Err("MP4 sample table truncated".into());
        }
        let fixed_size = be_u32(&bytes[stsz.payload_start + 4..stsz.payload_start + 8]);
        let sample_count = be_u32(&bytes[stsz.payload_start + 8..stsz.payload_start + 12]) as usize;
        let sizes = if fixed_size != 0 {
            vec![fixed_size as usize; sample_count]
        } else {
            let table_end = stsz.payload_start + 12 + sample_count * 4;
            if table_end > stsz.end {
                return Err("MP4 stsz table truncated".into());
            }
            bytes[stsz.payload_start + 12..table_end]
                .chunks_exact(4)
                .map(be_u32)
                .map(|size| size as usize)
                .collect()
        };
        let sync_samples = match stbl_boxes.iter().find(|entry| entry.kind == *b"stss") {
            None => Vec::new(),
            Some(stss) => {
                if stss.end - stss.payload_start < 8 {
                    return Err("MP4 stss truncated".into());
                }
                let entry_count =
                    be_u32(&bytes[stss.payload_start + 4..stss.payload_start + 8]) as usize;
                let table_end = stss.payload_start + 8 + entry_count * 4;
                if table_end > stss.end {
                    return Err("MP4 stss table truncated".into());
                }
                bytes[stss.payload_start + 8..table_end]
                    .chunks_exact(4)
                    .map(be_u32)
                    .collect()
            }
        };
        let stsc_count = be_u32(&bytes[stsc.payload_start + 4..stsc.payload_start + 8]) as usize;
        let stsc_end = stsc.payload_start + 8 + stsc_count * 12;
        if stsc_end > stsc.end {
            return Err("MP4 stsc table truncated".into());
        }
        let stsc_entries = bytes[stsc.payload_start + 8..stsc_end]
            .chunks_exact(12)
            .map(|entry| (be_u32(&entry[0..4]) as usize, be_u32(&entry[4..8]) as usize))
            .collect::<Vec<_>>();
        if stco.end - stco.payload_start < 8 {
            return Err("MP4 chunk offset table truncated".into());
        }
        let chunk_count = be_u32(&bytes[stco.payload_start + 4..stco.payload_start + 8]) as usize;
        let offset_width = if stco.kind == *b"co64" { 8 } else { 4 };
        let offset_end = stco.payload_start + 8 + chunk_count * offset_width;
        if offset_end > stco.end {
            return Err("MP4 chunk offset entries truncated".into());
        }
        let offsets = (0..chunk_count)
            .map(|index| {
                let start = stco.payload_start + 8 + index * offset_width;
                if offset_width == 8 {
                    be_u64(&bytes[start..start + 8]) as usize
                } else {
                    be_u32(&bytes[start..start + 4]) as usize
                }
            })
            .collect::<Vec<_>>();
        let mut samples = Vec::with_capacity(sample_count);
        let mut sample_index = 0;
        for (chunk_zero, offset) in offsets.into_iter().enumerate() {
            let chunk_one = chunk_zero + 1;
            let entry_index = stsc_entries
                .iter()
                .rposition(|(first_chunk, _)| *first_chunk <= chunk_one)
                .ok_or_else(|| "MP4 stsc has no chunk mapping".to_string())?;
            let samples_per_chunk = stsc_entries[entry_index].1;
            let mut cursor = offset;
            for _ in 0..samples_per_chunk {
                let size = *sizes
                    .get(sample_index)
                    .ok_or_else(|| "MP4 chunk has too many samples".to_string())?;
                let sample_end = cursor
                    .checked_add(size)
                    .ok_or_else(|| "MP4 sample overflow".to_string())?;
                if sample_end > bytes.len() {
                    return Err("MP4 sample offset outside file".into());
                }
                samples.push(bytes[cursor..sample_end].to_vec());
                cursor = sample_end;
                sample_index += 1;
            }
        }
        if sample_index != sizes.len() {
            return Err("MP4 chunk/sample count mismatch".into());
        }
        let avcc = if handler == *b"vide" {
            let stsd = required_box(&stbl_boxes, *b"stsd")?;
            if stsd.end - stsd.payload_start < 16 {
                return Err("MP4 stsd truncated".into());
            }
            let entry_start = stsd.payload_start + 8;
            let entry_size = be_u32(&bytes[entry_start..entry_start + 4]) as usize;
            let child_start = entry_start + 8 + 78;
            let entry_end = entry_start
                .checked_add(entry_size)
                .ok_or_else(|| "MP4 stsd overflow".to_string())?;
            if child_start > entry_end || entry_end > stsd.end {
                return Err("MP4 visual sample entry truncated".into());
            }
            let sample_entry_boxes = mp4_boxes(&bytes, child_start, entry_end)?;
            let avcc = required_box(&sample_entry_boxes, *b"avcC")?;
            Some(bytes[avcc.payload_start..avcc.end].to_vec())
        } else {
            None
        };
        tracks.push(RawTrack {
            handler,
            samples,
            avcc,
            sync_samples,
        });
    }
    Ok(tracks)
}

fn raw_track(path: &Path, handler: [u8; 4]) -> RawTrack {
    raw_mp4_tracks(path)
        .unwrap()
        .into_iter()
        .find(|track| track.handler == handler)
        .unwrap_or_else(|| panic!("track missing: {}", String::from_utf8_lossy(&handler)))
}

fn contains_contiguous(haystack: &[Vec<u8>], needle: &[Vec<u8>]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn contains_contiguous_nals(haystack: &[Vec<Vec<u8>>], needle: &[Vec<Vec<u8>>]) -> bool {
    !needle.is_empty()
        && needle.len() <= haystack.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn h264_nals(sample: &[u8], length_width: usize) -> Result<Vec<Vec<u8>>, String> {
    if !(1..=4).contains(&length_width) {
        return Err("invalid H.264 length prefix width".into());
    }
    let mut cursor = 0;
    let mut nals = Vec::new();
    while cursor < sample.len() {
        if sample.len() - cursor < length_width {
            return Err("H.264 length prefix truncated".into());
        }
        let mut size = 0usize;
        for byte in &sample[cursor..cursor + length_width] {
            size = (size << 8) | usize::from(*byte);
        }
        cursor += length_width;
        let end = cursor
            .checked_add(size)
            .ok_or_else(|| "H.264 NAL overflow".to_string())?;
        if size == 0 || end > sample.len() {
            return Err("H.264 NAL outside access unit".into());
        }
        nals.push(sample[cursor..end].to_vec());
        cursor = end;
    }
    Ok(nals)
}

fn h264_length_width(avcc: &[u8]) -> Result<usize, String> {
    avcc.get(4)
        .map(|value| usize::from(value & 0x03) + 1)
        .ok_or_else(|| "avcC truncated".to_string())
}

pub(super) fn assert_raw_middle_passthrough(middle: &Path, output: &Path) {
    let source_video = raw_track(middle, *b"vide");
    let output_video = raw_track(output, *b"vide");
    let source_audio = raw_track(middle, *b"soun");
    let output_audio = raw_track(output, *b"soun");
    assert!(
        contains_contiguous(&output_audio.samples, &source_audio.samples),
        "middle AAC raw AU ledger changed, reordered, or missing"
    );
    if contains_contiguous(&output_video.samples, &source_video.samples) {
        return;
    }
    let source_avcc = source_video.avcc.as_ref().expect("source avcC missing");
    let output_avcc = output_video.avcc.as_ref().expect("output avcC missing");
    assert_eq!(source_avcc, output_avcc, "H.264 avcC SPS/PPS changed");
    let source_width = h264_length_width(source_avcc).unwrap();
    let output_width = h264_length_width(output_avcc).unwrap();
    let source_nals = source_video
        .samples
        .iter()
        .map(|sample| h264_nals(sample, source_width).unwrap())
        .collect::<Vec<_>>();
    let output_nals = output_video
        .samples
        .iter()
        .map(|sample| h264_nals(sample, output_width).unwrap())
        .collect::<Vec<_>>();
    assert!(
        contains_contiguous_nals(&output_nals, &source_nals),
        "middle H.264 NAL payload/order/count changed or missing"
    );
}

/// The `stss` half of an export's acceptance (task3710): the video track must
/// index its key frames, and the index must not lie. `expected` is the exact
/// 1-based sample list -- exact, because "non-empty" would also pass an
/// `export_writer` that stamped `CleanPoint` on every sample, which puts delta
/// frames into `stss` and makes the index worse than absent.
pub(super) fn assert_video_sync_samples(output: &Path, expected: &[u32]) {
    let video = raw_track(output, *b"vide");
    assert_eq!(
        video.sync_samples, expected,
        "exported video stss (sync sample table) is not the expected key-frame index"
    );
    assert!(
        video.sync_samples.windows(2).all(|pair| pair[0] < pair[1]),
        "exported video stss entries must strictly increase: {:?}",
        video.sync_samples
    );
    assert!(
        video
            .sync_samples
            .iter()
            .all(|entry| *entry >= 1 && *entry as usize <= video.samples.len()),
        "exported video stss entries must be 1-based sample numbers within stsz ({} samples): {:?}",
        video.samples.len(),
        video.sync_samples
    );
    // An AAC track normally has no `stss` at all (every AAC frame is a sync
    // sample); the walker must read that as "empty", not as a parse failure.
    for track in raw_mp4_tracks(output).unwrap() {
        if track.handler == *b"soun" {
            assert!(
                track
                    .sync_samples
                    .iter()
                    .all(|entry| *entry >= 1 && *entry as usize <= track.samples.len()),
                "audio stss, when present, must still be within its own stsz: {:?}",
                track.sync_samples
            );
        }
    }
}

fn mp4_box(kind: [u8; 4], payload: Vec<u8>) -> Vec<u8> {
    let mut result = Vec::with_capacity(payload.len() + 8);
    result.extend_from_slice(&((payload.len() + 8) as u32).to_be_bytes());
    result.extend_from_slice(&kind);
    result.extend_from_slice(&payload);
    result
}

fn synthetic_audio_mp4(co64: bool, invalid_offset: bool, with_stss: bool) -> Vec<u8> {
    let samples = [vec![1, 2, 3], vec![4, 5]];
    let mut mdat_payload = Vec::new();
    for sample in &samples {
        mdat_payload.extend_from_slice(sample);
    }
    let mdat = mp4_box(*b"mdat", mdat_payload);
    let mut stsz = vec![0; 4];
    stsz.extend_from_slice(&0u32.to_be_bytes());
    stsz.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for sample in &samples {
        stsz.extend_from_slice(&(sample.len() as u32).to_be_bytes());
    }
    let mut stsc = vec![0; 4];
    stsc.extend_from_slice(&1u32.to_be_bytes());
    stsc.extend_from_slice(&1u32.to_be_bytes());
    stsc.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    stsc.extend_from_slice(&1u32.to_be_bytes());
    let offset = if invalid_offset { 10_000 } else { 8 };
    let mut offsets = vec![0; 4];
    offsets.extend_from_slice(&1u32.to_be_bytes());
    if co64 {
        offsets.extend_from_slice(&(offset as u64).to_be_bytes());
    } else {
        offsets.extend_from_slice(&(offset as u32).to_be_bytes());
    }
    let mut children = [
        mp4_box(*b"stsz", stsz),
        mp4_box(*b"stsc", stsc),
        mp4_box(if co64 { *b"co64" } else { *b"stco" }, offsets),
    ]
    .concat();
    if with_stss {
        let mut stss = vec![0; 4];
        stss.extend_from_slice(&1u32.to_be_bytes());
        stss.extend_from_slice(&1u32.to_be_bytes());
        children.extend(mp4_box(*b"stss", stss));
    }
    let stbl = mp4_box(*b"stbl", children);
    let minf = mp4_box(*b"minf", stbl);
    let mut hdlr = vec![0; 8];
    hdlr.extend_from_slice(b"soun");
    let mdia = mp4_box(*b"mdia", [mp4_box(*b"hdlr", hdlr), minf].concat());
    [mdat, mp4_box(*b"moov", mp4_box(*b"trak", mdia))].concat()
}

#[test]
fn raw_mp4_ledger_reads_stco_co64_and_rejects_invalid_offsets() {
    for co64 in [false, true] {
        let path = std::env::temp_dir().join(format!("task009-ledger-{co64}.mp4"));
        fs::write(&path, synthetic_audio_mp4(co64, false, false)).unwrap();
        let track = raw_track(&path, *b"soun");
        assert_eq!(track.samples, vec![vec![1, 2, 3], vec![4, 5]]);
        fs::remove_file(path).unwrap();
    }
    let path = std::env::temp_dir().join("task009-ledger-invalid-offset.mp4");
    fs::write(&path, synthetic_audio_mp4(false, true, false)).unwrap();
    assert!(raw_mp4_tracks(&path).unwrap_err().contains("outside file"));
    fs::remove_file(path).unwrap();
}

/// Task3710: `stss` is optional, so the walker reads "no box" as an empty
/// index and never as a parse error -- an audio track that omits it is the
/// normal case, and a track that carries one must still walk.
#[test]
fn raw_mp4_ledger_reads_the_optional_stss_when_present() {
    let without = std::env::temp_dir().join("task3710-ledger-no-stss.mp4");
    fs::write(&without, synthetic_audio_mp4(false, false, false)).unwrap();
    let track = raw_track(&without, *b"soun");
    assert!(track.sync_samples.is_empty());
    assert_eq!(track.samples, vec![vec![1, 2, 3], vec![4, 5]]);
    fs::remove_file(without).unwrap();

    let with = std::env::temp_dir().join("task3710-ledger-with-stss.mp4");
    fs::write(&with, synthetic_audio_mp4(false, false, true)).unwrap();
    let track = raw_track(&with, *b"soun");
    assert_eq!(track.sync_samples, vec![1]);
    assert_eq!(track.samples, vec![vec![1, 2, 3], vec![4, 5]]);
    fs::remove_file(with).unwrap();
}

#[test]
fn h264_ledger_preserves_nal_boundaries_and_rejects_changes() {
    let sample = [0, 2, 0x65, 1, 0, 1, 6];
    assert_eq!(h264_nals(&sample, 2).unwrap(), vec![vec![0x65, 1], vec![6]]);
    assert!(h264_nals(&sample[..3], 2).is_err());
    assert!(contains_contiguous_nals(
        &[vec![vec![1]], vec![vec![2]], vec![vec![3]]],
        &[vec![vec![2]], vec![vec![3]]]
    ));
    assert!(!contains_contiguous_nals(
        &[vec![vec![1]], vec![vec![2]], vec![vec![3]]],
        &[vec![vec![3]], vec![vec![2]]]
    ));
}
