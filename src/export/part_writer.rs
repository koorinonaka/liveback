use std::{
    path::Path,
    sync::atomic::AtomicBool,
    thread,
    time::{Duration, Instant},
};

use super::failure;
use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFSample, IMFSourceReader, MF2DBuffer_LockFlags_Read, MFCreateMediaType,
    MFMediaType_Video, MFVideoFormat_NV12, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::{capture, encoder, ring_buffer};

use super::mixdown::{silent_aac_frame, Mixdown};
use super::plan::check_cancel;
use super::probe::{
    audio_stream_indices, open_reader, probe_source_types, video_fingerprint, SourceTypes,
    VideoFingerprint, BOUNDARY_FRAME_RATE_CANDIDATES,
};
use super::{AudioMix, ExportDiagnostics, ExportFailure, ExportPart, AAC_MAX_ERROR_100NS};

#[cfg(test)]
pub(super) fn export_part(
    part: &ExportPart,
    partial: &Path,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<(), ExportFailure> {
    export_part_reporting(
        part,
        partial,
        &AudioMix::default(),
        PartContext {
            cancel,
            diagnostics,
            progress: None,
        },
        None,
    )
}

/// [`export_part`] with the audio half of a request (t261003-b713).
#[cfg(test)]
pub(super) fn export_part_mixed(
    part: &ExportPart,
    partial: &Path,
    audio_mix: &AudioMix,
) -> Result<(), ExportFailure> {
    export_part_reporting(
        part,
        partial,
        audio_mix,
        PartContext {
            cancel: &AtomicBool::new(false),
            diagnostics: &mut ExportDiagnostics::default(),
            progress: None,
        },
        None,
    )
}

/// What every stage of writing one part carries through (t261025-00ad): the
/// cancel flag it polls, the diagnostics it fills, and the progress callback
/// (task1350) when there is one.
pub(super) struct PartContext<'a> {
    pub(super) cancel: &'a AtomicBool,
    pub(super) diagnostics: &'a mut ExportDiagnostics,
    pub(super) progress: Option<&'a dyn Fn(i64)>,
}

/// `export_part` with a callback that says how far into this part the
/// writer has got, in 100ns from the part's start (task1350).
///
/// Without it a gapless recording -- which is nearly every recording, since
/// task063 keeps the audio continuous and segments therefore rarely break --
/// is one part, and the only progress the UI ever saw was the one published
/// after the part finished. Measured: 24 seconds of "書き出し中 0%".
pub(super) fn export_part_reporting(
    part: &ExportPart,
    partial: &Path,
    audio_mix: &AudioMix,
    context: PartContext<'_>,
    phase: Option<&dyn Fn(bool)>,
) -> Result<(), ExportFailure> {
    let PartContext {
        cancel,
        diagnostics,
        progress,
    } = context;
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
    // Decision 14: one track (or none) is copied exactly as before -- no mix,
    // no levels. Two or more (the most any segment in the range has) get the
    // mix as audio track 0, and the originals after it unless this is a clip.
    let mut mixdown = (source_types.audio.len() >= 2)
        .then(|| Mixdown::new(&audio_mix.levels))
        .transpose()?;
    let originals = match &mixdown {
        None => Some(0),
        Some(_) if audio_mix.mix_only => None,
        Some(_) => Some(1),
    };
    let output_audio = mixdown
        .iter()
        .map(|mix| mix.media_type().clone())
        .chain(
            originals
                .into_iter()
                .flat_map(|_| source_types.audio.iter().cloned()),
        )
        .collect::<Vec<_>>();
    let writer = encoder::Mp4ExportWriter::create(
        &source_types.video,
        &output_audio,
        partial.to_path_buf(),
    )?;
    write_sink_writer_part(
        &writer,
        part,
        &source_types,
        &config,
        &runtime,
        (mixdown.as_mut(), originals),
        PartContext {
            cancel,
            diagnostics: &mut *diagnostics,
            progress,
        },
    )?;
    check_cancel(cancel)?;
    if let Some(phase) = phase {
        phase(true);
    }
    diagnostics.nudged_video_samples += writer.nudged_video_samples();
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
/// task1850 measured 0 pairs with `dts <= previous dts` in exported clips and
/// read that as "the file is fine, leave timestamps alone". The premise was
/// wrong (t261108-68a2): recorded segments do hold same-dts video pairs (a
/// hold frame and the real frame 100 ns after it), and the clips had none
/// because the MPEG-4 sink dropped the second sample of each pair -- a
/// referenced frame, so dav1d lost dozens of frames after it. Ordering here
/// still never nudges anything; `Mp4ExportWriter::write_video` pushes such a
/// sample two output ticks past the previous one so the sink keeps both. The
/// re-encode warning itself is the separate VFR-onto-`r_frame_rate` rounding
/// task1850 described (`.agents/tasks/evidence/1850-export-dts/`).
pub(super) fn order_items<S>(items: &mut [(i64, StreamRef, S)]) {
    items.sort_by_key(|(timestamp, stream, _)| (*timestamp, *stream));
}

fn write_sink_writer_part(
    writer: &encoder::Mp4ExportWriter,
    part: &ExportPart,
    source_types: &SourceTypes,
    config: &encoder::EncoderConfig,
    runtime: &encoder::MfRuntime,
    // The mix, if there is one, and the output stream the original tracks
    // start at (`None`: a clip, which writes the mix alone).
    (mut mixdown, originals): (Option<&mut Mixdown>, Option<usize>),
    context: PartContext<'_>,
) -> Result<(), ExportFailure> {
    let PartContext {
        cancel,
        diagnostics,
        progress,
    } = context;
    let mut part_origin = None;
    let mut silent = None;
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
        let audio = collect_audio_segment(segment, start, end, part, cancel, diagnostics)?;
        let mut items = video
            .into_iter()
            .map(|(sample, timestamp)| (timestamp, StreamRef::Video, sample))
            .collect::<Vec<_>>();
        if let Some(shift) = originals {
            // A track this segment does not have yet (added later in the
            // recording) is silence on track 0's grid, so every output track
            // runs the whole part (t261003-b713).
            let missing = audio.tracks..source_types.audio.len();
            if !missing.is_empty() && audio.track0().next().is_some() {
                if silent.is_none() {
                    silent = Some(silent_aac_frame()?);
                }
                let silent = silent.as_ref().expect("made just above");
                for track in missing {
                    for (timestamp, _) in audio.track0() {
                        items.push((timestamp, StreamRef::Audio(track + shift), silent.clone()));
                    }
                }
            }
            items.extend(audio.samples.iter().map(|(sample, timestamp, _, track)| {
                (*timestamp, StreamRef::Audio(track + shift), sample.clone())
            }));
        }
        if let Some(mix) = mixdown.as_deref_mut() {
            let first = audio.track0().next();
            let last = audio.track0().last();
            if let (Some((first, _)), Some((last, duration))) = (first, last) {
                let window = (first + part.start_100ns, last + duration + part.start_100ns);
                items.extend(
                    mix.segment(segment, window, part.start_100ns)?
                        .into_iter()
                        .map(|(timestamp, sample)| (timestamp, StreamRef::Audio(0), sample)),
                );
            }
        }
        order_items(&mut items);
        write_items(writer, items, &mut part_origin, progress, cancel)?;
    }
    if let Some(mix) = mixdown {
        let tail = mix
            .finish()?
            .into_iter()
            .map(|(timestamp, sample)| (timestamp, StreamRef::Audio(0), sample))
            .collect();
        write_items(writer, tail, &mut part_origin, progress, cancel)?;
    }
    Ok(())
}

fn write_items(
    writer: &encoder::Mp4ExportWriter,
    items: Vec<(i64, StreamRef, IMFSample)>,
    part_origin: &mut Option<i64>,
    progress: Option<&dyn Fn(i64)>,
    cancel: &AtomicBool,
) -> Result<(), ExportFailure> {
    for (timestamp, stream, sample) in items {
        check_cancel(cancel)?;
        let origin = *part_origin.get_or_insert(timestamp);
        // `checked_sub` only catches arithmetic overflow; a timestamp
        // genuinely before the origin yields Some(negative), which the
        // muxer must never see.
        let rebased = timestamp
            .checked_sub(origin)
            .filter(|rebased| *rebased >= 0)
            .ok_or_else(|| ExportFailure::new(failure::pts_before_origin(timestamp, origin)))?;
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
        encoder::mf_sample::for_each_sample(
            &reader,
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            |error| ExportFailure::new(failure::video_sample_read_failed(error)),
            |timestamp, sample| {
                check_cancel(cancel)?;
                let absolute = segment.start_100ns + timestamp;
                samples.push((sample, absolute - part_start));
                diagnostics.passthrough_video_samples += 1;
                Ok(())
            },
        )?;
    }
    Ok(samples)
}

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
            return Err(ExportFailure::new(failure::aac_discontinuity(
                expected,
                next_timestamp,
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
    // **The seams are boundaries too** (t260918-cd71). Units are contiguous
    // across segments, so this segment's first start is where the previous
    // segment's last unit ends, and its last end is where the next one's first
    // starts -- but each of those sits only in the *other* list. A requested
    // edge within half a unit of a seam, on the far side of it, found nothing
    // nearer than a whole unit away and failed the 10.67ms check (measured:
    // `range 10 30.01` against a segment opening at 30.0009s, 19.5ms off).
    // Picking the seam means this segment contributes no audio at that edge;
    // the neighbour, an interior segment, supplies all of its own.
    let start_candidates = starts
        .iter()
        .chain(ends.last())
        .copied()
        .collect::<Vec<_>>();
    let end_candidates = starts
        .first()
        .into_iter()
        .chain(&ends)
        .copied()
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
        nearest_boundary(&start_candidates, part.start_100ns, true)
            .ok_or_else(|| ExportFailure::new(failure::aac_start_boundary_missing()))?
    } else {
        i64::MIN
    };
    let audio_end = if segment_end == part.end_100ns {
        nearest_boundary(&end_candidates, part.end_100ns, false)
            .ok_or_else(|| ExportFailure::new(failure::aac_end_boundary_missing()))?
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
        return Err(ExportFailure::new(failure::aac_boundary_drift()));
    }
    Ok((audio_start, audio_end))
}

/// A failed step of the NV12 decode or repack, named for the user.
fn nv12_step(step: &str, error: windows::core::Error) -> ExportFailure {
    ExportFailure::new(failure::nv12_step_failed(step, error))
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
    let decoded = MFCreateMediaType().map_err(|error| nv12_step("media type creation", error))?;
    decoded
        .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|error| nv12_step("major type", error))?;
    decoded
        .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|error| nv12_step("subtype", error))?;
    decoded
        .SetUINT64(&MF_MT_FRAME_SIZE, fingerprint.frame_size)
        .map_err(|error| nv12_step("frame size", error))?;
    decoded
        .SetUINT64(
            &MF_MT_FRAME_RATE,
            (u64::from(fingerprint.frame_rate) << 32) | 1,
        )
        .map_err(|error| nv12_step("frame rate", error))?;
    reader
        .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &decoded)
        .map_err(|error| nv12_step("decoder setup", error))?;
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
            .map_err(|error| ExportFailure::new(failure::video_timestamp_read_failed(error)))?;
        output.push((sample, timestamp));
    }
    Ok(())
}

/// The encoder for a boundary re-encode, negotiated against the source's own
/// SPS.
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
        //
        // **NVENC's own API first, then the MFT** (t261026-3ff2). Since
        // t261024-071a an NVIDIA machine records through NVENC's API, whose
        // SPS the MFT cannot reproduce at any rate (level 51 against the
        // MFT's resolution-picked one, 3 reference frames against 1, and VUI
        // flags no MFT attribute sets). The same NVENC configuration does
        // reproduce it, so it is just another candidate through the same
        // strict `codec_compatible`. Where NVENC cannot open (another vendor,
        // an old driver) that is not an attempt worth reporting: the MFT's is.
        let codec = source_types.video_fingerprint.codec;
        // An encoder that opened: the match, or one more attempt to report.
        let mut try_candidate =
            |candidate: encoder::HardwareVideoEncoder,
             backend: &str|
             -> Result<Option<encoder::HardwareVideoEncoder>, ExportFailure> {
                created_any = true;
                candidate.request_next_keyframe()?;
                let fingerprint = video_fingerprint(&candidate.output_media_type()?)?;
                if fingerprint.codec_compatible(&source_types.video_fingerprint) {
                    return Ok(Some(candidate));
                }
                attempts.push(format!(
                    "boundary({frame_rate}fps, codec={codec:?}{backend})={fingerprint:?}"
                ));
                Ok(None)
            };
        if let Ok(candidate) = encoder::HardwareVideoEncoder::create_nvenc(
            &candidate_config,
            device,
            codec,
            encoder::EncoderInput::Nv12,
        ) {
            if let Some(candidate) = try_candidate(candidate, ", nvenc")? {
                matched = Some(candidate);
                break;
            }
        }
        match encoder::HardwareVideoEncoder::create_with_codec(&candidate_config, device, codec) {
            Ok(candidate) => {
                if let Some(candidate) = try_candidate(candidate, "")? {
                    matched = Some(candidate);
                    break;
                }
            }
            // A rate this GPU will not encode at is not the export's
            // failure to report -- unless *none* of them worked, in which
            // case the encoder's own error is the useful one.
            Err(error) => attempts.push(failure::boundary_attempt_failed(
                frame_rate,
                format!("{error:?}"),
            )),
        }
    }
    match matched {
        Some(candidate) => Ok(candidate),
        None if !created_any => Err(ExportFailure::new(failure::boundary_encoder_unavailable(
            format!("{:?}", source_types.video_fingerprint.codec),
            attempts.join(", "),
        ))),
        None => Err(ExportFailure::new(failure::incompatible_codec_config(
            format!("{:?}", source_types.video_fingerprint),
            attempts.join(", "),
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
    let (device, _context, _) = capture::create_d3d_device()
        .map_err(|error| ExportFailure::new(failure::boundary_device_failed(error)))?;
    let mut hardware = create_boundary_encoder(&device, config, source_types)?;
    let mut output = Vec::new();
    unsafe {
        let reader = open_reader(&segment.path)?;
        decode_source_to_nv12(&reader, &source_types.video_fingerprint)?;
        encoder::mf_sample::for_each_sample(
            &reader,
            MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
            |error| nv12_step("frame read", error),
            |timestamp, sample| {
                check_cancel(cancel)?;
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
                Ok(())
            },
        )?;
    }
    hardware.begin_drain()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hardware.drain_complete() {
        check_cancel(cancel)?;
        collect_encoded(&mut hardware, &mut output)?;
        if Instant::now() >= deadline {
            return Err(ExportFailure::new(failure::boundary_drain_timeout()));
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
        .map_err(|error| nv12_step("buffer fetch", error))?;
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
        .map_err(|error| nv12_step("buffer lock", error))?;
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
    encoder::mf_sample::from_bytes(&packed, None, None)
        .map_err(|error| nv12_step("packed sample", error))
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
        collect_encoded(hardware, output)?;
        if hardware.has_input_credit() {
            hardware.process_input_sample(input, timestamp)?;
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(ExportFailure::new(failure::boundary_input_timeout()));
        }
        thread::yield_now();
    }
}

fn collect_audio_segment(
    segment: &ring_buffer::SegmentRecord,
    segment_start: i64,
    segment_end: i64,
    part: &ExportPart,
    cancel: &AtomicBool,
    diagnostics: &mut ExportDiagnostics,
) -> Result<SegmentAudio, ExportFailure> {
    let mut samples: Vec<(IMFSample, i64, i64, usize)> = Vec::new();
    let tracks;
    unsafe {
        let reader = open_reader(&segment.path)?;
        // Video-only segment: contributes no audio rather than aborting the
        // export. Returning early also skips the boundary search below, which
        // would otherwise fail on the empty sample list.
        let streams = audio_stream_indices(&reader);
        tracks = streams.len();
        if streams.is_empty() {
            return Ok(SegmentAudio {
                samples: Vec::new(),
                tracks,
            });
        }
        for (track, stream) in streams.iter().copied().enumerate() {
            // Skip the priming access unit and undo the shift that seated it
            // (task204): it is a duplicate of the previous segment's last
            // block, and passing it through would splice 21ms of repeated
            // audio into the export at every seam. Each track carries its own
            // shift (task1250/task1260).
            let audio_offset = segment.audio_offsets_100ns.get(track).copied().unwrap_or(0);
            encoder::mf_sample::for_each_sample(
                &reader,
                stream,
                |error| ExportFailure::new(failure::aac_sample_read_failed(error)),
                |timestamp, sample| {
                    check_cancel(cancel)?;
                    if timestamp >= audio_offset {
                        let duration = sample.GetSampleDuration().unwrap_or_default();
                        samples.push((
                            sample,
                            segment.start_100ns + timestamp - audio_offset,
                            duration,
                            track,
                        ));
                    }
                    Ok(())
                },
            )?;
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
        selected.push((sample, absolute - part.start_100ns, duration, track));
        diagnostics.passthrough_audio_samples += 1;
    }
    Ok(SegmentAudio {
        samples: selected,
        tracks,
    })
}

/// One segment's share of the copied audio: `(sample, time from the part's
/// start, duration, track)`, and how many audio tracks the segment itself has
/// -- fewer than the part's when a track was added later in the recording.
struct SegmentAudio {
    samples: Vec<(IMFSample, i64, i64, usize)>,
    tracks: usize,
}

impl SegmentAudio {
    /// Track 0's selected access units, `(time, duration)`: the grid every
    /// other track is cut to (task1280) and the one silence and the mix are
    /// laid on.
    fn track0(&self) -> impl Iterator<Item = (i64, i64)> + '_ {
        self.samples
            .iter()
            .filter(|(_, _, _, track)| *track == 0)
            .map(|(_, timestamp, duration, _)| (*timestamp, *duration))
    }
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
