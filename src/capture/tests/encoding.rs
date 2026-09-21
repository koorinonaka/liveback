//! GPU NV12 conversion and hardware H.264 encode/finalize tests.
use super::transform::create_test_source_with_pixels;
use super::*;

#[test]
fn gpu_nv12_converter_allocates_video_encoder_surface() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, _context, _) =
        create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let encoder = crate::encoder::HardwareVideoEncoder::create(
        &crate::encoder::EncoderConfig {
            output_dir: std::path::PathBuf::from("buffer"),
            output_size: size,
            frame_rate: 60,
        },
        &device,
    )
    .expect("hardware H.264 encoder should initialize");
    let sample = encoder
        .allocate_input_sample()
        .expect("allocator must provide an NV12 sample");
    let texture = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample)
        .expect("allocator sample must expose its D3D11 texture");
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe {
        texture.GetDesc(&mut desc);
    }
    assert_eq!(
        desc.Format,
        windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_NV12
    );
    assert_ne!(
        desc.BindFlags & windows::Win32::Graphics::Direct3D11::D3D11_BIND_VIDEO_ENCODER.0 as u32,
        0
    );
}

#[test]
fn verify_nv12_conversion_uses_studio_range_bt709() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("BGRA pipeline should initialize");
    let converter = GpuNv12Converter::new(&device, &context, size)
        .expect("GPU NV12 converter should initialize");
    let encoder = crate::encoder::HardwareVideoEncoder::create(
        &crate::encoder::EncoderConfig {
            output_dir: std::path::PathBuf::from("buffer"),
            output_size: size,
            frame_rate: 60,
        },
        &device,
    )
    .expect("hardware H.264 encoder should initialize");

    // Input/output sizes are identical so aspect_fit does not scale, ruling out
    // sampler interpolation as a source of luma drift.
    for (value, expected_luma, label) in [(0xFFu8, 235i32, "white"), (0x00u8, 16i32, "black")] {
        let source = create_test_source_with_pixels(
            &device,
            size,
            &vec![value; size.width as usize * size.height as usize * 4],
        )
        .expect("solid-color BGRA source should initialize");
        let output = pipeline
            .transform(&source, size, Inset::default(), false)
            .expect("SDR transform should draw");
        let sample = encoder
            .allocate_input_sample()
            .expect("allocator must provide an NV12 sample");
        let nv12 = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample)
            .expect("allocator sample must expose its D3D11 texture");
        converter
            .convert_into(&output, &nv12)
            .expect("BGRA to NV12 conversion should succeed");
        let luma = read_nv12_luma_pixel(
            &device,
            &context,
            &nv12,
            size.width as u32 / 2,
            size.height as u32 / 2,
        )
        .expect("center luma readback should succeed");
        let delta = i32::from(luma) - expected_luma;
        assert!(
            delta.abs() <= 2,
            "{label} input {value:#04x} must map to studio-range luma ~{expected_luma}, \
             got {luma} (delta {delta})"
        );
    }
}

/// Task066 black-frame regression, made checkable offline (no in-app
/// hover needed): the old client-side thumbnail path silently produced a
/// transparent-then-black canvas whenever `drawImage` ran on a video that
/// wasn't decode-ready yet (Context: `HAVE_NOTHING`/`HAVE_METADATA`).
/// `TransformPipeline::transform`'s render target is cleared to black
/// (`ClearRenderTargetView`) before the actual frame is drawn on top, so
/// the equivalent failure mode here — the draw silently not happening —
/// would produce the exact same symptom: a black thumbnail. Drives a
/// bright solid-color frame through the real transform → BGRA readback →
/// shared `bgra_to_resized_jpeg` helper → JPEG decode, and asserts the
/// result is nowhere near black.
#[test]
fn segment_thumbnail_from_transform_output_is_not_black() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 320,
        height: 180,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("BGRA pipeline should initialize");
    let source = create_test_source_with_pixels(
        &device,
        size,
        &vec![0xFFu8; size.width as usize * size.height as usize * 4],
    )
    .expect("solid white BGRA source should initialize");
    let output = pipeline
        .transform(&source, size, Inset::default(), false)
        .expect("SDR transform should draw");
    let (width, height, bgra) = gpu::read_bgra_texture(&device, &context, &output)
        .expect("BGRA texture readback should succeed");
    let jpeg = targets::bgra_to_resized_jpeg(width, height, &bgra, 160, 90)
        .expect("thumbnail JPEG encode should succeed");
    let decoded = image::load_from_memory(&jpeg)
        .expect("encoded thumbnail must be a valid JPEG")
        .to_rgba8();
    assert_eq!(decoded.dimensions(), (160, 90));
    let pixel_count = decoded.pixels().len() as u64;
    let total_luma: u64 = decoded
        .pixels()
        .map(|pixel| u64::from(pixel[0]) + u64::from(pixel[1]) + u64::from(pixel[2]))
        .sum();
    let average_luma_per_channel = total_luma / (pixel_count * 3);
    assert!(
        average_luma_per_channel > 150,
        "a bright input must not produce a near-black thumbnail (black-frame \
         regression): average per-channel value {average_luma_per_channel}/255"
    );
}

/// `write_segment_thumbnail` must never leave a reader-visible half-written
/// file: it writes to `.partial.jpg` then `fs::rename`s to the final
/// `.jpg`, mirroring the segment mp4 writer's own atomicity (see its doc
/// comment). Asserts both that the partial file never survives a
/// successful write and that the final file is a decodable JPEG.
#[test]
fn write_segment_thumbnail_leaves_only_the_final_jpg_behind() {
    let dir =
        std::env::temp_dir().join(format!("livia-thumbnail-atomicity-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let (width, height) = (4u32, 4u32);
    let bgra = vec![0xFFu8; (width * height * 4) as usize];
    write_segment_thumbnail(&dir, 3, width, height, &bgra).unwrap();

    let (partial, final_path) = ring_buffer::store::thumbnail_paths(&dir, 3);
    assert!(
        !partial.exists(),
        "partial file must not survive a successful write"
    );
    assert!(final_path.is_file());
    image::load_from_memory(&std::fs::read(&final_path).unwrap())
        .expect("final thumbnail file must be a valid JPEG");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Copies `texture` to a CPU-readable staging texture and reads back a single
/// luma byte of an NV12 texture at (`x`, `y`); NV12's Y plane occupies the
/// texture's first `Height` rows at one byte per pixel.
fn read_nv12_luma_pixel(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    texture: &ID3D11Texture2D,
    x: u32,
    y: u32,
) -> windows::core::Result<u8> {
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    unsafe { texture.GetDesc(&mut desc) };
    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..desc
    };
    let mut staging = None;
    unsafe {
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
    }
    let staging = staging.ok_or_else(windows::core::Error::from_win32)?;
    unsafe {
        context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let value = *mapped
            .pData
            .cast::<u8>()
            .add((y * mapped.RowPitch) as usize + x as usize);
        context.Unmap(&staging, 0);
        Ok(value)
    }
}

#[test]
fn gpu_nv12_surface_is_accepted_by_hardware_h264_encoder() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("BGRA source surface should initialize");
    let converter = GpuNv12Converter::new(&device, &context, size)
        .expect("GPU NV12 converter should initialize");
    let mut encoder = crate::encoder::HardwareVideoEncoder::create(
        &crate::encoder::EncoderConfig {
            output_dir: std::path::PathBuf::from("buffer"),
            output_size: size,
            frame_rate: 60,
        },
        &device,
    )
    .expect("hardware H.264 encoder should initialize");
    let mut produced = Vec::new();
    for index in 0..120 {
        produced.extend(encoder.poll().expect("hardware H.264 events should poll"));
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().expect("allocator sample");
            let texture = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample)
                .expect("allocator texture");
            converter
                .convert_into(&pipeline.output, &texture)
                .expect("BGRA to allocator NV12 should succeed");
            encoder
                .process_input_sample(&sample, index * 166_667)
                .expect("input only after credit");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    produced.extend(encoder.poll().expect("hardware H.264 events should poll"));
    assert!(
        !produced.is_empty(),
        "hardware H.264 encoder should emit an encoded sample"
    );
    encoder
        .begin_drain()
        .expect("drain command should be accepted");
    for _ in 0..100 {
        encoder.poll().expect("drain events should poll");
        if encoder.drain_complete() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    assert!(
        encoder.drain_complete(),
        "async MFT must signal METransformDrainComplete"
    );
}

#[test]
fn hardware_mft_h264_samples_finalize_to_mp4_atomically() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .unwrap();
    let converter = GpuNv12Converter::new(&device, &context, size).unwrap();
    let directory = std::env::temp_dir().join(format!("livia-task005-{}", std::process::id()));
    let config = crate::encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: size,
        frame_rate: 60,
    };
    let mut encoder = crate::encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let aac = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = crate::encoder::SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    let mut encoded_count = 0;
    let mut finalized = Vec::new();
    let mut pending_aac = crate::encoder::PendingAac::default();
    for index in 0..180 {
        for sample in encoder.poll().unwrap() {
            finalized.extend(muxer.push_video_sample(&sample).unwrap());
            encoded_count += 1;
        }
        let pcm = vec![0.0_f32; 1024 * 2];
        for sample in aac.encode_f32(&pcm, index * 166_667).unwrap() {
            finalized.extend(pending_aac.offer(&mut muxer, &sample));
        }
        if encoder.has_input_credit() {
            if muxer.should_force_keyframe(index * 166_667) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            let texture =
                crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample).unwrap();
            converter.convert_into(&pipeline.output, &texture).unwrap();
            encoder
                .process_input_sample(&sample, index * 166_667)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for sample in encoder.poll().unwrap() {
        finalized.extend(muxer.push_video_sample(&sample).unwrap());
        encoded_count += 1;
    }
    finalized.extend(pending_aac.flush(&mut muxer));
    pending_aac.assert_drained();
    assert_ne!(
        encoded_count, 0,
        "hardware MFT must emit compressed H.264 samples"
    );
    finalized.extend(muxer.finalize().unwrap());
    assert!(
        !finalized.is_empty(),
        "at least one MP4 segment must finalize"
    );
    let final_path = directory.join("segment-00000000000000000000.mp4");
    assert!(final_path.is_file());
    let inspection = crate::encoder::inspect_finalized_mp4(&final_path).unwrap();
    assert!(inspection.first_sample_clean_point);
    assert!(inspection.duration_100ns > 0);
    assert!(
        inspection.has_aac_audio,
        "MP4 must contain an AAC audio stream"
    );
    assert_eq!(inspection.audio_sample_rate, 48_000);
    assert_eq!(inspection.audio_channels, 2);
    assert!((160_000..=224_000).contains(&inspection.audio_bitrate));
    assert!(!directory
        .join("segment-00000000000000000000.partial.mp4")
        .exists());
    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(directory);
    } else {
        eprintln!("kept acceptance output: {}", directory.display());
    }
}

/// Task085: reproduces the real defect through the actual production
/// pipeline (`SegmentMuxer` -> `Mp4SegmentWriter::finalize` ->
/// `encoder::mp4_boxes::finalize_fragmented_mp4`, the exact single-pass call
/// sequence a real recording goes through) rather than a hand-built box
/// fixture: finalizes a segment that received real H.264 video samples but
/// zero AAC audio samples at all — precisely what happens when a segment is
/// shorter than one AAC frame (~21.33ms) around a genuine WGC "映像フレーム
/// 中断" capture-interruption gap — and confirms the resulting file both
/// declares no audio track and is accepted by `MFCreateSourceReaderFromURL`
/// (which rejected the pre-fix shape with `MF_E_UNSUPPORTED_BYTESTREAM_TYPE`
/// / 0xC00D36C4, confirmed against real captured segments during this
/// task's investigation).
#[test]
fn task085_finalizes_a_video_only_segment_that_export_can_still_open() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .unwrap();
    let converter = GpuNv12Converter::new(&device, &context, size).unwrap();
    let directory = std::env::temp_dir().join(format!("livia-task085-{}", std::process::id()));
    let config = crate::encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: size,
        frame_rate: 60,
    };
    let mut encoder = crate::encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let aac = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = crate::encoder::SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    let mut encoded_count = 0;
    // No `push_aac_sample` call anywhere in this test: the whole point is a
    // segment whose audio track never receives a single sample, matching
    // the real degenerate-segment shape exactly.
    for index in 0..30 {
        for sample in encoder.poll().unwrap() {
            muxer.push_video_sample(&sample).unwrap();
            encoded_count += 1;
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().unwrap();
            let texture =
                crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample).unwrap();
            converter.convert_into(&pipeline.output, &texture).unwrap();
            encoder
                .process_input_sample(&sample, index * 166_667)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for sample in encoder.poll().unwrap() {
        muxer.push_video_sample(&sample).unwrap();
        encoded_count += 1;
    }
    assert_ne!(
        encoded_count, 0,
        "hardware MFT must emit at least one compressed H.264 sample"
    );

    let events = muxer.finalize().unwrap();
    let finalized_path = events
        .into_iter()
        .find_map(|event| match event {
            crate::encoder::EncoderEvent::Finalized(metadata) => Some(metadata.path),
            _ => None,
        })
        .expect("the video-only segment must still finalize, not silently vanish");
    assert!(finalized_path.is_file());

    // `inspect_finalized_mp4` itself calls `MFCreateSourceReaderFromURL` on
    // this path (the same API export's `open_reader` calls, and the one
    // that rejected the pre-fix shape with `MF_E_UNSUPPORTED_BYTESTREAM_TYPE`
    // against real captured segments) — an `Ok` result here is direct proof
    // Media Foundation accepts this video-only file, not merely that this
    // app's own box-parsing thinks it looks right.
    let inspection = crate::encoder::inspect_finalized_mp4(&finalized_path)
        .expect("export's MFCreateSourceReaderFromURL must accept a video-only degenerate segment");
    assert!(
        !inspection.has_aac_audio,
        "a track that never received a single sample must be dropped entirely, not left as an empty traf"
    );
    assert!(
        inspection.duration_100ns > 0,
        "the real video samples must survive untouched"
    );

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn hardware_mft_h264_output_signals_studio_range_bt709_and_avoids_clipping() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .unwrap();
    let converter = GpuNv12Converter::new(&device, &context, size).unwrap();
    let directory = std::env::temp_dir().join(format!("livia-task033-{}", std::process::id()));
    let config = crate::encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: size,
        frame_rate: 60,
    };
    let mut encoder = crate::encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let mut muxer =
        crate::encoder::SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap());
    // A full-white source is the worst case for range clipping: if the encoded
    // stream were still full-range (pre-Task033 behavior), luma would sit at
    // 255; studio-range BT.709 must place it at 235.
    let source = create_test_source_with_pixels(
        &device,
        size,
        &vec![0xFFu8; size.width as usize * size.height as usize * 4],
    )
    .expect("solid white BGRA source should initialize");
    let output = pipeline
        .transform(&source, size, Inset::default(), false)
        .expect("SDR transform should draw");
    let mut encoded_count = 0;
    let mut finalized = Vec::new();
    for index in 0..180 {
        for sample in encoder.poll().unwrap() {
            finalized.extend(muxer.push_video_sample(&sample).unwrap());
            encoded_count += 1;
        }
        if encoder.has_input_credit() {
            if muxer.should_force_keyframe(index * 166_667) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            let texture =
                crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample).unwrap();
            converter.convert_into(&output, &texture).unwrap();
            encoder
                .process_input_sample(&sample, index * 166_667)
                .unwrap();
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    for sample in encoder.poll().unwrap() {
        finalized.extend(muxer.push_video_sample(&sample).unwrap());
        encoded_count += 1;
    }
    assert_ne!(
        encoded_count, 0,
        "hardware MFT must emit compressed H.264 samples"
    );
    finalized.extend(muxer.finalize().unwrap());
    assert!(
        !finalized.is_empty(),
        "at least one MP4 segment must finalize"
    );
    let final_path = directory.join("segment-00000000000000000000.mp4");
    assert!(final_path.is_file());

    let ffprobe_output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_range,color_space,color_transfer,color_primaries,profile,has_b_frames",
            "-of",
            "default=noprint_wrappers=1",
        ])
        .arg(&final_path)
        .output()
        .expect("ffprobe should be runnable (see Preconditions: Gyan.FFmpeg winget package)");
    assert!(
        ffprobe_output.status.success(),
        "ffprobe failed: {}",
        String::from_utf8_lossy(&ffprobe_output.stderr)
    );
    let stdout = String::from_utf8_lossy(&ffprobe_output.stdout);
    for expected in [
        "color_range=tv",
        "color_space=bt709",
        "color_transfer=bt709",
        "color_primaries=bt709",
        // task1000. `profile=High` is the CABAC/8x8 switch actually reaching
        // the SPS rather than merely being asked for.
        "profile=High",
        // The other half of that change, and the load-bearing one:
        // `Mp4SegmentWriter::write_video_sample_at` places samples by
        // presentation time and writes no `ctts`, so a stream with B-frames
        // would be muxed out of order. High permits them; the low-latency mode
        // in `configure_codec_api` is what keeps them out, and this is the
        // assertion that notices if that ever stops being true.
        "has_b_frames=0",
    ] {
        assert!(
            stdout.contains(expected),
            "ffprobe output missing {expected}: {stdout}"
        );
    }

    // YMAX comes from ffprobe, not ffmpeg (task510). The old reading ran
    // `ffmpeg -vf "signalstats,metadata=print:file=-"`, which **crashes**
    // (0xC0000025) on this machine once the clip is more than a few dozen
    // frames long. Since NVENC's output differs run to run, it crashed on
    // roughly three runs in four -- and because nothing checked the exit
    // status, the empty stdout that followed was reported as "expected at
    // least one YMAX sample", i.e. as a defect in the encoder. The encoder was
    // never wrong: read the same statistic through ffprobe and it is 235.
    //
    // `movie=` parses `:` as its own argument separator, so an absolute
    // Windows path cannot be passed inline. Running in the file's directory
    // and naming the bare file sidesteps the quoting problem rather than
    // trying to win it.
    let file_name = final_path
        .file_name()
        .expect("finalized segment has a file name");
    let signalstats_output = std::process::Command::new("ffprobe")
        .current_dir(&directory)
        .args(["-v", "error", "-f", "lavfi", "-i"])
        .arg(format!("movie={},signalstats", file_name.to_string_lossy()))
        .args([
            "-show_entries",
            "frame_tags=lavfi.signalstats.YMAX",
            "-of",
            "default=nw=1:nk=1",
        ])
        .output()
        .expect("ffprobe should be runnable (see Preconditions: Gyan.FFmpeg winget package)");
    // Both of these were missing before, and that is the whole reason a
    // crashing external tool read as a product defect: a failed command must
    // fail the test *as a failed command*.
    assert!(
        signalstats_output.status.success(),
        "ffprobe failed to measure YMAX ({}): {}",
        signalstats_output.status,
        String::from_utf8_lossy(&signalstats_output.stderr)
    );
    let signalstats_text = String::from_utf8_lossy(&signalstats_output.stdout);
    let ymax_values: Vec<i32> = signalstats_text
        .lines()
        .filter_map(|value| value.trim().parse::<i32>().ok())
        .collect();
    assert!(
        !ymax_values.is_empty(),
        "ffprobe exited cleanly but reported no YMAX samples, so the measurement \
         did not happen -- this is a measurement failure, not an encoder defect. \
         stdout: {signalstats_text} stderr: {}",
        String::from_utf8_lossy(&signalstats_output.stderr)
    );
    let max_ymax = *ymax_values.iter().max().unwrap();
    assert!(
        max_ymax <= 240,
        "studio-range luma must not clip to full-range white (255); got YMAX={max_ymax}"
    );

    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(directory);
    } else {
        eprintln!("kept acceptance output: {}", directory.display());
    }
}

#[test]
#[ignore = "RTX 4070 hardware acceptance: runs 12 synthetic GPU minutes across six cases"]
fn rtx_hardware_encoder_two_minute_acceptance_matrix() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let cases = [
        (1920, 1080, 30),
        (1920, 1080, 60),
        (2560, 1440, 30),
        (2560, 1440, 60),
        (3840, 2160, 30),
        (3840, 2160, 60),
    ];
    let selected = std::env::var("LIVIA_TASK005_MATRIX_CASE").ok();
    for (case_index, (width, height, fps)) in cases.into_iter().enumerate() {
        if selected
            .as_deref()
            .is_some_and(|value| value != case_index.to_string())
        {
            continue;
        }
        run_two_minute_h264_case(CaptureSize { width, height }, fps);
    }
}

fn run_two_minute_h264_case(size: CaptureSize, fps: u8) {
    let (device, context, _) = create_d3d_device().expect("D3D11 video device");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .unwrap();
    let converter = GpuNv12Converter::new(&device, &context, size).unwrap();
    let directory = std::env::temp_dir().join(format!(
        "livia-task005-{fps}-{}x{}-{}",
        size.width,
        size.height,
        std::process::id()
    ));
    let config = crate::encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: size,
        frame_rate: fps,
    };
    let mut encoder = crate::encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let mut muxer =
        crate::encoder::SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap());
    let duration = 10_000_000 / i64::from(fps);
    let target_frames = usize::from(fps) * 120;
    let mut submitted = 0usize;
    let mut finalized = Vec::new();
    while submitted < target_frames {
        finalized.extend(encoder.poll_to_muxer(&mut muxer).unwrap());
        if encoder.has_input_credit() {
            let timestamp = submitted as i64 * duration;
            if muxer.should_force_keyframe(timestamp) {
                encoder.request_next_keyframe().unwrap();
            }
            let sample = encoder.allocate_input_sample().unwrap();
            let nv12 = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample).unwrap();
            converter.convert_into(&pipeline.output, &nv12).unwrap();
            encoder.process_input_sample(&sample, timestamp).unwrap();
            submitted += 1;
        } else {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
    encoder.begin_drain().unwrap();
    for _ in 0..5_000 {
        finalized.extend(encoder.poll_to_muxer(&mut muxer).unwrap());
        if encoder.drain_complete() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(encoder.drain_complete(), "{size:?} {fps}fps drain complete");
    finalized.extend(muxer.finalize().unwrap());
    assert!(
        finalized.len() >= 59,
        "{size:?} {fps}fps needs ~60 independent segments"
    );
    for event in finalized {
        let crate::encoder::EncoderEvent::Finalized(metadata) = event else {
            continue;
        };
        let inspection = crate::encoder::inspect_finalized_mp4(&metadata.path).unwrap();
        assert!(inspection.first_sample_clean_point);
        assert!(inspection.duration_100ns > 0);
    }
    if std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_none() {
        let _ = std::fs::remove_dir_all(directory);
    } else {
        eprintln!("kept acceptance output: {}", directory.display());
    }
}

/// Task1260: with several audio tracks queued, the sink is fed the oldest
/// sample across *all* of them.
///
/// This is a liveness property, not a style choice. The fMP4 sink interleaves
/// every stream and refuses one that runs ahead of the others (task410/task610),
/// so draining track 0 to exhaustion before touching track 1 is the same stall
/// that killed recordings before the queue existed.
#[test]
fn the_oldest_queued_audio_sample_goes_first_whichever_track_it_is_on() {
    use crate::capture::worker::next_pending_track;

    // Track 1 is behind, so it goes first even though track 0 has samples.
    assert_eq!(
        next_pending_track(&[Some(200), Some(100), Some(300)], &[false; 3]),
        Some(1)
    );
    // A blocked track is skipped, and the next-oldest unblocked one wins.
    assert_eq!(
        next_pending_track(&[Some(200), Some(100), Some(300)], &[false, true, false]),
        Some(0)
    );
    // Empty queues are not "oldest": they are simply not candidates.
    assert_eq!(
        next_pending_track(&[None, Some(500), None], &[false; 3]),
        Some(1)
    );
    // Everything blocked, or everything empty, ends the round.
    assert_eq!(next_pending_track(&[Some(1), Some(2)], &[true, true]), None);
    assert_eq!(next_pending_track(&[None, None], &[false, false]), None);
    // Ties go to the lower track, which keeps the order deterministic.
    assert_eq!(
        next_pending_track(&[Some(100), Some(100)], &[false, false]),
        Some(0)
    );
}

/// Task1260: an extra audio track is resolved by executable name at recording
/// start, and an executable that is not running gets no track at all.
#[test]
fn an_audio_track_executable_resolves_to_a_running_process_or_to_nothing() {
    let own = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .expect("this test binary has a name");
    assert_eq!(
        crate::capture::targets::first_process_id_for_executable(&own),
        Some(std::process::id()),
        "the test process should resolve to itself"
    );
    // Windows filenames are case-insensitive and so is the lookup: a list the
    // user typed by hand must not miss a process over capitalisation.
    assert_eq!(
        crate::capture::targets::first_process_id_for_executable(&own.to_uppercase()),
        Some(std::process::id())
    );
    assert_eq!(
        crate::capture::targets::first_process_id_for_executable("definitely-not-running-1260.exe"),
        None
    );
    assert_eq!(
        crate::capture::targets::first_process_id_for_executable("  "),
        None
    );
}

/// Task t260911-e7f3: the capture death of 2026-09-11 17:16:34 JST.
///
/// The shape of that incident, reproduced through the production pipeline: the
/// video encoder's output stops reaching the muxer for a couple of seconds, a
/// segment boundary falls *inside* the backlog that builds up, and the moment
/// the stall clears the whole backlog is poured into the **new** segment in one
/// `poll_to_muxer` pass -- with no chance for the AAC queued behind it to be
/// offered, because `flush_pending_aac` only ever runs after that pass returns
/// (`capture/worker.rs`, "After the hold feed and its drain, never before").
///
/// The fMP4 sink will not take video that far ahead of an audio track that has
/// received nothing at all, so it refuses; `release_held_video` used to turn
/// that refusal into `EncoderStartError{kind: Device}` once the 250ms budget
/// ran out, and the recording died with `video_accepted=18`,
/// `first_audio=[None]`.
///
/// The stall itself is modelled by holding the encoder's output in a `Vec`
/// instead of pushing it: what matters is the backlog standing at the muxer's
/// door with a boundary in it, not why the encoder went quiet.
///
/// Deliberately written against the shipped API only, so the same test body
/// runs on the tree before the fix: measured 2026-09-12, it dies there on
/// backlog sample 19.
#[test]
fn t260911_e7f3_a_video_backlog_across_a_boundary_does_not_kill_the_recording() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .unwrap();
    let converter = GpuNv12Converter::new(&device, &context, size).unwrap();
    let directory = std::env::temp_dir().join(format!("livia-t260911e7f3-{}", std::process::id()));
    let config = crate::encoder::EncoderConfig {
        output_dir: directory.clone(),
        output_size: size,
        frame_rate: 60,
    };
    let mut encoder = crate::encoder::HardwareVideoEncoder::create(&config, &device).unwrap();
    let aac = crate::capture::audio::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = crate::encoder::SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    let mut pending_aac = crate::encoder::PendingAac::default();
    let mut finalized = Vec::new();
    let silence = vec![0.0_f32; 1024 * 2];
    let mut pushed_video = 0u64;

    // 1.5s of a healthy recording: segment 0 opens and both tracks are moving.
    const HEALTHY_FRAMES: i64 = 90;
    // 3s of backlog on top of it. `SEGMENT_DURATION_100NS` is 2s, so the
    // boundary is requested at frame ~120 -- inside the stall -- and its clean
    // point is one of the samples held back.
    const STALLED_FRAMES: i64 = 180;

    let feed = |encoder: &mut crate::encoder::HardwareVideoEncoder,
                muxer: &mut crate::encoder::SegmentMuxer,
                index: i64| {
        if !encoder.has_input_credit() {
            return;
        }
        let timestamp = index * 166_667;
        if muxer.should_force_keyframe(timestamp) {
            encoder.request_next_keyframe().unwrap();
        }
        let sample = encoder.allocate_input_sample().unwrap();
        let texture = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample).unwrap();
        converter.convert_into(&pipeline.output, &texture).unwrap();
        encoder.process_input_sample(&sample, timestamp).unwrap();
    };

    for index in 0..HEALTHY_FRAMES {
        for sample in encoder.poll().unwrap() {
            finalized.extend(
                muxer
                    .push_video_sample(&sample)
                    .expect("healthy video push"),
            );
            pushed_video += 1;
        }
        for sample in aac.encode_f32(&silence, index * 166_667).unwrap() {
            finalized.extend(pending_aac.offer(&mut muxer, &sample));
        }
        feed(&mut encoder, &mut muxer, index);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    // The stall. Output is pulled out of the MFT (so the encoder keeps running)
    // but goes nowhere near the muxer, and the AAC that keeps arriving is never
    // offered -- exactly the state the capture thread is in while it cannot
    // drain.
    let mut video_backlog = Vec::new();
    let mut audio_backlog = std::collections::VecDeque::new();
    for index in HEALTHY_FRAMES..HEALTHY_FRAMES + STALLED_FRAMES {
        video_backlog.extend(encoder.poll().unwrap());
        for sample in aac.encode_f32(&silence, index * 166_667).unwrap() {
            audio_backlog.push_back(sample);
        }
        feed(&mut encoder, &mut muxer, index);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    println!(
        "t260911-e7f3: backlog is {} video samples and {} AAC samples",
        video_backlog.len(),
        audio_backlog.len()
    );
    assert!(
        video_backlog.len() >= 100,
        "the backlog has to be big enough to outrun the sink's interleave window"
    );

    // The stall clears: one pass pours the whole backlog in, boundary and all,
    // with no opportunity to offer a single AAC sample in between.
    for (position, sample) in video_backlog.iter().enumerate() {
        finalized.extend(muxer.push_video_sample(sample).unwrap_or_else(|error| {
            panic!("backlog sample {position} killed the recording: {error:?}")
        }));
        pushed_video += 1;
    }
    // The whole point of the fixture: the sink really did say no, and the
    // recording is still alive to hear it. Without this the test could pass on
    // a machine whose sink never refuses and prove nothing at all.
    let refused_in_burst = muxer.pending_video_len();
    println!("t260911-e7f3: the sink refused {refused_in_burst} of the backlog");
    assert!(
        refused_in_burst > 0,
        "the backlog never outran the sink's interleave window, so nothing was tested"
    );

    // Whatever the sink would not take is still owed to it, and the audio that
    // was waiting is what unblocks it. `poll_to_muxer` is what the capture loop
    // calls every turn and `flush_pending_aac` is what runs after it -- the
    // same alternation, here on what the stall left behind.
    for _ in 0..2_000 {
        finalized.extend(muxer.drain_pending_video().expect("video retry"));
        for sample in encoder.poll().unwrap() {
            finalized.extend(muxer.push_video_sample(&sample).expect("video retry"));
            pushed_video += 1;
        }
        match audio_backlog.pop_front() {
            Some(sample) => finalized.extend(pending_aac.offer(&mut muxer, &sample)),
            None => finalized.extend(pending_aac.flush(&mut muxer)),
        }
        if audio_backlog.is_empty() && muxer.pending_video_len() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    encoder.begin_drain().unwrap();
    for _ in 0..2_000 {
        finalized.extend(muxer.drain_pending_video().expect("drain"));
        for sample in encoder.poll().unwrap() {
            finalized.extend(muxer.push_video_sample(&sample).expect("drain"));
            pushed_video += 1;
        }
        finalized.extend(pending_aac.flush(&mut muxer));
        if encoder.drain_complete() && muxer.pending_video_len() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        muxer.pending_video_len(),
        0,
        "the held samples were never re-offered successfully"
    );
    pending_aac.assert_drained();
    // `finalize` is the last chance the sink gets at anything it is still
    // refusing; every sample counted above has to be in the boxes below.
    finalized.extend(muxer.finalize().unwrap());

    let segments: Vec<_> = finalized
        .into_iter()
        .filter_map(|event| match event {
            crate::encoder::EncoderEvent::Finalized(metadata) => Some(metadata.path),
            _ => None,
        })
        .collect();
    println!("t260911-e7f3: {} segments finalized", segments.len());
    assert!(
        segments.len() >= 2,
        "the boundary inside the backlog has to produce a second segment"
    );
    // Surviving the refusal is only half of it: a sample the sink would not take
    // has to be offered again, not dropped. Every frame that went in comes back
    // out of the finalized boxes, or the fix has traded a dead recording for a
    // silently holed one.
    let mut video_samples = 0u64;
    for path in &segments {
        let inspection = crate::encoder::inspect_finalized_mp4(path).unwrap();
        assert!(inspection.duration_100ns > 0, "{path:?} is empty");
        for track in crate::encoder::dump_segment_track_summary(path).expect("track summary") {
            if !track.is_audio {
                video_samples += track.sample_count;
            }
        }
    }
    println!("t260911-e7f3: {video_samples} video samples in the segments, {pushed_video} pushed");
    assert_eq!(
        video_samples, pushed_video,
        "the backlog must reach the segments, not be dropped on the refusal"
    );
    let _ = std::fs::remove_dir_all(&directory);
}

/// task4280: `convert_into` used to leak its `ID3D11VideoProcessorInputView`
/// every frame (`D3D11_VIDEO_PROCESSOR_STREAM::pInputSurface` is
/// `ManuallyDrop`). Each leaked view held a reference to the device, so a
/// stopped recording's device -- and every texture and pool on it -- was never
/// destroyed: +21.3 MB of dedicated GPU memory per recording on the dev machine.
/// The device's refcount must come back to where it was after any number of
/// conversions.
#[test]
fn nv12_conversion_gives_back_its_device_references() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let size = CaptureSize {
        width: 320,
        height: 180,
    };
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("BGRA pipeline should initialize");
    let converter = GpuNv12Converter::new(&device, &context, size)
        .expect("GPU NV12 converter should initialize");
    let encoder = crate::encoder::HardwareVideoEncoder::create(
        &crate::encoder::EncoderConfig {
            output_dir: std::path::PathBuf::from("buffer"),
            output_size: size,
            frame_rate: 60,
        },
        &device,
    )
    .expect("hardware H.264 encoder should initialize");
    let source = create_test_source_with_pixels(
        &device,
        size,
        &vec![0x80; size.width as usize * size.height as usize * 4],
    )
    .expect("BGRA source should initialize");
    let output = pipeline
        .transform(&source, size, Inset::default(), false)
        .expect("SDR transform should draw");
    let sample = encoder
        .allocate_input_sample()
        .expect("allocator must provide an NV12 sample");
    let nv12 = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample)
        .expect("allocator sample must expose its D3D11 texture");
    // One warm-up conversion first, so anything the driver creates once and
    // keeps is inside the baseline rather than counted as a leak.
    converter
        .convert_into(&output, &nv12)
        .expect("BGRA to NV12 conversion should succeed");
    let before = unsafe { crate::capture::leak_probe::com_refcount(&device) };
    const CONVERSIONS: u32 = 20;
    for _ in 0..CONVERSIONS {
        converter
            .convert_into(&output, &nv12)
            .expect("BGRA to NV12 conversion should succeed");
    }
    let after = unsafe { crate::capture::leak_probe::com_refcount(&device) };
    assert_eq!(
        after, before,
        "{CONVERSIONS} conversions left the device refcount at {after} (was {before}): \
         a per-frame view is not being released"
    );
}

/// Seconds since the UNIX epoch with millis, for lining probe output up with
/// an external `measure-vram.ps1` log.
/// Binds a `DispatcherQueue` to the calling thread and hands back the controller,
/// which only has to stay alive -- the probe never calls it. `windows`'s own
/// `CreateDispatcherQueueController` sits behind the crate's `System` feature
/// (it returns the projected `DispatcherQueueController`), and this one export is
/// not worth turning that feature on for the whole app, so link it directly.
/// `DQTYPE_THREAD_CURRENT` is the combination `Direct3D11CaptureFramePool::Create`
/// wants; `DQTAT_COM_NONE` leaves the apartment alone (the probe is already MTA).
fn task4280_bind_dispatcher_queue() -> windows::core::Result<windows::core::IUnknown> {
    use windows::Win32::System::WinRT::{
        DispatcherQueueOptions, DQTAT_COM_NONE, DQTYPE_THREAD_CURRENT,
    };
    // `name` carries no `.dll`: with `kind = "raw-dylib"` rustc appends it, and
    // "coremessaging.dll.dll" makes the whole test binary die at load with
    // STATUS_DLL_NOT_FOUND (measured 2026-09-18 -- every test, not just this one).
    #[link(name = "coremessaging", kind = "raw-dylib")]
    extern "system" {
        fn CreateDispatcherQueueController(
            options: DispatcherQueueOptions,
            controller: *mut *mut core::ffi::c_void,
        ) -> windows::core::HRESULT;
    }
    let options = DispatcherQueueOptions {
        dwSize: std::mem::size_of::<DispatcherQueueOptions>() as u32,
        threadType: DQTYPE_THREAD_CURRENT,
        apartmentType: DQTAT_COM_NONE,
    };
    let mut raw = std::ptr::null_mut();
    unsafe { CreateDispatcherQueueController(options, &mut raw) }.ok()?;
    Ok(unsafe { <windows::core::IUnknown as windows::core::Interface>::from_raw(raw) })
}

fn task4280_stamp() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}.{:03}", now.as_secs(), now.subsec_millis())
}

/// task4280 residue probe, half 1: everything a recording builds on its own
/// D3D11 device *except* WGC -- device, transform pipeline, NV12 video
/// processor, hardware encoder -- at 1600x900, created, driven for 120 frames
/// and dropped, four times, with 15 s of rest after each drop. Run by hand
/// with `measure-vram.ps1` pointed at this test process: a step per round
/// means the residue is in here; flat means it is not.
#[test]
#[ignore = "task4280 probe: needs an external VRAM logger on this process"]
fn task4280_probe_gpu_pipeline_rounds() {
    let size = CaptureSize {
        width: 1600,
        height: 900,
    };
    eprintln!(
        "probe=gpu_pipeline pid={} start={}",
        std::process::id(),
        task4280_stamp()
    );
    std::thread::sleep(std::time::Duration::from_secs(12));
    for round in 1..=4 {
        {
            let (device, context, _) = create_d3d_device().expect("D3D11 device");
            let pipeline = TransformPipeline::new(
                device.clone(),
                context.clone(),
                size,
                gpu::HDR_WHITE_POINT_FLOOR,
            )
            .expect("pipeline");
            let converter = GpuNv12Converter::new(&device, &context, size).expect("converter");
            let encoder = crate::encoder::HardwareVideoEncoder::create(
                &crate::encoder::EncoderConfig {
                    output_dir: std::path::PathBuf::from("buffer"),
                    output_size: size,
                    frame_rate: 60,
                },
                &device,
            )
            .expect("encoder");
            let source = create_test_source_with_pixels(
                &device,
                size,
                &vec![0x80; size.width as usize * size.height as usize * 4],
            )
            .expect("source");
            for _ in 0..120 {
                let output = pipeline
                    .transform(&source, size, Inset::default(), false)
                    .expect("transform");
                let sample = encoder.allocate_input_sample().expect("sample");
                let nv12 = crate::encoder::HardwareVideoEncoder::input_sample_texture(&sample)
                    .expect("nv12");
                converter.convert_into(&output, &nv12).expect("convert");
            }
            eprintln!("round={round} built_and_used={}", task4280_stamp());
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        eprintln!("round={round} dropped={}", task4280_stamp());
        std::thread::sleep(std::time::Duration::from_secs(15));
    }
    eprintln!("probe=gpu_pipeline end={}", task4280_stamp());
}

/// task4280 residue probe, half 2: WGC alone -- a fresh device, capture item,
/// free-threaded frame pool (2 buffers, like the recorder) and session on the
/// window in `LIVEBACK_4280_HWND` (hex), frames pulled for 10 s, then closed
/// and dropped, four times with 15 s of rest. Same reading as half 1.
///
/// `LIVEBACK_4280_WGC_VARIANT` picks what one round keeps across rounds, so the
/// residue (measured 2026-09-13: +6.2 MB per round at 1600x900, about one BGRA
/// buffer of the two-buffer pool) can be pinned on a single object without
/// another build. An unknown value panics on purpose -- a typo that silently
/// measured `baseline` would read as "this variant changes nothing".
///
/// - `baseline`: what 2026-09-13 measured (`probe-half-wgc.csv`), everything fresh.
/// - `close_frames`: `Direct3D11CaptureFrame::Close()` before dropping each frame.
///   Neither this probe nor `frame_arrived_handler` in `worker.rs` ever closes a
///   frame, and the residue is one pool buffer per round, so this is the first
///   one to run; if it goes flat the app fix is the same one line.
/// - `cached_item`: one `GraphicsCaptureItem` for the window, reused by every round.
/// - `shared_device`: one D3D11/`IDirect3DDevice` pair, reused by every round.
/// - `dispatcher`: `Direct3D11CaptureFramePool::Create` instead of
///   `CreateFreeThreaded`. `Create` hands frames to the calling thread's
///   `DispatcherQueue`, so the probe binds one to this thread (once, before the
///   rounds -- a thread can only ever have one) and pumps messages while it pulls
///   frames. Measured 2026-09-17: `close_frames` / `cached_item` / `shared_device`
///   all step exactly like `baseline`, so the free-threaded pool's own allocation
///   is what is left to test.
/// - `reuse_pool`: one free-threaded pool for the whole run, built in round 1 and
///   `Recreate`d onto each later round's fresh device (same size, 2 buffers), a
///   new session every round, `pool.Close()` only after round 4. Measured
///   2026-09-18: `dispatcher` steps like `baseline` too, and a pool built and
///   dropped without `StartCapture` leaves nothing, so this splits "a pool per
///   round" from "a started session per round". Flat means the per-round pool is
///   the holder (the app fix is to keep one pool across recordings); still
///   stepping means the residue sits outside the pool (session / item / DWM).
///   Measured 2026-09-18 19:53 (the GTX 1080 desk): round 1 runs
///   (`frames=599`), round 2's `CreateCaptureSession` on the recreated pool
///   panics with `0x8000FFFF` (E_UNEXPECTED). Not yet split: a second session
///   on one pool, or `Recreate` onto a different device -- task4280's
///   Verification names the same-device contrast.
///
/// `SetIsBorderRequired`/`SetIsCursorCaptureEnabled` need no run at all -- the
/// recorder sets both (`worker.rs`) and this probe sets neither, and the two leak
/// the same 6.2 MB per round, so they are not what holds the buffer.
///
/// **A variant is only readable next to a `baseline` from the same run**, on the
/// same desk and the same fixture geometry: the step is about 4 B per captured
/// pixel, and the fixture's physical size is what sets it (2026-09-17: 1067x600
/// instead of 1600x900 turned +6.2 MB into +2.6 MB per round). And a flat variant
/// is only a finding when its `frames=` is the same few hundred as `baseline`'s --
/// a variant that captured nothing is a broken probe, not a fixed leak.
#[test]
#[ignore = "task4280 probe: needs LIVEBACK_4280_HWND and an external VRAM logger"]
fn task4280_probe_wgc_rounds() {
    use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
    let variant =
        std::env::var("LIVEBACK_4280_WGC_VARIANT").unwrap_or_else(|_| "baseline".to_owned());
    assert!(
        matches!(
            variant.as_str(),
            "baseline"
                | "close_frames"
                | "cached_item"
                | "shared_device"
                | "dispatcher"
                | "reuse_pool"
        ),
        "unknown LIVEBACK_4280_WGC_VARIANT={variant}"
    );
    let text = std::env::var("LIVEBACK_4280_HWND").expect("LIVEBACK_4280_HWND");
    let raw = isize::from_str_radix(text.trim_start_matches("0x"), 16).expect("hex hwnd");
    let hwnd = windows::Win32::Foundation::HWND(raw as *mut _);
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .expect("COM");
    }
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .expect("interop");
    let shared_device =
        (variant == "shared_device").then(|| create_d3d_device().expect("D3D11 device"));
    let shared_item: Option<GraphicsCaptureItem> = (variant == "cached_item")
        .then(|| unsafe { interop.CreateForWindow(hwnd) }.expect("capture item"));
    // The dispatcher pool wants a `DispatcherQueue` on this thread. Bind it once,
    // here, and keep the controller alive for every round.
    //
    // The `Create` before the binding was meant as a control ("no queue, no pool"),
    // and it did not discriminate: measured 2026-09-18 on an MTA thread it returns
    // `unexpectedly ok` without any queue. It stays for the log line, but what says
    // this variant really ran is the `Create` branch below plus `frames=599` -- and
    // what the run showed is that a pool built and dropped without `StartCapture`
    // leaves nothing behind (the first sample of the round-1 CSV is 0.0 MB), so the
    // residue is tied to a started session, not to constructing a pool.
    let _queue = (variant == "dispatcher").then(|| {
        let (_device, _context, direct3d) = create_d3d_device().expect("D3D11 device");
        let item: GraphicsCaptureItem =
            unsafe { interop.CreateForWindow(hwnd) }.expect("capture item");
        let without_queue = Direct3D11CaptureFramePool::Create(
            &direct3d,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            item.Size().expect("item size"),
        );
        eprintln!(
            "dispatcher control: Create without a queue -> {}",
            match &without_queue {
                Ok(_) => "unexpectedly ok".to_owned(),
                Err(error) => format!("{:?}", error.code()),
            }
        );
        drop(without_queue);
        task4280_bind_dispatcher_queue().expect("dispatcher queue")
    });
    eprintln!(
        "probe=wgc variant={variant} pid={} start={}",
        std::process::id(),
        task4280_stamp()
    );
    std::thread::sleep(std::time::Duration::from_secs(12));
    // `reuse_pool` keeps this one pool across rounds; every other variant leaves it `None`.
    let mut shared_pool: Option<Direct3D11CaptureFramePool> = None;
    for round in 1..=4 {
        {
            let round_device = if shared_device.is_some() {
                None
            } else {
                Some(create_d3d_device().expect("D3D11 device"))
            };
            let (_device, _context, direct3d) = if shared_device.is_some() {
                &shared_device
            } else {
                &round_device
            }
            .as_ref()
            .expect("a device for this round");
            let round_item: Option<GraphicsCaptureItem> = if shared_item.is_some() {
                None
            } else {
                Some(unsafe { interop.CreateForWindow(hwnd) }.expect("capture item"))
            };
            let item = if shared_item.is_some() {
                &shared_item
            } else {
                &round_item
            }
            .as_ref()
            .expect("a capture item for this round");
            let size = item.Size().expect("item size");
            let pool = if variant == "reuse_pool" {
                match &shared_pool {
                    Some(pool) => {
                        pool.Recreate(
                            direct3d,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            2,
                            size,
                        )
                        .expect("frame pool recreate");
                        eprintln!("round={round} pool=recreated");
                        pool.clone()
                    }
                    None => {
                        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                            direct3d,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            2,
                            size,
                        )
                        .expect("frame pool (reused)");
                        eprintln!("round={round} pool=created");
                        shared_pool = Some(pool.clone());
                        pool
                    }
                }
            } else if variant == "dispatcher" {
                Direct3D11CaptureFramePool::Create(
                    direct3d,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    size,
                )
                .expect("frame pool (dispatcher)")
            } else {
                Direct3D11CaptureFramePool::CreateFreeThreaded(
                    direct3d,
                    DirectXPixelFormat::B8G8R8A8UIntNormalized,
                    2,
                    size,
                )
                .expect("frame pool")
            };
            let session = pool.CreateCaptureSession(item).expect("session");
            session.StartCapture().expect("start");
            let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut frames = 0u32;
            let mut close_failed = 0u32;
            while std::time::Instant::now() < until {
                if variant == "dispatcher" {
                    // A thread-bound `DispatcherQueue` only runs while the thread
                    // pumps messages, so the dispatcher pool goes quiet without this.
                    unsafe {
                        let mut message = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                        while windows::Win32::UI::WindowsAndMessaging::PeekMessageW(
                            &mut message,
                            None,
                            0,
                            0,
                            windows::Win32::UI::WindowsAndMessaging::PM_REMOVE,
                        )
                        .as_bool()
                        {
                            windows::Win32::UI::WindowsAndMessaging::DispatchMessageW(&message);
                        }
                    }
                }
                if let Ok(frame) = pool.TryGetNextFrame() {
                    frames += 1;
                    if variant == "close_frames" && frame.Close().is_err() {
                        close_failed += 1;
                    }
                    drop(frame);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            session.Close().expect("session close");
            // `reuse_pool` closes its pool once, after the last round.
            if variant != "reuse_pool" || round == 4 {
                pool.Close().expect("pool close");
                shared_pool = None;
            }
            eprintln!(
                "round={round} frames={frames} close_failed={close_failed} closed={}",
                task4280_stamp()
            );
        }
        eprintln!("round={round} dropped={}", task4280_stamp());
        std::thread::sleep(std::time::Duration::from_secs(15));
    }
    eprintln!("probe=wgc variant={variant} end={}", task4280_stamp());
}
