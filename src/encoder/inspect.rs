use std::{io, path::Path};

use windows::Win32::Media::MediaFoundation::{
    IMFSourceReader, MFAudioFormat_AAC, MFAudioFormat_PCM, MFCreateMediaType,
    MFCreateSourceReaderFromURL, MFMediaType_Audio, MFVideoFormat_AV1, MFVideoFormat_H264,
    MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
    MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_AVG_BITRATE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use super::{device_error, EncoderStartError, EncoderStartErrorKind, MfRuntime};

#[derive(Debug, PartialEq, Eq)]
pub struct Mp4SegmentInspection {
    pub sample_count: u32,
    pub first_sample_clean_point: bool,
    pub duration_100ns: i64,
    pub configured_bitrate: u32,
    pub has_aac_audio: bool,
    pub audio_sample_rate: u32,
    pub audio_channels: u32,
    pub audio_bitrate: u32,
    pub audio_sample_count: u32,
    pub first_audio_timestamp_100ns: Option<i64>,
    pub last_audio_timestamp_100ns: Option<i64>,
}

/// Walks the video stream: how many samples, whether the first is a clean
/// point, and the span they cover. Regressing timestamps fail the inspection --
/// a segment whose samples go backwards is not one this app wrote.
///
/// # Safety
///
/// Media Foundation must be running on this thread, and `reader` opened on it.
unsafe fn scan_video_samples(
    reader: &IMFSourceReader,
) -> Result<(u32, bool, Option<i64>, i64), EncoderStartError> {
    let mut sample_count = 0;
    let mut first_clean_point = false;
    let mut first_timestamp = None;
    let mut previous_timestamp = None;
    let mut last_timestamp = 0;
    loop {
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
            .map_err(|error| device_error("MP4 Source Reader sample", error))?;
        if let Some(sample) = sample {
            if previous_timestamp.is_some_and(|previous| timestamp < previous) {
                return Err(EncoderStartError {
                    kind: EncoderStartErrorKind::UnsupportedFormat,
                    diagnostics: "MP4 sample timestamps regress".into(),
                });
            }
            if sample_count == 0 {
                first_clean_point = sample
                    .GetUINT32(
                        &windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint,
                    )
                    .unwrap_or_default()
                    != 0;
                first_timestamp = Some(timestamp);
            }
            previous_timestamp = Some(timestamp);
            last_timestamp = timestamp + sample.GetSampleDuration().unwrap_or_default();
            sample_count += 1;
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            break;
        }
    }
    Ok((
        sample_count,
        first_clean_point,
        first_timestamp,
        last_timestamp,
    ))
}

/// The same walk over the AAC stream, when there is one: sample count and the
/// first and last timestamps. Duplicate or regressing timestamps fail.
///
/// # Safety
///
/// Media Foundation must be running on this thread, and `reader` opened on it.
unsafe fn scan_audio_samples(
    reader: &IMFSourceReader,
    has_aac_audio: bool,
) -> Result<(u32, Option<i64>, Option<i64>), EncoderStartError> {
    let mut audio_sample_count = 0;
    let mut first_audio_timestamp = None;
    let mut previous_audio_timestamp = None;
    let mut last_audio_timestamp = None;
    if has_aac_audio {
        loop {
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .map_err(|error| device_error("MP4 Source Reader audio sample", error))?;
            if sample.is_some() {
                if previous_audio_timestamp.is_some_and(|previous| timestamp <= previous) {
                    return Err(EncoderStartError {
                        kind: EncoderStartErrorKind::UnsupportedFormat,
                        diagnostics: "MP4 audio sample timestamps duplicate or regress".into(),
                    });
                }
                previous_audio_timestamp = Some(timestamp);
                first_audio_timestamp.get_or_insert(timestamp);
                last_audio_timestamp = Some(timestamp);
                audio_sample_count += 1;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
    }
    Ok((
        audio_sample_count,
        first_audio_timestamp,
        last_audio_timestamp,
    ))
}

pub fn inspect_finalized_mp4(path: &Path) -> Result<Mp4SegmentInspection, EncoderStartError> {
    let _runtime = MfRuntime::start()?;
    unsafe {
        let url = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
        let reader = MFCreateSourceReaderFromURL(
            &url,
            None::<&windows::Win32::Media::MediaFoundation::IMFAttributes>,
        )
        .map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Io,
            diagnostics: format!("MP4 Source Reader open failed: {error}"),
        })?;
        let media_type = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32)
            .map_err(|error| device_error("MP4 video media type", error))?;
        // Both codecs this app records (task1770). The gate exists to reject a
        // file this app did not write, not to prefer one codec: everything
        // below reads sample timing and container-level attributes, none of
        // which is H.264's. Left hardcoded, an AV1 session inspected as
        // "not H.264" would report a defect that is not there.
        let subtype = media_type.GetGUID(&MF_MT_SUBTYPE).ok();
        if subtype != Some(MFVideoFormat_H264) && subtype != Some(MFVideoFormat_AV1) {
            return Err(EncoderStartError {
                kind: EncoderStartErrorKind::UnsupportedFormat,
                diagnostics: format!("MP4 video stream is neither H.264 nor AV1: {subtype:?}"),
            });
        }
        let configured_bitrate = media_type.GetUINT32(&MF_MT_AVG_BITRATE).unwrap_or_default();
        let (sample_count, first_clean_point, first_timestamp, last_timestamp) =
            scan_video_samples(&reader)?;
        if sample_count == 0
            || !first_clean_point
            || first_timestamp.is_none_or(|timestamp| timestamp < 0)
        {
            return Err(EncoderStartError { kind: EncoderStartErrorKind::UnsupportedFormat, diagnostics: format!("MP4 segment invalid: samples={sample_count}, clean={first_clean_point}, first={first_timestamp:?}") });
        }
        let audio = reader
            .GetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32)
            .ok();
        let has_aac_audio = audio.as_ref().is_some_and(|media_type| {
            media_type.GetGUID(&MF_MT_SUBTYPE).ok() == Some(MFAudioFormat_AAC)
        });
        let audio_sample_rate = audio
            .as_ref()
            .and_then(|media_type| media_type.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).ok())
            .unwrap_or_default();
        let audio_channels = audio
            .as_ref()
            .and_then(|media_type| media_type.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).ok())
            .unwrap_or_default();
        let audio_bitrate = audio
            .as_ref()
            .and_then(|media_type| media_type.GetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND).ok())
            .unwrap_or_default()
            * 8;
        let (audio_sample_count, first_audio_timestamp, last_audio_timestamp) =
            scan_audio_samples(&reader, has_aac_audio)?;
        Ok(Mp4SegmentInspection {
            sample_count,
            first_sample_clean_point: first_clean_point,
            duration_100ns: last_timestamp,
            configured_bitrate,
            has_aac_audio,
            audio_sample_rate,
            audio_channels,
            audio_bitrate,
            audio_sample_count,
            first_audio_timestamp_100ns: first_audio_timestamp,
            last_audio_timestamp_100ns: last_audio_timestamp,
        })
    }
}

/// Task062 Step1: per-track box-level measurement of a finalized fMP4 segment
/// (`mdhd.timescale`, summed `trun` sample durations, sample count), read
/// directly from the container's own boxes rather than decoded via Media
/// Foundation. Lets the "audio AUs are being discarded at the segment
/// boundary" hypothesis be checked against real recorded segments without
/// re-recording anything.
/// `total_sample_duration_ticks` is in this track's own `timescale` (e.g.
/// 48000 for AAC audio, commonly 60000 for this app's video) — divide by
/// `timescale` for seconds, not by 10_000_000.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackBoxSummary {
    pub track_id: u32,
    pub is_audio: bool,
    pub timescale: u32,
    pub sample_count: u64,
    pub total_sample_duration_ticks: i64,
}

pub fn dump_segment_track_summary(path: &Path) -> io::Result<Vec<TrackBoxSummary>> {
    super::mp4_boxes::summarize_tracks(path).map(|entries| {
        entries
            .into_iter()
            .map(|entry| TrackBoxSummary {
                track_id: entry.track_id,
                is_audio: entry.is_audio,
                timescale: entry.timescale,
                sample_count: entry.sample_count,
                total_sample_duration_ticks: entry.total_sample_duration_ticks,
            })
            .collect()
    })
}

pub fn inspect_pcm_peak(path: &Path) -> Result<i16, EncoderStartError> {
    let _runtime = MfRuntime::start()?;
    unsafe {
        let url = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
        let reader = MFCreateSourceReaderFromURL(
            &url,
            None::<&windows::Win32::Media::MediaFoundation::IMFAttributes>,
        )
        .map_err(|error| device_error("MP4 audio Source Reader open", error))?;
        let pcm = MFCreateMediaType().map_err(|error| device_error("PCM media type", error))?;
        pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
            .map_err(|error| device_error("PCM major type", error))?;
        pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)
            .map_err(|error| device_error("PCM subtype", error))?;
        pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, 48_000)
            .map_err(|error| device_error("PCM sample rate", error))?;
        pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, 2)
            .map_err(|error| device_error("PCM channels", error))?;
        pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
            .map_err(|error| device_error("PCM bits", error))?;
        pcm.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)
            .map_err(|error| device_error("PCM block alignment", error))?;
        pcm.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 192_000)
            .map_err(|error| device_error("PCM byte rate", error))?;
        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32, None, &pcm)
            .map_err(|error| device_error("AAC to PCM decoder", error))?;
        let mut peak = 0_i16;
        loop {
            let mut flags = 0;
            let mut timestamp = 0;
            let mut sample = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32,
                    0,
                    None,
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample),
                )
                .map_err(|error| device_error("PCM sample", error))?;
            if let Some(sample) = sample {
                let buffer = sample
                    .ConvertToContiguousBuffer()
                    .map_err(|error| device_error("PCM buffer", error))?;
                let mut bytes = std::ptr::null_mut();
                let mut current = 0;
                buffer
                    .Lock(&mut bytes, None, Some(&mut current))
                    .map_err(|error| device_error("PCM buffer lock", error))?;
                let values = std::slice::from_raw_parts(
                    bytes.cast::<i16>(),
                    current as usize / std::mem::size_of::<i16>(),
                );
                for value in values {
                    peak = peak.max(value.saturating_abs());
                }
                buffer
                    .Unlock()
                    .map_err(|error| device_error("PCM buffer unlock", error))?;
            }
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
        }
        Ok(peak)
    }
}
