//! Real-bitstream fixture exports (video-only segment, task009 clip).
use super::ledger::{assert_raw_middle_passthrough, assert_video_sync_samples};
use super::*;

/// Real 160x120/30fps output of this project's own hardware H.264 encoder,
/// captured once so the video-only fixture below needs no GPU encode
/// session at test time. `SEQUENCE_HEADER` is the media type's
/// `MF_MT_MPEG_SEQUENCE_HEADER` (SPS+PPS), `IDR`/`DELTA` are the first two
/// compressed samples verbatim. Genuine bitstream bytes matter because
/// `probe_source_types` parses the SPS for profile/level; nothing in the
/// export path decodes the slices themselves.
const FIXTURE_SEQUENCE_HEADER: &str =
    "000000016742c02095a0a11f966a020202800001f400007530078e15500000000168ce3c80";
const FIXTURE_IDR: &str = "000000010910000000016742c02095a0a11f966a02020280\
    0001f400007530078e15500000000168ce3c800000000165b80406bc462800052f31c000\
    72ae38000e55c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9\
    c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9\
    e0";
const FIXTURE_DELTA: &str = "0000000109300000000161e02018f028c0";
const FIXTURE_FRAME_DURATION_100NS: i64 = 333_333;

fn fixture_bytes(hex: &str) -> Vec<u8> {
    let hex = hex.replace([' ', '\n'], "");
    (0..hex.len() / 2)
        .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap())
        .collect()
}

unsafe fn fixture_sample(
    bytes: &[u8],
    time_100ns: i64,
    duration_100ns: i64,
    clean_point: bool,
) -> IMFSample {
    use windows::Win32::Media::MediaFoundation::{
        MFCreateMemoryBuffer, MFCreateSample, MFSampleExtension_CleanPoint,
    };
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len() as u32).unwrap();
        let mut destination = std::ptr::null_mut();
        buffer.Lock(&mut destination, None, None).unwrap();
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), destination, bytes.len());
        buffer.Unlock().unwrap();
        buffer.SetCurrentLength(bytes.len() as u32).unwrap();
        let sample = MFCreateSample().unwrap();
        sample.AddBuffer(&buffer).unwrap();
        sample.SetSampleTime(time_100ns).unwrap();
        sample.SetSampleDuration(duration_100ns).unwrap();
        if clean_point {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1).unwrap();
        }
        sample
    }
}

/// Writes a segment with a video track and no audio track whatsoever,
/// through the production `Mp4SegmentWriter` (`aac_media_type: None` -- the
/// same call a recording makes when a segment receives no AAC sample, see
/// `capture.rs` task085), so what `export_part` opens is the real shape.
/// Also the playback worker's edge-seek fixture (t260913-b4a6): a keyframe and
/// the delta after it, which is the smallest GOP a seek can land inside.
pub(crate) fn write_video_only_segment(directory: &Path) -> ring_buffer::SegmentRecord {
    write_video_only_segment_at(directory, 30, FIXTURE_FRAME_DURATION_100NS)
}

/// [`write_video_only_segment`] with the configured frame rate and the gap to
/// the delta frame chosen by the caller (t260913-e5ce).
///
/// Two samples are the whole production shape: `release_held_video` overwrites
/// the first sample's duration with the measured interval to the second, and
/// `finish_sink` leaves the last one at the encoder's nominal `10^7 / fps` --
/// which is what `exclusive_segment_end` adds to the last sample time, so the
/// returned `end_100ns` is the one a manifest would carry. `delta_100ns` is
/// therefore both the measured interval and the second sample's timestamp:
/// pass the nominal value for an evenly paced segment, something else for the
/// uneven pacing a real capture delivers.
pub(crate) fn write_video_only_segment_at(
    directory: &Path,
    frame_rate: u8,
    delta_100ns: i64,
) -> ring_buffer::SegmentRecord {
    let nominal_100ns = 10_000_000 / i64::from(frame_rate);
    let config = encoder::EncoderConfig {
        output_dir: directory.to_path_buf(),
        output_size: capture::CaptureSize {
            width: 160,
            height: 120,
        },
        frame_rate,
    };
    let media_type = unsafe {
        let media_type = MFCreateMediaType().unwrap();
        media_type
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .unwrap();
        media_type
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
            .unwrap();
        media_type
            .SetUINT64(&MF_MT_FRAME_SIZE, (160u64 << 32) | 120)
            .unwrap();
        media_type
            .SetUINT64(&MF_MT_FRAME_RATE, (u64::from(frame_rate) << 32) | 1)
            .unwrap();
        media_type
            .SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_MT_INTERLACE_MODE,
                windows::Win32::Media::MediaFoundation::MFVideoInterlace_Progressive.0 as u32,
            )
            .unwrap();
        media_type
            .SetBlob(
                &MF_MT_MPEG_SEQUENCE_HEADER,
                &fixture_bytes(FIXTURE_SEQUENCE_HEADER),
            )
            .unwrap();
        media_type
    };
    let (partial, final_path) = ring_buffer::store::segment_paths(directory, 0);
    let writer = encoder::Mp4SegmentWriter::create_at_paths(
        &config,
        &media_type,
        None,
        0,
        partial,
        final_path,
    )
    .unwrap();
    unsafe {
        assert!(writer
            .write_video_sample_at(
                &fixture_sample(&fixture_bytes(FIXTURE_IDR), 0, nominal_100ns, true),
                0
            )
            .unwrap());
        assert!(writer
            .write_video_sample_at(
                &fixture_sample(
                    &fixture_bytes(FIXTURE_DELTA),
                    delta_100ns,
                    nominal_100ns,
                    false,
                ),
                delta_100ns,
            )
            .unwrap());
    }
    let path = writer.finalize().unwrap();
    ring_buffer::SegmentRecord {
        index: 0,
        bytes: fs::metadata(&path).unwrap().len(),
        path,
        start_100ns: 0,
        // `exclusive_segment_end`: the last sample plus the nominal interval.
        end_100ns: delta_100ns + nominal_100ns,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    }
}

/// Task124 regression: a segment with no audio track at all failed the
/// *entire* export with `AAC情報を読めません: ... (0xC00D36B3)` --
/// `MF_E_INVALIDSTREAMNUMBER`, Media Foundation's way of saying "that
/// stream does not exist" -- instead of degrading to a video-only output.
/// Reproduced on real recordings whose capture target had no audio at all.
#[test]
fn task124_exports_a_video_only_segment_instead_of_failing() {
    let directory = std::env::temp_dir().join(format!("livia-task124-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    // Held for the whole test so the writer's own `MfRuntime` dropping
    // between fixture and export never takes the MF refcount to zero.
    let _runtime = encoder::MfRuntime::start().unwrap();
    let segment = write_video_only_segment(&directory.join("segments"));

    // Part bounds exactly match the segment, so every frame goes through
    // the passthrough path -- no boundary re-encode, no GPU involved.
    let part = ExportPart {
        start_100ns: segment.start_100ns,
        end_100ns: segment.end_100ns,
        segments: vec![segment],
    };
    let partial = directory.join("clip.partial");
    let final_path = directory.join("clip.mp4");
    let mut diagnostics = ExportDiagnostics::default();
    export_part(
        &part,
        &partial,
        &final_path,
        &AtomicBool::new(false),
        &mut diagnostics,
    )
    .expect("a segment without an audio track must export video-only, not fail");
    fs::rename(&partial, &final_path).unwrap();

    let inspection = encoder::inspect_finalized_mp4(&final_path)
        .expect("the exported video-only MP4 must be readable");
    assert!(
        !inspection.has_aac_audio,
        "no audio existed to write, so the output must declare no audio track"
    );
    assert!(
        inspection.duration_100ns > 0,
        "the video samples must survive"
    );
    assert!(diagnostics.passthrough_video_samples > 0);
    assert_eq!(diagnostics.passthrough_audio_samples, 0);
    let _ = fs::remove_dir_all(&directory);
}

/// Task3710: the exported MP4 must index its key frames.
///
/// `export_writer::write` used to copy only buffer, time and duration onto the
/// sample it hands the muxer, so the MPEG4 sink was never told which samples
/// were clean points and wrote `stss` with `entry_count = 0`. Task3480
/// measured the consequence on real clips: `SetCurrentPosition` returns `S_OK`
/// and the source advertises `CAN_SEEK`, yet every seek lands at 0.000 s, up
/// to 10.1 s of decode on a 3-minute clip.
///
/// The fixture is the same no-GPU one task124 uses -- a real segment written
/// by the production `Mp4SegmentWriter` from prerecorded compressed samples,
/// exported on the segment's exact bounds so every sample takes the
/// *passthrough* path. That is the path whose `CleanPoint` was in doubt: these
/// samples come back out of a recorded container through the Source Reader,
/// not from an encoder MFT. The boundary re-encode path costs an encode
/// session and is checked on a real clip in the task's Verification.
///
/// The expectation is exact. Sample 1 is the IDR and sample 2 the delta
/// frame, so `[1]` is the only correct answer: `[]` is the old defect and
/// `[1, 2]` would be an unconditional stamp putting a non-key frame into the
/// index, which is worse than no index at all.
#[test]
fn an_exported_mp4_indexes_its_key_frames() {
    let directory = std::env::temp_dir().join(format!("livia-task3710-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    // Held for the whole test, exactly as task124 does: the fixture writer's
    // own `MfRuntime` must not take the MF refcount to zero before the export.
    let _runtime = encoder::MfRuntime::start().unwrap();
    let segment = write_video_only_segment(&directory.join("segments"));

    let part = ExportPart {
        start_100ns: segment.start_100ns,
        end_100ns: segment.end_100ns,
        segments: vec![segment],
    };
    let partial = directory.join("clip.partial");
    let final_path = directory.join("clip.mp4");
    let mut diagnostics = ExportDiagnostics::default();
    export_part(
        &part,
        &partial,
        &final_path,
        &AtomicBool::new(false),
        &mut diagnostics,
    )
    .expect("the fixture segment must export");
    fs::rename(&partial, &final_path).unwrap();
    assert!(
        diagnostics.passthrough_video_samples > 0,
        "this fixture must exercise the passthrough path, not a boundary re-encode"
    );

    assert_video_sync_samples(&final_path, &[1]);
    let _ = fs::remove_dir_all(&directory);
}

/// Task1770: an AV1 recording, read back through the *source reader* and
/// exported both ways.
///
/// The go/no-go probe task1770 ran earlier measured the encoder's own output
/// media type and the export sink. Neither is what `video_fingerprint` reads:
/// it opens a muxed segment with `MFCreateSourceReaderFromURL` and asks *that*
/// type for `MF_MT_MPEG_SEQUENCE_HEADER`. If the reader omits the attribute for
/// AV1, or reframes the bytes (av1C-wrapped rather than the bare OBU), every
/// AV1 export dies at probe -- passthrough included -- so this measures the
/// reader side before trusting it, and prints both sides for comparison.
///
/// Then it exports twice: once on the segment's exact bounds (passthrough, the
/// minimal success) and once trimmed at both ends (the boundary re-encoder).
///
/// Costs an AV1 encode session, so `#[ignore]` per the tasks README.
///
/// ```text
/// cargo test --lib av1_round_trips_through_the_source_reader -- --ignored --nocapture
/// ```
#[test]
#[ignore = "task1770 AV1 export round trip: costs an encode session, run alone"]
fn av1_round_trips_through_the_source_reader_and_exports() {
    use windows::Win32::Media::MediaFoundation::{
        MF_MT_MPEG_SEQUENCE_HEADER, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    };

    let directory = std::env::temp_dir().join(format!("livia-task1770-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let _runtime = encoder::MfRuntime::start().unwrap();
    let (device, _, _) = capture::create_d3d_device().expect("D3D11 device");
    let config = encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: capture::CaptureSize {
            width: 640,
            height: 360,
        },
        frame_rate: 30,
    };
    let mut video = encoder::HardwareVideoEncoder::create_with_codec(
        &config,
        &device,
        encoder::VideoCodec::Av1,
    )
    .expect("the hardware AV1 MFT should start");
    let av1_type = video.output_media_type().unwrap();
    let encoder_side = unsafe {
        let size = av1_type
            .GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER)
            .unwrap_or_default() as usize;
        let mut bytes = vec![0u8; size];
        let mut written = 0;
        if size > 0 {
            av1_type
                .GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut bytes, Some(&mut written))
                .unwrap();
        }
        bytes.truncate(written as usize);
        bytes
    };

    let (partial_path, final_path) = ring_buffer::store::segment_paths(&directory, 0);
    let writer = encoder::Mp4SegmentWriter::create_at_paths(
        &config,
        &av1_type,
        None,
        0,
        partial_path,
        final_path,
    )
    .expect("the muxer takes AV1 (task1750)");
    const FRAME_100NS: i64 = 333_333;
    let mut last_video_100ns = 0i64;
    let mut frames = 0u32;
    video.request_next_keyframe().unwrap();
    for index in 0..90i64 {
        for sample in video.poll().unwrap() {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * FRAME_100NS) };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
            last_video_100ns = timestamp;
            frames += 1;
        }
        if video.has_input_credit() {
            let sample = video.allocate_input_sample().unwrap();
            video
                .process_input_sample(&sample, index * FRAME_100NS)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    video.begin_drain().unwrap();
    for _ in 0..400 {
        let samples = video.poll().unwrap();
        if samples.is_empty() && video.drain_complete() {
            break;
        }
        for sample in samples {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or_default() };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
            last_video_100ns = timestamp;
            frames += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // Released before the reader opens the file: an encode session held open
    // here is one the boundary re-encode below cannot have.
    drop(video);
    let path = writer.finalize().unwrap();
    println!("task1770: AV1 fixture has {frames} frames, last pts {last_video_100ns}");

    // (1) The reader side, which is the one export actually reads.
    let reader_side = unsafe {
        let reader = super::super::probe::open_reader(&path).expect("the AV1 segment opens");
        let media_type = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .expect("the reader reports a video media type");
        video_fingerprint(&media_type)
    };
    println!(
        "task1770: encoder-side sequence header ({} bytes) = {encoder_side:02x?}",
        encoder_side.len()
    );
    match &reader_side {
        Ok(fingerprint) => println!(
            "task1770: reader-side fingerprint codec={:?}, sequence header ({} bytes) = {:02x?}",
            fingerprint.codec,
            fingerprint.sequence_header.len(),
            fingerprint.sequence_header
        ),
        Err(failure) => println!("task1770: reader-side fingerprint FAILED: {failure:?}"),
    }
    let fingerprint = reader_side.expect(
        "export identifies a stream by the sequence header the *reader* reports; without \
         one for AV1 no AV1 clip can be exported at all",
    );
    assert_eq!(fingerprint.codec, encoder::VideoCodec::Av1);

    let segment = ring_buffer::SegmentRecord {
        index: 0,
        bytes: fs::metadata(&path).unwrap().len(),
        path,
        start_100ns: 0,
        end_100ns: last_video_100ns + FRAME_100NS,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    };

    // (2) Passthrough: the part's bounds are the segment's, so every sample is
    // carried across compressed. This is task1770's minimal success.
    let mut diagnostics = ExportDiagnostics::default();
    let passthrough = directory.join("passthrough.mp4");
    export_part(
        &ExportPart {
            start_100ns: segment.start_100ns,
            end_100ns: segment.end_100ns,
            segments: vec![segment.clone()],
        },
        &directory.join("passthrough.partial"),
        &passthrough,
        &AtomicBool::new(false),
        &mut diagnostics,
    )
    .expect("an AV1 clip on segment bounds must export by passthrough");
    fs::rename(directory.join("passthrough.partial"), &passthrough).unwrap();
    let inspection = encoder::inspect_finalized_mp4(&passthrough)
        .expect("the exported AV1 MP4 must be readable");
    assert!(inspection.duration_100ns > 0);
    assert!(diagnostics.passthrough_video_samples > 0);
    assert_eq!(diagnostics.reencoded_video_frames, 0);
    println!(
        "task1770: passthrough export ok, duration_100ns={}, samples={}",
        inspection.duration_100ns, diagnostics.passthrough_video_samples
    );

    // (3) Boundary: both ends cut inside the segment, so `collect_boundary_video`
    // decodes to NV12 and re-encodes -- the part that has to reproduce the
    // source's sequence header byte for byte at some candidate frame rate.
    let mut boundary_diagnostics = ExportDiagnostics::default();
    let boundary = directory.join("boundary.mp4");
    let result = export_part(
        &ExportPart {
            start_100ns: segment.start_100ns + 5 * FRAME_100NS,
            end_100ns: segment.end_100ns - 5 * FRAME_100NS,
            segments: vec![segment],
        },
        &directory.join("boundary.partial"),
        &boundary,
        &AtomicBool::new(false),
        &mut boundary_diagnostics,
    );
    match &result {
        Ok(()) => println!(
            "task1770: boundary export ok, reencoded={} frames",
            boundary_diagnostics.reencoded_video_frames
        ),
        Err(failure) => println!("task1770: boundary export FAILED: {failure:?}"),
    }
    result.expect(
        "a trimmed AV1 clip must export; a failure here is the acceptance criteria's \
         'identify why' branch and the message above is the finding",
    );
    fs::rename(directory.join("boundary.partial"), &boundary).unwrap();
    assert!(boundary_diagnostics.reencoded_video_frames > 0);
    assert!(encoder::inspect_finalized_mp4(&boundary).is_ok());

    let _ = fs::remove_dir_all(&directory);
}

#[test]
fn task009_exports_real_game_clip_when_requested() {
    let Ok(manifest_path) = std::env::var("LIVIA_TASK009_MANIFEST") else {
        return;
    };
    let manifest: ring_buffer::SessionManifest =
        serde_json::from_slice(&fs::read(manifest_path).unwrap()).unwrap();
    let first = manifest.segments.first().unwrap();
    let duration_100ns = std::env::var("LIVIA_TASK009_DURATION_100NS")
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(300_000_000);
    let start = first.start_100ns + 2_000_000;
    let end = start + duration_100ns;
    let part = plan_parts(&manifest, start, end).unwrap().remove(0);
    let output =
        std::env::temp_dir().join(format!("livia-task009-integration-{}", std::process::id()));
    let _ = fs::remove_dir_all(&output);
    fs::create_dir_all(&output).unwrap();
    let partial = output.join("clip.partial");
    let final_path = output.join("clip.mp4");
    let mut diagnostics = ExportDiagnostics::default();
    export_part(
        &part,
        &partial,
        &final_path,
        &AtomicBool::new(false),
        &mut diagnostics,
    )
    .unwrap();
    fs::rename(&partial, &final_path).unwrap();
    let inspection = encoder::inspect_finalized_mp4(&final_path).unwrap();
    println!(
        "Task009 inspection: requested_100ns={}, duration_100ns={}",
        duration_100ns, inspection.duration_100ns
    );
    assert!(inspection.has_aac_audio);
    assert!(inspection.first_sample_clean_point);
    let expected_duration = part.end_100ns - part.start_100ns;
    assert!(inspection.duration_100ns > expected_duration - 10_000_000);
    if duration_100ns > encoder::SEGMENT_DURATION_100NS {
        assert!(diagnostics.passthrough_video_samples > 0);
    }
    assert!(diagnostics.reencoded_video_frames > 0);
    assert!(diagnostics.passthrough_audio_samples > 0);
    if part.segments.len() >= 3 {
        let middle = &part.segments[1];
        assert_raw_middle_passthrough(&middle.path, &final_path);
    }
    println!(
        "Task009 integration: duration_100ns={}, passthrough_video={}, reencoded_video={}, audio={}",
        inspection.duration_100ns,
        diagnostics.passthrough_video_samples,
        diagnostics.reencoded_video_frames,
        diagnostics.passthrough_audio_samples
    );
    let _ = fs::remove_dir_all(output);
}

/// Task1280: a segment with three audio tracks exports with three audio
/// tracks, in the same order -- after the mix that t261003-b713 puts first.
///
/// The fixture is written by the production `Mp4SegmentWriter` in its
/// multi-track shape (task1260), so what `export_part` opens is the real thing
/// a recording produces. Each track is given a **different number of AAC
/// frames**, which is what makes the order checkable at the other end: an
/// export that merged, dropped or reordered them cannot produce the same
/// counts in the same order.
#[test]
fn task1280_exports_every_audio_track_in_track_order() {
    use windows::Win32::Media::MediaFoundation::*;

    let directory = std::env::temp_dir().join(format!("livia-task1280-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let _runtime = encoder::MfRuntime::start().unwrap();
    let (device, _, _) = capture::create_d3d_device().expect("D3D11 device");
    let config = encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: capture::CaptureSize {
            width: 640,
            height: 360,
        },
        frame_rate: 30,
    };
    let mut video = encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let h264 = video.output_media_type().unwrap();
    let aac_encoder = capture::audio::AacEncoder::create().expect("system AAC MFT");
    let aac = aac_encoder.output_media_type().clone();

    const TRACKS: usize = 3;
    let strides = [1usize, 2, 3];
    let writer =
        encoder::Mp4SegmentWriter::create_with_audio(&config, &h264, Some(&aac), TRACKS, 0)
            .unwrap();
    let mut written = [0u64; TRACKS];
    let mut frame = 0usize;
    let mut last_video_100ns = 0i64;
    for index in 0..45i64 {
        for sample in video.poll().unwrap() {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * 333_333) };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
            last_video_100ns = timestamp;
        }
        let pcm = vec![0.0_f32; 1024 * 2];
        for sample in aac_encoder.encode_f32(&pcm, index * 166_667).unwrap() {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * 166_667) };
            for (track, stride) in strides.iter().enumerate() {
                if !frame.is_multiple_of(*stride) {
                    continue;
                }
                for _ in 0..50 {
                    if writer
                        .write_aac_sample_on(track, &sample, timestamp)
                        .unwrap()
                    {
                        written[track] += 1;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            frame += 1;
        }
        if video.has_input_credit() {
            let sample = video.allocate_input_sample().unwrap();
            video
                .process_input_sample(&sample, index * 333_333)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    video.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = video.poll().unwrap();
        if samples.is_empty() && video.drain_complete() {
            break;
        }
        for sample in samples {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or_default() };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
            last_video_100ns = timestamp;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let path = writer.finalize().unwrap();
    println!("task1280: fixture wrote {written:?} AAC frames per track");

    let segment = ring_buffer::SegmentRecord {
        index: 0,
        bytes: fs::metadata(&path).unwrap().len(),
        path,
        start_100ns: 0,
        end_100ns: last_video_100ns + 333_333,
        thumbnail_path: None,
        // One per track, as a recording writes (task1250). Zero: the fixture
        // has no priming unit to seat.
        audio_offsets_100ns: vec![0; TRACKS],
    };
    let part = ExportPart {
        start_100ns: segment.start_100ns,
        end_100ns: segment.end_100ns,
        segments: vec![segment],
    };
    let partial = directory.join("clip.partial");
    let final_path = directory.join("clip.mp4");
    let mut diagnostics = ExportDiagnostics::default();
    export_part(
        &part,
        &partial,
        &final_path,
        &AtomicBool::new(false),
        &mut diagnostics,
    )
    .expect("a multi-track segment must export");
    fs::rename(&partial, &final_path).unwrap();

    // Counted through the Source Reader, one reader per track: an exported MP4
    // is not fragmented, so the box-level summary reports zero samples for
    // every track of it (measured). `audio_stream_indices` is the production
    // helper, so this checks the order the export path itself believes in.
    let mut read_back: Vec<u64> = Vec::new();
    unsafe {
        let url = windows::core::HSTRING::from(final_path.to_string_lossy().as_ref());
        let probe = MFCreateSourceReaderFromURL(&url, None).expect("the export opens as media");
        let streams = super::super::probe::audio_stream_indices(&probe);
        drop(probe);
        // t261003-b713 (decision 13): a multi-track export writes the mix as
        // audio track 0 and every original after it, so the originals are
        // checked from the second stream on.
        assert_eq!(
            streams.len(),
            TRACKS + 1,
            "the export must carry the mix and every audio track the segment had"
        );
        for stream in streams.into_iter().skip(1) {
            let reader = MFCreateSourceReaderFromURL(&url, None).unwrap();
            for other in 0..32u32 {
                if reader.GetNativeMediaType(other, 0).is_err() {
                    break;
                }
                let _ = reader.SetStreamSelection(other, other == stream);
            }
            let mut samples = 0u64;
            for _ in 0..4000 {
                let mut flags = 0;
                let mut timestamp = 0;
                let mut sample = None;
                reader
                    .ReadSample(
                        stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .unwrap();
                if sample.is_some() {
                    samples += 1;
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
            read_back.push(samples);
        }
    }
    println!("task1280: exported tracks read back as {read_back:?}");
    // Same counts in the same order: the export is a passthrough remux, so
    // every frame that went in comes out on the track it went in on.
    assert_eq!(
        read_back,
        written.to_vec(),
        "audio tracks came back as {read_back:?}, not the {written:?} that went in"
    );
    assert!(
        diagnostics.passthrough_audio_samples >= written.iter().sum::<u64>(),
        "every track should have gone through the passthrough path"
    );
    let _ = fs::remove_dir_all(&directory);
}

/// One real segment written by the production `Mp4SegmentWriter`, one audio
/// track per entry of `tones`: a 440 Hz sine at that amplitude (0.0 = digital
/// silence). Returns the record with `start_100ns` placed where the caller
/// says, as the next segment of a recording would be.
fn write_tone_segment(
    directory: &Path,
    tones: &[f32],
    start_100ns: i64,
) -> ring_buffer::SegmentRecord {
    let _runtime = encoder::MfRuntime::start().unwrap();
    let (device, _, _) = capture::create_d3d_device().expect("D3D11 device");
    let config = encoder::EncoderConfig {
        output_dir: directory.to_path_buf(),
        output_size: capture::CaptureSize {
            width: 640,
            height: 360,
        },
        frame_rate: 30,
    };
    let mut video = encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let h264 = video.output_media_type().unwrap();
    // One AAC encoder per track: a shared one would run two signals through
    // one encoder's state. They all emit the same AAC type.
    let aac_encoders = tones
        .iter()
        .map(|_| capture::audio::AacEncoder::create().expect("system AAC MFT"))
        .collect::<Vec<_>>();
    let aac = aac_encoders[0].output_media_type().clone();
    let writer =
        encoder::Mp4SegmentWriter::create_with_audio(&config, &h264, Some(&aac), tones.len(), 0)
            .unwrap();
    let mut last_video_100ns = 0i64;
    for index in 0..45i64 {
        for sample in video.poll().unwrap() {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * 333_333) };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
            last_video_100ns = timestamp;
        }
        for (track, amplitude) in tones.iter().enumerate() {
            let pcm = (0..2048)
                .map(|n| {
                    let frame = (index as usize * 1024 + n / 2) as f32;
                    amplitude * (frame * 440.0 * std::f32::consts::TAU / 48_000.0).sin()
                })
                .collect::<Vec<_>>();
            for sample in aac_encoders[track]
                .encode_f32(&pcm, index * 213_333)
                .unwrap()
            {
                let timestamp = unsafe { sample.GetSampleTime().unwrap_or(index * 213_333) };
                for _ in 0..50 {
                    if writer
                        .write_aac_sample_on(track, &sample, timestamp)
                        .unwrap()
                    {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
        if video.has_input_credit() {
            let sample = video.allocate_input_sample().unwrap();
            video
                .process_input_sample(&sample, index * 333_333)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    video.begin_drain().unwrap();
    for _ in 0..200 {
        let samples = video.poll().unwrap();
        if samples.is_empty() && video.drain_complete() {
            break;
        }
        for sample in samples {
            let timestamp = unsafe { sample.GetSampleTime().unwrap_or_default() };
            assert!(writer.write_video_sample_at(&sample, timestamp).unwrap());
            last_video_100ns = timestamp;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let path = writer.finalize().unwrap();
    ring_buffer::SegmentRecord {
        index: 0,
        bytes: fs::metadata(&path).unwrap().len(),
        path,
        start_100ns,
        end_100ns: start_100ns + last_video_100ns + 333_333,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0; tones.len()],
    }
}

/// Every audio stream of an MP4, decoded to 48 kHz stereo float, as the RMS of
/// the first and second half of `split_100ns` (absolute file time). Streams in
/// the order the export path itself sees them (`audio_stream_indices`).
fn audio_rms_around(path: &Path, split_100ns: i64) -> Vec<(f32, f32)> {
    use windows::Win32::Media::MediaFoundation::*;
    let url = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
    let streams = unsafe {
        let probe = MFCreateSourceReaderFromURL(&url, None).expect("the export opens as media");
        super::super::probe::audio_stream_indices(&probe)
    };
    streams
        .into_iter()
        .map(|stream| unsafe {
            let reader = MFCreateSourceReaderFromURL(&url, None).unwrap();
            for other in 0..32u32 {
                if reader.GetNativeMediaType(other, 0).is_err() {
                    break;
                }
                let _ = reader.SetStreamSelection(other, other == stream);
            }
            let pcm = MFCreateMediaType().unwrap();
            pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).unwrap();
            pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)
                .unwrap();
            pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).unwrap();
            pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32).unwrap();
            reader.SetCurrentMediaType(stream, None, &pcm).unwrap();
            let (mut sums, mut counts) = ([0f64; 2], [0usize; 2]);
            loop {
                let (mut flags, mut timestamp, mut sample) = (0, 0, None);
                reader
                    .ReadSample(
                        stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .unwrap();
                if let Some(sample) = sample {
                    let buffer = sample.ConvertToContiguousBuffer().unwrap();
                    let (mut bytes, mut length) = (std::ptr::null_mut(), 0u32);
                    buffer.Lock(&mut bytes, None, Some(&mut length)).unwrap();
                    let floats =
                        std::slice::from_raw_parts(bytes.cast::<f32>(), length as usize / 4);
                    // Skip the edges of each half: AAC's own fade-in and the
                    // block straddling the split are not what is measured.
                    let half = usize::from(timestamp >= split_100ns);
                    let away_from_edges =
                        (timestamp - split_100ns).abs() > 2_000_000 && timestamp > 2_000_000;
                    if away_from_edges {
                        for value in floats {
                            sums[half] += f64::from(value * value);
                        }
                        counts[half] += floats.len();
                    }
                    let _ = buffer.Unlock();
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
            // NaN where the stream has no samples at all: "silent" must mean
            // silence was written, not that the stream starts later.
            let rms = |half: usize| match counts[half] {
                0 => f32::NAN,
                n => (sums[half] / n as f64).sqrt() as f32,
            };
            (rms(0), rms(1))
        })
        .collect()
}

/// t261003-b713: a recording whose second track was switched on mid-way --
/// the first segment has one audio track, the next has two -- exports instead
/// of failing `segments_config_mismatch`, and:
/// - an export carries the mix first, then both original tracks, the added
///   one silent before it existed and sounding after (decisions 13, 6);
/// - the mix follows the saved levels: muting the added track leaves the mix
///   at track 0's level alone (decision 12);
/// - a clip carries the mix alone (decision 12).
#[test]
fn t261003_b713_a_track_added_mid_recording_exports_with_silence_and_a_mix() {
    let directory = std::env::temp_dir().join(format!("livia-b713-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    // Held for the whole test: each fixture call starts and stops its own,
    // and the read-back below needs Media Foundation still running.
    let _runtime = encoder::MfRuntime::start().unwrap();
    // Each segment in its own folder: the writer names its file by segment
    // index, so a shared folder would let the second overwrite the first.
    let (first_dir, second_dir) = (directory.join("0"), directory.join("1"));
    fs::create_dir_all(&first_dir).unwrap();
    fs::create_dir_all(&second_dir).unwrap();
    let first = write_tone_segment(&first_dir, &[0.2], 0);
    let second = write_tone_segment(&second_dir, &[0.2, 0.4], first.end_100ns);
    let split = first.end_100ns;
    let part = ExportPart {
        start_100ns: first.start_100ns,
        end_100ns: second.end_100ns,
        segments: vec![first, second],
    };
    let level = |volume_percent, muted| TrackLevel {
        volume_percent,
        muted,
    };
    let export = |name: &str, audio_mix: AudioMix| {
        let partial = directory.join(format!("{name}.partial"));
        let final_path = directory.join(format!("{name}.mp4"));
        export_part_mixed(&part, &partial, &audio_mix).expect("a grown session must export");
        fs::rename(&partial, &final_path).unwrap();
        let rms = audio_rms_around(&final_path, split);
        println!("b713 {name}: per-stream rms (before, after) = {rms:?}");
        rms
    };
    let loud = |value: f32| value > 0.05;
    let quiet = |value: f32| value < 0.005;

    let full = export(
        "export",
        AudioMix {
            levels: vec![level(100, false), level(100, false)],
            mix_only: false,
        },
    );
    assert_eq!(full.len(), 3, "mix + the two original tracks: {full:?}");
    let (mix, track0, added) = (full[0], full[1], full[2]);
    assert!(
        loud(track0.0) && loud(track0.1),
        "track 0 sounds throughout"
    );
    assert!(
        quiet(added.0),
        "the added track is silence before it existed"
    );
    assert!(loud(added.1), "and sounds after");
    assert!(
        mix.1 > track0.1 * 1.2,
        "the mix carries the added track once it exists"
    );

    let muted = export(
        "muted",
        AudioMix {
            levels: vec![level(100, false), level(100, true)],
            mix_only: false,
        },
    );
    assert!(
        (muted[0].1 - muted[1].1).abs() < muted[1].1 * 0.15,
        "muting the added track leaves the mix at track 0's level: {muted:?}"
    );

    let clip = export(
        "clip",
        AudioMix {
            levels: Vec::new(),
            mix_only: true,
        },
    );
    assert_eq!(clip.len(), 1, "a clip is the mix alone: {clip:?}");
    assert!(loud(clip[0].0) && loud(clip[0].1));
    let _ = fs::remove_dir_all(&directory);
}

/// Every audio stream of an MP4, decoded to 48 kHz stereo float, as the RMS of
/// each `window_100ns` slice of file time (`NaN` where a stream has no sample
/// at all). Streams in the order the export path sees them.
fn audio_rms_windows(path: &Path, window_100ns: i64) -> Vec<Vec<f32>> {
    use windows::Win32::Media::MediaFoundation::*;
    let url = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
    let streams = unsafe {
        let probe = MFCreateSourceReaderFromURL(&url, None).expect("the file opens as media");
        super::super::probe::audio_stream_indices(&probe)
    };
    streams
        .into_iter()
        .map(|stream| unsafe {
            let reader = MFCreateSourceReaderFromURL(&url, None).unwrap();
            for other in 0..32u32 {
                if reader.GetNativeMediaType(other, 0).is_err() {
                    break;
                }
                let _ = reader.SetStreamSelection(other, other == stream);
            }
            let pcm = MFCreateMediaType().unwrap();
            pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).unwrap();
            pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float).unwrap();
            pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)
                .unwrap();
            pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2).unwrap();
            pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32).unwrap();
            reader.SetCurrentMediaType(stream, None, &pcm).unwrap();
            let mut sums: Vec<(f64, usize)> = Vec::new();
            loop {
                let (mut flags, mut timestamp, mut sample) = (0, 0, None);
                reader
                    .ReadSample(
                        stream,
                        0,
                        None,
                        Some(&mut flags),
                        Some(&mut timestamp),
                        Some(&mut sample),
                    )
                    .unwrap();
                if let Some(sample) = sample {
                    let buffer = sample.ConvertToContiguousBuffer().unwrap();
                    let (mut bytes, mut length) = (std::ptr::null_mut(), 0u32);
                    buffer.Lock(&mut bytes, None, Some(&mut length)).unwrap();
                    let floats =
                        std::slice::from_raw_parts(bytes.cast::<f32>(), length as usize / 4);
                    let slot = usize::try_from(timestamp / window_100ns).unwrap_or(0);
                    if sums.len() <= slot {
                        sums.resize(slot + 1, (0.0, 0));
                    }
                    for value in floats {
                        sums[slot].0 += f64::from(value * value);
                    }
                    sums[slot].1 += floats.len();
                    let _ = buffer.Unlock();
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
            sums.into_iter()
                .map(|(sum, n)| match n {
                    0 => f32::NAN,
                    n => (sum / n as f64).sqrt() as f32,
                })
                .collect()
        })
        .collect()
}

/// A probe, not a check (t261003-b713, reusable): reads a real session (`.lvb`)
/// named by `LVB_PROBE_SESSION`, prints each segment's audio track count, then
/// exports the whole session through the production writer twice -- as an
/// export (mix + every original track) and as a clip (the mix alone) -- and
/// prints every output stream's RMS per 5 s. `LVB_PROBE_LEVELS` (`100,0m,..`:
/// percent per track, `m` = muted) sets the mix levels. Never touches the
/// session: segments are copied out to a temp folder first.
///
/// ```powershell
/// $env:LVB_PROBE_SESSION = "<root>\<id>.lvb"
/// cargo test --lib probe_session_tracks -- --ignored --nocapture
/// ```
#[test]
#[ignore = "probe: needs LVB_PROBE_SESSION pointing at a recorded session"]
fn probe_session_tracks() {
    // `LVB_PROBE_MP4` instead: just print that file's streams (a clip, an export).
    if let Ok(file) = std::env::var("LVB_PROBE_MP4") {
        let _runtime = encoder::MfRuntime::start().unwrap();
        for (stream, windows) in audio_rms_windows(Path::new(&file), 50_000_000)
            .iter()
            .enumerate()
        {
            let line = windows
                .iter()
                .map(|rms| format!("{rms:.5}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("PROBE file stream {stream} rms/5s: {line}");
        }
        return;
    }
    let session = std::path::PathBuf::from(
        std::env::var("LVB_PROBE_SESSION").expect("LVB_PROBE_SESSION = a .lvb path"),
    );
    let levels = std::env::var("LVB_PROBE_LEVELS")
        .unwrap_or_default()
        .split(',')
        .filter(|item| !item.is_empty())
        .map(|item| TrackLevel {
            volume_percent: item.trim_end_matches('m').parse().unwrap_or(100),
            muted: item.ends_with('m'),
        })
        .collect::<Vec<_>>();
    let directory = std::env::temp_dir().join(format!("livia-probe-{}", std::process::id()));
    let _ = fs::remove_dir_all(&directory);
    fs::create_dir_all(&directory).unwrap();
    let _runtime = encoder::MfRuntime::start().unwrap();
    let mut reader =
        ring_buffer::container::ContainerReader::open_for_reading(&session).expect("session");
    let snapshot = reader.snapshot().clone();
    println!("PROBE tracks named: {:?}", snapshot.audio_tracks);
    let mut segments = Vec::new();
    for entry in &snapshot.segments {
        let path = directory.join(format!("{}.mp4", entry.index));
        fs::write(&path, reader.read_segment(entry.index).unwrap()).unwrap();
        segments.push(ring_buffer::SegmentRecord {
            index: entry.index,
            bytes: entry.data.body.len,
            path,
            start_100ns: entry.start_100ns,
            end_100ns: entry.end_100ns,
            thumbnail_path: None,
            audio_offsets_100ns: entry.audio_offsets_100ns.clone(),
        });
    }
    let origin = segments[0].start_100ns;
    let counts = segments
        .iter()
        .map(|segment| {
            format!(
                "{:.0}s:{}",
                (segment.start_100ns - origin) as f64 / 1e7,
                segment.audio_offsets_100ns.len()
            )
        })
        .collect::<Vec<_>>();
    println!(
        "PROBE per-segment track count (start:count): {}",
        counts.join(" ")
    );
    let part = ExportPart {
        start_100ns: origin,
        end_100ns: segments.last().unwrap().end_100ns,
        segments,
    };
    for (name, mix_only) in [("export", false), ("clip", true)] {
        let partial = directory.join(format!("{name}.partial"));
        let file = directory.join(format!("{name}.mp4"));
        let audio_mix = AudioMix {
            levels: levels.clone(),
            mix_only,
        };
        export_part_mixed(&part, &partial, &audio_mix).expect("the session exports");
        fs::rename(&partial, &file).unwrap();
        for (stream, windows) in audio_rms_windows(&file, 50_000_000).iter().enumerate() {
            let line = windows
                .iter()
                .map(|rms| format!("{rms:.5}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("PROBE {name} stream {stream} rms/5s: {line}");
        }
    }
    let _ = fs::remove_dir_all(&directory);
}
