use super::parse::*;
use super::rewrite::*;
use super::test_builders::*;
use std::{collections::HashMap, fs};

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
    use crate::encoder::mp4_boxes::{
        finalize_fragmented_mp4_bytes, inject_tfdt_if_missing_bytes, test_builders,
    };

    const GRID: usize = 64 * 1024;
    const VIDEO_TRACK_ID: u32 = 1;

    fn straddling(bytes: &[u8]) -> Vec<(usize, usize)> {
        test_builders::straddling(bytes, GRID)
    }

    /// `moov` plus a `moof`/`mdat` pair opening at each of `starts`.
    fn file(starts: &[usize], moof: &[u8]) -> Vec<u8> {
        let moov = make_box(b"moov", &make_trak(VIDEO_TRACK_ID, 10_000_000, false));
        test_builders::file(&moov, starts, moof, &[0u8; 64])
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
        let shift = moof_offsets(&injected_probe)[1].0 - moof_offsets(&probe)[1].0;
        let injected_len = moof_offsets(&injected_probe)[1].1;
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
            moof_offsets(&finalized).len(),
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

/// Task1900's repack, on synthesized layouts so it runs without Media
/// Foundation and without a recording. The defect it exists for: Windows' MP4
/// source silently ends the presentation at a `moof` that crosses one of its
/// 64KiB read boundaries.
mod unstraddle_tests {
    use crate::encoder::mp4_boxes::test_builders::{self, make_box, moof_offsets};
    use crate::encoder::mp4_boxes::unstraddle_fragments;

    const GRID: usize = 64 * 1024;
    /// Big enough that a fragment placed one byte before a boundary crosses it.
    const MOOF_LEN: usize = 600;

    /// A `MOOF_LEN`-byte `moof` whose single `traf` is `default-base-is-moof`.
    fn moof() -> Vec<u8> {
        let mut tfhd = 0x0002_0000u32.to_be_bytes().to_vec(); // default-base-is-moof
        tfhd.extend_from_slice(&1u32.to_be_bytes()); // track id
        let traf = make_box(b"traf", &make_box(b"tfhd", &tfhd));
        let mut body = traf;
        body.extend_from_slice(&make_box(b"free", &vec![0u8; MOOF_LEN - body.len() - 16]));
        let out = make_box(b"moof", &body);
        assert_eq!(out.len(), MOOF_LEN);
        out
    }

    /// `moov`, then a `moof`/`mdat` pair starting at each of `at` -- so a test
    /// says where a fragment sits rather than deriving it from box arithmetic.
    fn file(at: &[usize]) -> Vec<u8> {
        let moov = make_box(b"moov", &[0u8; 1000]);
        test_builders::file(&moov, at, &moof(), &[0u8; 2_000])
    }

    fn straddling(bytes: &[u8]) -> Vec<usize> {
        test_builders::straddling(bytes, GRID)
            .into_iter()
            .map(|(at, _)| at)
            .collect()
    }

    #[test]
    fn a_file_whose_fragments_already_clear_the_boundary_is_not_copied() {
        let bytes = file(&[2_000, GRID + 2_000]);
        assert!(
            straddling(&bytes).is_empty(),
            "the fixture already straddles"
        );
        assert!(
            unstraddle_fragments(&bytes).is_none(),
            "a healthy file must reach MF as it is, with no copy"
        );
    }

    /// The shape of the recording that made this task: one fragment lying
    /// across a 64KiB boundary, everything else ordinary.
    #[test]
    fn a_straddling_fragment_is_pushed_clear() {
        let bytes = file(&[2_000, GRID - 100]);
        assert_eq!(
            straddling(&bytes),
            vec![GRID - 100],
            "fixture is not the case"
        );
        let padded = unstraddle_fragments(&bytes).expect("a straddle must be repacked");
        assert!(
            straddling(&padded).is_empty(),
            "still straddling after the repack: {:?}",
            straddling(&padded)
        );
        assert_eq!(
            moof_offsets(&padded).len(),
            2,
            "the repack lost or invented a fragment"
        );
        assert_eq!(
            moof_offsets(&padded)[0].0,
            2_000,
            "a fragment that was already clear must not move"
        );
    }

    /// Padding moves `moof` and `mdat` together, which is only correct while
    /// the sample offsets are relative to the `moof`. An absolute
    /// `base_data_offset` means hands off.
    #[test]
    fn a_fragment_with_an_absolute_base_offset_is_left_alone() {
        let mut bytes = file(&[2_000, GRID - 100]);
        assert_eq!(straddling(&bytes).len(), 1, "fixture is not the case");
        // `base-data-offset-present` in the second fragment's `tfhd`: the moof
        // header, then its `traf` header, then the `tfhd` header.
        let flags = moof_offsets(&bytes)[1].0 + 8 + 8 + 8;
        bytes[flags..flags + 4].copy_from_slice(&0x0002_0001u32.to_be_bytes());
        assert!(
            unstraddle_fragments(&bytes).is_none(),
            "a file with absolute sample offsets must not be repacked"
        );
    }

    /// The repack moves bytes around, so what it must never do is change them.
    /// Task1920 shares this function with the export read path, where a lost or
    /// shifted `mdat` byte would be a corrupted clip rather than a stutter --
    /// so the payload is checked, not just the fragment offsets.
    #[test]
    fn the_sample_data_survives_the_repack_byte_for_byte() {
        // Each `mdat` payload is filled with its own fragment number, so a
        // payload that moved to the wrong place reads as the wrong value.
        let mut bytes = make_box(b"moov", &vec![0u8; 1000]);
        let starts = [GRID - 100, 3 * GRID - 200];
        for (index, start) in starts.iter().enumerate() {
            let gap = start - bytes.len();
            bytes.extend_from_slice(&make_box(b"free", &vec![0u8; gap - 8]));
            bytes.extend_from_slice(&moof());
            bytes.extend_from_slice(&make_box(b"mdat", &vec![index as u8 + 1; 2_000]));
        }
        let before = payloads(&bytes, b"mdat");
        assert_eq!(before.len(), 2 * 2_000, "fixture is not the case");
        assert_eq!(straddling(&bytes).len(), 2, "fixture is not the case");

        let padded = unstraddle_fragments(&bytes).expect("a straddle must be repacked");

        assert!(straddling(&padded).is_empty());
        assert_eq!(
            payloads(&padded, b"mdat"),
            before,
            "the sample data must come out of the repack unchanged"
        );
        assert_eq!(
            payloads(&padded, b"moof"),
            payloads(&bytes, b"moof"),
            "the fragment headers must come out of the repack unchanged"
        );
        // Every byte accounted for: the box chain walks to the exact end, so
        // nothing was inserted with a length that overlaps or undershoots.
        let mut at = 0usize;
        while at < padded.len() {
            let size = u32::from_be_bytes(padded[at..at + 4].try_into().unwrap()) as usize;
            assert!(size >= 8, "box at {at} has no length");
            at += size;
        }
        assert_eq!(
            at,
            padded.len(),
            "the box chain does not end at the file end"
        );
    }

    /// Every top-level box of `kind`, payload only.
    fn payloads(bytes: &[u8], kind: &[u8; 4]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 8 <= bytes.len() {
            let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
            if &bytes[at + 4..at + 8] == kind {
                out.extend_from_slice(&bytes[at + 8..at + size]);
            }
            at += size;
        }
        out
    }

    /// Truncated or otherwise unparseable bytes go through untouched -- opening
    /// them is Media Foundation's to report, not this function's to guess at.
    #[test]
    fn a_file_this_function_cannot_parse_is_passed_through() {
        let bytes = file(&[2_000, GRID - 100]);
        assert!(unstraddle_fragments(&bytes[..bytes.len() - 100]).is_none());
        assert!(unstraddle_fragments(&[0u8; 3]).is_none());
        assert!(unstraddle_fragments(&[]).is_none());
    }
}
