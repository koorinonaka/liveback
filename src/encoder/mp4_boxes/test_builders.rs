//! Synthetic ISO BMFF fixtures for the tests of every module that reads or
//! rewrites fragmented MP4 -- this one, `ring_buffer` recovery, the export
//! ledger and probe, and playback's repack (t261025-5982). Before this each kept
//! its own copy of `make_box`, and three of them their own 64KiB-straddle check.
//!
//! `ring_buffer/container/tests.rs` keeps its own `fmp4` builders: livia-thumb
//! compiles that test module through `#[path]`, where this module is not there.

/// The production box builder, under the name the fixtures have always used.
pub(crate) use super::rewrite::boxed as make_box;

pub(crate) fn make_tkhd(track_id: u32) -> Vec<u8> {
    // version(1)+flags(3)+creation_time(4)+modification_time(4)+track_ID(4)
    let mut payload = vec![0u8; 16];
    payload[12..16].copy_from_slice(&track_id.to_be_bytes());
    make_box(b"tkhd", &payload)
}

pub(crate) fn make_mdhd(timescale: u32) -> Vec<u8> {
    // version(1)+flags(3)+creation_time(4)+modification_time(4)+timescale(4)
    let mut payload = vec![0u8; 16];
    payload[12..16].copy_from_slice(&timescale.to_be_bytes());
    make_box(b"mdhd", &payload)
}

pub(crate) fn make_hdlr(is_audio: bool) -> Vec<u8> {
    // version(1)+flags(3)+pre_defined(4)+handler_type(4)
    let mut payload = vec![0u8; 12];
    payload[8..12].copy_from_slice(if is_audio { b"soun" } else { b"vide" });
    make_box(b"hdlr", &payload)
}

pub(crate) fn make_trak(track_id: u32, timescale: u32, is_audio: bool) -> Vec<u8> {
    let mdia_payload = [make_mdhd(timescale), make_hdlr(is_audio)].concat();
    let trak_payload = [make_tkhd(track_id), make_box(b"mdia", &mdia_payload)].concat();
    make_box(b"trak", &trak_payload)
}

pub(crate) fn make_tfhd_minimal(track_id: u32) -> Vec<u8> {
    // version_flags(4, all zero: no base-data-offset, no default_sample_duration)
    // + track_ID(4)
    let mut payload = vec![0u8; 8];
    payload[4..8].copy_from_slice(&track_id.to_be_bytes());
    make_box(b"tfhd", &payload)
}

pub(crate) fn make_trun_one_sample(duration: u32) -> Vec<u8> {
    // version_flags = 0x000100 (sample-duration-present only) + sample_count=1
    // + sample_duration.
    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0000_0100u32.to_be_bytes());
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&duration.to_be_bytes());
    make_box(b"trun", &payload)
}

pub(crate) fn make_traf(track_id: u32, sample_duration: u32) -> Vec<u8> {
    let payload = [
        make_tfhd_minimal(track_id),
        make_trun_one_sample(sample_duration),
    ]
    .concat();
    make_box(b"traf", &payload)
}

/// Every top-level `moof` in `bytes`, as `(start, size)`. A flat walk of the
/// box headers, deliberately not the parser under test.
pub(crate) fn moof_offsets(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let size = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if &bytes[at + 4..at + 8] == b"moof" {
            out.push((at, size));
        }
        at += size;
    }
    out
}

/// Every `moof` in `bytes` that crosses a `grid` boundary, as `(start, size)`
/// -- Media Foundation's MP4 source reads in 64KiB blocks.
pub(crate) fn straddling(bytes: &[u8], grid: usize) -> Vec<(usize, usize)> {
    moof_offsets(bytes)
        .into_iter()
        .filter(|(at, size)| at / grid != (at + size - 1) / grid)
        .collect()
}

/// `moov`, then a `moof`/`mdat` pair opening at each of `starts`, with `free`
/// filler in between -- so a test states where a fragment sits rather than
/// deriving it from box arithmetic.
pub(crate) fn file(moov: &[u8], starts: &[usize], moof: &[u8], mdat: &[u8]) -> Vec<u8> {
    let mut out = moov.to_vec();
    for (index, start) in starts.iter().enumerate() {
        let gap = start.checked_sub(out.len()).expect("fragments in order");
        assert!(gap >= 8, "no room for the filler before fragment {index}");
        out.extend_from_slice(&make_box(b"free", &vec![0u8; gap - 8]));
        assert_eq!(out.len(), *start);
        out.extend_from_slice(moof);
        out.extend_from_slice(&make_box(b"mdat", mdat));
    }
    out
}
