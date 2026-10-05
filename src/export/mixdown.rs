//! The mixed audio track of a multi-track export or clip (t261003-b713).
//!
//! Decisions 12-15: every track is decoded, scaled by its saved level (never
//! the review screen's master), summed, peak-limited and encoded once more as
//! AAC. The original tracks themselves are still copied untouched beside it --
//! this only builds the extra one.
use windows::Win32::Media::MediaFoundation::{
    IMFMediaType, IMFSample, MFAudioFormat_Float, MFCreateMediaType, MFMediaType_Audio,
    MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READERF_ENDOFSTREAM,
};

use crate::capture::audio::{AacEncoder, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};
use crate::ui_state::export_failure as msg;
use crate::ui_state::locale::active as ui_locale;

use super::probe::{audio_stream_indices, open_reader};
use super::{ExportFailure, TrackLevel};

const CHANNELS: usize = AUDIO_CHANNELS as usize;

/// 100ns units to whole frames at the AAC rate, rounded to nearest.
fn frames_of(span_100ns: i64) -> i64 {
    (span_100ns * i64::from(AUDIO_SAMPLE_RATE) + 5_000_000).div_euclid(10_000_000)
}

fn encode_failed(error: impl std::fmt::Display) -> ExportFailure {
    ExportFailure::new(msg::encoder_device_failed(
        ui_locale(),
        format!("mixdown AAC: {error}"),
    ))
}

/// One continuous mixed track across a part: one encoder, fed segment by
/// segment in order, so the AAC stream has no seam of its own.
pub(super) struct Mixdown {
    encoder: AacEncoder,
    levels: Vec<TrackLevel>,
    /// Where the last fed window ended, relative to the part's start.
    end: Option<i64>,
}

impl Mixdown {
    pub(super) fn new(levels: &[TrackLevel]) -> Result<Self, ExportFailure> {
        // The system AAC MFT is a COM object; the helper's main thread and a
        // test thread have not necessarily joined an apartment.
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
        };
        Ok(Self {
            encoder: AacEncoder::create().map_err(encode_failed)?,
            levels: levels.to_vec(),
            end: None,
        })
    }

    pub(super) fn media_type(&self) -> &IMFMediaType {
        self.encoder.output_media_type()
    }

    /// Mixes `[window.0, window.1)` (absolute 100ns) of one segment and
    /// returns the AAC it encodes to, stamped relative to `part_start`. The
    /// window is the segment's selected track-0 access units, so the mix
    /// covers exactly what the original tracks do.
    pub(super) fn segment(
        &mut self,
        segment: &crate::ring_buffer::SegmentRecord,
        window: (i64, i64),
        part_start: i64,
    ) -> Result<Vec<(i64, IMFSample)>, ExportFailure> {
        let blocks = unsafe { decode_tracks(segment)? };
        let frames = frames_of(window.1 - window.0).max(0) as usize;
        let mut pcm = mix_blocks(window.0, frames, &blocks, &self.levels);
        limit_peaks(&mut pcm);
        let at = window.0 - part_start;
        self.end = Some(window.1 - part_start);
        self.encode(&pcm, at)
    }

    /// Pushes the encoder's last access units out with trailing silence and
    /// keeps only those that start inside the mixed span (the AAC MFT holds a
    /// frame back and has no drain here).
    pub(super) fn finish(&mut self) -> Result<Vec<(i64, IMFSample)>, ExportFailure> {
        let Some(end) = self.end else {
            return Ok(Vec::new());
        };
        let tail = vec![0.0; 4 * 1024 * CHANNELS];
        Ok(self
            .encode(&tail, end)?
            .into_iter()
            .filter(|(timestamp, _)| *timestamp < end)
            .collect())
    }

    fn encode(&self, pcm: &[f32], at: i64) -> Result<Vec<(i64, IMFSample)>, ExportFailure> {
        self.encoder
            .encode_f32(pcm, at)
            .map_err(encode_failed)?
            .into_iter()
            .map(|sample| {
                let timestamp = unsafe { sample.GetSampleTime() }.map_err(encode_failed)?;
                Ok((timestamp, sample))
            })
            .collect()
    }
}

/// Sums every track's decoded blocks into `frames` interleaved frames starting
/// at `window_start`, each scaled by its own level. A block is placed by its
/// own timestamp, so a track that is missing in part (or entirely) leaves
/// silence there rather than shifting anything.
pub(super) fn mix_blocks(
    window_start: i64,
    frames: usize,
    blocks: &[(usize, i64, Vec<f32>)],
    levels: &[TrackLevel],
) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * CHANNELS];
    for (track, absolute, block) in blocks {
        let scale = levels.get(*track).copied().unwrap_or_default().scale();
        if scale == 0.0 {
            continue;
        }
        let first = frames_of(absolute - window_start);
        for (frame, samples) in block.chunks_exact(CHANNELS).enumerate() {
            let Ok(at) = usize::try_from(first + frame as i64) else {
                continue;
            };
            if at >= frames {
                break;
            }
            for (slot, sample) in out[at * CHANNELS..][..CHANNELS].iter_mut().zip(samples) {
                *slot += sample * scale;
            }
        }
    }
    out
}

/// Decision 15. A soft knee rather than playback's hard clamp
/// (`playback::mixer::clip`): below `KNEE` (-1.9 dBFS) nothing changes, so a
/// mix that fits is untouched; above it the excess is folded into the
/// remaining headroom with `tanh`, which approaches but never reaches full
/// scale. Stateless per sample: no look-ahead, no gain riding between blocks.
pub(super) fn limit_peaks(pcm: &mut [f32]) {
    const KNEE: f32 = 0.8;
    const ROOM: f32 = 1.0 - KNEE;
    for sample in pcm {
        let level = sample.abs();
        if level > KNEE {
            *sample = sample.signum() * (KNEE + ROOM * ((level - KNEE) / ROOM).tanh());
        }
    }
}

/// One AAC access unit of digital silence, steady state, for a track a segment
/// does not have (it was added later in the recording). The export writer
/// stamps a fresh sample with the time it is given, so this one sample serves
/// every position.
pub(super) fn silent_aac_frame() -> Result<IMFSample, ExportFailure> {
    let _ = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
    let encoder = AacEncoder::create().map_err(encode_failed)?;
    encoder
        .encode_f32(&vec![0.0; 8 * 1024 * CHANNELS], 0)
        .map_err(encode_failed)?
        .pop()
        .ok_or_else(|| encode_failed("the encoder returned no silent frame"))
}

/// Every audio track of a segment, decoded to 48 kHz stereo float: `(track,
/// absolute 100ns, interleaved block)`. Same timing rule as the copied tracks:
/// a block before its track's priming shift is the priming unit and is
/// dropped, and the shift is undone on the rest (task204).
///
/// # Safety
///
/// Media Foundation must be initialised on this thread.
pub(super) unsafe fn decode_tracks(
    segment: &crate::ring_buffer::SegmentRecord,
) -> Result<Vec<(usize, i64, Vec<f32>)>, ExportFailure> {
    unsafe {
        let reader = open_reader(&segment.path)?;
        let read_failed = |error: windows::core::Error| {
            ExportFailure::new(msg::aac_sample_read_failed(ui_locale(), error))
        };
        let mut out = Vec::new();
        for (track, stream) in audio_stream_indices(&reader).into_iter().enumerate() {
            let pcm = MFCreateMediaType().map_err(read_failed)?;
            pcm.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
                .map_err(read_failed)?;
            pcm.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_Float)
                .map_err(read_failed)?;
            pcm.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)
                .map_err(read_failed)?;
            pcm.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, u32::from(AUDIO_CHANNELS))
                .map_err(read_failed)?;
            pcm.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 32)
                .map_err(read_failed)?;
            reader
                .SetCurrentMediaType(stream, None, &pcm)
                .map_err(read_failed)?;
            let offset = segment.audio_offsets_100ns.get(track).copied().unwrap_or(0);
            loop {
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
                    .map_err(read_failed)?;
                if let Some(sample) = sample.filter(|_| timestamp >= offset) {
                    let buffer = sample.ConvertToContiguousBuffer().map_err(read_failed)?;
                    let mut bytes = std::ptr::null_mut();
                    let mut length = 0u32;
                    buffer
                        .Lock(&mut bytes, None, Some(&mut length))
                        .map_err(read_failed)?;
                    let floats = std::slice::from_raw_parts(
                        bytes.cast::<f32>(),
                        length as usize / std::mem::size_of::<f32>(),
                    )
                    .to_vec();
                    let _ = buffer.Unlock();
                    out.push((track, segment.start_100ns + timestamp - offset, floats));
                }
                if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                    break;
                }
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn level(volume_percent: u8, muted: bool) -> TrackLevel {
        TrackLevel {
            volume_percent,
            muted,
        }
    }

    fn block(value: f32, frames: usize) -> Vec<f32> {
        vec![value; frames * CHANNELS]
    }

    /// Each track's own volume and mute decide its share of the sum.
    #[test]
    fn each_track_is_scaled_by_its_own_level() {
        let blocks = vec![(0, 0, block(0.2, 4)), (1, 0, block(0.4, 4))];
        let mixed = mix_blocks(0, 4, &blocks, &[level(100, false), level(50, false)]);
        assert!(mixed.iter().all(|s| (s - 0.4).abs() < 1e-6), "{mixed:?}");
        let mixed = mix_blocks(0, 4, &blocks, &[level(100, false), level(100, true)]);
        assert!(mixed.iter().all(|s| (s - 0.2).abs() < 1e-6), "{mixed:?}");
        // A track past the end of the levels plays at 100%.
        let mixed = mix_blocks(0, 4, &blocks, &[]);
        assert!(mixed.iter().all(|s| (s - 0.6).abs() < 1e-6), "{mixed:?}");
    }

    /// A block lands where its timestamp says; a track that starts later
    /// leaves the earlier frames to the others alone.
    #[test]
    fn a_block_is_placed_by_its_own_timestamp() {
        // 1024 frames = 213_333 100ns at 48 kHz.
        let blocks = vec![(0, 0, block(0.1, 2048)), (1, 213_333, block(0.3, 1024))];
        let mixed = mix_blocks(0, 2048, &blocks, &[]);
        assert!((mixed[1023 * CHANNELS] - 0.1).abs() < 1e-6);
        assert!((mixed[1024 * CHANNELS] - 0.4).abs() < 1e-6);
        assert!((mixed[2047 * CHANNELS] - 0.4).abs() < 1e-6);
    }

    /// Decision 15: a sum past full scale comes back inside it, and one that
    /// fits below the knee is untouched.
    #[test]
    fn peaks_are_limited_and_quiet_audio_is_untouched() {
        let mut loud = mix_blocks(0, 4, &[(0, 0, block(0.9, 4)), (1, 0, block(0.9, 4))], &[]);
        limit_peaks(&mut loud);
        assert!(loud.iter().all(|s| *s < 1.0 && *s > 0.8), "{loud:?}");
        let mut negative = vec![-1.8f32];
        limit_peaks(&mut negative);
        assert!(negative[0] > -1.0 && negative[0] < -0.8);
        let mut quiet = vec![0.5f32, -0.79];
        limit_peaks(&mut quiet);
        assert_eq!(quiet, vec![0.5, -0.79]);
    }
}
