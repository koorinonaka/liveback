use super::audio_out::{AUDIO_CHANNELS, AUDIO_RATE};
use super::segment_reader::{open_segment, SegmentReader, VideoFormat};
use super::*;
use crate::ui_state::timeline::HNS_PER_SECOND;
use std::time::Instant;
use windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager;

fn status(engine: &PlaybackEngine) -> PlaybackStatus {
    *engine.shared().status.lock().unwrap()
}

/// t260913-2527's AC5: whatever shape the engine kept the recording-sized
/// picture in, what a screenshot gets back is the recording's own resolution and
/// the same pixels the CPU path would have produced.
///
/// The NV12 half is the one that can go wrong -- it is the variant the GPU path
/// introduced, and `save_current_frame` hands the result straight to
/// `RgbaImage::from_raw`, which is a silent `None` if the length is off by a
/// byte.
#[test]
fn a_screenshot_gets_the_recording_sized_picture_from_either_shape() {
    const WIDTH: u32 = 64;
    const HEIGHT: u32 = 32;
    // A recognisable frame rather than a flat one: mid-grey chroma with a luma
    // ramp, so a wrong plane offset moves the pixels instead of matching by
    // coincidence.
    let mut nv12 = vec![128u8; WIDTH as usize * HEIGHT as usize * 3 / 2];
    for (index, luma) in nv12[..WIDTH as usize * HEIGHT as usize]
        .iter_mut()
        .enumerate()
    {
        *luma = 16 + (index % 220) as u8;
    }
    let expected = livia_pixels::nv12_to_rgba(&nv12, WIDTH, HEIGHT, WIDTH);
    assert_eq!(expected.len(), WIDTH as usize * HEIGHT as usize * 4);

    // The control for the cache assert below: two conversions of the same
    // planes are two live allocations, so comparing addresses does detect a
    // redone conversion instead of passing for free.
    let again = livia_pixels::nv12_to_rgba(&nv12, WIDTH, HEIGHT, WIDTH);
    assert!(!std::ptr::eq(expected.as_ptr(), again.as_ptr()));

    let kept = FullFrame::nv12(nv12);
    assert_eq!(kept.rgba(WIDTH, HEIGHT), expected.as_slice());
    // The paused stage asks again on every tick of a resize drag, so the second
    // answer has to be the same bytes and not merely the same value --
    // converting per resize is the ~3ms of UI-thread work task990 moved off.
    assert!(
        std::ptr::eq(
            kept.rgba(WIDTH, HEIGHT).as_ptr(),
            kept.rgba(WIDTH, HEIGHT).as_ptr()
        ),
        "the NV12 conversion is kept, not redone"
    );
    let kept = FullFrame::Rgba(expected.clone());
    assert_eq!(kept.rgba(WIDTH, HEIGHT), expected.as_slice());
}

/// Opens a segment the way playback does -- unless `LIVIA_FORCE_BGRA` is set,
/// which takes the pre-task206 RGB32 + `swizzle_bgra` path instead. That is how
/// the before and after of the NV12 switch are measured against each other on
/// one machine and one recording, rather than against a number from a
/// different session.
fn open_measured(bytes: &[u8], playlist_index: usize) -> Result<SegmentReader, String> {
    if std::env::var_os("LIVIA_FORCE_BGRA").is_some() {
        super::segment_reader::open_as(bytes, playlist_index, VideoFormat::Bgra, None)
    } else {
        open_segment(bytes, playlist_index)
    }
}

/// The session id every measurement below runs against, plus the buffer folder
/// to look for it in.
///
/// `buffer_root()` answers the default unless something has pointed it
/// elsewhere, and nothing does inside a test process -- so on a machine that
/// moved its buffer (task164 made the folder a setting) every one of these
/// measurements failed to find the recording it was handed. `LIVIA_BUFFER_ROOT`
/// is how the caller says where to look; unset keeps the old behaviour.
fn measured_session() -> String {
    if let Some(root) = std::env::var_os("LIVIA_BUFFER_ROOT") {
        CaptureController::apply_buffer_root(Some(root.into())).expect("buffer root applies");
    }
    std::env::var("LIVIA_PLAYBACK_SESSION")
        .expect("set LIVIA_PLAYBACK_SESSION to the session id to measure")
}

/// Decode cost and frame count for a single segment, with no scheduling in
/// the way -- this is what separates "the decoder is slow" from "the engine
/// is pacing badly" when the fps number disappoints.
#[test]
#[ignore = "real buffer decode measurement (task128): needs LIVIA_PLAYBACK_SESSION"]
fn measures_single_segment_decode() {
    let session_id = measured_session();
    let controller = CaptureController::new();
    let manifest = controller
        .load_session(&session_id)
        .expect("session manifest loads");
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let segment = &snapshot.segments[snapshot.segments.len() / 2];
    let lease = controller
        .acquire_review_lease(Some(session_id.clone()), vec![segment.index])
        .expect("lease");
    let bytes = controller
        .read_review_segment(&lease, segment.index)
        .expect("segment bytes");
    println!("segment {} is {} bytes", segment.index, bytes.len());

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");
    let mut reader = open_measured(&bytes, 0).expect("segment opens");
    println!(
        "{}x{} stride {}",
        reader.width, reader.height, reader.stride
    );

    let started = Instant::now();
    let mut frames = 0;
    let mut first = None;
    let mut last = 0;
    while let Some((timestamp, _rgba)) = reader.next_video() {
        first.get_or_insert(timestamp);
        last = timestamp;
        frames += 1;
    }
    let elapsed = started.elapsed();
    let span = (last - first.unwrap_or(0)) as f64 / HNS_PER_SECOND as f64;
    println!(
        "decoded {frames} frames in {:.0}ms ({:.2}ms/frame) covering {:.2}s of media \
             ({:.1} fps of content); segment declares {:.2}s",
        elapsed.as_secs_f64() * 1000.0,
        elapsed.as_secs_f64() * 1000.0 / frames.max(1) as f64,
        span,
        frames as f64 / span.max(0.001),
        (segment.end_100ns - segment.start_100ns) as f64 / HNS_PER_SECOND as f64,
    );
    controller.release_review_lease(&lease);
    assert!(frames > 0, "no frames decoded at all");
}

/// Timings of one decoder arm, in the three shapes the production insight
/// scopes measure (t260915-e6c8).
#[derive(Debug)]
struct ArmTiming {
    /// Fresh reader, first `ReadSample`. The twin of `playback_prefetch_first`
    /// (31ms measured): production opens a reader per segment, so the decoder's
    /// own instantiation is inside this number.
    first_ms: f64,
    /// `SetCurrentPosition(0)` on the warm reader, then the first `ReadSample`.
    /// The twin of `playback_seek_decode` at `skipped=0` (20-30ms measured).
    seek_ms: f64,
    /// Mean of the reads after that. The twin of `playback_read_sample`
    /// (0.27ms measured).
    steady_ms: f64,
    /// How many reads that mean is over.
    steady_frames: usize,
    /// Whether the first sample's buffer was a D3D11 texture. This is the
    /// control, not a statistic: see the caller.
    dxgi: bool,
}

/// One open-decode-seek-decode pass over `bytes`, with (`Some`) or without
/// (`None`) a D3D11 device for the decoder to land on.
fn measure_decoder_arm(bytes: &[u8], manager: Option<&IMFDXGIDeviceManager>) -> ArmTiming {
    const STEADY: usize = 30;

    let mut reader = super::segment_reader::open_as(bytes, 0, VideoFormat::Nv12, manager)
        .expect("the fixture segment opens as NV12");

    let started = Instant::now();
    let (_, sample) = reader
        .next_video_sample()
        .expect("a first frame out of the fixture segment");
    let first_ms = started.elapsed().as_secs_f64() * 1000.0;
    // The production helper, not an inline cast: what `gpu_frame` branches on
    // is exactly this, so the control tests the thing the worker will use.
    let dxgi = reader.dxgi_texture(&sample).is_some();
    drop(sample);

    // Zero is a keyframe by construction, so this is the `skipped=0` case --
    // the seek's own decode with nothing decoded-and-thrown-away on top of it.
    assert!(reader.seek_local(0), "the source accepted a seek to 0");
    let started = Instant::now();
    reader
        .next_video_sample()
        .expect("a frame after the seek to 0");
    let seek_ms = started.elapsed().as_secs_f64() * 1000.0;

    let started = Instant::now();
    let mut steady_frames = 0;
    while steady_frames < STEADY && reader.next_video_sample().is_some() {
        steady_frames += 1;
    }
    let steady_ms = started.elapsed().as_secs_f64() * 1000.0 / steady_frames.max(1) as f64;

    ArmTiming {
        first_ms,
        seek_ms,
        steady_ms,
        steady_frames,
        dxgi,
    }
}

/// A real H.264 segment of this project's own making, `width`x`height`, as the
/// bytes of the first segment the muxer finalized.
///
/// The allocator's NV12 surfaces are submitted untouched, so the picture is
/// uniform and the stream is near-empty. That is deliberate for a *floor*
/// measurement -- what is left when the bitrate is taken away is the decoder's
/// own pipeline -- and it is exactly why `steady_ms` below must not be compared
/// with production's 0.27ms.
fn encode_measurement_segment(width: i32, height: i32, frames: i64) -> Vec<u8> {
    use crate::encoder::{EncoderConfig, EncoderEvent, HardwareVideoEncoder, SegmentMuxer};

    let (device, _context, _direct3d) =
        crate::capture::create_d3d_device().expect("a D3D11 device for the fixture encode");
    let directory = std::env::temp_dir().join(format!("livia-e6c8-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).expect("fixture directory");
    let config = EncoderConfig {
        output_dir: directory.clone(),
        output_size: crate::capture::CaptureSize { width, height },
        frame_rate: 30,
    };
    let mut encoder =
        HardwareVideoEncoder::create(&config, &device).expect("a hardware H.264 encoder");
    let mut muxer = SegmentMuxer::new(
        config.clone(),
        encoder.output_media_type().expect("encoder output type"),
    );
    for index in 0..frames {
        for sample in encoder.poll().expect("encoder poll") {
            muxer
                .push_video_sample(&sample)
                .expect("muxer takes a sample");
        }
        if encoder.has_input_credit() {
            let sample = encoder.allocate_input_sample().expect("an input surface");
            encoder
                .process_input_sample(&sample, index * 333_333)
                .expect("the encoder takes the frame");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    encoder.begin_drain().expect("drain starts");
    for _ in 0..400 {
        let samples = encoder.poll().expect("encoder poll");
        if samples.is_empty() && encoder.drain_complete() {
            break;
        }
        for sample in samples {
            muxer
                .push_video_sample(&sample)
                .expect("muxer takes a sample");
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let path = muxer
        .finalize()
        .expect("the muxer finalizes")
        .into_iter()
        .find_map(|event| match event {
            EncoderEvent::Finalized(meta) => Some(meta.path),
            _ => None,
        })
        .expect("the fixture encode finalized at least one segment");
    let bytes = std::fs::read(&path).expect("the fixture segment reads back");
    let _ = std::fs::remove_dir_all(&directory);
    bytes
}

/// Step 1 of t260915-e6c8, and the rebuild of the
/// `measures_hardware_versus_software_readsample` that
/// `.agents/docs/spike-seek-decode.md` names and `src/` no longer has.
///
/// The question is narrow: does handing the Source Reader an
/// `IMFDXGIDeviceManager` move the *floor* that `playback_prefetch_first`
/// (31ms) and `playback_seek_decode` (20-30ms even at `skipped=0`) sit on? If
/// it does not, the rest of the task does not happen, and this test is the
/// evidence for closing it.
///
/// Three things make it evidence rather than a hope:
///
/// - **The arms are interleaved, not blocked.** A block of software then a
///   block of hardware confounds the arm with whatever the machine was doing
///   for those seconds (tasks README). Each round runs one of each; every
///   round's numbers are printed, so the spread *is* the instrument check.
/// - **The control is asserted, not logged.** The hardware arm's first buffer
///   must be an `IMFDXGIBuffer` and the software arm's must not. A manager the
///   reader quietly ignored would otherwise read as "hardware is no faster",
///   which is the one wrong answer that closes the task.
/// - **The fixture is in memory.** `memory_byte_stream`, so none of the 56ms
///   `playback_open_segment` (HDD) is in any of these numbers.
///
/// Nothing here asserts a win. A hardware arm that ties is a result the task
/// accepts, and the numbers are the deliverable either way.
#[test]
#[ignore = "t260915-e6c8 Step 1: encodes a 1080p fixture, then decodes it 10 times"]
fn measures_hardware_versus_software_readsample() {
    const ROUNDS: usize = 5;

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");

    let bytes = encode_measurement_segment(1920, 1080, 180);
    println!("fixture segment: {} bytes", bytes.len());

    // The production object, not a device built here: this is the manager
    // `PlaybackWorker::decoder_manager` hands to every reader it opens, so the
    // arm below measures the path the app takes rather than a lookalike.
    let mut scaler = super::gpu::GpuScaler::new().expect("a GPU scaler on this machine");

    let mut software = Vec::new();
    let mut hardware = Vec::new();
    for round in 0..ROUNDS {
        let sw = measure_decoder_arm(&bytes, None);
        let hw = measure_decoder_arm(&bytes, Some(scaler.manager()));
        println!(
            "round {round}: software first {:.2}ms seek {:.2}ms steady {:.3}ms ({} frames) | hardware first {:.2}ms seek {:.2}ms steady {:.3}ms ({} frames)",
            sw.first_ms,
            sw.seek_ms,
            sw.steady_ms,
            sw.steady_frames,
            hw.first_ms,
            hw.seek_ms,
            hw.steady_ms,
            hw.steady_frames,
        );
        software.push(sw);
        hardware.push(hw);
    }

    let mean = |arms: &[ArmTiming], pick: fn(&ArmTiming) -> f64| {
        arms.iter().map(pick).sum::<f64>() / arms.len() as f64
    };
    for (name, arms) in [("software", &software), ("hardware", &hardware)] {
        println!(
            "{name} mean over {ROUNDS}: first {:.2}ms, seek {:.2}ms, steady {:.3}ms",
            mean(arms, |arm| arm.first_ms),
            mean(arms, |arm| arm.seek_ms),
            mean(arms, |arm| arm.steady_ms),
        );
    }

    assert!(
        hardware.iter().all(|arm| arm.dxgi),
        "the hardware arm handed back system memory: the reader ignored the D3D manager, so nothing above is a software-versus-hardware comparison"
    );
    assert!(
        software.iter().all(|arm| !arm.dxgi),
        "the software arm handed back a D3D11 texture, which it cannot do without a manager"
    );
    assert!(
        software
            .iter()
            .chain(&hardware)
            .all(|arm| arm.steady_frames > 0),
        "an arm decoded nothing after its seek"
    );

    // End to end, on the decoder's own surface rather than the one
    // `a_decoder_texture_blits_to_the_same_picture_as_its_planes` builds. Two
    // things only a real decoder has: the frame is a *slice of a texture
    // array*, and the surface is padded to the decoder's own alignment. H.264
    // decoding is bit-exact, so the planes the blit reads back have to equal
    // the software decoder's own bytes for the same frame -- byte for byte, not
    // approximately.
    //
    // t260917-aca5: `FRAMES` of them in lockstep rather than the first one, so
    // the decoder's surface pool gets to turn over and hand over a slice other
    // than 0 -- the index the copy has to honour. Every frame is compared, and
    // the slice sequence is printed: whether any non-zero slice turned up is
    // the measurement, not an assumption.
    const FRAMES: usize = 30;
    let mut soft = super::segment_reader::open_as(&bytes, 0, VideoFormat::Nv12, None)
        .expect("the software arm opens");
    let mut hard =
        super::segment_reader::open_as(&bytes, 0, VideoFormat::Nv12, Some(scaler.manager()))
            .expect("the hardware arm opens");
    let source = (hard.width, hard.height);
    let target = (source.0 / 2, source.1 / 2);
    let mut rgba = vec![0u8; target.0 as usize * target.1 as usize * 4];
    let mut slices = Vec::new();
    for frame in 0..FRAMES {
        let (_, soft_sample) = soft.next_video_sample().expect("a software frame");
        let expected = soft
            .nv12_planes(&soft_sample, None)
            .expect("system-memory planes");
        let (_, hard_sample) = hard.next_video_sample().expect("a hardware frame");
        let (texture, slice) = hard
            .dxgi_texture(&hard_sample)
            .expect("the hardware arm's frame is a decoder texture");
        slices.push(slice);
        let mut planes = Vec::new();
        assert_eq!(
            scaler.convert_texture_and_scale(
                (&texture, slice),
                source,
                target,
                &mut rgba,
                &mut planes,
                super::gpu::Readback::Now,
            ),
            super::gpu::Converted::This,
            "the blit refused the decoder's own texture (frame {frame}, slice {slice})"
        );
        assert!(
            planes == expected,
            "the NV12 read back out of the decoder's texture is not the frame the decoder \
             decoded (frame {frame}, slice {slice})"
        );
    }
    let non_zero = slices.iter().filter(|&&slice| slice != 0).count();
    println!("the decoder handed over slices {slices:?}");
    println!(
        "{FRAMES} frames byte-for-byte equal to the software arm; {non_zero} of them from a non-zero slice"
    );
}

/// Segment-boundary cost and continuity: for each consecutive pair, how long
/// the fetch + reader open + first decode takes (the stall a crossing pays on
/// the playback thread today), and whether the video/audio timestamps at the
/// seam line up with the manifest.
#[test]
#[ignore = "real buffer boundary measurement: needs LIVIA_PLAYBACK_SESSION"]
fn measures_segment_boundaries() {
    let session_id = measured_session();
    let controller = CaptureController::new();
    let manifest = controller
        .load_session(&session_id)
        .expect("session manifest loads");
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");
    let take = std::env::var("LIVIA_BOUNDARY_SEGMENTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8usize);
    let mut prev_last_video: Option<i64> = None;
    let mut prev_last_audio: Option<i64> = None;
    for segment in snapshot.segments.iter().take(take) {
        let t0 = Instant::now();
        let lease = controller
            .acquire_review_lease(Some(session_id.clone()), vec![segment.index])
            .expect("lease");
        let bytes = controller
            .read_review_segment(&lease, segment.index)
            .expect("segment bytes");
        let t_read = t0.elapsed();
        let t1 = Instant::now();
        let mut reader = open_measured(&bytes, 0).expect("segment opens");
        let t_open = t1.elapsed();
        let t2 = Instant::now();
        let first_video = reader.next_video().map(|(ts, _)| ts);
        let t_first_video = t2.elapsed();
        let t3 = Instant::now();
        let first_audio = reader.next_audio().map(|(ts, s)| (ts, s.len()));
        let t_first_audio = t3.elapsed();
        let mut frames = 1;
        let mut last_video = first_video.unwrap_or(0);
        let mut per_frame = Vec::new();
        while let Some((ts, _)) = {
            let t = Instant::now();
            let r = reader.next_video();
            per_frame.push(t.elapsed().as_micros() as u64);
            r
        } {
            frames += 1;
            last_video = ts;
        }
        let mut audio_blocks = 0usize;
        let mut audio_samples = first_audio.map(|(_, n)| n).unwrap_or(0);
        let mut last_audio = first_audio.map(|(ts, n)| ts + hns_for(n));
        while let Some((ts, s)) = reader.next_audio() {
            audio_blocks += 1;
            audio_samples += s.len();
            last_audio = Some(ts + hns_for(s.len()));
        }
        let written = manifest
            .segments
            .iter()
            .find(|s| s.index == segment.index)
            .and_then(|s| {
                // Through `buffer_root`, not a hand-rebuilt default: a machine
                // with a configured buffer folder keeps its recordings there.
                let path = CaptureController::buffer_root()
                    .join(&session_id)
                    .join(s.path.file_name()?);
                crate::encoder::dump_segment_track_summary(&path).ok()
            })
            .map(|tracks| {
                tracks
                    .iter()
                    .map(|t| {
                        format!(
                            "{}:{}smp/{:.3}s",
                            if t.is_audio { "a" } else { "v" },
                            t.sample_count,
                            t.total_sample_duration_ticks as f64 / f64::from(t.timescale.max(1))
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        per_frame.sort_unstable();
        let med = per_frame.get(per_frame.len() / 2).copied().unwrap_or(0);
        let max = per_frame.last().copied().unwrap_or(0);
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        // Gap between the previous segment's last video frame (absolute) and
        // this segment's first, and same for audio.
        let seam_v_ms = prev_last_video
            .map(|pl| (segment.start_100ns + first_video.unwrap_or(0) - pl) as f64 / 10_000.0);
        let seam_a_ms = prev_last_audio
            .zip(first_audio)
            .map(|(pa, (fa, _))| (segment.start_100ns + fa - pa) as f64 / 10_000.0);
        println!(
            "seg {:>3} {:>8}B [{written}] decl {:.3}s | read {:.1}ms open {:.1}ms firstV {:.1}ms firstA {:.1}ms | {} frames med {}us max {}us | video {:.3}..{:.3}s audio {}blk {:.3}s..{:.3}s ({:.3}s) | seam video {:?}ms audio {:?}ms",
            segment.index,
            bytes.len(),
            (segment.end_100ns - segment.start_100ns) as f64 / HNS_PER_SECOND as f64,
            ms(t_read),
            ms(t_open),
            ms(t_first_video),
            ms(t_first_audio),
            frames,
            med,
            max,
            first_video.unwrap_or(0) as f64 / HNS_PER_SECOND as f64,
            last_video as f64 / HNS_PER_SECOND as f64,
            audio_blocks + usize::from(first_audio.is_some()),
            first_audio.map(|(ts, _)| ts).unwrap_or(0) as f64 / HNS_PER_SECOND as f64,
            last_audio.unwrap_or(0) as f64 / HNS_PER_SECOND as f64,
            audio_samples as f64 / 2.0 / f64::from(AUDIO_RATE),
            seam_v_ms,
            seam_a_ms,
        );
        prev_last_video = Some(segment.start_100ns + last_video);
        prev_last_audio = last_audio.map(|la| segment.start_100ns + la);
        controller.release_review_lease(&lease);
    }
}

/// Raw ReadSample trace for one segment: every call's HRESULT, flags and
/// timestamp, so a dropped-vs-errored-vs-never-written frame can be told apart.
#[test]
#[ignore = "real buffer raw read trace: needs LIVIA_PLAYBACK_SESSION"]
fn traces_raw_video_reads() {
    use windows::Win32::Media::MediaFoundation::MF_SOURCE_READER_FIRST_VIDEO_STREAM;
    let session_id = measured_session();
    let index: u64 = std::env::var("LIVIA_TRACE_SEGMENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let controller = CaptureController::new();
    let lease = controller
        .acquire_review_lease(Some(session_id.clone()), vec![index])
        .expect("lease");
    let bytes = controller
        .read_review_segment(&lease, index)
        .expect("segment bytes");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");
    let reader = open_measured(&bytes, 0).expect("segment opens");
    let mut calls = 0;
    loop {
        let mut flags = 0u32;
        let mut timestamp = 0i64;
        let mut sample = None;
        let t = Instant::now();
        let hr = unsafe {
            reader.reader.ReadSample(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                0,
                None,
                Some(&mut flags),
                Some(&mut timestamp),
                Some(&mut sample),
            )
        };
        let (dur, disc) = sample
            .as_ref()
            .map(|s| unsafe {
                (
                    s.GetSampleDuration().unwrap_or(-1),
                    s.GetUINT32(
                        &windows::Win32::Media::MediaFoundation::MFSampleExtension_Discontinuity,
                    )
                    .unwrap_or(0),
                )
            })
            .unwrap_or((-1, 0));
        println!(
            "call {calls:>3} hr={:?} flags={flags:#x} ts={:.3}s dur={:.3}s disc={disc} sample={} took {}us",
            hr.as_ref().err().map(|e| e.code()),
            timestamp as f64 / HNS_PER_SECOND as f64,
            dur as f64 / HNS_PER_SECOND as f64,
            sample.is_some(),
            t.elapsed().as_micros()
        );
        calls += 1;
        if hr.is_err() || flags & 0x2 != 0 || calls > 200 {
            break;
        }
    }
    controller.release_review_lease(&lease);
}

// The swizzle's own correctness tests live with it, in `livia-pixels`. What
// stays here is the cost gate below: it has to be measured through the app's
// own dependency edge, which is where the dev-profile override applies.

/// Task200 cost gate: one 1080p bottom-up frame through the swizzle. Printed
/// rather than asserted (machine-dependent); the boundary test's per-frame
/// median is the number the AC reads.
#[test]
#[ignore = "timing print"]
fn measures_swizzle_cost() {
    use super::segment_reader::swizzle_bgra;
    let (w, h) = (1920u32, 1080u32);
    let source = vec![0x7Fu8; (w * h * 4) as usize];
    let _ = swizzle_bgra(&source, w, h, -(w as i32 * 4));
    let started = Instant::now();
    for _ in 0..50 {
        std::hint::black_box(swizzle_bgra(&source, w, h, -(w as i32 * 4)));
    }
    println!(
        "swizzle 1080p: {:.2}ms/frame",
        started.elapsed().as_secs_f64() * 1000.0 / 50.0
    );
}

/// Dumps one decoded frame to a PNG so the orientation and colour the reader
/// produces can be eyeballed against the capture-side thumbnail written from
/// the same texture (task200: the ADVANCED->basic Video Processor switch could
/// have changed the stride sign, and a vertical flip passes every unit test).
#[test]
#[ignore = "writes a PNG for visual inspection: needs LIVIA_PLAYBACK_SESSION and LIVIA_FRAME_PNG"]
fn dumps_a_decoded_frame_to_png() {
    let session_id = measured_session();
    let out = std::env::var("LIVIA_FRAME_PNG").expect("set LIVIA_FRAME_PNG to the output path");
    let index: u64 = std::env::var("LIVIA_TRACE_SEGMENT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let controller = CaptureController::new();
    let lease = controller
        .acquire_review_lease(Some(session_id.clone()), vec![index])
        .expect("lease");
    let bytes = controller
        .read_review_segment(&lease, index)
        .expect("segment bytes");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");
    let mut reader = open_measured(&bytes, 0).expect("segment opens");
    let (timestamp, rgba) = reader.next_video().expect("a frame decodes");
    println!(
        "segment {index} frame at {:.3}s, {}x{}, stride {} -> {out}",
        timestamp as f64 / HNS_PER_SECOND as f64,
        reader.width,
        reader.height,
        reader.stride,
    );
    image::RgbaImage::from_raw(reader.width, reader.height, rgba)
        .expect("frame sizes match")
        .save(&out)
        .expect("png writes");
    controller.release_review_lease(&lease);
}

/// Decodes a segment's audio straight off disk and reports how the first
/// access unit sits against the ones after it (task204). Splitting the block
/// into eighths is what tells priming loss from a splice: overlap-add loss is
/// a sin^2 ramp up from near silence across the block, not a step at its edge.
///
/// Returns (level of first AU relative to the following blocks, per-eighth
/// levels relative to the same), both in dB.
#[cfg(test)]
fn seam_shape(pcm: &[f32], audio_offset_100ns: i64) -> Option<(f64, Vec<f64>)> {
    const AU: usize = 1024 * AUDIO_CHANNELS as usize;
    // A primed segment carries a duplicate of the previous segment's last unit
    // at its head; the reader drops it, so the shape that matters starts after
    // it -- exactly what playback will hear.
    let pcm = if audio_offset_100ns > 0 {
        pcm.get(AU..)?
    } else {
        pcm
    };
    if pcm.len() < AU * 3 {
        return None;
    }
    let db = |window: &[f32]| -> f64 {
        let sum: f64 = window.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
        20.0 * (sum / window.len().max(1) as f64).sqrt().max(1e-12).log10()
    };
    let following = db(&pcm[AU..AU * 3]);
    let eighths = pcm[..AU]
        .chunks(AU / 8)
        .map(|chunk| db(chunk) - following)
        .collect();
    Some((db(&pcm[..AU]) - following, eighths))
}

/// Task204 end-to-end: segments written *with* priming must decode without the
/// fade-in. Point `LIVIA_SEGMENT_DIR` at a directory of segment mp4s -- e.g.
/// what `task204_each_segment_after_the_first_opens_with_a_priming_access_unit`
/// leaves behind when run with `LIVIA_KEEP_ACCEPTANCE_OUTPUT=1` -- and
/// `LIVIA_AUDIO_OFFSET=1` to say they carry priming.
#[test]
#[ignore = "task204 primed-segment check: needs LIVIA_SEGMENT_DIR"]
fn measures_the_audio_of_segments_on_disk() {
    let dir = std::env::var("LIVIA_SEGMENT_DIR").expect("set LIVIA_SEGMENT_DIR");
    let primed: i64 = std::env::var("LIVIA_AUDIO_OFFSET")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .expect("segment dir")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "mp4"))
        .collect();
    paths.sort();
    let mut worst_first_eighth = f64::NEG_INFINITY;
    let mut checked = 0;
    for (position, path) in paths.iter().enumerate() {
        let bytes = std::fs::read(path).expect("segment reads");
        let mut reader = open_measured(&bytes, 0).expect("segment opens");
        let mut pcm = Vec::new();
        while let Some((_, block)) = reader.next_audio() {
            pcm.extend_from_slice(&block);
        }
        // The first segment of a recording has no predecessor to prime from.
        let offset = if position == 0 { 0 } else { primed };
        let Some((first, eighths)) = seam_shape(&pcm, offset) else {
            println!("{}: too little audio", path.display());
            continue;
        };
        println!(
            "{}: first AU {first:+.1}dB vs following; eighths {}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            eighths
                .iter()
                .map(|d| format!("{d:+.1}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        if position > 0 {
            worst_first_eighth = worst_first_eighth.max(-eighths[0]);
            checked += 1;
        }
    }
    assert!(checked > 0, "no segment after the first had audio");
    println!("worst first-eighth dip after a seam: {worst_first_eighth:.1}dB");
}

/// Task204: is the click at a seam an AAC priming loss?
///
/// Each segment is decoded by its own fresh Source Reader, so the first access
/// unit of segment N+1 has no predecessor to overlap-add with and its 1024
/// samples come out attenuated. This measures that directly: the level of the
/// first decoded block against the blocks that follow it, and the step across
/// the seam against the steps normally found inside a block.
#[test]
#[ignore = "task204 seam audio measurement: needs LIVIA_PLAYBACK_SESSION"]
fn measures_the_audio_across_a_seam() {
    let session_id = measured_session();
    let controller = CaptureController::new();
    let manifest = controller
        .load_session(&session_id)
        .expect("session manifest loads");
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");

    let decode = |index: u64| -> Vec<f32> {
        let lease = controller
            .acquire_review_lease(Some(session_id.clone()), vec![index])
            .expect("lease");
        let bytes = controller
            .read_review_segment(&lease, index)
            .expect("segment bytes");
        let mut reader = open_measured(&bytes, 0).expect("segment opens");
        let mut pcm = Vec::new();
        while let Some((_, block)) = reader.next_audio() {
            pcm.extend_from_slice(&block);
        }
        controller.release_review_lease(&lease);
        pcm
    };
    // Root-mean-square of one channel-interleaved window, in dBFS.
    let db = |window: &[f32]| -> f64 {
        if window.is_empty() {
            return f64::NEG_INFINITY;
        }
        let sum: f64 = window.iter().map(|s| f64::from(*s) * f64::from(*s)).sum();
        20.0 * (sum / window.len() as f64).sqrt().max(1e-12).log10()
    };
    const AU: usize = 1024 * AUDIO_CHANNELS as usize;

    let mut reported = 0;
    for pair in snapshot.segments.windows(2).take(6) {
        let (previous, next) = (decode(pair[0].index), decode(pair[1].index));
        if previous.is_empty() || next.len() < AU * 3 {
            println!("segment {} -> {}: no audio", pair[0].index, pair[1].index);
            continue;
        }
        let first = db(&next[..AU]);
        let following = db(&next[AU..AU * 3]);
        // The step across the seam, against the largest step inside the block
        // that follows it -- a click is a discontinuity far bigger than the
        // waveform's own slope.
        let seam_step = (next[0] - previous[previous.len() - AUDIO_CHANNELS as usize]).abs();
        let inner_step = next[..AU]
            .windows(AUDIO_CHANNELS as usize + 1)
            .map(|w| (w[AUDIO_CHANNELS as usize] - w[0]).abs())
            .fold(0.0f32, f32::max);
        // The shape inside the first AU is what tells priming loss from a
        // splice: overlap-add loss is a sin^2 ramp up from near zero over the
        // block, not a step at its edge. Printed against the level the blocks
        // after it settle at.
        let chunks: Vec<String> = next[..AU]
            .chunks(AU / 8)
            .map(|chunk| format!("{:.1}", db(chunk) - following))
            .collect();
        println!(
            "segment {} -> {}: first AU {first:.1}dB, following {following:.1}dB \
             (delta {:.1}dB), seam step {seam_step:.4} vs largest inner step {inner_step:.4}\n\
             \x20   first-AU eighths vs following (dB): {}",
            pair[0].index,
            pair[1].index,
            first - following,
            chunks.join(" "),
        );
        reported += 1;
    }
    assert!(reported > 0, "no seam had audio on both sides");
}

fn hns_for(interleaved_samples: usize) -> i64 {
    (interleaved_samples as i64 / 2) * HNS_PER_SECOND / i64::from(AUDIO_RATE)
}

/// Drives the real engine over a real recorded session and prints the
/// numbers task128's Acceptance Criteria ask for: sustained fps, dropped
/// frames, and how long forward/backward seeks take. Manual because it
/// needs a populated `%LOCALAPPDATA%\Liveback\buffer` and decodes in
/// real time.
#[test]
#[ignore = "real buffer playback measurement (task128): set LIVIA_PLAYBACK_SESSION \
                to a session id under %LOCALAPPDATA%\\Liveback\\buffer and run with \
                --ignored --nocapture"]
fn measures_playback_over_a_real_session() {
    let session_id = measured_session();
    let controller = CaptureController::new();
    let manifest = controller
        .load_session(&session_id)
        .expect("session manifest loads");
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let start = snapshot.start_100ns();
    println!(
        "session {session_id}: {} segments, {:.1}s, live edge {:.1}s",
        snapshot.segments.len(),
        snapshot.full_duration_100ns() as f64 / HNS_PER_SECOND as f64,
        snapshot.live_edge_100ns as f64 / HNS_PER_SECOND as f64,
    );

    let engine = PlaybackEngine::start(
        controller.clone(),
        snapshot.clone(),
        start,
        100,
        false,
        None,
        Box::new(|| {}),
    );
    // The first frame is decoded during construction; wait for it so the
    // measurement below is playback, not startup.
    thread::sleep(Duration::from_millis(500));
    assert!(engine.is_alive(), "engine thread died during startup");
    println!("audio output available: {}", status(&engine).audio_enabled);

    // ---- sustained playback ----
    engine.send(PlaybackCommand::Play);
    let measured = Duration::from_secs(30);
    // Sample the endpoint while it plays: a queue that stays non-empty is
    // the device actually consuming audio, which a "wrote some bytes"
    // counter alone would not show.
    let mut audio_samples = Vec::new();
    let sampling = Instant::now();
    while sampling.elapsed() < measured {
        thread::sleep(Duration::from_millis(500));
        audio_samples.push(status(&engine).audio_queued_ms);
    }
    let playing = status(&engine);
    let non_empty = audio_samples.iter().filter(|ms| **ms > 0).count();
    println!(
        "audio: {} of {} polls had a non-empty endpoint queue (min {:?}ms, max {:?}ms), \
             {} frames written ({:.1}s of audio at {AUDIO_RATE}Hz)",
        non_empty,
        audio_samples.len(),
        audio_samples.iter().min(),
        audio_samples.iter().max(),
        playing.audio_frames_written,
        playing.audio_frames_written as f64 / f64::from(AUDIO_RATE),
    );
    engine.send(PlaybackCommand::Pause { reason: "test" });
    thread::sleep(Duration::from_millis(200));
    let elapsed = measured.as_secs_f64();
    println!(
        "playback {elapsed:.0}s: shown {} frames, late {}, last reported fps {}, \
             drift corrections {}, advanced {:.1}s",
        playing.frames_shown,
        playing.frames_late,
        playing.actual_fps,
        playing.drift_corrections,
        (playing.position_100ns - start) as f64 / HNS_PER_SECOND as f64,
    );
    assert!(
        playing.frames_shown > 0,
        "no frames were decoded -- the segments never opened"
    );

    // ---- seek latency, both directions ----
    // Backward seeks are the React engine's weak point (its fetch window
    // had to be re-centred), so they are measured explicitly.
    let report = |label: &str, target: i64| {
        // A request landing in one of this session's gaps is carried to the
        // next recorded frame, so the expected landing point is the snapped
        // one -- comparing against the raw request would just measure the
        // gap layout.
        let expected = snapshot
            .snap_to_recorded_time(target.clamp(start, snapshot.live_edge_100ns - 1))
            .expect("target snaps to a recorded time");
        let started = Instant::now();
        engine.send(PlaybackCommand::Seek {
            target_100ns: target,
            precise: true,
        });
        // Poll rather than sleep a fixed amount: the number wanted is how
        // long the seek actually took to land.
        let mut waited = Duration::ZERO;
        while waited < Duration::from_secs(10) {
            thread::sleep(Duration::from_millis(5));
            waited += Duration::from_millis(5);
            let current = status(&engine);
            if (current.position_100ns - expected).abs() < HNS_PER_SECOND {
                println!(
                    "seek {label} -> {:.1}s: landed in {}ms (engine reported {}ms)",
                    expected as f64 / HNS_PER_SECOND as f64,
                    started.elapsed().as_millis(),
                    current.last_seek_ms,
                );
                return;
            }
        }
        panic!(
            "seek {label} to {expected} never landed (position {})",
            status(&engine).position_100ns
        );
    };
    let span = snapshot.live_edge_100ns - start;
    report("forward far", start + span * 3 / 4);
    report("backward far", start + span / 8);
    report("forward near", start + span / 4);
    report("backward near", start + span / 8);
    report("to the very start", start);

    // Repeated backward seeks: the MSE engine had to re-centre its fetch
    // window for each one, which is the behaviour task119's hold cadence
    // was slowed down to accommodate. Same pattern the React baseline runs.
    let mut cursor = start + span * 9 / 10;
    for step in 0..8 {
        cursor -= 40 * HNS_PER_SECOND;
        report(&format!("repeat backward {step}"), cursor);
        thread::sleep(Duration::from_millis(400));
    }

    // ---- gap crossing ----
    let gap = snapshot
        .segments
        .windows(2)
        .find(|pair| pair[1].start_100ns > pair[0].end_100ns)
        .map(|pair| (pair[0].end_100ns, pair[1].start_100ns));
    if let Some((gap_start, gap_end)) = gap {
        println!(
            "gap {:.1}s..{:.1}s",
            gap_start as f64 / HNS_PER_SECOND as f64,
            gap_end as f64 / HNS_PER_SECOND as f64,
        );
        engine.send(PlaybackCommand::Seek {
            target_100ns: gap_start - HNS_PER_SECOND / 2,
            precise: true,
        });
        thread::sleep(Duration::from_millis(300));
        engine.send(PlaybackCommand::Play);
        thread::sleep(Duration::from_secs(3));
        let crossed = status(&engine);
        engine.send(PlaybackCommand::Pause { reason: "test" });
        println!(
            "after playing across the gap: position {:.1}s, playing {}",
            crossed.position_100ns as f64 / HNS_PER_SECOND as f64,
            crossed.playing,
        );
        assert!(
            crossed.position_100ns >= gap_end,
            "playback did not cross the gap: stuck at {}",
            crossed.position_100ns
        );
    }

    // ---- live edge ----
    engine.send(PlaybackCommand::Seek {
        target_100ns: snapshot.live_edge_100ns - HNS_PER_SECOND,
        precise: true,
    });
    thread::sleep(Duration::from_millis(300));
    engine.send(PlaybackCommand::Play);
    thread::sleep(Duration::from_secs(3));
    let parked = status(&engine);
    println!(
        "at the live edge: position {:.1}s, playing {}, at_live_edge {}",
        parked.position_100ns as f64 / HNS_PER_SECOND as f64,
        parked.playing,
        parked.at_live_edge,
    );
    assert!(engine.is_alive(), "engine thread died during the run");
}

/// A precise seek that skips the colour conversion on the frames it passes
/// must still land on the same frame, with the same pixels, as one that
/// converted every frame on the way (task207).
///
/// This is the off-by-one check. The risk the split introduces is converting a
/// different sample than the one whose timestamp was tested, which a seek would
/// show as a picture one frame away from where the playhead says it is -- and
/// which no timing measurement would catch.
#[test]
#[ignore = "real buffer decode comparison (task207): needs LIVIA_PLAYBACK_SESSION"]
fn a_seek_that_skips_conversion_lands_on_the_same_frame() {
    let session_id = measured_session();
    let controller = CaptureController::new();
    let manifest = controller
        .load_session(&session_id)
        .expect("session manifest loads");
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let segment = &snapshot.segments[snapshot.segments.len() / 2];
    let lease = controller
        .acquire_review_lease(Some(session_id.clone()), vec![segment.index])
        .expect("lease");
    let bytes = controller
        .read_review_segment(&lease, segment.index)
        .expect("segment bytes");
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let _runtime = MfRuntime::start().expect("MF starts");

    // Several targets across the segment, so this covers landing right after
    // the keyframe as well as a full GOP's worth of skipping.
    let span = segment.end_100ns - segment.start_100ns;
    for numerator in [1, 2, 3] {
        let target = span * numerator / 4;

        // The pre-task207 path: convert every frame, keep the first one at or
        // after the target.
        let mut old = open_measured(&bytes, 0).expect("segment opens");
        old.seek_local(target);
        let mut old_landing = None;
        while let Some((local, rgba)) = old.next_video() {
            if local + HNS_PER_SECOND / 60 >= target {
                old_landing = Some((local, rgba));
                break;
            }
        }

        // The task207 path: skip the conversion until the target is reached.
        let mut new = open_measured(&bytes, 0).expect("segment opens");
        new.seek_local(target);
        let mut new_landing = None;
        let mut skipped = 0;
        while let Some((local, sample)) = new.next_video_sample() {
            if local + HNS_PER_SECOND / 60 >= target {
                let rgba = new
                    .convert(&sample, None)
                    .expect("the landing frame converts");
                new_landing = Some((local, rgba));
                break;
            }
            skipped += 1;
        }

        let (old_ts, old_rgba) = old_landing.expect("the old path lands somewhere");
        let (new_ts, new_rgba) = new_landing.expect("the new path lands somewhere");
        println!(
            "target {:.3}s: landed at {:.3}s both ways, {skipped} frames skipped, {} bytes",
            target as f64 / HNS_PER_SECOND as f64,
            new_ts as f64 / HNS_PER_SECOND as f64,
            new_rgba.len(),
        );
        assert_eq!(
            old_ts, new_ts,
            "skipping the conversion moved the landing frame"
        );
        assert!(
            old_rgba == new_rgba,
            "same timestamp but different pixels -- the converted sample is not the one that was tested"
        );
        // A target a quarter into the segment has to pass frames to get there;
        // if nothing was skipped this test proved nothing.
        assert!(skipped > 0, "no frames were skipped, so nothing was tested");
    }
    controller.release_review_lease(&lease);
}

/// Task1270, on a real multi-track recording: point
/// `LIVIA_TASK1270_CONTAINER` at a `.lvb` recorded with several audio tracks
/// and this decodes each track of one segment separately and reports how loud
/// it is.
///
/// The point is **separation**, measured rather than listened to: the tracks
/// hold different applications, so their levels have to differ. It also runs
/// the mix both ways -- everything on, and one track muted -- and checks the
/// muted one contributes nothing.
#[test]
#[ignore = "real recording measurement (task1270): set LIVIA_TASK1270_CONTAINER to a \
            multi-track .lvb and run with --ignored --nocapture"]
fn task1270_reports_track_levels_of_a_recorded_container_when_requested() {
    use crate::playback::mixer::{clip, track_scale, TrackAudioBuffer};

    let path = std::env::var("LIVIA_TASK1270_CONTAINER")
        .expect("LIVIA_TASK1270_CONTAINER names the .lvb to measure");
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        );
    }
    let _runtime = crate::encoder::MfRuntime::start().expect("Media Foundation");
    let mut container = crate::ring_buffer::container::ContainerReader::open_for_reading(
        std::path::Path::new(&path),
    )
    .expect("the container opens");
    let snapshot = container.snapshot().clone();
    // The middle segment, so any lead-in silence at the head of the recording
    // is not what gets measured.
    let segment = snapshot.segments[snapshot.segments.len() / 2].clone();
    let bytes = container
        .read_segment(segment.index)
        .expect("segment bytes");
    let mut reader = match crate::playback::segment_reader::open_segment(&bytes, 0) {
        Ok(reader) => reader,
        Err(error) => panic!(
            "segment {} ({} bytes) did not open: {error}",
            segment.index,
            bytes.len()
        ),
    };
    let tracks = reader.audio_streams.len();
    println!(
        "task1270: segment {} has {tracks} audio tracks, reader stream order {:?}",
        segment.index, reader.audio_streams
    );
    assert!(tracks >= 2, "this test needs a multi-track recording");

    let rms = |samples: &[f32]| -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        (samples
            .iter()
            .map(|s| f64::from(*s) * f64::from(*s))
            .sum::<f64>()
            / samples.len() as f64)
            .sqrt()
    };

    let mut decoded: Vec<Vec<f32>> = Vec::new();
    for track in 0..tracks {
        let mut all = Vec::new();
        while let Some((_, block)) = reader.next_audio_on(track) {
            all.extend_from_slice(&block);
        }
        println!(
            "task1270: track {track}: {} samples, rms {:.5}",
            all.len(),
            rms(&all)
        );
        decoded.push(all);
    }
    for (track, samples) in decoded.iter().enumerate() {
        assert!(!samples.is_empty(), "track {track} decoded to nothing");
    }
    // Different applications, therefore different audio. What matters is that
    // the tracks are not the *same* stream twice -- an application that
    // happens to be quiet still has to be its own track, so this compares
    // content rather than loudness.
    let differing = decoded[0]
        .iter()
        .zip(decoded[1].iter())
        .filter(|(a, b)| (*a - *b).abs() > 1e-9)
        .count();
    let peak = |samples: &[f32]| samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
    println!(
        "task1270: {differing} of {} samples differ; peaks {:.5} / {:.5}",
        decoded[0].len().min(decoded[1].len()),
        peak(&decoded[0]),
        peak(&decoded[1])
    );
    assert!(
        differing > 0,
        "both tracks decoded to identical audio, so they are not separate streams"
    );

    // And the mix behaves: track 1 audible, then muted.
    let frames = 480usize * 2;
    let base: Vec<f32> = decoded[0].iter().copied().take(frames).collect();
    let extra: Vec<f32> = decoded[1].iter().copied().take(frames).collect();
    let mix = |gain: f32| -> Vec<f32> {
        let mut out = base.clone();
        let mut buffer = TrackAudioBuffer::new();
        buffer.push(0, &extra);
        buffer.mix_into(0, &mut out, gain);
        clip(&mut out);
        out
    };
    let audible = mix(track_scale(100, false));
    let muted = mix(track_scale(100, true));
    println!(
        "task1270: mixed rms audible {:.5}, muted {:.5}, target alone {:.5}",
        rms(&audible),
        rms(&muted),
        rms(&base)
    );
    assert!(
        (rms(&muted) - rms(&base)).abs() < 1e-9,
        "a muted track must contribute nothing"
    );
    assert!(
        audible
            .iter()
            .zip(base.iter())
            .any(|(mixed, alone)| (*mixed - *alone).abs() > 1e-9),
        "an audible track must change the mix"
    );
}

/// Task1640: which returned buffer the engine may write the next frame into.
/// Only an exact fit -- a spare from a differently sized recording would be
/// written short and published with the rest of the previous picture in it.
#[test]
fn only_an_exactly_sized_spare_is_taken_back() {
    let bytes = 1920 * 1080 * 4;
    let mut slot = Some(vec![0u8; bytes]);
    assert_eq!(
        super::take_fitting(&mut slot, bytes).map(|spare| spare.len()),
        Some(bytes)
    );
    assert!(slot.is_none(), "the taken spare leaves the slot empty");

    // Nothing returned yet: the engine allocates, it never waits.
    assert!(super::take_fitting(&mut slot, bytes).is_none());

    // A misfit is refused *and* dropped -- keeping it would block every later
    // frame from being recycled.
    let mut slot = Some(vec![0u8; 1280 * 720 * 4]);
    assert!(super::take_fitting(&mut slot, bytes).is_none());
    assert!(slot.is_none(), "a misfit is not left behind");
}

/// t260914-862b: the two full-frame variants have their own slots, so a frame
/// returned by one path never evicts the other path's spare.
///
/// Both variants go in and both come out in the same run -- an "it is not
/// there" assert alone would pass on an engine that simply kept no spares at
/// all. The recycles are interleaved with the takes in both orders, because a
/// single shared slot is latest-wins: whichever variant arrived second would be
/// the one that survives, and testing one order would let that half pass.
#[test]
fn each_full_frame_variant_keeps_its_own_spare() {
    const WIDTH: usize = 4;
    const HEIGHT: usize = 2;
    const RGBA_BYTES: usize = WIDTH * HEIGHT * 4;
    const NV12_BYTES: usize = WIDTH * HEIGHT * 3 / 2;

    let shared = PlaybackShared::default();
    shared.recycle_full(FullFrame::Rgba(vec![0u8; RGBA_BYTES]));
    shared.recycle_full(FullFrame::nv12(vec![0u8; NV12_BYTES]));
    assert_eq!(
        shared.take_nv12_spare(NV12_BYTES).map(|spare| spare.len()),
        Some(NV12_BYTES),
        "the NV12 planes must survive an RGBA frame coming back first"
    );
    assert_eq!(
        shared.take_spare(RGBA_BYTES).map(|spare| spare.len()),
        Some(RGBA_BYTES),
        "the RGBA buffer must not have been evicted by the NV12 planes"
    );

    // The other order, on a fresh pair of slots: same claim, opposite arrival.
    let shared = PlaybackShared::default();
    shared.recycle_full(FullFrame::nv12(vec![0u8; NV12_BYTES]));
    shared.recycle_full(FullFrame::Rgba(vec![0u8; RGBA_BYTES]));
    assert_eq!(
        shared.take_spare(RGBA_BYTES).map(|spare| spare.len()),
        Some(RGBA_BYTES)
    );
    assert_eq!(
        shared.take_nv12_spare(NV12_BYTES).map(|spare| spare.len()),
        Some(NV12_BYTES)
    );

    // The GPU path's own door for the bare planes it holds before a frame is
    // built around them (the failed-blit return) reaches the same slot.
    shared.recycle_nv12(vec![0u8; NV12_BYTES]);
    assert_eq!(
        shared.take_nv12_spare(NV12_BYTES).map(|spare| spare.len()),
        Some(NV12_BYTES)
    );
    assert!(
        shared.take_spare(NV12_BYTES).is_none(),
        "the RGBA slot never sees the planes"
    );
}

/// t260917-efbe: `release_spares` empties all three slots and says how much
/// each held. The control is the same fill without the call: every slot still
/// hands its buffer out, so an empty result below is the release and not slots
/// that never kept anything.
#[test]
fn release_spares_empties_all_three_slots() {
    const RGBA: usize = 16 * 8 * 4;
    const NV12: usize = 16 * 8 * 3 / 2;
    const SCALED: usize = 8 * 4 * 4;
    let fill = |shared: &PlaybackShared| {
        shared.recycle(vec![0u8; RGBA]);
        shared.recycle_nv12(vec![0u8; NV12]);
        shared.recycle_scaled(vec![0u8; SCALED]);
    };
    let take_all = |shared: &PlaybackShared| {
        [
            shared.take_spare(RGBA).map(|spare| spare.len()),
            shared.take_nv12_spare(NV12).map(|spare| spare.len()),
            shared.take_scaled_spare(SCALED).map(|spare| spare.len()),
        ]
    };

    let kept = PlaybackShared::default();
    fill(&kept);
    assert_eq!(take_all(&kept), [Some(RGBA), Some(NV12), Some(SCALED)]);

    let released = PlaybackShared::default();
    fill(&released);
    assert_eq!(released.release_spares(), [RGBA, NV12, SCALED]);
    assert_eq!(take_all(&released), [None, None, None]);
    assert_eq!(
        released.release_spares(),
        [0, 0, 0],
        "a second release finds nothing, which is what keeps its log line to one per quiet period"
    );
}

/// Task1640: the recycled buffer arrives dirty, so a sample too short for the
/// frame it claims must leave black rows behind rather than the last frame's.
#[test]
fn a_short_sample_blacks_out_the_rows_it_cannot_fill() {
    use super::segment_reader::{nv12_to_rgba_into, swizzle_bgra_into};
    let (width, height) = (64u32, 32u32);
    let bytes = (width * height * 4) as usize;

    let mut recycled = vec![0xCDu8; bytes];
    let half = vec![0x80u8; (width * height / 2) as usize * 4];
    swizzle_bgra_into(&mut recycled, &half, width, height, width as i32 * 4);
    assert!(
        recycled[bytes / 2..].iter().all(|byte| *byte == 0),
        "rows the source could not supply are black, not the previous frame"
    );

    let mut recycled = vec![0xCDu8; bytes];
    // Y plane only: every chroma row is missing, so nothing can be converted.
    let luma_only = vec![0x40u8; (width * height) as usize];
    nv12_to_rgba_into(&mut recycled, &luma_only, width, height, width);
    assert!(
        recycled.iter().all(|byte| *byte == 0),
        "an NV12 sample with no chroma leaves black, not the previous frame"
    );
}

/// Task1900's repack, on synthesized layouts so it runs without Media
/// Foundation and without a recording. The defect it exists for: Windows' MP4
/// source silently ends the presentation at a `moof` that crosses one of its
/// 64KiB read boundaries.
mod unstraddle_tests {
    use crate::encoder::mp4_boxes::unstraddle_fragments;

    const GRID: usize = 64 * 1024;
    /// Big enough that a fragment placed one byte before a boundary crosses it.
    const MOOF_LEN: usize = 600;

    fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A `MOOF_LEN`-byte `moof` whose single `traf` is `default-base-is-moof`.
    fn moof() -> Vec<u8> {
        let mut tfhd = 0x0002_0000u32.to_be_bytes().to_vec(); // default-base-is-moof
        tfhd.extend_from_slice(&1u32.to_be_bytes()); // track id
        let traf = boxed(b"traf", &boxed(b"tfhd", &tfhd));
        let mut body = traf;
        body.extend_from_slice(&boxed(b"free", &vec![0u8; MOOF_LEN - body.len() - 16]));
        let out = boxed(b"moof", &body);
        assert_eq!(out.len(), MOOF_LEN);
        out
    }

    /// `moov`, then a `moof`/`mdat` pair starting at each of `at` -- so a test
    /// says where a fragment sits rather than deriving it from box arithmetic.
    fn file(at: &[usize]) -> Vec<u8> {
        let mut out = boxed(b"moov", &vec![0u8; 1000]);
        for (index, start) in at.iter().enumerate() {
            let gap = start.checked_sub(out.len()).expect("fragments in order");
            assert!(gap >= 8, "no room for the filler before fragment {index}");
            out.extend_from_slice(&boxed(b"free", &vec![0u8; gap - 8]));
            assert_eq!(out.len(), *start);
            out.extend_from_slice(&moof());
            out.extend_from_slice(&boxed(b"mdat", &vec![0u8; 2_000]));
        }
        out
    }

    fn moof_offsets(bytes: &[u8]) -> Vec<(usize, usize)> {
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

    fn straddling(bytes: &[u8]) -> Vec<usize> {
        moof_offsets(bytes)
            .into_iter()
            .filter(|(at, size)| at / GRID != (at + size - 1) / GRID)
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
        let mut bytes = boxed(b"moov", &vec![0u8; 1000]);
        let starts = [GRID - 100, 3 * GRID - 200];
        for (index, start) in starts.iter().enumerate() {
            let gap = start - bytes.len();
            bytes.extend_from_slice(&boxed(b"free", &vec![0u8; gap - 8]));
            bytes.extend_from_slice(&moof());
            bytes.extend_from_slice(&boxed(b"mdat", &vec![index as u8 + 1; 2_000]));
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

/// The clip engine: one standalone mp4, opened through [`PlaybackEngine::start_file`]
/// rather than through a ring-buffer lease (task3500).
///
/// Runs on the 900-byte two-frame export `clips::tests` already keeps as hex --
/// a real file the MP4 source resolves, with no encode session and no ffmpeg on
/// PATH. It is video-only and 0.07s long, so what it can prove is the seam:
/// the file opens, a frame reaches the mailbox, the transport answers commands,
/// and teardown returns. Audio and a real seek need a real clip and live in the
/// task's `Verification`.
#[cfg(test)]
mod clip_engine_tests {
    use super::*;
    use crate::clips::tests::{cleanup, fixture};

    /// How long a step gets before it counts as hung. Generous: the first open
    /// pays a cold MF startup on the engine thread.
    const PATIENCE: Duration = Duration::from_secs(10);

    fn settle(engine: &PlaybackEngine, what: &str, mut done: impl FnMut(PlaybackStatus) -> bool) {
        let deadline = Instant::now() + PATIENCE;
        while Instant::now() < deadline {
            if done(status(engine)) {
                return;
            }
            assert!(
                engine.is_alive(),
                "the engine thread died waiting for {what}"
            );
            thread::sleep(Duration::from_millis(10));
        }
        panic!("timed out waiting for {what}: {:?}", status(engine));
    }

    #[test]
    fn a_standalone_clip_opens_publishes_and_answers_commands() {
        let path = fixture("playback-engine");
        let duration_100ns = crate::clips::details(&path)
            .duration_100ns
            .expect("the fixture export reports a duration");
        assert!(duration_100ns > 0, "duration {duration_100ns}");

        let engine = PlaybackEngine::start_file(
            path.clone(),
            duration_100ns,
            0,
            100,
            false,
            None,
            Box::new(|| {}),
        );

        // The poster frame: `Worker::new` seeks to the initial position, which
        // opens the file and decodes one picture before `run` even starts.
        settle(&engine, "the first frame", |_| {
            engine.shared().frame.lock().unwrap().is_some()
        });
        {
            let shared = engine.shared();
            let frame = shared.frame.lock().unwrap();
            let frame = frame.as_ref().expect("checked above");
            assert!(
                frame.width > 0 && frame.height > 0,
                "{}x{}",
                frame.width,
                frame.height
            );
            assert_eq!(
                frame.rgba.len(),
                frame.width as usize * frame.height as usize * 4,
                "RGBA buffer does not match {}x{}",
                frame.width,
                frame.height
            );
        }

        // Seek: acked with the exact target the command carried (task2910).
        // Half-way rather than the end, which the transport clamps.
        let target = duration_100ns / 2;
        engine.send(PlaybackCommand::Seek {
            target_100ns: target,
            precise: true,
        });
        settle(&engine, "the seek ack", |status| {
            status.acked_seek_100ns == Some(target)
        });

        // Play, then Pause. `playing` is deliberately not the only accepted
        // answer: the fixture is two frames, so the transport can reach the end
        // and stop between the command and the first poll -- a frame having
        // been shown is the same evidence that Play ran.
        //
        // The position is the third witness, and the one this needs (measured
        // 2026-09-13: without it the step failed about one run in three). The
        // seek above lands on the *last* of the two frames and publishes it, so
        // playing from there has no further frame to show: `frames_shown` never
        // moves, and the transport is back at the end well inside the 10ms poll
        // interval. A paused transport does not move the position, so a position
        // past where the seek left it can only mean Play ran.
        let before = status(&engine);
        engine.send(PlaybackCommand::Play);
        settle(&engine, "play", |status| {
            status.playing
                || status.frames_shown > before.frames_shown
                || status.position_100ns > before.position_100ns
        });
        engine.send(PlaybackCommand::Pause { reason: "test" });
        settle(&engine, "pause", |status| !status.playing);
        // Nothing observable off a headless endpoint; what is asserted is that
        // the command does not take the engine down with it.
        engine.send(PlaybackCommand::SetVolume {
            percent: 40,
            muted: false,
        });
        settle(&engine, "the volume command", |_| true);
        assert!(engine.is_alive(), "the engine died on SetVolume");

        // Shutdown joins, and joins *bounded*: `Drop` releases `stop_tx` before
        // the join so a worker asleep on a frame deadline wakes for it
        // (task1620). An unbounded join is the UI-thread freeze AC 4 is about.
        let teardown = Instant::now();
        drop(engine);
        let took = teardown.elapsed();
        assert!(
            took < PATIENCE,
            "dropping the clip engine took {took:?}, which is not a bounded join"
        );

        cleanup(&path);
    }

    /// t260915-1fc3: why the clips screen drops its engine to write a comment.
    /// The stage target is on the engine by the time `start_file` returns
    /// (t260915-c1ca), not by the time its worker gets round to reading it.
    ///
    /// No sleep and no `settle` before either `display_target` assert: the
    /// claim is that the value was stored before the worker thread existed, so
    /// it cannot depend on how far that thread has got. `None` is the control
    /// that the call carries what it was given rather than a default, and it
    /// also measures the fixture's own size. The poster assert is the point of
    /// the change: `Worker::new` decodes that frame before `run` starts, and it
    /// comes out at the target, every time, with nothing to race.
    #[test]
    fn the_stage_target_is_on_the_engine_before_start_returns() {
        let path = fixture("playback-target");
        let duration_100ns = crate::clips::details(&path)
            .duration_100ns
            .expect("the fixture export reports a duration");
        let open = |target: Option<(u32, u32)>| {
            PlaybackEngine::start_file(
                path.clone(),
                duration_100ns,
                0,
                100,
                false,
                target,
                Box::new(|| {}),
            )
        };

        let bare = open(None);
        assert_eq!(bare.shared().display_target(), None);
        settle(&bare, "the full-resolution poster", |_| {
            bare.shared().frame.lock().unwrap().is_some()
        });
        let (width, height) = {
            let shared = bare.shared();
            let frame = shared.frame.lock().unwrap();
            let frame = frame.as_ref().expect("checked above");
            // No target, so nothing to resample. Checked as "full-res", not as
            // `full.is_none()`: since t260917-d792 a decoder-texture poster is
            // blitted 1:1 on the GPU and carries its NV12 in `full` at the
            // same size; only the CPU pair leaves `full` empty.
            if let Some((full_width, full_height, _)) = &frame.full {
                assert_eq!(
                    (*full_width, *full_height),
                    (frame.width, frame.height),
                    "no target, so nothing to resample"
                );
            }
            (frame.width, frame.height)
        };
        drop(bare);
        assert!(width >= 2 && height >= 2, "{width}x{height}");

        let target = (width / 2, height / 2);
        let engine = open(Some(target));
        assert_eq!(engine.shared().display_target(), Some(target));
        settle(&engine, "the resampled poster", |_| {
            engine.shared().frame.lock().unwrap().is_some()
        });
        {
            let shared = engine.shared();
            let frame = shared.frame.lock().unwrap();
            let frame = frame.as_ref().expect("checked above");
            assert_eq!((frame.width, frame.height), target);
            assert!(frame.full.is_some(), "the poster was resampled");
        }
        drop(engine);
        cleanup(&path);
    }

    /// The shell's mp4 property handler opens the store read-write exclusively
    /// (t260914-b680), so while an engine holds the file the write is refused;
    /// once `Drop` has joined the worker the same write goes through; and an
    /// engine opened again at a position comes back there, on the written file.
    #[test]
    fn a_clip_comment_is_written_between_dropping_and_reopening_the_engine() {
        let path = fixture("playback-comment");
        let duration_100ns = crate::clips::details(&path)
            .duration_100ns
            .expect("the fixture export reports a duration");
        let open = |at: i64| {
            PlaybackEngine::start_file(
                path.clone(),
                duration_100ns,
                at,
                100,
                false,
                None,
                Box::new(|| {}),
            )
        };

        let engine = open(0);
        // The file is opened by the first decode, so the refusal is only
        // meaningful once a frame exists.
        settle(&engine, "the first frame", |_| {
            engine.shared().frame.lock().unwrap().is_some()
        });
        let from_start = status(&engine).position_100ns;
        let refused = crate::clips::write_comment(&path, "held");
        println!("while the engine holds the clip: {refused:?}");
        assert!(refused.is_err(), "the engine's hold must refuse the write");

        drop(engine);
        crate::clips::write_comment(&path, "released")
            .expect("the same write, once the engine let go");
        assert_eq!(
            crate::clips::read_comment(&path).as_deref(),
            Some("released")
        );

        let target = duration_100ns / 2;
        let engine = open(target);
        settle(&engine, "the reopened first frame", |_| {
            engine.shared().frame.lock().unwrap().is_some()
        });
        let landed = status(&engine).position_100ns;
        println!("opened at 0 -> {from_start}; reopened at {target} -> {landed} (duration {duration_100ns})");
        // A precise seek may land on the frame's own time rather than the tick
        // asked for, so: not the start (the first open is the control), and
        // inside the clip.
        assert_eq!(from_start, 0);
        assert!(
            landed > 0 && landed <= duration_100ns,
            "reopened at {target}, landed at {landed}"
        );
        drop(engine);
        cleanup(&path);
    }
}
