//! Request validation, planner and filename tests.
use std::ffi::OsStr;

use super::*;

fn segment(index: u64, start: i64, end: i64) -> ring_buffer::SegmentRecord {
    ring_buffer::SegmentRecord {
        index,
        path: PathBuf::from(format!("C:\\buffer\\segment-{index}.mp4")),
        start_100ns: start,
        end_100ns: end,
        bytes: 1_000,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    }
}

#[test]
fn request_validation_rejects_empty_and_traversal() {
    let mut request = ExportRequest {
        session_id: "s".into(),
        start_100ns: 10,
        end_100ns: 20,
        game_name: "game".into(),
        output_file: None,
        audio_mix: Default::default(),
    };
    assert!(validate_request(&request).is_ok());
    request.end_100ns = 10;
    assert!(validate_request(&request).is_err());
    request.end_100ns = 20;
    // The traversal guard reads `output_file` now (task1210): that is the field
    // the save dialog fills and the only one the job turns into a path.
    request.output_file = Some(PathBuf::from("..\\outside\\clip.mp4"));
    assert!(validate_request(&request).is_err());
    request.output_file = Some(PathBuf::from("C:\\Videos\\Liveback\\clip.mp4"));
    assert!(validate_request(&request).is_ok());
}

#[test]
fn resolve_output_directory_accepts_any_absolute_folder_but_rejects_relative_and_traversal() {
    // Policy change (task 024): output_directory is no longer confined to
    // Videos\Liveback; any absolute path chosen via the native folder
    // dialog is accepted, as long as it isn't a relative/traversal path and
    // resolves to a plausible directory.
    assert!(resolve_output_directory(None)
        .unwrap()
        .ends_with(Path::new("Videos").join("Liveback")));

    let root = std::env::temp_dir().join(format!("livia-export-dest-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let custom = root.join("custom-destination");
    assert_eq!(
        resolve_output_directory(Some(&custom)).unwrap(),
        custom,
        "an absolute path whose parent exists is accepted even outside Videos\\Liveback"
    );

    assert!(resolve_output_directory(Some(Path::new("relative\\destination"))).is_err());
    assert!(resolve_output_directory(Some(Path::new("..\\outside"))).is_err());

    let missing_parent = root.join("no-such-parent").join("destination");
    assert!(
        resolve_output_directory(Some(&missing_parent)).is_err(),
        "a path whose parent directory doesn't exist is rejected rather than deep-created"
    );

    let file_path = root.join("not-a-directory.txt");
    fs::write(&file_path, b"x").unwrap();
    assert!(
        resolve_output_directory(Some(&file_path)).is_err(),
        "an existing file cannot be used as the output directory"
    );

    let _ = fs::remove_dir_all(&root);
}

/// With `LIVEBACK_BUFFER_ROOT` set, the default output folder moves into the
/// isolated root -- stills (`s` on the review screen) and any export that
/// never named a file. Otherwise an isolated agent run writes into the user's
/// own `Videos\Liveback`, which is what happened on task t260913-3842.
#[test]
fn a_buffer_root_override_takes_the_export_folder_with_it() {
    let over = Path::new(r"C:\Temp\livia-buffer-f386");
    let profile = OsStr::new(r"C:\Users\someone");

    assert_eq!(
        default_output_directory_from(Some(over), Some(profile)).unwrap(),
        over.join("Exports"),
        "the override outranks USERPROFILE"
    );
    assert_eq!(
        default_output_directory_from(Some(over), None).unwrap(),
        over.join("Exports"),
        "and stands on its own when USERPROFILE is missing"
    );
}

/// No override: byte for byte the answer this always gave, including the
/// failure when the environment has no `USERPROFILE`.
#[test]
fn without_an_override_exports_stay_in_videos_liveback() {
    let profile = OsStr::new(r"C:\Users\someone");

    assert_eq!(
        default_output_directory_from(None, Some(profile)).unwrap(),
        PathBuf::from(profile).join("Videos").join("Liveback")
    );
    assert_eq!(
        default_output_directory_from(None, None)
            .unwrap_err()
            .message,
        // t260928-e309 F15: 「フォルダ」, the spelling everywhere else.
        "ユーザーのビデオフォルダが見つかりません"
    );
}

#[test]
fn planner_uses_half_open_intersections_and_splits_gaps() {
    let manifest = ring_buffer::SessionManifest {
        audio_tracks: Vec::new(),
        version: 1,
        session_id: "s".into(),
        closed: false,
        retention_minutes: 15,
        segments: vec![segment(1, 0, 20), segment(2, 20, 40), segment(3, 60, 80)],
        gaps: vec![ring_buffer::TimelineGap {
            start_100ns: 40,
            end_100ns: 60,
            reason: "gap".into(),
        }],
        target_title: None,
        target_executable: None,
        target_executable_path: None,
        markers: vec![],
        note: None,
        protected: false,
    };
    let parts = plan_parts(&manifest, 10, 70).unwrap();
    assert_eq!(parts.len(), 2);
    assert_eq!((parts[0].start_100ns, parts[0].end_100ns), (10, 40));
    assert_eq!((parts[1].start_100ns, parts[1].end_100ns), (60, 70));
    assert_eq!(
        plan_parts(&manifest, 40, 60).unwrap_err().message,
        "この範囲には録画がありません"
    );
}

/// Task067: mirrors what `CaptureController::session_manifest` now does
/// for a recovered (or any relative-path) session before handing its
/// manifest to the export pipeline — resolve every segment path against
/// the session root — and confirms `validate_segment_paths` both accepts
/// the legitimately-resolved result and rejects a `..`-tampered one that
/// would otherwise still lexically pass `starts_with(root)`.
#[test]
fn validate_segment_paths_accepts_resolved_relative_paths_and_rejects_parent_dir_traversal() {
    let root = std::env::temp_dir().join(format!(
        "livia-task067-plan-{}-{}",
        std::process::id(),
        line!()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("segment-0.mp4"), b"a").unwrap();
    fs::write(root.join("segment-1.mp4"), b"b").unwrap();

    let mut manifest = ring_buffer::SessionManifest {
        audio_tracks: Vec::new(),
        version: 2,
        session_id: "s".into(),
        closed: true,
        retention_minutes: 15,
        segments: vec![
            ring_buffer::SegmentRecord {
                index: 0,
                path: "segment-0.mp4".into(),
                start_100ns: 0,
                end_100ns: 20,
                bytes: 1,
                thumbnail_path: None,
                audio_offsets_100ns: vec![0],
            },
            ring_buffer::SegmentRecord {
                index: 1,
                path: "segment-1.mp4".into(),
                start_100ns: 20,
                end_100ns: 40,
                bytes: 1,
                thumbnail_path: None,
                audio_offsets_100ns: vec![0],
            },
        ],
        gaps: vec![],
        target_title: None,
        target_executable: None,
        target_executable_path: None,
        markers: vec![],
        note: None,
        protected: false,
    };
    // `CaptureController::session_manifest` resolves relative paths
    // (`RingBuffer::resolve_segment_path`) before export ever sees them.
    for segment in &mut manifest.segments {
        segment.path = root.join(&segment.path);
    }
    let parts = plan_parts(&manifest, 0, 40).unwrap();
    assert!(validate_segment_paths(&parts, &root).is_ok());

    let mut tampered = manifest;
    // Simulates a resolved path from a manifest whose relative record
    // carried `..`: still lexically `starts_with(root)`, but resolves
    // outside it.
    tampered.segments[0].path = root.join("../outside.mp4");
    let tampered_parts = plan_parts(&tampered, 0, 40).unwrap();
    assert!(
        validate_segment_paths(&tampered_parts, &root).is_err(),
        "a resolved path carrying `..` must be rejected even though it lexically starts_with(root)"
    );

    let _ = fs::remove_dir_all(&root);
}

/// An interior seam must not be trimmed. A segment record ends at its
/// last *video* sample and the next starts at its first, so the seam is a video
/// frame wide (17.9ms at 56fps) while an AAC access unit is 21.3ms -- clipping
/// each segment to its own record dropped the unit straddling `end_100ns` and
/// the one falling inside the seam, and neither neighbour picked them up.
/// Measured on a real export: a 21.3ms hole at every 2s boundary, 43ms at 5 of
/// 13 of them.
#[test]
fn interior_seams_keep_every_access_unit() {
    let _runtime = encoder::MfRuntime::start().unwrap();
    const UNIT_100NS: i64 = 213_333;
    // Units straddling both ends of a segment whose record is 10_000_000..
    // 30_000_000 -- the shape the old window dropped.
    let samples = (0..6)
        .map(|index| unsafe {
            let sample = windows::Win32::Media::MediaFoundation::MFCreateSample().unwrap();
            (sample, 9_900_000 + index * UNIT_100NS, UNIT_100NS, 0usize)
        })
        .collect::<Vec<_>>();
    let interior = ExportPart {
        start_100ns: 0,
        end_100ns: 100_000_000,
        segments: Vec::new(),
    };
    assert_eq!(
        audio_window(&samples, 10_000_000, 30_000_000, &interior).unwrap(),
        (i64::MIN, i64::MAX),
        "an interior segment contributes all of its audio"
    );
    // The part's own edges still snap to real access-unit boundaries.
    let edges = ExportPart {
        start_100ns: 10_000_000,
        end_100ns: 30_000_000,
        segments: Vec::new(),
    };
    let (start, end) = audio_window(&samples, 10_000_000, 30_000_000, &edges).unwrap();
    assert!(
        samples
            .iter()
            .any(|(_, timestamp, _, _)| *timestamp == start),
        "the start edge lands on an access unit, got {start}"
    );
    assert!(
        samples
            .iter()
            .any(|(_, timestamp, duration, _)| *timestamp + *duration == end),
        "the end edge lands on an access unit, got {end}"
    );
}

/// t260918-cd71: a requested edge within half an access unit of a segment seam,
/// but on the far side of it, must snap to the seam. The seam is the one AU
/// boundary a segment's own list lacks -- the previous segment's last unit
/// ends exactly where this one's first starts. Numbers are segments 14/15 of
/// the recording that reproduced the failure (100ns off the session start):
/// `range 10 30.01` and `range 29.999 40` both failed with the 10.67ms error.
#[test]
fn an_edge_just_past_a_seam_snaps_to_the_seam() {
    let _runtime = encoder::MfRuntime::start().unwrap();
    const UNIT_100NS: i64 = 213_333;
    const SEAM: i64 = 300_081_305;
    let units = |first: i64, count: i64| {
        (0..count)
            .map(|index| unsafe {
                let sample = windows::Win32::Media::MediaFoundation::MFCreateSample().unwrap();
                (sample, first + index * UNIT_100NS, UNIT_100NS, 0usize)
            })
            .collect::<Vec<_>>()
    };
    // Last segment of the part: record starts 300_009_475, first unit at the
    // seam, requested end 1.9ms after it.
    let last = units(SEAM, 94);
    let part = ExportPart {
        start_100ns: 100_000_000,
        end_100ns: 300_100_000,
        segments: Vec::new(),
    };
    assert_eq!(
        audio_window(&last, 300_009_475, 300_100_000, &part).unwrap(),
        (i64::MIN, SEAM),
        "the end snaps back to the seam: this segment contributes no audio"
    );
    // First segment of the part: its last unit ends at the seam, requested
    // start 9.1ms before it.
    let first = units(SEAM - 94 * UNIT_100NS, 94);
    let part = ExportPart {
        start_100ns: 299_990_000,
        end_100ns: 400_000_000,
        segments: Vec::new(),
    };
    assert_eq!(
        audio_window(&first, 299_990_000, 300_009_472, &part).unwrap(),
        (SEAM, i64::MAX),
        "the start snaps forward to the seam: this segment contributes no audio"
    );
}

#[test]
fn aac_nearest_boundary_ties_inward() {
    assert_eq!(nearest_boundary(&[90, 110], 100, true), Some(110));
    assert_eq!(nearest_boundary(&[90, 110], 100, false), Some(90));
}

#[test]
fn filename_sanitization_and_progress_are_stable() {
    assert_eq!(sanitize_filename("My:Game?"), "My_Game_");
    assert_eq!(ratio_millionths(1, 4), 250_000);
    assert_eq!(ratio_millionths(5, 4), 1_000_000);
    assert_eq!(classify_frame_rate((200_000u64 << 32) | 3_571).unwrap(), 60);
    assert_eq!(classify_frame_rate((30u64 << 32) | 1).unwrap(), 30);
}

/// Task080 regression: the previous implementation only accepted
/// 45-75fps (bucketed to 60) or 20-40fps (bucketed to 30), rejecting
/// everything in between (and outside) with "対応外のH.264 frame
/// rateです" -- which is most of WGC's actual variable frame rate
/// output. Real recordings observed 12.485fps and 43.695fps, both of
/// which fell in that gap. Every plausible positive rate must bucket to
/// one of the candidates, with no gap.
///
/// Task370 added the 120 bucket. The bucket is only the *first* candidate
/// the boundary re-encoder tries -- see
/// `every_bucket_is_a_boundary_re_encode_candidate` for the part that
/// actually makes a 120fps recording exportable.
#[test]
fn classify_frame_rate_covers_the_full_plausible_range_with_no_gap() {
    let pack = |rate: f64| -> u64 {
        let denominator = 1_000_000u32;
        let numerator = (rate * denominator as f64).round() as u32;
        (u64::from(numerator) << 32) | u64::from(denominator)
    };
    assert_eq!(classify_frame_rate(pack(12.485)).unwrap(), 30);
    assert_eq!(classify_frame_rate(pack(43.695)).unwrap(), 30);
    assert_eq!(classify_frame_rate(pack(44.999)).unwrap(), 30);
    assert_eq!(classify_frame_rate(pack(45.0)).unwrap(), 60);
    assert_eq!(classify_frame_rate(pack(1.0)).unwrap(), 30);
    assert_eq!(classify_frame_rate(pack(89.999)).unwrap(), 60);
    assert_eq!(classify_frame_rate(pack(90.0)).unwrap(), 120);
    assert_eq!(classify_frame_rate(pack(120.0)).unwrap(), 120);
    assert!(classify_frame_rate(pack(0.5)).is_err());
    assert_eq!(classify_frame_rate(pack(120.385)).unwrap(), 120);
    assert!(classify_frame_rate(pack(241.0)).is_err());
    assert!(classify_frame_rate(0).is_err());
    assert!(classify_frame_rate(1u64 << 32).is_err());
}

/// Task370: the bucket a source classifies into must always be one the
/// boundary re-encoder will actually try, or the first attempt is wasted --
/// and, more importantly, 120 has to be *in* the list. A 120fps recording's
/// SPS (level 51) is reachable from neither 30 (level 40) nor 60 (level 42),
/// so while the list held only those two, every recording made on a 120fps
/// machine aborted with `IncompatibleCodecConfig`.
#[test]
fn every_bucket_is_a_boundary_re_encode_candidate() {
    let pack = |rate: f64| -> u64 {
        let denominator = 1_000_000u32;
        let numerator = (rate * denominator as f64).round() as u32;
        (u64::from(numerator) << 32) | u64::from(denominator)
    };
    assert!(BOUNDARY_FRAME_RATE_CANDIDATES.contains(&120));
    for rate in [1.0, 12.485, 44.999, 45.0, 59.94, 89.999, 90.0, 120.0] {
        let bucket = classify_frame_rate(pack(rate)).unwrap();
        assert!(
            BOUNDARY_FRAME_RATE_CANDIDATES.contains(&bucket),
            "{rate}fps bucketed to {bucket}, which the re-encoder never tries"
        );
    }
}

/// Task370 Steps 1. Which nominal frame rate the boundary re-encoder has to be
/// created at for its SPS to match a real recording's -- the question that
/// decides whether adding 120 to the candidate list is enough, or whether the
/// compatibility check itself has to change. Prints the source fingerprint and
/// the one every candidate rate produces, so the answer is the byte comparison
/// rather than an inference from `level`.
///
/// Needs a real recording: point `LIVIA_TASK370_SEGMENT` at one segment mp4.
#[test]
#[ignore = "task370 boundary re-encode measurement: needs LIVIA_TASK370_SEGMENT"]
fn measures_which_frame_rate_reproduces_the_source_sps() {
    let path = PathBuf::from(
        std::env::var("LIVIA_TASK370_SEGMENT").expect("set LIVIA_TASK370_SEGMENT to a segment mp4"),
    );
    let _runtime = encoder::MfRuntime::start().unwrap();
    let source = unsafe {
        let reader = open_reader(&path).unwrap();
        let video = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .unwrap();
        video_fingerprint(&video).unwrap()
    };
    println!("source={source:?}");
    let (device, _context, _) = capture::create_d3d_device().unwrap();
    for frame_rate in BOUNDARY_FRAME_RATE_CANDIDATES {
        let config = encoder::EncoderConfig {
            output_dir: std::env::temp_dir(),
            output_size: capture::CaptureSize {
                width: (source.frame_size >> 32) as i32,
                height: source.frame_size as u32 as i32,
            },
            frame_rate,
        };
        match encoder::HardwareVideoEncoder::create(&config, &device) {
            Ok(hardware) => {
                hardware.request_next_keyframe().unwrap();
                let fingerprint =
                    video_fingerprint(&hardware.output_media_type().unwrap()).unwrap();
                println!(
                    "boundary({frame_rate}fps) compatible={} {fingerprint:?}",
                    fingerprint.codec_compatible(&source)
                );
            }
            Err(error) => println!("boundary({frame_rate}fps) create failed: {error:?}"),
        }
    }
}

/// The save dialog already asked where and what to call it (task1050), so a
/// single-part export is written to exactly that file -- no collision counter,
/// no automatic name.
///
/// A selection that spans a gap still comes out as several files. They take
/// the chosen stem so they stay together in the folder, rather than one of
/// them silently overwriting the rest.
#[test]
fn a_named_export_lands_on_the_file_the_dialog_returned() {
    let chosen = PathBuf::from(r"D:\clips\my cut.mp4");
    assert_eq!(named_output_paths(&chosen, 1), vec![chosen.clone()]);
    assert_eq!(
        named_output_paths(&chosen, 3),
        vec![
            PathBuf::from(r"D:\clips\my cut_part01.mp4"),
            PathBuf::from(r"D:\clips\my cut_part02.mp4"),
            PathBuf::from(r"D:\clips\my cut_part03.mp4"),
        ]
    );
}

/// Task3010: the automatic naming used to be `{game}_{unix_seconds}.mp4`,
/// unreadable at a glance. It is now local wall-clock
/// `{game}_{YYYYMMDD-HHMMSS}.mp4` (design round18 §2/§4-1) -- pinned here so a
/// regression back to a raw counter fails a test instead of shipping quietly.
#[test]
fn automatic_export_names_use_a_readable_local_timestamp() {
    let root = std::env::temp_dir().join(format!(
        "livia-export-plan-timestamp-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();

    let first_path = allocate_output_paths(&root, "ShooterGame2", 1).remove(0);
    let name = first_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        matches_game_timestamp_mp4(&name),
        "{name} does not match {{game}}_{{YYYYMMDD-HHMMSS}}.mp4"
    );

    // Two allocations within the same second must not collide (Acceptance
    // Criteria: same-second saves are never overwritten). `allocate_output_paths`
    // only skips a name once something is actually there, same as a real save.
    fs::write(&first_path, b"stand-in for a real export").unwrap();
    let second = allocate_output_paths(&root, "ShooterGame2", 1).remove(0);
    assert_ne!(first_path, second);

    let _ = fs::remove_dir_all(&root);
}

/// Equivalent to `^.+_\d{8}-\d{6}(_\d{2})?\.mp4$` without pulling in `regex`
/// for one test -- `allocate_output_paths` appends a zero-padded collision
/// counter (`_NN`) after the timestamp when the same second is reused.
///
/// Matches from the *right*: `sanitize_filename` does not forbid `_` in the
/// game name (`split_once('_')` would misparse `"My Game_ Chapter 1"`), but
/// the 15-char timestamp always sits at a fixed offset from the end.
fn matches_game_timestamp_mp4(name: &str) -> bool {
    let Some(mut rest) = name.strip_suffix(".mp4") else {
        return false;
    };
    if rest.len() > 3 {
        let tail = &rest[rest.len() - 3..];
        if tail.starts_with('_') && tail[1..].bytes().all(|byte| byte.is_ascii_digit()) {
            rest = &rest[..rest.len() - 3];
        }
    }
    if rest.len() <= 16 {
        return false; // needs at least one game char + '_' + the 15-char stamp
    }
    let stamp_start = rest.len() - 15;
    if rest.as_bytes()[stamp_start - 1] != b'_' {
        return false;
    }
    let bytes = &rest.as_bytes()[stamp_start..];
    bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..].iter().all(u8::is_ascii_digit)
}

#[test]
fn the_dialog_opens_on_the_name_the_automatic_route_would_have_used() {
    let suggested = suggested_export_filename("My Game: Chapter 1");
    assert!(suggested.ends_with(".mp4"), "{suggested}");
    // Same sanitising as the automatic path -- the dialog is not a way to
    // smuggle a colon into a Windows filename.
    assert!(!suggested.contains(':'), "{suggested}");
    assert!(suggested.starts_with(&sanitize_filename("My Game: Chapter 1")));
}

/// Task1310: the boundary re-encoder reproduces the *source's* profile, not
/// the recording setting's.
///
/// task1000 moved recording from Baseline to High, and every session recorded
/// before it stopped exporting: the re-encoder created a High encoder, its SPS
/// could not match a Baseline one at any frame rate, and the export failed with
/// `IncompatibleCodecConfig`. This asks the encoder for each profile in turn
/// and checks the SPS that comes back says what was asked for.
#[test]
fn the_boundary_encoder_produces_the_profile_it_is_asked_for() {
    let _runtime = encoder::MfRuntime::start().unwrap();
    let (device, _context, _) = capture::create_d3d_device().expect("D3D11 device");
    let config = encoder::EncoderConfig {
        output_dir: std::env::temp_dir(),
        output_size: capture::CaptureSize {
            width: 1280,
            height: 720,
        },
        frame_rate: 60,
    };
    // Baseline (66) is what recordings before task1000 have; High (100) is what
    // they have after it. Main (77) is not something this app writes, so it is
    // not required to work -- only the two that appear in real files are.
    for profile in [66u32, 100] {
        let hardware = encoder::HardwareVideoEncoder::create_with_codec(
            &config,
            &device,
            encoder::VideoCodec::H264 { profile },
        )
        .expect("the hardware encoder must take both profiles");
        hardware.request_next_keyframe().unwrap();
        let fingerprint = video_fingerprint(&hardware.output_media_type().unwrap()).unwrap();
        println!(
            "asked for profile {profile}, SPS says {:?}",
            fingerprint.codec
        );
        assert_eq!(
            fingerprint.codec,
            encoder::VideoCodec::H264 { profile },
            "the SPS profile must be the one asked for, or a Baseline source can never be matched"
        );
    }

    // And the default is still High: recording did not change (task1000).
    let hardware = encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    hardware.request_next_keyframe().unwrap();
    let fingerprint = video_fingerprint(&hardware.output_media_type().unwrap()).unwrap();
    assert_eq!(
        fingerprint.codec,
        encoder::VideoCodec::H264 { profile: 100 },
        "recording stays on High"
    );
}

/// Builds the compressed media type a source reader would hand `probe`, without
/// a GPU: `video_fingerprint` reads declared attributes only, so a synthetic
/// type exercises the real branch at no encode-session cost (tasks README:
/// the suite is at the driver's concurrent-session limit).
fn synthetic_video_type(
    subtype: windows::core::GUID,
    sequence_header: &[u8],
) -> windows::Win32::Media::MediaFoundation::IMFMediaType {
    use windows::Win32::Media::MediaFoundation::*;
    unsafe {
        let media_type = MFCreateMediaType().unwrap();
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .unwrap();
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype).unwrap();
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, (1920u64 << 32) | 1080)
            .unwrap();
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, (60u64 << 32) | 1)
            .unwrap();
        media_type
            .SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, sequence_header)
            .unwrap();
        media_type
    }
}

/// Task1770, and the same shape of accident task1310 already paid for once:
/// **an AV1 source must never be fingerprinted as H.264.**
///
/// `MF_MT_MPEG2_PROFILE` is reused across codec families -- the AV1 MFT reports
/// its own `seq_profile` (measured: 1, Main) in it -- so a fingerprint carrying
/// a bare `profile: u32` gives the boundary re-encoder a number that looks like
/// a `profile_idc` and nothing that says it isn't one. It would then build an
/// H.264 encoder for an AV1 clip, and every candidate frame rate would miss for
/// a reason no error message could name. Carrying `VideoCodec` makes that
/// unrepresentable, and this is the test that says so.
#[test]
fn an_av1_source_is_fingerprinted_as_av1_and_never_as_h264() {
    use windows::Win32::Media::MediaFoundation::{MFVideoFormat_AV1, MFVideoFormat_H264};

    let _runtime = encoder::MfRuntime::start().unwrap();

    // A real 4-byte-start-code Annex-B SPS header: profile_idc 0x64 (High),
    // level_idc 0x28 (4.0), which is what `sps_profile_level` reads out.
    let h264_header = [0u8, 0, 0, 1, 0x67, 0x64, 0x00, 0x28, 0xAC, 0xD9];
    let h264 = video_fingerprint(&synthetic_video_type(MFVideoFormat_H264, &h264_header)).unwrap();
    assert_eq!(h264.codec, encoder::VideoCodec::H264 { profile: 0x64 });
    assert_eq!(h264.level, 0x28);

    // 17 bytes, the size measured on this machine for an AV1 sequence header
    // OBU. The bytes are never parsed -- they are compared whole -- so what
    // matters is that they survive verbatim.
    let av1_header: Vec<u8> = (0..17u8).collect();
    let av1 = video_fingerprint(&synthetic_video_type(MFVideoFormat_AV1, &av1_header)).unwrap();
    assert_eq!(av1.codec, encoder::VideoCodec::Av1);
    assert_eq!(
        av1.sequence_header, av1_header,
        "the AV1 sequence header must not go through `normalize_annex_b`: AV1 has \
         no start codes, and rewriting bytes that merely look like one would make \
         the boundary comparison compare something the encoder never produced"
    );

    // And a codec this app does not write is refused, rather than being read as
    // whichever branch happens to come first.
    assert!(video_fingerprint(&synthetic_video_type(
        windows::Win32::Media::MediaFoundation::MFVideoFormat_HEVC,
        &h264_header,
    ))
    .is_err());
}

/// The other half of the same guard: identical bytes under two codecs are not
/// compatible. `codec_compatible` is what decides whether the boundary
/// re-encoder's output may be spliced into the source, and a comparison blind
/// to the codec would accept a splice that cannot decode.
#[test]
fn identical_sequence_header_bytes_under_different_codecs_are_incompatible() {
    let h264 = VideoFingerprint {
        codec: encoder::VideoCodec::H264 { profile: 100 },
        sequence_header: vec![1, 2, 3],
        level: 0,
        frame_size: (1920u64 << 32) | 1080,
        frame_rate: 60,
    };
    let av1 = VideoFingerprint {
        codec: encoder::VideoCodec::Av1,
        ..h264.clone()
    };
    assert!(h264.codec_compatible(&h264.clone()));
    assert!(!h264.codec_compatible(&av1));
    // Two AV1 streams that agree byte for byte still are: AV1's seq_profile and
    // seq_level_idx live inside those bytes, so nothing else is left to check.
    assert!(av1.codec_compatible(&av1.clone()));
}

/// Task1350: progress counts the part being written, and never lies about it.
///
/// Before this, a gapless recording was one part and the only figure the UI
/// ever saw was the one published *after* it finished -- 24 seconds of
/// "書き出し中 0%", measured on a real 26:58 session.
#[test]
fn progress_inside_a_part_climbs_without_overrunning_it() {
    use crate::export::plan::{processed_with_part, ratio_millionths};

    let part = 10 * 10_000_000i64; // ten seconds
    let total = part; // one part, which is the case this exists for

    // Nothing written yet is 0%, and the finished part is 100%.
    assert_eq!(ratio_millionths(processed_with_part(0, part, 0), total), 0);
    assert_eq!(
        ratio_millionths(processed_with_part(0, part, part), total),
        1_000_000
    );

    // In between it climbs, and every step is strictly above the last.
    let mut previous = 0;
    for tenth in 1..10 {
        let ratio = ratio_millionths(processed_with_part(0, part, tenth * 10_000_000), total);
        assert!(ratio > previous, "{ratio} did not climb past {previous}");
        assert!(ratio < 1_000_000, "only a finished part is 100%");
        previous = ratio;
    }

    // A sample past the part's nominal end -- the last ones can sit a frame
    // beyond it -- must not report more than the part holds.
    assert_eq!(
        processed_with_part(0, part, part + 5_000_000),
        part,
        "an export must never claim more than it has written"
    );
    // Nor may a stray negative pull it backwards.
    assert_eq!(processed_with_part(part, part, -1), part);

    // With a part already behind it, the figure carries that forward.
    assert_eq!(processed_with_part(part, part, part / 2), part + part / 2);
    assert_eq!(
        ratio_millionths(processed_with_part(part, part, part / 2), 2 * part),
        750_000
    );
}

/// task1850: the writer sorts samples *inside one segment only*
/// (`part_writer::order_items`). That is enough to keep an export's timestamps
/// monotonic for the muxer only because `plan_parts` hands a part its segments
/// already ascending and non-overlapping, so segment N's samples all precede
/// segment N+1's. Break either half and a real export starts writing backward
/// DTS -- which is what the ffmpeg warning investigated in task1850 would
/// genuinely mean, as opposed to the VFR-to-CFR rounding it actually was.
#[test]
fn export_write_order_is_monotonic_across_segment_boundaries() {
    use crate::export::part_writer::{order_items, StreamRef};

    let manifest = ring_buffer::SessionManifest {
        audio_tracks: Vec::new(),
        version: 1,
        session_id: "s".into(),
        closed: false,
        retention_minutes: 15,
        // Handed to the planner out of order on purpose: it is the planner's
        // job to make them ascending, and the writer leans on that.
        segments: vec![segment(3, 40, 60), segment(1, 0, 20), segment(2, 20, 40)],
        gaps: vec![],
        target_title: None,
        target_executable: None,
        target_executable_path: None,
        markers: vec![],
        note: None,
        protected: false,
    };
    let parts = plan_parts(&manifest, 0, 60).unwrap();
    assert_eq!(parts.len(), 1, "no gap, so one part");
    let part = &parts[0];

    // Half one: what the planner promises the writer.
    for pair in part.segments.windows(2) {
        assert!(
            pair[0].end_100ns <= pair[1].start_100ns,
            "segments must be ascending and non-overlapping, got {:?} then {:?}",
            (pair[0].start_100ns, pair[0].end_100ns),
            (pair[1].start_100ns, pair[1].end_100ns),
        );
    }

    // Half two: per-segment ordering over those segments is globally monotone.
    // Each segment's samples are fed in a deliberately jumbled arrival order.
    let mut written = Vec::new();
    for segment in &part.segments {
        let base = segment.start_100ns;
        let mut items = vec![
            (base + 15, StreamRef::Audio(1), ()),
            (base + 5, StreamRef::Video, ()),
            (base + 15, StreamRef::Video, ()),
            (base + 15, StreamRef::Audio(0), ()),
            (base, StreamRef::Video, ()),
        ];
        order_items(&mut items);
        written.extend(
            items
                .into_iter()
                .map(|(timestamp, stream, ())| (timestamp, stream)),
        );
    }

    assert_eq!(written.len(), 15, "no sample may be dropped");
    for pair in written.windows(2) {
        assert!(
            pair[0].0 <= pair[1].0,
            "write order went backwards: {:?} then {:?}",
            pair[0],
            pair[1],
        );
    }

    // At one timestamp: video first, then audio in track order (task1280).
    assert_eq!(
        written[..5],
        [
            (0, StreamRef::Video),
            (5, StreamRef::Video),
            (15, StreamRef::Video),
            (15, StreamRef::Audio(0)),
            (15, StreamRef::Audio(1)),
        ]
    );
}

/// t260919-4866: a boundary re-encode of a 388x218 window recording panicked
/// with `range end index 134788 out of range for slice of length 134400`,
/// failing every clip export of that session. The decoder's buffer was
/// pitch 448 x 224 rows (locked length 150,528), but the rows were derived
/// from `GetContiguousLength` = 134,400 -- 200 rows, fewer than the frame's
/// 218 -- so the chroma copy ran past the slice built from them. The layouts
/// below are the two measured on real recordings; every byte the repack must
/// not copy (pitch padding, padded rows) is 0xEE, which no row value uses.
#[test]
fn nv12_repack_takes_its_rows_from_the_locked_length() {
    const PADDING: u8 = 0xEE;
    let luma = |row: usize| (row % 200) as u8 + 1;
    let chroma = |row: usize| (row % 100) as u8 + 128;
    // (width, height, pitch, rows the decoder padded the luma plane to)
    for (width, height, pitch, rows) in [
        (388usize, 218usize, 448usize, 224usize),
        (1920, 1032, 1920, 1040),
    ] {
        let mut source = vec![PADDING; pitch * rows * 3 / 2];
        for row in 0..height {
            source[row * pitch..][..width].fill(luma(row));
        }
        for row in 0..height / 2 {
            source[(rows + row) * pitch..][..width].fill(chroma(row));
        }
        let packed = super::super::part_writer::repack_nv12(&source, pitch, width, height)
            .unwrap_or_else(|| panic!("{width}x{height} at pitch {pitch} needs a repack"));
        assert_eq!(packed.len(), width * height * 3 / 2, "{width}x{height}");
        for row in 0..height {
            assert!(
                packed[row * width..][..width]
                    .iter()
                    .all(|&byte| byte == luma(row)),
                "{width}x{height}: luma row {row}"
            );
        }
        for row in 0..height / 2 {
            assert!(
                packed[(height + row) * width..][..width]
                    .iter()
                    .all(|&byte| byte == chroma(row)),
                "{width}x{height}: chroma row {row}"
            );
        }
    }
    // Already the frame exactly: nothing to repack.
    assert!(
        super::super::part_writer::repack_nv12(&vec![0u8; 388 * 218 * 3 / 2], 388, 388, 218)
            .is_none()
    );
    // A buffer too short for the frame at its own pitch -- the length the old
    // derivation sliced to -- is passed through, not indexed past its end.
    assert!(super::super::part_writer::repack_nv12(&vec![0u8; 134_400], 448, 388, 218).is_none());
}
