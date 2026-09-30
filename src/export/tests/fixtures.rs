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
/// tracks, in the same order.
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
        assert_eq!(
            streams.len(),
            TRACKS,
            "the export must carry every audio track the segment had"
        );
        for stream in streams {
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
