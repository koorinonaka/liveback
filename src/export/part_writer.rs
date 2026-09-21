use std::{
    path::Path,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFSample, IMFSourceReader, MF2DBuffer_LockFlags_Read, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFVideoFormat_NV12, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::{capture, encoder, ring_buffer};

use super::plan::check_cancel;
use super::probe::{
    audio_stream_indices, open_reader, probe_source_types, video_fingerprint, SourceTypes,
    VideoFingerprint, BOUNDARY_FRAME_RATE_CANDIDATES,
};
use super::{ExportDiagnostics, ExportFailure, ExportPart, AAC_MAX_ERROR_100NS};

#[cfg(test)]
pub(super) fn export_part(
    part: &ExportPart,
    partial: &Path,
    _final_path: &Path,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<(), ExportFailure> {
    export_part_reporting(part, partial, _final_path, cancel, diagnostics, None, None)
}

/// The same, with a callback that says how far into this part the writer has
/// got, in 100ns from the part's start (task1350).
///
/// Without it a gapless recording -- which is nearly every recording, since
/// task063 keeps the audio continuous and segments therefore rarely break --
/// is one part, and the only progress the UI ever saw was the one published
/// after the part finished. Measured: 24 seconds of "書き出し中 0%".
pub(super) fn export_part_reporting(
    part: &ExportPart,
    partial: &Path,
    _final_path: &Path,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
    phase: Option<&dyn Fn(bool)>,
    progress: Option<&dyn Fn(i64)>,
) -> Result<(), ExportFailure> {
    // A single MfRuntime (MFStartup/MFShutdown pair) held for this whole
    // export, instead of each per-segment helper starting and shutting down
    // Media Foundation on its own (task083). The previous per-call pattern
    // cycled MFStartup/MFShutdown repeatedly within a single export (once
    // for probing, then twice per segment), and every real crash captured
    // in Windows Event Viewer faulted at the identical offset inside
    // mfreadwrite.dll -- consistent with MF's internal work queues not
    // being fully quiesced by one MFShutdown before the very next
    // MFStartup reinitializes, not with a bug in this app's own safe code.
    let runtime = encoder::MfRuntime::start()?;
    let source_types = probe_source_types(&part.segments, &runtime)?;
    let frame_size = source_types.video_fingerprint.frame_size;
    let config = encoder::EncoderConfig {
        output_dir: partial
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf(),
        output_size: capture::CaptureSize {
            width: (frame_size >> 32) as i32,
            height: frame_size as u32 as i32,
        },
        frame_rate: source_types.video_fingerprint.frame_rate,
    };
    let writer = encoder::Mp4ExportWriter::create(
        &source_types.video,
        &source_types.audio,
        partial.to_path_buf(),
    )?;
    write_sink_writer_part(
        &writer,
        part,
        progress,
        &source_types,
        &config,
        &runtime,
        cancel,
        diagnostics,
    )?;
    check_cancel(cancel)?;
    if let Some(phase) = phase {
        phase(true);
    }
    writer.finalize()?;
    Ok(())
}

/// Which stream of the output a sample belongs to (task1280). Ordered so
/// video sorts before audio at the same timestamp, and audio tracks sort among
/// themselves in track order -- the order that *is* their identity in an MP4.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum StreamRef {
    Video,
    Audio(usize),
}

/// Puts one segment's collected samples into the order they are written in:
/// by timestamp, then video before audio, then audio track order. The sink
/// interleaves what it is given, and a stable order keeps an export
/// reproducible.
///
/// This is the writer's only ordering step, so it is also what keeps the
/// muxer's timestamps monotonic. Sorting per segment is enough because
/// `plan::plan_parts` hands a part its segments already sorted by
/// `start_100ns` and non-overlapping -- every sample of segment N precedes
/// every sample of segment N+1. `export_write_order_is_monotonic_across_
/// segment_boundaries` in `tests/planning.rs` pins both halves of that.
///
/// task1850, for whoever greps their way here from
///
///   "Application provided invalid, non monotonically increasing dts to muxer"
///
/// ffmpeg does say that about our exports, and it is not this code. Measured
/// on the three clips task1770 exported (`.agents/tasks/evidence/1850-export-
/// dts/dts-measurement-2026-08-27.txt`): the written packets contain 0 pairs
/// with `dts <= previous dts` and 0 packets with `duration == 0`, in both
/// video and audio, and `ffmpeg -i clip.mp4 -c copy -f null -` is silent. The
/// message appears only when ffmpeg *re-encodes*. The capture is variable-rate
/// -- WGC delivers frames as the compositor produces them, measured spacing
/// 6.2-20.6 ms -- while the MP4 advertises `r_frame_rate 56/1`, so on
/// re-encode ffmpeg rescales every frame onto that 17.857 ms grid and two
/// frames inside one tick round to the same integer: "X >= X", equal and never
/// backward, video stream only (AAC is constant-rate and never collides).
/// `round(stored_pts * 56)` reproduces the warned-about DTS values exactly --
/// same values, same order, same count -- in all three clips, and
/// `-fps_mode vfr` silences it by dropping the colliding frame. It is a
/// consumer-side choice about a VFR stream, not a defect in the file. Do not
/// "fix" it here: reordering or nudging timestamps to satisfy that message
/// would corrupt real ones.
pub(super) fn order_items<S>(items: &mut [(i64, StreamRef, S)]) {
    items.sort_by_key(|(timestamp, stream, _)| (*timestamp, *stream));
}

#[allow(clippy::too_many_arguments)]
fn write_sink_writer_part(
    writer: &encoder::Mp4ExportWriter,
    part: &ExportPart,
    progress: Option<&dyn Fn(i64)>,
    source_types: &SourceTypes,
    config: &encoder::EncoderConfig,
    runtime: &encoder::MfRuntime,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<(), ExportFailure> {
    let mut part_origin = None;
    for segment in &part.segments {
        check_cancel(cancel)?;
        let start = part.start_100ns.max(segment.start_100ns);
        let end = part.end_100ns.min(segment.end_100ns);
        let partial = start > segment.start_100ns || end < segment.end_100ns;
        let video = if partial {
            collect_boundary_video(
                segment,
                (start, end, part.start_100ns),
                source_types,
                config,
                runtime,
                cancel,
                diagnostics,
            )?
        } else {
            collect_passthrough_video(segment, part.start_100ns, runtime, cancel, diagnostics)?
        };
        let audio = collect_audio_segment(segment, start, end, part, runtime, cancel, diagnostics)?;
        let mut items = video
            .into_iter()
            .map(|(sample, timestamp)| (timestamp, StreamRef::Video, sample))
            .chain(
                audio
                    .into_iter()
                    .map(|(sample, timestamp, track)| (timestamp, StreamRef::Audio(track), sample)),
            )
            .collect::<Vec<_>>();
        order_items(&mut items);
        for (timestamp, stream, sample) in items {
            check_cancel(cancel)?;
            let origin = *part_origin.get_or_insert(timestamp);
            // `checked_sub` only catches arithmetic overflow; a timestamp
            // genuinely before the origin yields Some(negative), which the
            // muxer must never see.
            let rebased = timestamp
                .checked_sub(origin)
                .filter(|rebased| *rebased >= 0)
                .ok_or_else(|| {
                    ExportFailure::new(format!(
                        "export PTSがoriginより前です: pts={timestamp}, origin={origin}"
                    ))
                })?;
            match stream {
                StreamRef::Video => writer.write_video(&sample, rebased)?,
                StreamRef::Audio(track) => writer.write_aac(track, &sample, rebased)?,
            }
            // `rebased` is this sample's distance from the part's start, which
            // is exactly the progress figure the caller wants (task1350).
            // Reported on video only: audio for the same instant arrives around
            // it, and one stream is enough to move a bar. Throttling is the
            // caller's business -- it owns the channel this goes down.
            if let (Some(progress), StreamRef::Video) = (progress, stream) {
                progress(rebased);
            }
        }
    }
    Ok(())
}

fn collect_passthrough_video(
    segment: &ring_buffer::SegmentRecord,
    part_start: i64,
    _runtime: &encoder::MfRuntime,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<Vec<(IMFSample, i64)>, ExportFailure> {
    let mut samples = Vec::new();
    unsafe {
        let reader = open_reader(&segment.path)?;
        loop {
            check_cancel(cancel)?;
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .map_err(|error| ExportFailure::new(format!("映像sample読取失敗: {error}")))?;
            if let Some(sample) = sample {
                let absolute = segment.start_100ns + timestamp;
                samples.push((sample, absolute - part_start));
                diagnostics.passthrough_video_samples += 1;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
    }
    Ok(samples)
}

#[allow(clippy::too_many_arguments)]
/// The encoder for a boundary re-encode, negotiated against the source's own
/// SPS.
/// Fails the export when one track's access units are more than a single AU
/// apart, which would splice a silent hole into the output.
fn check_audio_continuity(samples: &[(IMFSample, i64, i64, usize)]) -> Result<(), ExportFailure> {
    for window in samples.windows(2) {
        let (_, previous_timestamp, previous_duration, previous_track) = &window[0];
        let (_, next_timestamp, _, next_track) = &window[1];
        if previous_track != next_track {
            continue;
        }
        let expected = previous_timestamp.saturating_add(*previous_duration);
        if (*next_timestamp - expected).abs() > *previous_duration {
            return Err(ExportFailure::new(format!(
                "AAC内部不連続が1 AUを超えます: expected={expected}, actual={next_timestamp}"
            )));
        }
    }
    Ok(())
}

/// The window of audio this segment contributes, in absolute time.
///
/// **Boundaries come from track 0 alone**, and the window they define is
/// applied to every track (task1280). The tracks are independent captures that
/// started microseconds apart, so letting each pick its own boundary would cut
/// them at slightly different places and the export would drift apart from
/// itself.
pub(super) fn audio_window(
    samples: &[(IMFSample, i64, i64, usize)],
    segment_start: i64,
    segment_end: i64,
    part: &ExportPart,
) -> Result<(i64, i64), ExportFailure> {
    // **Boundaries come from track 0 alone**, and the window they define is
    // applied to every track (task1280). The tracks are independent captures
    // that started microseconds apart, so letting each pick its own boundary
    // would cut them at slightly different places and the export would drift
    // apart from itself.
    let starts = samples
        .iter()
        .filter(|(_, _, _, track)| *track == 0)
        .map(|(_, timestamp, _, _)| *timestamp)
        .collect::<Vec<_>>();
    let ends = samples
        .iter()
        .filter(|(_, _, _, track)| *track == 0)
        .map(|(_, timestamp, duration, _)| timestamp + duration)
        .collect::<Vec<_>>();
    // **An interior seam is not trimmed at all.** A segment's
    // `end_100ns` is its last *video* sample's timestamp and the next one's
    // `start_100ns` is its first, so the seam is a video frame wide (17.9ms at
    // 56fps) while an AAC access unit is 21.3ms: clipping to the segment's own
    // record dropped every unit that straddled `end_100ns`, plus the one that
    // fell entirely inside the seam. Measured on a real export: a 21.3ms hole
    // at every 2s segment boundary, 43ms at 5 of 13 of them -- audible as the
    // audio breaking up. There is nothing to choose here anyway: an access unit
    // is written to exactly one segment (`push_aac_sample_on` splits on the new
    // segment's `start_timestamp_100ns`), so taking all of an interior
    // segment's audio can neither duplicate nor skip one. The priming unit,
    // which *is* a copy of its predecessor's last, is already skipped by its
    // `audio_offsets_100ns` shift above.
    let audio_start = if segment_start == part.start_100ns {
        nearest_boundary(&starts, part.start_100ns, true)
            .ok_or_else(|| ExportFailure::new("AAC開始境界がありません"))?
    } else {
        i64::MIN
    };
    let audio_end = if segment_end == part.end_100ns {
        nearest_boundary(&ends, part.end_100ns, false)
            .ok_or_else(|| ExportFailure::new("AAC終了境界がありません"))?
    } else {
        i64::MAX
    };
    // `segment_start == part.start_100ns` only ever holds for the export's
    // very first segment (part.start_100ns is fixed for the whole part, and
    // segments are chronologically non-overlapping), and likewise
    // `segment_end == part.end_100ns` only for its very last. At either true
    // edge of a recording, real audio capture (WGC) starts producing samples
    // some tens of milliseconds after video capture begins -- a known
    // startup-latency characteristic, not a bug -- so `audio_start` can
    // legitimately sit well outside `AAC_MAX_ERROR_100NS` of the requested
    // boundary even though it is genuinely the earliest (or latest) audio
    // sample this segment has. `nearest_boundary` having picked exactly
    // `starts.first()` (or `ends.last()`) is how that "no closer candidate
    // exists" case is told apart from a real mismatch elsewhere in the
    // timeline (e.g. an actual gap/drift bug), where the chosen boundary
    // would instead be some interior sample and a large error is worth
    // failing loudly on.
    let start_is_earliest_available = starts.first() == Some(&audio_start);
    let end_is_latest_available = ends.last() == Some(&audio_end);
    if segment_start == part.start_100ns
        && !start_is_earliest_available
        && (audio_start - part.start_100ns).abs() > AAC_MAX_ERROR_100NS
        || segment_end == part.end_100ns
            && !end_is_latest_available
            && (audio_end - part.end_100ns).abs() > AAC_MAX_ERROR_100NS
    {
        return Err(ExportFailure::new("AAC境界誤差が10.67msを超えます"));
    }
    Ok((audio_start, audio_end))
}

/// Points the source reader at NV12 output matching the source's own frame
/// size and rate, which is what the boundary re-encode feeds the encoder.
///
/// # Safety
///
/// Media Foundation must be initialised on this thread and `reader` must be a
/// reader opened on it.
unsafe fn decode_source_to_nv12(
    reader: &IMFSourceReader,
    fingerprint: &VideoFingerprint,
) -> Result<(), ExportFailure> {
    let decoded = MFCreateMediaType()
        .map_err(|error| ExportFailure::new(format!("NV12 media type作成失敗: {error}")))?;
    decoded
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|error| ExportFailure::new(format!("NV12 major type設定失敗: {error}")))?;
    decoded
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|error| ExportFailure::new(format!("NV12 subtype設定失敗: {error}")))?;
    decoded
        .SetUINT64(&MF_MT_FRAME_SIZE, fingerprint.frame_size)
        .map_err(|error| ExportFailure::new(format!("NV12 frame size設定失敗: {error}")))?;
    decoded
        .SetUINT64(
            &MF_MT_FRAME_RATE,
            (u64::from(fingerprint.frame_rate) << 32) | 1,
        )
        .map_err(|error| ExportFailure::new(format!("NV12 frame rate設定失敗: {error}")))?;
    reader
        .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &decoded)
        .map_err(|error| ExportFailure::new(format!("映像→NV12 decoder設定失敗: {error}")))?;
    Ok(())
}

/// Moves whatever the encoder has ready into `output`, stamped with the
/// timestamp it came back with.
fn collect_encoded(
    hardware: &mut encoder::HardwareVideoEncoder,
    output: &mut Vec<(IMFSample, i64)>,
) -> Result<(), ExportFailure> {
    for sample in hardware.poll()? {
        let timestamp = unsafe { sample.GetSampleTime() }
            .map_err(|error| ExportFailure::new(format!("映像timestamp読取失敗: {error}")))?;
        output.push((sample, timestamp));
    }
    Ok(())
}

fn create_boundary_encoder(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
    config: &encoder::EncoderConfig,
    source_types: &SourceTypes,
) -> Result<encoder::HardwareVideoEncoder, ExportFailure> {
    // `config.frame_rate` (from `classify_frame_rate`, Task080) is only ever an
    // approximation of the source's true nominal encode rate: it buckets an
    // *observed* rate computed from the container's actual sample timing, but
    // WGC's irregular frame delivery means that observed average can land far
    // from the rate the hardware encoder was really configured at when this
    // recording was made (confirmed on real data: segments whose H.264 SPS
    // level=40 -- consistent only with a true 60fps encode at this
    // resolution -- had a container-computed average of ~34-37fps, bucketing
    // to 30; and a true 120fps encode, level=51, bucketed all the way down to
    // 30). Rather than inferring the right rate from noisy timing, create at
    // the classified rate and, on an SPS mismatch, work through the remaining
    // candidates -- self-correcting, so it does not matter which one
    // `classify_frame_rate` guessed first.
    //
    // Task370: this used to flip between 30 and 60 only, written when those
    // were the only rates `encoder::validate_config` accepted. Since task164 it
    // also accepts 120, and every recording made on a 120fps machine was
    // therefore unexportable: level 51 is reachable from neither 30 (level 40)
    // nor 60 (level 42), so both attempts were bound to miss. Measured on a
    // real segment (`measures_which_frame_rate_reproduces_the_source_sps`):
    // creating at 120 reproduces the source's SPS byte for byte, so the
    // candidate list is the whole fix and `codec_compatible` stays strict.

    let ordered = std::iter::once(config.frame_rate).chain(
        BOUNDARY_FRAME_RATE_CANDIDATES
            .into_iter()
            .filter(|candidate| *candidate != config.frame_rate),
    );
    let mut attempts = Vec::new();
    let mut created_any = false;
    let mut matched = None;
    for frame_rate in ordered {
        let candidate_config = encoder::EncoderConfig {
            frame_rate,
            ..config.clone()
        };
        // **The profile comes from the source SPS, not from the recording
        // settings** (task1310). task1000 moved recording from Baseline to
        // High, which made every session recorded before it unexportable:
        // the boundary re-encoder created a High encoder, its SPS could not
        // match a Baseline one at any frame rate, and every candidate
        // missed. The profile is right there in the fingerprint, parsed out
        // of the real SPS bytes, and `eAVEncH264VProfile_*` uses the same
        // numbering as `profile_idc`.
        // **And the codec comes from the source too** (task1770). The
        // fingerprint carries `VideoCodec` rather than a bare profile
        // number precisely so this call cannot build an H.264 encoder for
        // an AV1 source: `MF_MT_MPEG2_PROFILE` is reused across codec
        // families, so an AV1 `seq_profile` of 1 read as a `profile_idc`
        // would have produced a plausible-looking H.264 request that could
        // never match -- the same silent mismatch in a new codec.
        match encoder::HardwareVideoEncoder::create_with_codec(
            &candidate_config,
            device,
            source_types.video_fingerprint.codec,
        ) {
            Ok(candidate) => {
                created_any = true;
                candidate.request_next_keyframe()?;
                let fingerprint = video_fingerprint(&candidate.output_media_type()?)?;
                if fingerprint.codec_compatible(&source_types.video_fingerprint) {
                    matched = Some(candidate);
                    break;
                }
                attempts.push(format!(
                    "boundary({frame_rate}fps, codec={:?})={fingerprint:?}",
                    source_types.video_fingerprint.codec
                ));
            }
            // A rate this GPU will not encode at is not the export's
            // failure to report -- unless *none* of them worked, in which
            // case the encoder's own error is the useful one.
            Err(error) => attempts.push(format!("boundary({frame_rate}fps)=作成失敗: {error:?}")),
        }
    }
    match matched {
        Some(candidate) => Ok(candidate),
        None if !created_any => {
            Err(ExportFailure::new(format!(
                "境界再エンコード用エンコーダ({:?})を作成できませんでした: {}",
                source_types.video_fingerprint.codec,
                attempts.join(", ")
            )))
        }
        None => Err(ExportFailure::new(format!(
            "映像設定が一致しないため安全に書き出しを中止しました (IncompatibleCodecConfig): source={:?}, {}",
            source_types.video_fingerprint,
            attempts.join(", ")
        ))),
    }
}

fn collect_boundary_video(
    segment: &ring_buffer::SegmentRecord,
    // The window this segment contributes, and the origin every sample is
    // rebased against: one argument because the three are one decision.
    (start, end, part_start): (i64, i64, i64),
    source_types: &SourceTypes,
    config: &encoder::EncoderConfig,
    _runtime: &encoder::MfRuntime,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<Vec<(IMFSample, i64)>, ExportFailure> {
    // `_context` must stay bound (not `_`): every other `create_d3d_device()`
    // call site in this codebase keeps the device context alive for the same
    // reason (see e.g. `capture.rs:2145`) -- dropping it immediately here
    // released it out from under the hardware encoder while it still
    // expected a live D3D11 device.
    let (device, _context, _) = capture::create_d3d_device().map_err(|error| {
        ExportFailure::new(format!("境界再エンコード用D3D11 device作成失敗: {error}"))
    })?;
    let mut hardware = create_boundary_encoder(&device, config, source_types)?;
    let mut output = Vec::new();
    unsafe {
        let reader = open_reader(&segment.path)?;
        decode_source_to_nv12(&reader, &source_types.video_fingerprint)?;
        loop {
            check_cancel(cancel)?;
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .map_err(|error| ExportFailure::new(format!("NV12 frame読取失敗: {error}")))?;
            if let Some(sample) = sample {
                let absolute = segment.start_100ns + timestamp;
                if absolute >= start && absolute < end {
                    submit_encoder_sample(
                        &mut hardware,
                        &packed_nv12(&sample, source_types.video_fingerprint.frame_size)?,
                        absolute - part_start,
                        cancel,
                        &mut output,
                    )?;
                    diagnostics.reencoded_video_frames += 1;
                }
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
    }
    hardware.begin_drain()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hardware.drain_complete() {
        check_cancel(cancel)?;
        collect_encoded(&mut hardware, &mut output)?;
        if Instant::now() >= deadline {
            return Err(ExportFailure::new("境界encoder drain timeout"));
        }
        thread::yield_now();
    }
    collect_encoded(&mut hardware, &mut output)?;
    Ok(output)
}

/// Repacks a decoded NV12 frame to the exact size the encoder was set up for.
///
/// Media Foundation's H.264 decoder hands back a buffer whose luma plane is
/// padded to a multiple of 16 rows -- measured on a real 1920x1032 recording:
/// pitch 1920, contiguous length 2,995,200, which is 1040 rows of luma followed
/// by 520 of chroma, not the 1032/516 the media type asks for. The encoder,
/// created at 1920x1032, looks for chroma at row 1032 and so read the luma
/// plane's 8 rows of zeroed padding as the first 8 chroma rows and every real
/// chroma row 8 rows late: the top 16 picture rows came out with U=V=0 (bright
/// green) and the rest of the frame's colour sat 16 rows below its luma.
///
/// Only the boundary re-encode feeds the encoder a decoded frame -- everything
/// else is passthrough -- so the damage was confined to a clip's first and last
/// seconds. A width needs no padding at 1920, which is why it took a window
/// recording at an odd *height* to show it; the same class of bug as task500 on
/// the playback side, where the buffer is likewise the authority on its own
/// layout and the media type only claims one.
///
/// The same reasoning covers a pitch wider than the frame, which 1920 happens
/// to hide. Buffers that are not 2D, are bottom-up (negative pitch), or already
/// match the frame exactly pass through untouched.
///
/// # Safety
///
/// Media Foundation must be initialised on this thread.
unsafe fn packed_nv12(sample: &IMFSample, frame_size: u64) -> Result<IMFSample, ExportFailure> {
    let width = (frame_size >> 32) as usize;
    let height = frame_size as u32 as usize;
    let buffer = sample
        .GetBufferByIndex(0)
        .map_err(|error| ExportFailure::new(format!("NV12 buffer取得失敗: {error}")))?;
    let Ok(two_d) = buffer.cast::<IMF2DBuffer2>() else {
        return Ok(sample.clone());
    };
    // The locked length is the buffer's own statement of how many rows it
    // holds. `GetContiguousLength` is not: it is the size the frame would
    // take *repacked*, and on a 388x218 window recording it came back as
    // 134,400 (400 x 224 x 3/2 -- the width rounded to 16, not the pitch)
    // while the buffer really was pitch 448 x 224 rows = 150,528 bytes.
    // Dividing that by the pitch gave 200 rows for a 218-row frame, and the
    // chroma copy ran off the end of the slice built from it (t260919-4866).
    let mut scanline0 = std::ptr::null_mut();
    let mut pitch = 0i32;
    let mut start = std::ptr::null_mut();
    let mut length = 0u32;
    two_d
        .Lock2DSize(
            MF2DBuffer_LockFlags_Read,
            &mut scanline0,
            &mut pitch,
            &mut start,
            &mut length,
        )
        .map_err(|error| ExportFailure::new(format!("NV12 buffer lock失敗: {error}")))?;
    // Nothing to do, or nothing this can safely reason about: hand the encoder
    // what the decoder gave it, exactly as before.
    let repacked = (pitch > 0 && !start.is_null())
        .then(|| {
            let source = std::slice::from_raw_parts(start, length as usize);
            repack_nv12(source, pitch as usize, width, height)
        })
        .flatten();
    let _ = two_d.Unlock2D();
    let Some(packed) = repacked else {
        return Ok(sample.clone());
    };
    let media = MFCreateMemoryBuffer(packed.len() as u32)
        .map_err(|error| ExportFailure::new(format!("NV12 packed buffer作成失敗: {error}")))?;
    let mut destination = std::ptr::null_mut();
    media
        .Lock(&mut destination, None, None)
        .map_err(|error| ExportFailure::new(format!("NV12 packed buffer lock失敗: {error}")))?;
    std::ptr::copy_nonoverlapping(packed.as_ptr(), destination, packed.len());
    let _ = media.Unlock();
    media
        .SetCurrentLength(packed.len() as u32)
        .map_err(|error| ExportFailure::new(format!("NV12 packed長設定失敗: {error}")))?;
    let out = MFCreateSample()
        .map_err(|error| ExportFailure::new(format!("NV12 packed sample作成失敗: {error}")))?;
    out.AddBuffer(&media)
        .map_err(|error| ExportFailure::new(format!("NV12 packed buffer追加失敗: {error}")))?;
    Ok(out)
}

/// The copy half of [`packed_nv12`], given the locked bytes: `None` when the
/// buffer already is the frame exactly, or when it is too short to hold
/// `height` luma rows and `height / 2` chroma rows at `pitch` (passthrough,
/// never a panic).
///
/// The rows the luma plane is padded to fall out of the length, the same way
/// playback reads them (`livia_pixels::nv12_plane_rows`): an NV12 buffer is
/// `pitch * rows * 3/2`. Measured layouts it has to get right: 1920x1032 in
/// pitch 1920 x 1040 rows (d916193d), and 388x218 in pitch 448 x 224 rows
/// (t260919-4866).
pub(super) fn repack_nv12(
    source: &[u8],
    pitch: usize,
    width: usize,
    height: usize,
) -> Option<Vec<u8>> {
    let rows = livia_pixels::nv12_plane_rows(source.len(), pitch, height);
    let chroma_rows = height / 2;
    if pitch < width || width == 0 || chroma_rows == 0 || (rows == height && pitch == width) {
        return None;
    }
    if source.len() < (rows + chroma_rows - 1) * pitch + width {
        return None;
    }
    let mut packed = vec![0u8; width * height * 3 / 2];
    for row in 0..height {
        packed[row * width..][..width].copy_from_slice(&source[row * pitch..][..width]);
    }
    for row in 0..chroma_rows {
        packed[(height + row) * width..][..width]
            .copy_from_slice(&source[(rows + row) * pitch..][..width]);
    }
    Some(packed)
}

fn submit_encoder_sample(
    hardware: &mut encoder::HardwareVideoEncoder,
    input: &IMFSample,
    timestamp: i64,
    cancel: &AtomicBool,
    output: &mut Vec<(IMFSample, i64)>,
) -> Result<(), ExportFailure> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        check_cancel(cancel)?;
        for sample in hardware.poll()? {
            let timestamp = unsafe { sample.GetSampleTime() }
                .map_err(|error| ExportFailure::new(format!("映像timestamp読取失敗: {error}")))?;
            output.push((sample, timestamp));
        }
        if hardware.has_input_credit() {
            hardware.process_input_sample(input, timestamp)?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ExportFailure::new("境界encoder input timeout"));
        }
        thread::yield_now();
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_audio_segment(
    segment: &ring_buffer::SegmentRecord,
    segment_start: i64,
    segment_end: i64,
    part: &ExportPart,
    _runtime: &encoder::MfRuntime,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<Vec<(IMFSample, i64, usize)>, ExportFailure> {
    let mut samples: Vec<(IMFSample, i64, i64, usize)> = Vec::new();
    unsafe {
        let reader = open_reader(&segment.path)?;
        // Video-only segment: contributes no audio rather than aborting the
        // export. Returning early also skips the boundary search below, which
        // would otherwise fail on the empty sample list.
        let streams = audio_stream_indices(&reader);
        if streams.is_empty() {
            return Ok(Vec::new());
        }
        for (track, stream) in streams.iter().copied().enumerate() {
            loop {
                check_cancel(cancel)?;
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
                    .map_err(|error| ExportFailure::new(format!("AAC sample読取失敗: {error}")))?;
                if let Some(sample) = sample {
                    // Skip the priming access unit and undo the shift that seated
                    // it (task204): it is a duplicate of the previous segment's
                    // last block, and passing it through would splice 21ms of
                    // repeated audio into the export at every seam. Guarded rather
                    // than `continue`d so the end-of-stream check below still runs.
                    // Each track carries its own shift (task1250/task1260).
                    let audio_offset = segment.audio_offsets_100ns.get(track).copied().unwrap_or(0);
                    if timestamp >= audio_offset {
                        let duration = sample.GetSampleDuration().unwrap_or_default();
                        samples.push((
                            sample,
                            segment.start_100ns + timestamp - audio_offset,
                            duration,
                            track,
                        ));
                    }
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
        }
    }
    // Sorted by track first, then time: the continuity check below is a
    // property of one track, and interleaving them would read every crossing
    // between tracks as a discontinuity.
    samples.sort_by_key(|(_, timestamp, _, track)| (*track, *timestamp));
    check_audio_continuity(&samples)?;
    let (audio_start, audio_end) = audio_window(&samples, segment_start, segment_end, part)?;
    let mut selected = Vec::new();
    for (sample, absolute, duration, track) in samples {
        let end = absolute + duration;
        if absolute < audio_start || end > audio_end {
            continue;
        }
        selected.push((sample, absolute - part.start_100ns, track));
        diagnostics.passthrough_audio_samples += 1;
    }
    Ok(selected)
}

pub(super) fn nearest_boundary(
    values: &[i64],
    requested: i64,
    start_boundary: bool,
) -> Option<i64> {
    values.iter().copied().min_by(|left, right| {
        let left_distance = (*left - requested).abs();
        let right_distance = (*right - requested).abs();
        left_distance.cmp(&right_distance).then_with(|| {
            if start_boundary {
                right.cmp(left) // tie: later start is inward
            } else {
                left.cmp(right) // tie: earlier end is inward
            }
        })
    })
}
