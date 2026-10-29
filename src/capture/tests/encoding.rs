//! GPU NV12 conversion and hardware H.264 encode/finalize tests.
use super::transform::create_test_source_with_pixels;
use super::*;

use crate::encoder::{EncoderConfig, HardwareVideoEncoder};
use windows::Win32::Media::MediaFoundation::IMFSample;

/// The device and the transform pipeline at `size`: where every GPU test here
/// starts.
fn gpu_at(size: CaptureSize) -> (ID3D11Device, ID3D11DeviceContext, TransformPipeline) {
    let (device, context, _) = create_d3d_device().expect("D3D11 video device should be available");
    let pipeline = TransformPipeline::new(
        device.clone(),
        context.clone(),
        size,
        gpu::HDR_WHITE_POINT_FLOOR,
    )
    .expect("BGRA pipeline should initialize");
    (device, context, pipeline)
}

/// A recording's GPU half without the capture (t261025-1b1a): the device, the
/// transform, the NV12 converter and a hardware encoder, all at `size`. Built
/// the same way in about ten tests before it was one value.
struct EncodeRig {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pipeline: TransformPipeline,
    converter: GpuNv12Converter,
    config: EncoderConfig,
    encoder: HardwareVideoEncoder,
}

impl EncodeRig {
    /// With the Media Foundation H.264 encoder (`create`, not the NVENC path).
    fn new(size: CaptureSize, frame_rate: u8, output_dir: impl AsRef<std::path::Path>) -> Self {
        Self::with(size, frame_rate, output_dir, |config, device| {
            HardwareVideoEncoder::create(config, device)
                .expect("hardware H.264 encoder should initialize")
        })
    }

    /// With whatever encoder `make` builds for the rig's config and device.
    fn with(
        size: CaptureSize,
        frame_rate: u8,
        output_dir: impl AsRef<std::path::Path>,
        make: impl FnOnce(&EncoderConfig, &ID3D11Device) -> HardwareVideoEncoder,
    ) -> Self {
        let (device, context, pipeline) = gpu_at(size);
        let converter = GpuNv12Converter::new(&device, &context, size)
            .expect("GPU NV12 converter should initialize");
        let config = EncoderConfig {
            output_dir: output_dir.as_ref().to_path_buf(),
            output_size: size,
            frame_rate,
        };
        let encoder = make(&config, &device);
        Self {
            device,
            context,
            pipeline,
            converter,
            config,
            encoder,
        }
    }
}

/// A fresh encoder input sample and the texture behind it.
fn input_sample(encoder: &HardwareVideoEncoder) -> (IMFSample, ID3D11Texture2D) {
    let sample = encoder
        .allocate_input_sample()
        .expect("allocator must provide an input sample");
    let texture = HardwareVideoEncoder::input_sample_texture(&sample)
        .expect("allocator sample must expose its D3D11 texture");
    (sample, texture)
}

/// One frame in: `source` put into the encoder's own surface in the form it
/// takes -- NV12 by conversion, ARGB32 by copy, as `EncodePath::submit` does --
/// and handed over at `pts_100ns`. The caller has checked there is credit.
fn feed(
    encoder: &mut HardwareVideoEncoder,
    converter: &GpuNv12Converter,
    source: &ID3D11Texture2D,
    pts_100ns: i64,
) {
    let (sample, target) = input_sample(encoder);
    match encoder.input() {
        crate::encoder::EncoderInput::Nv12 => converter.convert_into(source, &target),
        crate::encoder::EncoderInput::Argb32 => converter.copy_into(source, &target),
    }
    .unwrap();
    encoder.process_input_sample(&sample, pts_100ns).unwrap();
}

/// `feed` when there is credit, asking for a keyframe first when the muxer
/// wants a boundary at `pts_100ns`. Whether this frame asked for one.
fn feed_at_boundary(
    encoder: &mut HardwareVideoEncoder,
    muxer: &mut crate::encoder::SegmentMuxer,
    converter: &GpuNv12Converter,
    source: &ID3D11Texture2D,
    pts_100ns: i64,
) -> bool {
    if !encoder.has_input_credit() {
        return false;
    }
    let boundary = muxer.should_force_keyframe(pts_100ns);
    if boundary {
        encoder.request_next_keyframe().unwrap();
    }
    feed(encoder, converter, source, pts_100ns);
    boundary
}

#[test]
fn gpu_nv12_converter_allocates_video_encoder_surface() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let EncodeRig { encoder, .. } = EncodeRig::new(size, 60, "buffer");
    let (_sample, texture) = input_sample(&encoder);
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
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let EncodeRig {
        device,
        context,
        pipeline,
        converter,
        encoder,
        ..
    } = EncodeRig::new(size, 60, "buffer");

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
        let (_sample, nv12) = input_sample(&encoder);
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
    let size = CaptureSize {
        width: 320,
        height: 180,
    };
    let (device, context, pipeline) = gpu_at(size);
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

/// t261017-613d: the boundary thumbnail is queued and read back later without
/// waiting. Polling the non-waiting read-back until it answers must give the
/// same picture the waiting one does -- and the source's colour, so a read
/// of the staging texture before the copy landed cannot pass as equal.
#[test]
fn a_non_waiting_thumbnail_readback_returns_what_the_waiting_one_does() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let size = CaptureSize {
        width: 320,
        height: 180,
    };
    let (device, _context, pipeline) = gpu_at(size);
    let colour = [0x20u8, 0x80, 0xE0, 0xFF];
    let source = create_test_source_with_pixels(
        &device,
        size,
        &colour.repeat(size.width as usize * size.height as usize),
    )
    .expect("solid BGRA source should initialize");
    let output = pipeline
        .transform(&source, size, Inset::default(), false)
        .expect("SDR transform should draw");

    pipeline
        .queue_thumbnail(&output)
        .expect("thumbnail copy should queue");
    let started = std::time::Instant::now();
    let polled = loop {
        if let Some(frame) = pipeline
            .collect_thumbnail(false)
            .expect("a non-waiting read-back is ready or not, never an error")
        {
            break frame;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the non-waiting read-back never became ready: the copy was not flushed"
        );
        std::thread::yield_now();
    };

    pipeline
        .queue_thumbnail(&output)
        .expect("thumbnail copy should queue");
    let waited = pipeline
        .collect_thumbnail(true)
        .expect("thumbnail readback should succeed")
        .expect("a waiting read-back always returns the picture");

    assert!(polled == waited, "polled and waited read-backs differ");
    let (width, height, bgra) = &polled;
    let centre = ((height / 2) as usize * *width as usize + (width / 2) as usize) * 4;
    for (channel, expected) in colour.iter().enumerate().take(3) {
        assert!(
            bgra[centre + channel].abs_diff(*expected) <= 2,
            "channel {channel}: {} against the source's {expected}",
            bgra[centre + channel]
        );
    }
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
    unsafe {
        let staging = gpu::create_staging_texture(device, texture)?;
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
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let EncodeRig {
        pipeline,
        converter,
        mut encoder,
        ..
    } = EncodeRig::new(size, 60, "buffer");
    let mut produced = Vec::new();
    for index in 0..120 {
        produced.extend(encoder.poll().expect("hardware H.264 events should poll"));
        if encoder.has_input_credit() {
            feed(&mut encoder, &converter, &pipeline.output, index * 166_667);
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
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let directory = TmpDir::new("livia-task005").keep_on_request();
    let EncodeRig {
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::new(size, 60, &directory);
    let aac = crate::encoder::aac::AacEncoder::create().expect("system AAC MFT");
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
            feed(&mut encoder, &converter, &pipeline.output, index * 166_667);
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
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let directory = TmpDir::new("livia-task085");
    let EncodeRig {
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::new(size, 60, &directory);
    let aac = crate::encoder::aac::AacEncoder::create().expect("system AAC MFT");
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
            feed(&mut encoder, &converter, &pipeline.output, index * 166_667);
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
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let directory = TmpDir::new("livia-task033").keep_on_request();
    let EncodeRig {
        device,
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::new(size, 60, &directory);
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
            feed(&mut encoder, &converter, &output, index * 166_667);
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
}

#[test]
#[ignore = "NVIDIA hardware acceptance: runs 12 synthetic GPU minutes across six cases"]
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
    let directory = TmpDir::new(&format!(
        "livia-task005-{fps}-{}x{}",
        size.width, size.height
    ))
    .keep_on_request();
    let EncodeRig {
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::new(size, fps, &directory);
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
            feed(&mut encoder, &converter, &pipeline.output, timestamp);
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
/// Task t260924-b3d1: `first_process_id_for_executable` deliberately returns
/// only the *first* Toolhelp32 match and stops (see its doc comment), so a
/// concurrent copy of this same test binary can legitimately win that race;
/// assert membership in the full process snapshot instead of exact identity.
#[test]
fn an_audio_track_executable_resolves_to_a_running_process_or_to_nothing() {
    let own = std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .expect("this test binary has a name");
    let is_self_among_running = |executable_name: &str| {
        let running = crate::capture::targets::running_processes()
            .expect("a Toolhelp32 snapshot must find this very process");
        running
            .pids
            .get(&executable_name.to_ascii_lowercase())
            .is_some_and(|pids| pids.contains(&std::process::id()))
    };
    assert!(
        is_self_among_running(&own),
        "the test process should resolve to itself"
    );
    // Windows filenames are case-insensitive and so is the lookup: a list the
    // user typed by hand must not miss a process over capitalisation.
    assert!(is_self_among_running(&own.to_uppercase()));
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
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let directory = TmpDir::new("livia-t260911e7f3");
    let EncodeRig {
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::new(size, 60, &directory);
    let aac = crate::encoder::aac::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = crate::encoder::SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    let mut pending_aac = crate::encoder::PendingAac::default();
    let mut finalized = Vec::new();
    let silence = vec![0.0_f32; 1024 * 2];
    let mut pushed_video = 0u64;

    // One segment at this fixture's 60fps (`166_667` per frame): the frame the
    // boundary is requested at.
    const SEGMENT_FRAMES: i64 = crate::encoder::SEGMENT_DURATION_100NS * 60 / 10_000_000;
    // Three quarters of a segment of a healthy recording (1.5s at 2s
    // segments): segment 0 opens and both tracks are moving.
    const HEALTHY_FRAMES: i64 = SEGMENT_FRAMES * 3 / 4;
    // 3s of backlog on top of it -- fixed, not a segment multiple, since the
    // backlog has to outrun the sink's interleave window (the `>= 100` below).
    // The boundary is requested at frame `SEGMENT_FRAMES` -- a quarter segment
    // into the stall -- and its clean point is one of the samples held back.
    const STALLED_FRAMES: i64 = 180;

    let mut boundary_frames = Vec::new();

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
        if feed_at_boundary(
            &mut encoder,
            &mut muxer,
            &converter,
            &pipeline.output,
            index * 166_667,
        ) {
            boundary_frames.push(index);
        }
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
        if feed_at_boundary(
            &mut encoder,
            &mut muxer,
            &converter,
            &pipeline.output,
            index * 166_667,
        ) {
            boundary_frames.push(index);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // The fixture's premise, checked rather than assumed (t261012-38b1): the
    // one boundary is asked for inside the stall, so its clean point is held
    // back with the rest. One asked for in the healthy part would rotate before
    // the stall and leave the backlog with no boundary in it at all.
    println!("t260911-e7f3: boundary requested at frames {boundary_frames:?}");
    assert!(
        boundary_frames.len() == 1
            && (HEALTHY_FRAMES..HEALTHY_FRAMES + STALLED_FRAMES).contains(&boundary_frames[0]),
        "the boundary has to be requested inside the stall, got frames {boundary_frames:?}"
    );
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

/// Task t261007-a63e: the capture deaths of 2026-10-04 21:47 and 2026-10-07
/// 00:27 JST (a game window at 120fps).
///
/// Video stopped arriving for about a second while audio kept coming. When it
/// came back, the AAC that piled up during the stall outran the old segment's
/// video and the sink refused it -- ordinary back-pressure, until the resumed
/// video's keyframe rotated that segment into `closing`. A `closing` segment
/// never receives another video sample, so its refusal can never clear; the
/// refused sample stayed at the head of the FIFO, nothing behind it reached the
/// new segment (`first_audio=[None]`), the new segment's sink then refused its
/// video too, and `PENDING_AAC_LIMIT` killed the recording.
///
/// The stall is five seconds, and that length is the fixture. How much audio
/// the sink takes ahead of its video is not a fixed lead: measured on one test desk,
/// a 2s backlog was refused after ~45, ~2 or no samples at all depending only on
/// how fast the sink's own thread ran (E-core affinity or not), and the "none"
/// runs never reached `closing`'s refusal. Five seconds is past the most the
/// sink was seen to hold, so the refusal does not depend on the scheduler. The
/// resume is past the segment boundary, so the backlog predates the new segment.
#[test]
fn t261007_a63e_audio_the_closing_segment_refuses_does_not_stall_the_next_one() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let size = CaptureSize {
        width: 640,
        height: 360,
    };
    let directory = TmpDir::new("livia-t261007a63e");
    let EncodeRig {
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::new(size, 60, &directory);
    let aac = crate::encoder::aac::AacEncoder::create().expect("system AAC MFT");
    let mut muxer = crate::encoder::SegmentMuxer::with_aac(
        config.clone(),
        encoder.output_media_type().unwrap(),
        aac.output_media_type().clone(),
    );
    let mut pending_aac = crate::encoder::PendingAac::default();
    let mut finalized = Vec::new();
    // One video frame's worth of 48kHz stereo per call, so the audio timeline
    // keeps pace with the video one instead of running 28% ahead of it.
    let silence = vec![0.0_f32; 800 * 2];

    // Half a segment healthy, 5s with no video at all, then a segment healthy
    // again (1s / 5s / 2s at 2s segments). The resume is past
    // `SEGMENT_DURATION_100NS`, so its keyframe opens segment 1, and the
    // resumed part ends a frame short of segment 1's own boundary.
    const SEGMENT_FRAMES: i64 = crate::encoder::SEGMENT_DURATION_100NS * 60 / 10_000_000;
    const HEALTHY_FRAMES: i64 = SEGMENT_FRAMES / 2;
    const STALLED_FRAMES: i64 = 300;
    const RESUMED_FRAMES: i64 = SEGMENT_FRAMES;
    let resume_100ns = (HEALTHY_FRAMES + STALLED_FRAMES) * 166_667;

    let mut boundary_frames = Vec::new();
    // One turn of the capture loop: video into the muxer first, then the AAC
    // queue -- the order `capture::worker` keeps ("after the hold feed and its
    // drain, never before").
    let pump = |encoder: &mut crate::encoder::HardwareVideoEncoder,
                muxer: &mut crate::encoder::SegmentMuxer,
                pending_aac: &mut crate::encoder::PendingAac,
                finalized: &mut Vec<crate::encoder::EncoderEvent>| {
        finalized.extend(muxer.drain_pending_video().expect("video retry"));
        for sample in encoder.poll().unwrap() {
            finalized.extend(muxer.push_video_sample(&sample).expect("video push"));
        }
        finalized.extend(pending_aac.flush(muxer));
    };

    for index in 0..HEALTHY_FRAMES {
        pump(&mut encoder, &mut muxer, &mut pending_aac, &mut finalized);
        for sample in aac.encode_f32(&silence, index * 166_667).unwrap() {
            finalized.extend(pending_aac.offer(&mut muxer, &sample));
        }
        if feed_at_boundary(
            &mut encoder,
            &mut muxer,
            &converter,
            &pipeline.output,
            index * 166_667,
        ) {
            boundary_frames.push(index);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }

    // The stall: no frame reaches the encoder, and the capture thread is not
    // there to offer the audio that keeps arriving.
    let mut audio_backlog = Vec::new();
    for index in HEALTHY_FRAMES..HEALTHY_FRAMES + STALLED_FRAMES {
        audio_backlog.extend(aac.encode_f32(&silence, index * 166_667).unwrap());
    }

    // The stall clears: the whole backlog is offered at once, then video and
    // audio go on together.
    for sample in &audio_backlog {
        finalized.extend(pending_aac.offer(&mut muxer, sample));
    }
    for index in HEALTHY_FRAMES + STALLED_FRAMES..HEALTHY_FRAMES + STALLED_FRAMES + RESUMED_FRAMES {
        pump(&mut encoder, &mut muxer, &mut pending_aac, &mut finalized);
        for sample in aac.encode_f32(&silence, index * 166_667).unwrap() {
            finalized.extend(pending_aac.offer(&mut muxer, &sample));
        }
        if feed_at_boundary(
            &mut encoder,
            &mut muxer,
            &converter,
            &pipeline.output,
            index * 166_667,
        ) {
            boundary_frames.push(index);
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    // The fixture's premise, checked rather than assumed (t261012-38b1): the
    // only boundary is the resume's, so segment 0 is the one closing with the
    // whole stall's audio in it and segment 1 is the last one.
    println!("t261007-a63e: boundary requested at frames {boundary_frames:?}");
    assert!(
        boundary_frames.len() == 1 && boundary_frames[0] >= HEALTHY_FRAMES + STALLED_FRAMES,
        "the only boundary has to be the resume's, got frames {boundary_frames:?}"
    );
    encoder.begin_drain().unwrap();
    for _ in 0..2_000 {
        pump(&mut encoder, &mut muxer, &mut pending_aac, &mut finalized);
        if encoder.drain_complete() && muxer.pending_video_len() == 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }

    println!(
        "t261007-a63e: open segment {:?} starts at {resume_100ns} or later; AAC queue head {:?}; \
         {} video samples held; {} AAC samples dropped as refused by closing",
        muxer.open_audio_placement(),
        pending_aac.front_timestamp(),
        muxer.pending_video_len(),
        muxer.dropped_audio_refused_by_closing()
    );
    // What killed the recording: the head of the AAC queue never leaves, and
    // the new segment's video waits on audio that is stuck behind it.
    pending_aac.assert_drained();
    assert_eq!(
        muxer.pending_video_len(),
        0,
        "the new segment's video is still waiting on audio"
    );
    // The control: the stall's audio really went to the closing segment, so
    // the path under test was reached. It used to be "the closing segment
    // refused something", but since t261010-5ba5 (one GOP per fragment) the
    // sink no longer refuses audio running ahead of a closing segment's video:
    // 0 refusals with 5s and 30s of backlog, and segment 0 kept all ~6s of
    // its audio (282 AAC frames, measured). The drop branch in
    // `push_aac_sample_on` stays for a sink that does refuse.
    let closing = finalized
        .iter()
        .find_map(|event| match event {
            crate::encoder::EncoderEvent::Finalized(metadata) if metadata.index == 0 => {
                Some(metadata.path.clone())
            }
            _ => None,
        })
        .expect("the resume past the boundary has to finalize segment 0");
    let closing_tracks =
        crate::encoder::dump_segment_track_summary(&closing).expect("track summary");
    let closing_audio = closing_tracks
        .iter()
        .filter(|track| track.is_audio)
        .map(|track| track.sample_count)
        .sum::<u64>()
        + u64::from(muxer.dropped_audio_refused_by_closing());
    // 6s of 48kHz in 1024-frame AACs is ~281 (360 frames at 2s segments,
    // 5.5s and ~258 at 1s); anything near the healthy part alone means the
    // stall's audio never reached the closing segment at all. The floor is the
    // 2s fixture's 260 of 360 frames, kept in proportion to this fixture's
    // length (t261012-38b1).
    let floor = 260 * u64::try_from(HEALTHY_FRAMES + STALLED_FRAMES).unwrap() / 360;
    assert!(
        closing_audio >= floor,
        "the closing segment saw only {closing_audio} AAC samples (floor {floor}), \
         so the stall never reached it"
    );
    finalized.extend(muxer.finalize().unwrap());

    let resumed = finalized
        .iter()
        .find_map(|event| match event {
            crate::encoder::EncoderEvent::Finalized(metadata) if metadata.index == 1 => {
                Some(metadata.path.clone())
            }
            _ => None,
        })
        .expect("the resume past the boundary has to open segment 1");
    let tracks = crate::encoder::dump_segment_track_summary(&resumed).expect("track summary");
    println!("t261007-a63e: segment 1 tracks {tracks:?}");
    assert!(
        tracks
            .iter()
            .any(|track| track.is_audio && track.sample_count > 0),
        "segment 1 has to carry audio after the stall"
    );
    assert!(
        tracks
            .iter()
            .any(|track| !track.is_audio && track.sample_count > 0),
        "segment 1 has to carry video after the stall"
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
    let size = CaptureSize {
        width: 320,
        height: 180,
    };
    let EncodeRig {
        device,
        pipeline,
        converter,
        encoder,
        ..
    } = EncodeRig::new(size, 60, "buffer");
    let source = create_test_source_with_pixels(
        &device,
        size,
        &vec![0x80; size.width as usize * size.height as usize * 4],
    )
    .expect("BGRA source should initialize");
    let output = pipeline
        .transform(&source, size, Inset::default(), false)
        .expect("SDR transform should draw");
    let (_sample, nv12) = input_sample(&encoder);
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
            let EncodeRig {
                device,
                pipeline,
                converter,
                encoder,
                ..
            } = EncodeRig::new(size, 60, "buffer");
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
                let (_sample, nv12) = input_sample(&encoder);
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
///   Measured 2026-09-18 19:53: round 1 runs
///   (`frames=599`), round 2's `CreateCaptureSession` on the recreated pool
///   panics with `0x8000FFFF` (E_UNEXPECTED). Not yet split: a second session
///   on one pool, or `Recreate` onto a different device -- task4280's
///   Verification names the same-device contrast.
/// - `reuse_pool_same_device`: that contrast. One device, one item and one pool
///   for the whole run (no `Recreate`), a new session every round, `pool.Close()`
///   only after round 4. Round 2 failing with `0x8000FFFF` again means a pool that
///   held a closed session takes no new one; running flat means the per-round pool
///   (on its own device) is the holder. Measured 2026-09-26: round 2 fails with `0x8000FFFF` again.
/// - `reuse_pool_recreate_same_device`: the remaining cell. Same device, item and
///   pool as above, but `Recreate` onto that same device before each later round.
///   Measured 2026-09-26 20:24: `Recreate` returns Ok, then
///   round 2's `CreateCaptureSession` fails with `0x8000FFFF` again. With
///   `reuse_pool` and `reuse_pool_same_device` that is all three cells failing at
///   the same call: a pool takes one session in its lifetime, so reusing the pool
///   across recordings is not an app-side fix.
///
/// - `hold_session` / `hold_session_idle` / `hold_session_resize` /
///   `switch_target`: t260929-ea5e's cells (one session kept alive across
///   recordings), run by `task_ea5e_probe_held_session` -- see its doc.
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
                | "reuse_pool_same_device"
                | "reuse_pool_recreate_same_device"
                | "hold_session"
                | "hold_session_idle"
                | "hold_session_resize"
                | "switch_target"
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
    if variant.starts_with("hold_session") || variant == "switch_target" {
        task_ea5e_probe_held_session(&variant, &interop, hwnd);
        return;
    }
    let recreate_same_device = variant == "reuse_pool_recreate_same_device";
    let same_device_pool = variant == "reuse_pool_same_device" || recreate_same_device;
    let shared_device = (variant == "shared_device" || same_device_pool)
        .then(|| create_d3d_device().expect("D3D11 device"));
    let shared_item: Option<GraphicsCaptureItem> = (variant == "cached_item" || same_device_pool)
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
            let pool = if variant == "reuse_pool" || same_device_pool {
                match &shared_pool {
                    // Same device and item every round, so no `Recreate`: only the
                    // session is new.
                    Some(pool) if same_device_pool => {
                        if recreate_same_device {
                            pool.Recreate(
                                direct3d,
                                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                                2,
                                size,
                            )
                            .expect("frame pool recreate");
                            eprintln!("round={round} pool=recreated_same_device");
                        } else {
                            eprintln!("round={round} pool=reused");
                        }
                        pool.clone()
                    }
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
            // The `reuse_pool*` variants close their pool once, after the last round.
            if !(variant == "reuse_pool" || same_device_pool) || round == 4 {
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

/// t260929-ea5e: what one WGC session kept alive across recordings costs, next
/// to `task4280_probe_wgc_rounds`'s `baseline` (one fresh session per round,
/// which leaves ~4.3 B per captured pixel behind every round).
///
/// - `hold_session`: device, item, free-threaded pool (2 buffers) and session are
///   built once and started once. Each round pulls frames for 10 s exactly like
///   `baseline` (main-thread `TryGetNextFrame` every 10 ms), then logs the same
///   `round=N dropped=<epoch>` line and idles 15 s while the `FrameArrived`
///   handler takes and `Close`s every frame (nobody consumes). The session is
///   closed only after round 4 (`released=<epoch>`, then 15 s more), so the
///   per-round step and the one-off cost of the final close read separately.
/// - `hold_session_resize`: `hold_session`, plus a resize of the fixture window
///   away from and back to its own size in every round's pull window, so each
///   round runs `pool.Recreate` twice (the recorder's `recreate_pool_if_resized`
///   arguments) and ends at the geometry it started with.
/// - `hold_session_idle`: phases of `LIVEBACK_EA5E_PHASE_S` seconds (default 60):
///   `no_session` (device only) -> `held_drain` (session, handler takes and closes)
///   -> `held_stall` (session, handler takes nothing: the pool fills and WGC stops
///   handing frames over) -> `held_consume` (main-thread pull, as `baseline`;
///   no pipeline, no encoder) -> close -> `released` (device only again). Each
///   phase line carries the process's `QueryProcessCycleTime` and the handler's
///   arrival count, so CPU and arrival fps come out of this log; GPU memory and
///   engine utilisation come from the external sampler.
/// - `switch_target`: one device; every round a fresh item, pool and session on
///   `LIVEBACK_4280_HWND` (odd rounds) or `LIVEBACK_4280_HWND2` (even rounds),
///   closed at the end of the round -- what switching target costs when the
///   device is kept.
fn task_ea5e_probe_held_session(
    variant: &str,
    interop: &windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop,
    hwnd: windows::Win32::Foundation::HWND,
) {
    use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
    use std::sync::{Arc, Mutex};
    use windows::Graphics::Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem};
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Graphics::SizeInt32;
    let format = DirectXPixelFormat::B8G8R8A8UIntNormalized;
    let secs = std::time::Duration::from_secs;
    // Not in the `windows` features this crate enables (Win32_System_WindowsProgramming).
    #[link(name = "kernel32")]
    extern "system" {
        fn QueryProcessCycleTime(process: isize, cycles: *mut u64) -> i32;
        fn QueryThreadCycleTime(thread: isize, cycles: *mut u64) -> i32;
    }
    // Pseudo handles: GetCurrentProcess() / GetCurrentThread().
    let cycles = || {
        let mut value = 0u64;
        assert!(unsafe { QueryProcessCycleTime(-1, &mut value) } != 0);
        value
    };
    // Cycles one busy core spends per ms on this machine: spin this thread for
    // 500 ms of wall time and read its own counter, so phase cycles convert to
    // "% of one core" without trusting GetThreadTimes (KNOWLEDGE.md).
    let per_ms = {
        let (mut c0, mut c1) = (0u64, 0u64);
        assert!(unsafe { QueryThreadCycleTime(-2, &mut c0) } != 0);
        let started = std::time::Instant::now();
        while started.elapsed() < std::time::Duration::from_millis(500) {
            std::hint::spin_loop();
        }
        assert!(unsafe { QueryThreadCycleTime(-2, &mut c1) } != 0);
        (c1 - c0) as f64 / started.elapsed().as_secs_f64() / 1000.0
    };
    eprintln!(
        "probe=wgc variant={variant} pid={} cycles_per_ms={per_ms:.0} start={}",
        std::process::id(),
        task4280_stamp()
    );
    std::thread::sleep(secs(12));
    let (_device, _context, direct3d) = create_d3d_device().expect("D3D11 device");
    if variant == "switch_target" {
        let text = std::env::var("LIVEBACK_4280_HWND2").expect("LIVEBACK_4280_HWND2");
        let raw = isize::from_str_radix(text.trim_start_matches("0x"), 16).expect("hex hwnd2");
        let hwnd2 = windows::Win32::Foundation::HWND(raw as *mut _);
        for round in 1..=4 {
            {
                let target = if round % 2 == 1 { hwnd } else { hwnd2 };
                let item: GraphicsCaptureItem =
                    unsafe { interop.CreateForWindow(target) }.expect("capture item");
                let size = item.Size().expect("item size");
                let pool =
                    Direct3D11CaptureFramePool::CreateFreeThreaded(&direct3d, format, 2, size)
                        .expect("frame pool");
                let session = pool.CreateCaptureSession(&item).expect("session");
                session.StartCapture().expect("start");
                let until = std::time::Instant::now() + secs(10);
                let mut frames = 0u32;
                while std::time::Instant::now() < until {
                    if pool.TryGetNextFrame().is_ok() {
                        frames += 1;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                session.Close().expect("session close");
                pool.Close().expect("pool close");
                eprintln!(
                    "round={round} target={:?} size={}x{} frames={frames} closed={}",
                    target.0,
                    size.Width,
                    size.Height,
                    task4280_stamp()
                );
            }
            eprintln!("round={round} dropped={}", task4280_stamp());
            std::thread::sleep(secs(15));
        }
        eprintln!("probe=wgc variant={variant} end={}", task4280_stamp());
        return;
    }

    // 0 = the handler takes and closes each frame, 1 = it takes nothing (stall),
    // 2 = it takes nothing and the main thread pulls (consume).
    const DRAIN: u8 = 0;
    const STALL: u8 = 1;
    const CONSUME: u8 = 2;
    let mode = Arc::new(AtomicU8::new(CONSUME));
    let arrived = Arc::new(AtomicU32::new(0));
    let recreated = Arc::new(AtomicU32::new(0));
    // Same check as the recorder's `recreate_pool_if_resized`, shared by the
    // handler and the main-thread pull so a resize is followed in every mode.
    let follow_size = {
        let size = Arc::new(Mutex::new(SizeInt32::default()));
        // The handler runs on WGC's thread, so the device crosses as an agile
        // reference, as in the recorder.
        let direct3d = windows::core::AgileReference::new(&direct3d).expect("agile device");
        let recreated = recreated.clone();
        move |pool: &Direct3D11CaptureFramePool,
              frame: &windows::Graphics::Capture::Direct3D11CaptureFrame| {
            let input = frame.ContentSize().expect("content size");
            let mut current = size.lock().expect("size lock");
            if current.Width == 0 {
                *current = input;
            } else if input.Width != current.Width || input.Height != current.Height {
                pool.Recreate(&direct3d.resolve().expect("device"), format, 2, input)
                    .expect("frame pool recreate");
                *current = input;
                recreated.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    let phase = |name: &str| {
        eprintln!(
            "phase={name} start={} cycles={} arrived={} recreated={}",
            task4280_stamp(),
            cycles(),
            arrived.load(Ordering::Relaxed),
            recreated.load(Ordering::Relaxed)
        );
    };
    let idle = variant == "hold_session_idle";
    let phase_len = secs(
        std::env::var("LIVEBACK_EA5E_PHASE_S")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(60),
    );
    if idle {
        phase("no_session");
        std::thread::sleep(phase_len);
    }
    let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd) }.expect("capture item");
    let start_size = item.Size().expect("item size");
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&direct3d, format, 2, start_size)
        .expect("frame pool");
    let token = {
        let mode = mode.clone();
        let arrived = arrived.clone();
        let follow_size = follow_size.clone();
        pool.FrameArrived(&windows::Foundation::TypedEventHandler::new(
            move |pool: windows::core::Ref<'_, Direct3D11CaptureFramePool>, _| {
                arrived.fetch_add(1, Ordering::Relaxed);
                if mode.load(Ordering::Relaxed) == DRAIN {
                    let pool = pool.ok()?;
                    while let Ok(frame) = pool.TryGetNextFrame() {
                        follow_size(pool, &frame);
                        frame.Close()?;
                    }
                }
                Ok(())
            },
        ))
        .expect("FrameArrived")
    };
    let session = pool.CreateCaptureSession(&item).expect("session");
    let border = session.SetIsBorderRequired(false);
    session.StartCapture().expect("start");
    eprintln!(
        "created={} size={}x{} border_off={}",
        task4280_stamp(),
        start_size.Width,
        start_size.Height,
        match &border {
            Ok(()) => "ok".to_owned(),
            Err(error) => format!("{:?}", error.code()),
        }
    );
    let pull = |seconds: u64, resize: bool| {
        let until = std::time::Instant::now() + secs(seconds);
        let resize_at = [
            std::time::Instant::now() + secs(2),
            std::time::Instant::now() + secs(5),
        ];
        let mut resized = 0;
        let mut frames = 0u32;
        while std::time::Instant::now() < until {
            if resize && resized < 2 && std::time::Instant::now() >= resize_at[resized] {
                // Away (-200 x -100 physical px), then back. Per-monitor-v2 for the
                // call so the fixture gets the pixels asked for (this process is
                // DPI-unaware and the fixture sits on a 96 dpi monitor).
                let (w, h) = if resized == 0 {
                    (start_size.Width - 200, start_size.Height - 100)
                } else {
                    (start_size.Width, start_size.Height)
                };
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::*;
                    let previous = windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(
                        windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
                    );
                    SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        w,
                        h,
                        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                    )
                    .expect("SetWindowPos");
                    let mut rect = windows::Win32::Foundation::RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(previous);
                    eprintln!(
                        "resize asked={w}x{h} window={}x{} at={}",
                        rect.right - rect.left,
                        rect.bottom - rect.top,
                        task4280_stamp()
                    );
                }
                resized += 1;
            }
            if let Ok(frame) = pool.TryGetNextFrame() {
                frames += 1;
                follow_size(&pool, &frame);
                drop(frame);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        frames
    };
    if idle {
        mode.store(DRAIN, Ordering::Relaxed);
        phase("held_drain");
        std::thread::sleep(phase_len);
        mode.store(STALL, Ordering::Relaxed);
        phase("held_stall");
        std::thread::sleep(phase_len);
        mode.store(CONSUME, Ordering::Relaxed);
        phase("held_consume");
        let frames = pull(phase_len.as_secs(), false);
        phase("held_consume_end");
        eprintln!("held_consume frames={frames}");
    } else {
        let resize = variant == "hold_session_resize";
        for round in 1..=4 {
            mode.store(CONSUME, Ordering::Relaxed);
            let frames = pull(10, resize);
            mode.store(DRAIN, Ordering::Relaxed);
            // Frames pulled while DRAIN was off may still sit in the pool; the
            // next arrival drains them.
            eprintln!(
                "round={round} frames={frames} arrived={} recreated={} closed={}",
                arrived.load(Ordering::Relaxed),
                recreated.load(Ordering::Relaxed),
                task4280_stamp()
            );
            eprintln!("round={round} dropped={}", task4280_stamp());
            std::thread::sleep(secs(15));
        }
    }
    // The recorder's teardown order (`CaptureSurface::close`).
    pool.RemoveFrameArrived(token).expect("RemoveFrameArrived");
    session.Close().expect("session close");
    pool.Close().expect("pool close");
    drop(item);
    if idle {
        phase("released");
        std::thread::sleep(phase_len);
        phase("released_end");
    } else {
        eprintln!("released={}", task4280_stamp());
        std::thread::sleep(secs(15));
    }
    eprintln!("probe=wgc variant={variant} end={}", task4280_stamp());
}

/// The colour bars t261021-8388's two encoder inputs are compared on, as
/// BGRA bytes: white, black, red, green, blue, 50% grey.
const COLOUR_BARS: [[u8; 4]; 6] = [
    [0xFF, 0xFF, 0xFF, 0xFF],
    [0x00, 0x00, 0x00, 0xFF],
    [0x00, 0x00, 0xFF, 0xFF],
    [0x00, 0xFF, 0x00, 0xFF],
    [0xFF, 0x00, 0x00, 0xFF],
    [0x80, 0x80, 0x80, 0xFF],
];

/// Records 120 frames of `COLOUR_BARS` through the real recording submit --
/// conversion or copy, whichever the session's input asks for -- and returns
/// that input with each bar's mean Y, U and V in the decoded first frame.
///
/// `recording` creates the session the way recording does
/// (`create_for_recording`: NVENC's API first, t261024-071a) and ignores
/// `inputs`; the third value then says whether NVENC took it. A fresh session
/// with input credit is NVENC: the MFT has none until its first
/// `METransformNeedInput` arrives through `poll`.
fn record_colour_bars(
    codec: crate::encoder::VideoCodec,
    inputs: &[crate::encoder::EncoderInput],
    recording: bool,
) -> (crate::encoder::EncoderInput, Vec<[f64; 3]>, bool) {
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let directory = TmpDir::new(&format!(
        "livia-8388-{}-{}-{}",
        matches!(codec, crate::encoder::VideoCodec::Av1),
        inputs.len(),
        recording,
    ));
    let EncodeRig {
        device,
        pipeline,
        converter,
        config,
        mut encoder,
        ..
    } = EncodeRig::with(size, 120, &directory, |config, device| {
        if recording {
            HardwareVideoEncoder::create_for_recording(config, device, codec)
        } else {
            HardwareVideoEncoder::create_taking(config, device, codec, inputs)
        }
        .unwrap()
    });
    let bar_width = size.width as usize / COLOUR_BARS.len();
    let mut pixels = Vec::with_capacity(size.width as usize * size.height as usize * 4);
    for _ in 0..size.height {
        for x in 0..size.width as usize {
            pixels.extend_from_slice(&COLOUR_BARS[(x / bar_width).min(COLOUR_BARS.len() - 1)]);
        }
    }
    let source = create_test_source_with_pixels(&device, size, &pixels).unwrap();
    let output = pipeline
        .transform(&source, size, Inset::default(), false)
        .unwrap();
    let nvenc = encoder.input_credit() > 0;
    let input = encoder.input();
    let mut muxer =
        crate::encoder::SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap());
    let mut submitted = 0i64;
    let started = std::time::Instant::now();
    while submitted < 120 && started.elapsed() < std::time::Duration::from_secs(20) {
        for sample in encoder.poll().unwrap() {
            muxer.push_video_sample(&sample).unwrap();
        }
        if encoder.has_input_credit() {
            let pts = submitted * 83_333;
            if muxer.should_force_keyframe(pts) {
                encoder.request_next_keyframe().unwrap();
            }
            feed(&mut encoder, &converter, &output, pts);
            submitted += 1;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(submitted, 120, "{input:?}: all 120 frames must go in");
    for sample in encoder.poll().unwrap() {
        muxer.push_video_sample(&sample).unwrap();
    }
    muxer.finalize().unwrap();
    let segment = directory.join("segment-00000000000000000000.mp4");
    // Both halves of the comparison must also *say* studio-range BT.709, or a
    // player would stretch the values this test compares (task033).
    let probed = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=color_range,color_space",
            "-of",
            "default=nw=1",
        ])
        .arg(&segment)
        .output()
        .expect("ffprobe should be runnable (see Preconditions: Gyan.FFmpeg winget package)");
    let probed = String::from_utf8_lossy(&probed.stdout);
    for expected in ["color_range=tv", "color_space=bt709"] {
        assert!(
            probed.contains(expected),
            "{input:?}: missing {expected}: {probed}"
        );
    }
    let decoded = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(&segment)
        .args([
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "yuv420p",
            "-",
        ])
        .output()
        .expect("ffmpeg should be runnable (see Preconditions: Gyan.FFmpeg winget package)");
    assert!(
        decoded.status.success(),
        "ffmpeg failed to decode {}: {}",
        segment.display(),
        String::from_utf8_lossy(&decoded.stderr)
    );
    let _ = std::fs::remove_dir_all(&directory);
    let (width, height) = (size.width as usize, size.height as usize);
    let frame = decoded.stdout;
    assert_eq!(frame.len(), width * height * 3 / 2, "one yuv420p frame");
    let (luma, chroma) = frame.split_at(width * height);
    let (u_plane, v_plane) = chroma.split_at(width * height / 4);
    // The inner part of each bar only: edges blur under 4:2:0 and the codec.
    let means = (0..COLOUR_BARS.len())
        .map(|bar| {
            let (x0, x1) = (bar * bar_width + 40, (bar + 1) * bar_width - 40);
            let (y0, y1) = (200, height - 200);
            let mean = |plane: &[u8], stride: usize, scale: usize| {
                let mut sum = 0u64;
                let mut count = 0u64;
                for y in y0 / scale..y1 / scale {
                    for x in x0 / scale..x1 / scale {
                        sum += u64::from(plane[y * stride + x]);
                        count += 1;
                    }
                }
                sum as f64 / count as f64
            };
            [
                mean(luma, width, 1),
                mean(u_plane, width / 2, 2),
                mean(v_plane, width / 2, 2),
            ]
        })
        .collect();
    (input, means, nvenc)
}

/// t261021-8388: handing the encoder ARGB32 must not change the recording's
/// colours. The encoder now does the RGB -> YUV conversion `VideoProcessorBlt`
/// used to, and a driver free to pick full range or BT.601 would shift every
/// bar -- which `verify_nv12_conversion_uses_studio_range_bt709` cannot see,
/// because it only reads the NV12 path's own output.
#[test]
fn argb32_input_records_the_same_colours_as_the_nv12_conversion() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use crate::encoder::{EncoderInput, VideoCodec};
    let mut codecs = vec![VideoCodec::H264_HIGH];
    if crate::encoder::av1_recording_supported() {
        codecs.push(VideoCodec::Av1);
    }
    for codec in codecs {
        let (reference_input, reference, _) =
            record_colour_bars(codec, &[EncoderInput::Nv12], false);
        assert_eq!(reference_input, EncoderInput::Nv12);
        // Control: the reference really is studio-range BT.709 bars, so a
        // comparison against it can fail. White and black luma sit at 235 / 16,
        // and the red bar's V is far from the grey bar's.
        assert!(
            (reference[0][0] - 235.0).abs() <= 2.0,
            "{codec:?} white: {reference:?}"
        );
        assert!(
            (reference[1][0] - 16.0).abs() <= 2.0,
            "{codec:?} black: {reference:?}"
        );
        assert!(
            reference[2][2] - reference[5][2] > 80.0,
            "{codec:?} red V: {reference:?}"
        );
        let (input, recorded, _) =
            record_colour_bars(codec, &[EncoderInput::Argb32, EncoderInput::Nv12], false);
        eprintln!("t261021-8388 {codec:?} nv12={reference:.1?} {input:?}={recorded:.1?}");
        if input != EncoderInput::Argb32 {
            // This encoder does not take ARGB32: it fell back, which is the
            // other half of the contract, and there is nothing to compare.
            continue;
        }
        for (bar, (want, got)) in reference.iter().zip(&recorded).enumerate() {
            for (plane, (want, got)) in ["Y", "U", "V"].iter().zip(want.iter().zip(got)) {
                assert!(
                    (want - got).abs() <= 2.0,
                    "{codec:?} bar {bar} {plane}: nv12 {want:.1}, argb32 {got:.1}"
                );
            }
        }
    }
}

/// t261024-071a: recording through NVENC's API must not change the colours.
/// NVENC takes the BGRA as ARGB and converts it itself, like the MFT's
/// ARGB32 input, so the range and matrix its VUI declares are what decides the
/// values: full range puts white at 255 instead of 235 (seen 2026-10-25 by
/// setting `videoFullRangeFlag`). The register-time buffer format is not one
/// of the ways to break it: registering ABGR still encoded these bars right,
/// the mapped format follows the texture's DXGI format.
#[test]
fn nvenc_records_the_same_colours_as_the_mft() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use crate::encoder::{EncoderInput, VideoCodec};
    let mut codecs = vec![VideoCodec::H264_HIGH];
    if crate::encoder::av1_recording_supported() {
        codecs.push(VideoCodec::Av1);
    }
    for codec in codecs {
        let (_, reference, _) = record_colour_bars(codec, &[EncoderInput::Nv12], false);
        let (input, recorded, nvenc) = record_colour_bars(codec, &[], true);
        eprintln!("t261024-071a {codec:?} mft-nv12={reference:.1?} nvenc={nvenc} {recorded:.1?}");
        if !nvenc {
            // No NVENC API here: recording fell back to the MFT, which the
            // t261021-8388 test above covers.
            continue;
        }
        assert_eq!(input, EncoderInput::Argb32);
        for (bar, (want, got)) in reference.iter().zip(&recorded).enumerate() {
            for (plane, (want, got)) in ["Y", "U", "V"].iter().zip(want.iter().zip(got)) {
                assert!(
                    (want - got).abs() <= 2.0,
                    "{codec:?} bar {bar} {plane}: mft {want:.1}, nvenc {got:.1}"
                );
            }
        }
    }
}

/// The OBU types in an AV1 temporal unit (low-overhead format, `obu_has_size_field`).
fn av1_obu_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let header = data[at];
        let extension = usize::from((header >> 2) & 1);
        types.push((header >> 3) & 0xF);
        at += 1 + extension;
        if (header >> 1) & 1 == 0 {
            break;
        }
        let (mut size, mut shift) = (0usize, 0);
        while at < data.len() {
            let byte = data[at];
            at += 1;
            size |= usize::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                break;
            }
        }
        at += size;
    }
    types
}

/// t261024-071a: what the muxer's `ctts`-less segments rely on, through NVENC.
/// Output times never go back (no B frames: `frameIntervalP = 1`), every IDR
/// carries its parameter sets in band (`repeatSPSPPS` / `repeatSeqHdr`), and so
/// each 2 s segment plays alone from its own first packet.
#[test]
fn nvenc_segments_play_alone_with_parameter_sets_in_band() {
    let _gpu = GPU_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    use crate::encoder::VideoCodec;
    use windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint;
    let mut codecs = vec![VideoCodec::H264_HIGH];
    if crate::encoder::av1_recording_supported() {
        codecs.push(VideoCodec::Av1);
    }
    let (device, context, _) = create_d3d_device().expect("D3D11 video device");
    let size = CaptureSize {
        width: 1920,
        height: 1080,
    };
    let source = create_test_source_with_pixels(
        &device,
        size,
        &vec![0x80u8; size.width as usize * size.height as usize * 4],
    )
    .unwrap();
    let converter = GpuNv12Converter::new(&device, &context, size).unwrap();
    for codec in codecs {
        let directory = TmpDir::new(&format!(
            "livia-071a-segments-{}",
            matches!(codec, VideoCodec::Av1),
        ));
        let config = crate::encoder::EncoderConfig {
            output_dir: directory.to_path_buf(),
            output_size: size,
            frame_rate: 60,
        };
        let mut encoder =
            crate::encoder::HardwareVideoEncoder::create_for_recording(&config, &device, codec)
                .unwrap();
        if encoder.input_credit() == 0 {
            eprintln!("t261024-071a {codec:?}: no NVENC API here, nothing to check");
            continue;
        }
        let mut muxer =
            crate::encoder::SegmentMuxer::new(config.clone(), encoder.output_media_type().unwrap());
        let (mut submitted, mut outputs, mut keyframes) = (0i64, 0, 0);
        let mut last_pts = i64::MIN;
        let mut check = |sample: &windows::Win32::Media::MediaFoundation::IMFSample| unsafe {
            let pts = sample.GetSampleTime().unwrap();
            assert!(
                pts > last_pts,
                "{codec:?}: output time went back {last_pts} -> {pts}"
            );
            last_pts = pts;
            outputs += 1;
            if sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) == 0 {
                return;
            }
            keyframes += 1;
            let buffer = sample.ConvertToContiguousBuffer().unwrap();
            let (mut data, mut length) = (std::ptr::null_mut(), 0);
            buffer.Lock(&mut data, None, Some(&mut length)).unwrap();
            let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
            buffer.Unlock().unwrap();
            let in_band = match codec {
                VideoCodec::Av1 => av1_obu_types(&bytes).contains(&1),
                VideoCodec::H264 { .. } => bytes
                    .windows(4)
                    .any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1F == 7),
            };
            assert!(
                in_band,
                "{codec:?}: IDR at {pts} without parameter sets in band"
            );
        };
        let started = std::time::Instant::now();
        while submitted < 180 && started.elapsed() < std::time::Duration::from_secs(20) {
            for sample in encoder.poll().unwrap() {
                check(&sample);
                muxer.push_video_sample(&sample).unwrap();
            }
            while encoder.has_input_credit() && submitted < 180 {
                let pts = submitted * 166_667;
                if muxer.should_force_keyframe(pts) {
                    encoder.request_next_keyframe().unwrap();
                }
                feed(&mut encoder, &converter, &source, pts);
                submitted += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        encoder.begin_drain().unwrap();
        while !encoder.drain_complete() && started.elapsed() < std::time::Duration::from_secs(25) {
            for sample in encoder.poll().unwrap() {
                check(&sample);
                muxer.push_video_sample(&sample).unwrap();
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert_eq!(
            (submitted, outputs),
            (180, 180),
            "{codec:?}: every frame comes back"
        );
        assert!(keyframes >= 2, "{codec:?}: keyframes {keyframes}");
        muxer.finalize().unwrap();
        let mut segments: Vec<_> = std::fs::read_dir(&directory)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
            .collect();
        segments.sort();
        assert!(
            segments.len() >= 2,
            "{codec:?}: 3 s at 2 s segments: {segments:?}"
        );
        for segment in &segments {
            let packets = std::process::Command::new("ffprobe")
                .args([
                    "-v",
                    "error",
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "packet=pts,flags",
                    "-of",
                    "csv=p=0",
                ])
                .arg(segment)
                .output()
                .expect(
                    "ffprobe should be runnable (see Preconditions: Gyan.FFmpeg winget package)",
                );
            let packets = String::from_utf8_lossy(&packets.stdout);
            let lines: Vec<&str> = packets.lines().collect();
            assert!(
                lines.first().is_some_and(|first| first.contains('K')),
                "{codec:?} {}: first packet is not a keyframe: {:?}",
                segment.display(),
                lines.first()
            );
            let times: Vec<i64> = lines
                .iter()
                .filter_map(|line| line.split(',').next()?.parse().ok())
                .collect();
            assert!(
                times.windows(2).all(|pair| pair[0] < pair[1]),
                "{codec:?} {}: packet times not increasing",
                segment.display()
            );
            let decoded = std::process::Command::new("ffmpeg")
                .args(["-v", "error", "-i"])
                .arg(segment)
                .args(["-f", "null", "-"])
                .output()
                .expect("ffmpeg should be runnable");
            assert!(
                decoded.status.success() && decoded.stderr.is_empty(),
                "{codec:?} {} does not decode alone: {}",
                segment.display(),
                String::from_utf8_lossy(&decoded.stderr)
            );
        }
        let _ = std::fs::remove_dir_all(&directory);
    }
}

/// t261024-071a: with no usable NVENC API, recording still records -- on the
/// MFT. The boundary is the real one, `LoadLibrary("nvEncodeAPI64.dll")`: this
/// test runs a copy of its own test binary in a child process, beside a
/// `nvEncodeAPI64.dll` without NVENC's exports (a copy of `version.dll`), so
/// `GetProcAddress` fails. The child loads that file before it creates the
/// D3D11 device: creating the device makes the NVIDIA driver load its own
/// `nvEncodeAPI64.dll` from the DriverStore, and a by-name `LoadLibrary`
/// returns whichever module of that name is already loaded (measured
/// 2026-10-25: without this the child got the driver's copy).
#[test]
fn recording_falls_back_to_the_mft_without_a_usable_nvenc_api() {
    const CHILD: &str = "LIVIA_071A_FAKE_NVENC_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use std::os::windows::ffi::OsStrExt;
        let fake = std::env::current_exe()
            .unwrap()
            .with_file_name("nvEncodeAPI64.dll");
        let wide: Vec<u16> = fake
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            windows::Win32::System::LibraryLoader::LoadLibraryW(windows::core::PCWSTR(
                wide.as_ptr(),
            ))
        }
        .expect("the fake nvEncodeAPI64.dll loads");
        let size = CaptureSize {
            width: 1920,
            height: 1080,
        };
        let output_dir = TmpDir::new("livia-071a-fallback").create();
        let EncodeRig {
            device,
            converter,
            mut encoder,
            ..
        } = EncodeRig::with(size, 60, &output_dir, |config, device| {
            HardwareVideoEncoder::create_for_recording(
                config,
                device,
                crate::encoder::VideoCodec::H264_HIGH,
            )
            .expect("recording must still get an encoder")
        });
        // Control: the DLL this process loaded under NVENC's name is the
        // fake beside the binary, not the driver's.
        let loaded = unsafe {
            let module = windows::Win32::System::LibraryLoader::GetModuleHandleW(
                windows::core::w!("nvEncodeAPI64.dll"),
            )
            .expect("the fake nvEncodeAPI64.dll stays loaded");
            let mut path = [0u16; 512];
            let length =
                windows::Win32::System::LibraryLoader::GetModuleFileNameW(Some(module), &mut path);
            String::from_utf16_lossy(&path[..length as usize])
        };
        assert_eq!(
            std::path::Path::new(&loaded),
            fake,
            "the copy must have loaded the fake DLL"
        );
        assert_eq!(
            encoder.input_credit(),
            0,
            "a fresh session with credit is NVENC; the MFT has none before its first poll"
        );
        let source = create_test_source_with_pixels(
            &device,
            size,
            &vec![0x80u8; size.width as usize * size.height as usize * 4],
        )
        .unwrap();
        let (mut submitted, mut outputs) = (0i64, 0);
        let started = std::time::Instant::now();
        while outputs < 10 && started.elapsed() < std::time::Duration::from_secs(20) {
            outputs += encoder.poll().unwrap().len();
            if encoder.has_input_credit() {
                feed(&mut encoder, &converter, &source, submitted * 166_667);
                submitted += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(outputs >= 10, "the MFT must record: {outputs} outputs");
        println!("CHILD ok outputs={outputs}");
        return;
    }
    let directory = TmpDir::new("livia-071a-fake-nvenc");
    std::fs::create_dir_all(&directory).unwrap();
    let binary = directory.join("livia-child.exe");
    std::fs::copy(std::env::current_exe().unwrap(), &binary).unwrap();
    let system = std::env::var_os("SystemRoot").unwrap();
    std::fs::copy(
        std::path::Path::new(&system)
            .join("System32")
            .join("version.dll"),
        directory.join("nvEncodeAPI64.dll"),
    )
    .unwrap();
    let run = std::process::Command::new(&binary)
        .args([
            "capture::tests::encoding::recording_falls_back_to_the_mft_without_a_usable_nvenc_api",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&run.stdout);
    let _ = std::fs::remove_dir_all(&directory);
    assert!(
        run.status.success() && stdout.contains("CHILD ok") && stdout.contains("1 passed"),
        "child run: {}\n{stdout}\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );
}
