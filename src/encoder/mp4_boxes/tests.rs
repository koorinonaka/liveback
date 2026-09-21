use super::parse::*;
use super::rewrite::*;
use std::{collections::HashMap, fs};

/// Builds a raw ISO BMFF box: 4-byte big-endian size, 4-byte kind, then payload.
fn make_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

#[test]
fn build_tfdt_box_encodes_version_1_and_base_media_decode_time() {
    let box_bytes = build_tfdt_box(0x0102030405060708);
    assert_eq!(box_bytes.len(), 20);
    assert_eq!(&box_bytes[0..4], &20u32.to_be_bytes());
    assert_eq!(&box_bytes[4..8], b"tfdt");
    assert_eq!(box_bytes[8], 1, "version 1 for 64-bit baseMediaDecodeTime");
    assert_eq!(&box_bytes[9..12], &[0, 0, 0], "flags must be zero");
    assert_eq!(
        &box_bytes[12..20],
        &0x0102030405060708u64.to_be_bytes(),
        "baseMediaDecodeTime must occupy the last 8 bytes"
    );
}

#[test]
fn read_boxes_enumerates_nested_boxes_at_each_level() {
    let child_a = make_box(b"aaaa", b"1234");
    let child_b = make_box(b"bbbb", b"56");
    let mut moof_payload = Vec::new();
    moof_payload.extend_from_slice(&child_a);
    moof_payload.extend_from_slice(&child_b);
    let moof = make_box(b"moof", &moof_payload);

    let top = read_boxes(&moof, 0, moof.len()).unwrap();
    assert_eq!(top.len(), 1);
    assert_eq!(top[0].kind, *b"moof");
    assert_eq!(top[0].start, 0);
    assert_eq!(top[0].end, moof.len());

    let children = read_boxes(&moof, top[0].payload_start, top[0].end).unwrap();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].kind, *b"aaaa");
    assert_eq!(children[0].payload_start - children[0].start, 8);
    assert_eq!(children[0].end - children[0].payload_start, 4);
    assert_eq!(children[1].kind, *b"bbbb");
    assert_eq!(children[1].end - children[1].payload_start, 2);
}

#[test]
fn moof_is_mse_compliant_rejects_traf_missing_tfdt() {
    // tfhd with version/flags all zero: no default-base-is-moof, and the
    // traf carries no tfdt at all.
    let tfhd_payload = [0u8, 0, 0, 0, /* track_id */ 0, 0, 0, 1];
    let tfhd = make_box(b"tfhd", &tfhd_payload);
    let traf = make_box(b"traf", &tfhd);
    let moof = make_box(b"moof", &traf);

    let top = read_boxes(&moof, 0, moof.len()).unwrap();
    assert!(!moof_is_mse_compliant(&moof, &top[0]));
}

fn make_tkhd(track_id: u32) -> Vec<u8> {
    // version(1)+flags(3)+creation_time(4)+modification_time(4)+track_ID(4)
    let mut payload = vec![0u8; 16];
    payload[12..16].copy_from_slice(&track_id.to_be_bytes());
    make_box(b"tkhd", &payload)
}

fn make_mdhd(timescale: u32) -> Vec<u8> {
    // version(1)+flags(3)+creation_time(4)+modification_time(4)+timescale(4)
    let mut payload = vec![0u8; 16];
    payload[12..16].copy_from_slice(&timescale.to_be_bytes());
    make_box(b"mdhd", &payload)
}

fn make_hdlr(is_audio: bool) -> Vec<u8> {
    // version(1)+flags(3)+pre_defined(4)+handler_type(4)
    let mut payload = vec![0u8; 12];
    payload[8..12].copy_from_slice(if is_audio { b"soun" } else { b"vide" });
    make_box(b"hdlr", &payload)
}

fn make_trak(track_id: u32, timescale: u32, is_audio: bool) -> Vec<u8> {
    let mdia_payload = [make_mdhd(timescale), make_hdlr(is_audio)].concat();
    let trak_payload = [make_tkhd(track_id), make_box(b"mdia", &mdia_payload)].concat();
    make_box(b"trak", &trak_payload)
}

fn make_tfhd_minimal(track_id: u32) -> Vec<u8> {
    // version_flags(4, all zero: no base-data-offset, no default_sample_duration)
    // + track_ID(4)
    let mut payload = vec![0u8; 8];
    payload[4..8].copy_from_slice(&track_id.to_be_bytes());
    make_box(b"tfhd", &payload)
}

fn make_trun_one_sample(duration: u32) -> Vec<u8> {
    // version_flags = 0x000100 (sample-duration-present only) + sample_count=1
    // + sample_duration.
    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0000_0100u32.to_be_bytes());
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&duration.to_be_bytes());
    make_box(b"trun", &payload)
}

fn make_traf(track_id: u32, sample_duration: u32) -> Vec<u8> {
    let payload = [
        make_tfhd_minimal(track_id),
        make_trun_one_sample(sample_duration),
    ]
    .concat();
    make_box(b"traf", &payload)
}

/// Task085: a `traf` that never accumulates a single sample across the whole
/// file (here, audio in a segment shorter than one AAC frame around a real
/// WGC capture-interruption gap) makes `MFCreateSourceReaderFromURL` reject
/// the *entire* file with `MF_E_UNSUPPORTED_BYTESTREAM_TYPE`, confirmed by
/// direct experiment against real captured segments (see task085's
/// Execution Log): a mid-file empty `traf` for a track that *does* have
/// samples elsewhere in the file is harmless, but a track with zero samples
/// anywhere is not, and merely dropping its empty `traf` is not enough —
/// its `trak`/`trex` in `moov` must go too. This reproduces the raw,
/// single-injection-pass shape real capture produces (no
/// `tfdt` yet, exactly one degenerate `traf`) and confirms the fix drops
/// the zero-sample track everywhere while leaving the real track intact.
#[test]
fn inject_tfdt_if_missing_drops_a_track_with_zero_samples_anywhere_in_the_file() {
    let video_track_id = 1;
    let audio_track_id = 2;

    let moov_payload = [
        make_trak(video_track_id, 10_000_000, false),
        make_trak(audio_track_id, 48_000, true),
    ]
    .concat();
    // The audio traf mirrors the real defect exactly: tfhd only, no trun at
    // all (zero samples), because the segment was shorter than one AAC frame.
    let moof_payload = [
        make_traf(video_track_id, 333_333),
        make_box(b"traf", &make_tfhd_minimal(audio_track_id)),
    ]
    .concat();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&make_box(b"ftyp", b"isomiso2mp41"));
    bytes.extend_from_slice(&make_box(b"moov", &moov_payload));
    bytes.extend_from_slice(&make_box(b"moof", &moof_payload));
    bytes.extend_from_slice(&make_box(b"mdat", b"video-sample-bytes"));

    let path = std::env::temp_dir().join(format!(
        "livia-task085-{}-{}.mp4",
        std::process::id(),
        line!()
    ));
    fs::write(&path, &bytes).unwrap();
    finalize_fragmented_mp4(&path, 0, &[0]).unwrap();
    let rewritten = fs::read(&path).unwrap();
    let _ = fs::remove_file(&path);

    let top = read_boxes(&rewritten, 0, rewritten.len()).unwrap();
    let moov_entry = top.iter().find(|entry| entry.kind == *b"moov").unwrap();
    let trak_track_ids: Vec<u32> = read_boxes(&rewritten, moov_entry.payload_start, moov_entry.end)
        .unwrap()
        .iter()
        .filter(|entry| entry.kind == *b"trak")
        .map(|trak| {
            let children = read_boxes(&rewritten, trak.payload_start, trak.end).unwrap();
            let tkhd = children
                .iter()
                .find(|entry| entry.kind == *b"tkhd")
                .unwrap();
            parse_tkhd_track_id(&rewritten, tkhd).unwrap()
        })
        .collect();
    assert_eq!(
        trak_track_ids,
        vec![video_track_id],
        "the zero-sample audio trak must be removed from moov, the video trak kept"
    );

    let moof_entry = top.iter().find(|entry| entry.kind == *b"moof").unwrap();
    let traf_entries: Vec<_> = read_boxes(&rewritten, moof_entry.payload_start, moof_entry.end)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == *b"traf")
        .collect();
    assert_eq!(
        traf_entries.len(),
        1,
        "the empty audio traf must be removed from moof, only the video traf kept"
    );
    let traf_children = read_boxes(
        &rewritten,
        traf_entries[0].payload_start,
        traf_entries[0].end,
    )
    .unwrap();
    let tfhd = traf_children
        .iter()
        .find(|entry| entry.kind == *b"tfhd")
        .unwrap();
    assert_eq!(
        parse_tfhd(&rewritten, tfhd).unwrap().track_id,
        video_track_id
    );
    assert!(
        traf_children.iter().any(|entry| entry.kind == *b"trun"),
        "the surviving video traf must still carry its real sample"
    );
    assert!(
        traf_children.iter().any(|entry| entry.kind == *b"tfdt"),
        "the surviving video traf must still get its tfdt (existing MSE-compliance behavior)"
    );
}

/// Reproduces Task062's root cause: `track_cursor_by_id` used to start every
/// track at 0, so a segment's true (non-zero) first audio offset was silently
/// discarded and every segment's audio restarted at 0. This confirms
/// `inject_tfdt_if_missing_bytes` now seeds each track's tfdt from the caller-given
/// per-track offset (converted to that track's own `mdhd.timescale`), not a
/// hardcoded 0 for every track.
#[test]
fn inject_tfdt_if_missing_seeds_audio_with_true_offset_and_video_stays_zero() {
    let video_track_id = 1;
    let audio_track_id = 2;
    let video_timescale = 10_000_000u32;
    let audio_timescale = 48_000u32;

    let moov_payload = [
        make_trak(video_track_id, video_timescale, false),
        make_trak(audio_track_id, audio_timescale, true),
    ]
    .concat();
    let moof_payload = [
        make_traf(video_track_id, 333_333),
        make_traf(audio_track_id, 1024),
    ]
    .concat();

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&make_box(b"ftyp", b"isomiso2mp41"));
    bytes.extend_from_slice(&make_box(b"moov", &moov_payload));
    bytes.extend_from_slice(&make_box(b"moof", &moof_payload));
    bytes.extend_from_slice(&make_box(b"mdat", &[0u8; 8]));

    let path = std::env::temp_dir().join(format!(
        "livia-task062-tfdt-{}-{}.mp4",
        std::process::id(),
        line!()
    ));
    std::fs::write(&path, &bytes).unwrap();

    let video_initial_100ns = 0i64;
    // 213_125 * 48_000 / 10_000_000 = 1023 exactly, so the expected value
    // below is not itself subject to the same truncation being tested.
    let audio_initial_100ns = 213_125i64;
    finalize_fragmented_mp4(&path, video_initial_100ns, &[audio_initial_100ns]).unwrap();

    let rewritten = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    let top = read_boxes(&rewritten, 0, rewritten.len()).unwrap();
    let moof_entry = top.iter().find(|entry| entry.kind == *b"moof").unwrap();
    let trafs: Vec<_> = read_boxes(&rewritten, moof_entry.payload_start, moof_entry.end)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == *b"traf")
        .collect();
    assert_eq!(trafs.len(), 2);

    let mut tfdt_by_track = HashMap::new();
    for traf in &trafs {
        let children = read_boxes(&rewritten, traf.payload_start, traf.end).unwrap();
        let tfhd = children
            .iter()
            .find(|entry| entry.kind == *b"tfhd")
            .unwrap();
        let tfhd_info = parse_tfhd(&rewritten, tfhd).unwrap();
        let tfdt = children
            .iter()
            .find(|entry| entry.kind == *b"tfdt")
            .expect("tfdt must be injected");
        assert_eq!(tfdt.end - tfdt.payload_start, 12, "version 1 tfdt payload");
        let base_media_decode_time = u64::from_be_bytes(
            rewritten[tfdt.payload_start + 4..tfdt.payload_start + 12]
                .try_into()
                .unwrap(),
        );
        tfdt_by_track.insert(tfhd_info.track_id, base_media_decode_time);
    }
    assert_eq!(
        tfdt_by_track.get(&video_track_id),
        Some(&0),
        "video's first fragment tfdt must stay 0"
    );
    assert_eq!(
        tfdt_by_track.get(&audio_track_id),
        Some(&1023),
        "audio's first fragment tfdt must reflect its true segment-relative \
             offset (213125 100ns at 48000 timescale = 1023), not be forced to 0"
    );
}

/// Task1930's half of finalize: the writer must not publish a segment whose
/// `moof` lies across one of Media Foundation's 64KiB read boundaries, and the
/// padding has to run **after** the tfdt injection that moves every `moof`.
/// Synthesized layouts, so these need neither a recording nor Media Foundation.
mod finalize_order_tests {
    use super::*;
    use crate::encoder::mp4_boxes::{finalize_fragmented_mp4_bytes, inject_tfdt_if_missing_bytes};

    const GRID: usize = 64 * 1024;
    const VIDEO_TRACK_ID: u32 = 1;

    /// Every `moof` in `bytes` that crosses a grid boundary, as `(start, size)`.
    fn straddling(bytes: &[u8]) -> Vec<(usize, usize)> {
        moofs(bytes)
            .into_iter()
            .filter(|(at, size)| at / GRID != (at + size - 1) / GRID)
            .collect()
    }

    fn moofs(bytes: &[u8]) -> Vec<(usize, usize)> {
        read_boxes(bytes, 0, bytes.len())
            .unwrap()
            .into_iter()
            .filter(|entry| entry.kind == *b"moof")
            .map(|entry| (entry.start, entry.end - entry.start))
            .collect()
    }

    /// `moov` plus a `moof`/`mdat` pair opening at each of `starts`, with
    /// `free` filler in between so a test states where a fragment sits.
    fn file(starts: &[usize], moof: &[u8]) -> Vec<u8> {
        let mut out = make_box(b"moov", &make_trak(VIDEO_TRACK_ID, 10_000_000, false));
        for start in starts {
            let gap = start.checked_sub(out.len()).expect("fragments in order");
            assert!(gap >= 8, "no room for the filler before a fragment");
            out.extend_from_slice(&make_box(b"free", &vec![0u8; gap - 8]));
            out.extend_from_slice(moof);
            out.extend_from_slice(&make_box(b"mdat", &[0u8; 64]));
        }
        out
    }

    /// A fragment the sink writes raw: no `tfdt`, so injection rebuilds it.
    fn raw_moof() -> Vec<u8> {
        make_box(b"moof", &make_traf(VIDEO_TRACK_ID, 333_333))
    }

    /// A fragment that is already MSE-compliant, which injection leaves alone.
    fn compliant_moof() -> Vec<u8> {
        // version 0, flags 0x020000 (default-base-is-moof), then the track id.
        let mut tfhd_payload = 0x0002_0000u32.to_be_bytes().to_vec();
        tfhd_payload.extend_from_slice(&VIDEO_TRACK_ID.to_be_bytes());
        let traf = [
            make_box(b"tfhd", &tfhd_payload),
            build_tfdt_box(0).to_vec(),
            make_trun_one_sample(333_333),
        ]
        .concat();
        make_box(b"moof", &make_box(b"traf", &traf))
    }

    /// The ordering this task exists for: a fragment that clears the boundary
    /// in the sink's output but is pushed across it by the `tfdt`s injection
    /// adds. Padding computed before the injection would be measured against
    /// positions the published file never has.
    #[test]
    fn a_straddle_the_tfdt_injection_creates_is_padded_away() {
        // What injection does to this layout, measured rather than assumed:
        // the second fragment moves by everything that grew ahead of it.
        let probe = file(&[2_000, GRID / 2], &raw_moof());
        let injected_probe = inject_tfdt_if_missing_bytes(&probe, 0, &[])
            .unwrap()
            .expect("a sink-shaped fixture must need the injection");
        let shift = moofs(&injected_probe)[1].0 - moofs(&probe)[1].0;
        let injected_len = moofs(&injected_probe)[1].1;
        assert!(shift > 0, "the injection must move the second fragment");

        // Placed so it ends one byte short of the boundary before injection,
        // and one `tfdt` past it afterwards.
        let bytes = file(&[2_000, GRID - injected_len + 1 - shift], &raw_moof());
        assert!(
            straddling(&bytes).is_empty(),
            "the sink's own output must not straddle, or the test proves nothing"
        );
        let injected = inject_tfdt_if_missing_bytes(&bytes, 0, &[])
            .unwrap()
            .expect("fixture must need the injection");
        assert_eq!(
            straddling(&injected).len(),
            1,
            "fixture is not the case: the injection must create the straddle"
        );

        let finalized = finalize_fragmented_mp4_bytes(&bytes, 0, &[])
            .unwrap()
            .expect("finalize must rewrite a segment that ends up straddling");
        assert!(
            straddling(&finalized).is_empty(),
            "published segment still straddles: {:?}",
            straddling(&finalized)
        );
        assert_eq!(
            moofs(&finalized).len(),
            2,
            "the padding lost or invented a fragment"
        );
    }

    /// The other half of the order: an already-compliant segment, where
    /// injection declines to touch anything, still gets padded. Compliance and
    /// grid placement are unrelated defects -- task1900 measured a straddling
    /// segment and its healthy neighbour with identical `tfhd` flags.
    #[test]
    fn a_straddle_is_padded_even_when_the_injection_declines() {
        let moof = compliant_moof();
        let bytes = file(&[2_000, GRID - moof.len() + 1], &moof);
        assert_eq!(
            straddling(&bytes).len(),
            1,
            "fixture is not the case: the second fragment must straddle"
        );
        assert!(
            inject_tfdt_if_missing_bytes(&bytes, 0, &[])
                .unwrap()
                .is_none(),
            "fixture is not the case: the injection must decline this input"
        );

        let finalized = finalize_fragmented_mp4_bytes(&bytes, 0, &[])
            .unwrap()
            .expect("padding must run even when the injection returns None");
        assert!(
            straddling(&finalized).is_empty(),
            "published segment still straddles: {:?}",
            straddling(&finalized)
        );
    }
}
