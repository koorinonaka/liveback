use windows::Win32::{
    Media::MediaFoundation::{
        AACMFTEncoder, IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC, MFAudioFormat_PCM,
        MFCreateMediaType, MFMediaType_Audio, MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_ALL,
        MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
        MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    },
    System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
};

use super::hardware::{first_available_type, process_output_once, with_mft_activates};

/// The one audio format every track is recorded in: 48kHz stereo, AAC at
/// 192kbps. The capture side normalizes whatever an endpoint delivers to the
/// first two; export's mixdown encodes at the same rate.
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_BITRATE: u32 = 192_000;

/// Converts a frame count at `sample_rate` to duration in 100ns units (QPC's unit).
pub(crate) fn frames_to_100ns(frames: u64, sample_rate: u32) -> i64 {
    (frames as i64 * 10_000_000) / sample_rate.max(1) as i64
}

pub struct AacEncoder {
    transform: IMFTransform,
    output_type: IMFMediaType,
}

// SAFETY: created on the capture worker and moved, once, to the audio thread
// that encodes with it (t261025-cec6); one thread uses it at a time. Both
// threads are MTA (`CoInitializeEx(COINIT_MULTITHREADED)`), so the system AAC
// MFT's interfaces stay valid across the move.
unsafe impl Send for AacEncoder {}

/// One AAC access unit as plain bytes: what an audio thread hands the capture
/// worker (t261025-cec6), so the encode runs off the 120fps thread.
/// `write_aac` reads only the buffer, the time and the duration of a sample.
pub struct AacUnit {
    time_100ns: i64,
    duration_100ns: i64,
    bytes: Vec<u8>,
}

impl AacUnit {
    fn read(sample: &IMFSample) -> windows::core::Result<Self> {
        unsafe {
            Ok(Self {
                bytes: super::mf_sample::with_bytes(sample, <[u8]>::to_vec)?,
                time_100ns: sample.GetSampleTime()?,
                duration_100ns: sample.GetSampleDuration()?,
            })
        }
    }

    /// The sample the muxer takes, rebuilt on the capture worker.
    pub fn to_sample(&self) -> windows::core::Result<IMFSample> {
        super::mf_sample::from_bytes(
            &self.bytes,
            Some(self.time_100ns),
            Some(self.duration_100ns),
        )
    }
}

impl AacEncoder {
    /// System AAC MFT, fixed 48 kHz stereo float input and 192 kbps output.
    pub fn create() -> windows::core::Result<Self> {
        unsafe {
            if let Ok(transform) =
                CoCreateInstance::<_, IMFTransform>(&AACMFTEncoder, None, CLSCTX_INPROC_SERVER)
            {
                if let (Ok(input), Ok(output)) = (pcm_i16_media_type(), aac_media_type()) {
                    if transform.SetInputType(0, &input, 0).is_ok()
                        && transform.SetOutputType(0, &output, 0).is_ok()
                    {
                        return Ok(Self {
                            output_type: transform.GetOutputCurrentType(0)?,
                            transform,
                        });
                    }
                }
            }
            let result = with_mft_activates(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_ALL,
                None,
                None,
                |entries| {
                    entries
                        .iter()
                        .filter_map(|entry| entry.as_ref())
                        .find_map(|activate| {
                            let transform = activate.ActivateObject::<IMFTransform>().ok()?;
                            let input =
                                supported_audio_type(&transform, true, MFAudioFormat_PCM).ok()?;
                            transform.SetInputType(0, &input, 0).ok()?;
                            let output =
                                supported_audio_type(&transform, false, MFAudioFormat_AAC).ok()?;
                            transform.SetOutputType(0, &output, 0).ok()?;
                            let output_type = transform.GetOutputCurrentType(0).ok()?;
                            Some(Self {
                                transform,
                                output_type,
                            })
                        })
                },
            )?;
            result.ok_or_else(windows::core::Error::from_win32)
        }
    }

    pub fn output_media_type(&self) -> &IMFMediaType {
        &self.output_type
    }

    pub fn encode_f32(
        &self,
        pcm: &[f32],
        timestamp_100ns: i64,
    ) -> windows::core::Result<Vec<IMFSample>> {
        let pcm: Vec<i16> = pcm
            .iter()
            .map(|sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
            .collect();
        self.encode_i16(&pcm, timestamp_100ns)
    }

    /// `encode_f32`, read out into bytes an audio thread can send.
    pub fn encode_units(
        &self,
        pcm: &[f32],
        timestamp_100ns: i64,
    ) -> windows::core::Result<Vec<AacUnit>> {
        self.encode_f32(pcm, timestamp_100ns)?
            .iter()
            .map(AacUnit::read)
            .collect()
    }

    fn encode_i16(
        &self,
        pcm: &[i16],
        timestamp_100ns: i64,
    ) -> windows::core::Result<Vec<IMFSample>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            let bytes =
                std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), std::mem::size_of_val(pcm));
            let sample = super::mf_sample::from_bytes(
                bytes,
                Some(timestamp_100ns),
                Some(frames_to_100ns(
                    (pcm.len() / usize::from(AUDIO_CHANNELS)) as u64,
                    AUDIO_SAMPLE_RATE,
                )),
            )?;
            self.transform.ProcessInput(0, &sample, 0)?;
            let mut output = Vec::new();
            loop {
                match process_output_once(&self.transform) {
                    Ok(Some(sample)) => output.push(sample),
                    Ok(None) => {}
                    Err(("ProcessOutput", error))
                        if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT =>
                    {
                        break
                    }
                    Err((_, error)) => return Err(error),
                }
            }
            Ok(output)
        }
    }
}

fn supported_audio_type(
    transform: &IMFTransform,
    input: bool,
    subtype: windows::core::GUID,
) -> windows::core::Result<IMFMediaType> {
    unsafe {
        first_available_type::<windows::core::Error>(transform, input, subtype, |media_type| {
            media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
            media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)?;
            if input {
                media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_SAMPLE_RATE * 4)?;
            } else {
                media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
            }
            Ok(())
        })?
        .ok_or_else(windows::core::Error::from_win32)
    }
}

fn pcm_i16_media_type() -> windows::core::Result<IMFMediaType> {
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
        media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
        media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_SAMPLE_RATE * 4)?;
        Ok(media_type)
    }
}

fn aac_media_type() -> windows::core::Result<IMFMediaType> {
    unsafe {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
        media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
        media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)?;
        media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
        Ok(media_type)
    }
}
