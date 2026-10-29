use std::mem::ManuallyDrop;

use windows::Win32::{
    Media::MediaFoundation::{
        AACMFTEncoder, IMFActivate, IMFMediaType, IMFSample, IMFTransform, MFAudioFormat_AAC,
        MFAudioFormat_PCM, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
        MFMediaType_Audio, MFTEnumEx, MFT_CATEGORY_AUDIO_ENCODER, MFT_ENUM_FLAG_ALL,
        MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MF_E_TRANSFORM_NEED_MORE_INPUT,
        MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_BLOCK_ALIGNMENT,
        MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE,
    },
    System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
};

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
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
            buffer.Unlock()?;
            Ok(Self {
                time_100ns: sample.GetSampleTime()?,
                duration_100ns: sample.GetSampleDuration()?,
                bytes,
            })
        }
    }

    /// The sample the muxer takes, rebuilt on the capture worker.
    pub fn to_sample(&self) -> windows::core::Result<IMFSample> {
        sample_from_bytes(&self.bytes, self.time_100ns, self.duration_100ns)
    }
}

fn sample_from_bytes(
    bytes: &[u8],
    time_100ns: i64,
    duration_100ns: i64,
) -> windows::core::Result<IMFSample> {
    unsafe {
        let sample = MFCreateSample()?;
        let buffer = MFCreateMemoryBuffer(bytes.len().max(1) as u32)?;
        let mut dest = std::ptr::null_mut();
        buffer.Lock(&mut dest, None, None)?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(bytes.len() as u32)?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(time_100ns)?;
        sample.SetSampleDuration(duration_100ns)?;
        Ok(sample)
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
            let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0;
            MFTEnumEx(
                MFT_CATEGORY_AUDIO_ENCODER,
                MFT_ENUM_FLAG_ALL,
                None,
                None,
                &mut activations,
                &mut count,
            )?;
            let entries = std::slice::from_raw_parts(activations, count as usize);
            let result = entries
                .iter()
                .filter_map(|entry| entry.as_ref())
                .find_map(|activate| {
                    let transform = activate.ActivateObject::<IMFTransform>().ok()?;
                    let input = supported_audio_type(&transform, true, MFAudioFormat_PCM).ok()?;
                    transform.SetInputType(0, &input, 0).ok()?;
                    let output = supported_audio_type(&transform, false, MFAudioFormat_AAC).ok()?;
                    transform.SetOutputType(0, &output, 0).ok()?;
                    let output_type = transform.GetOutputCurrentType(0).ok()?;
                    Some(Self {
                        transform,
                        output_type,
                    })
                });
            crate::encoder::release_mft_activates(activations, count);
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
            let sample = sample_from_bytes(
                bytes,
                timestamp_100ns,
                frames_to_100ns(
                    (pcm.len() / usize::from(AUDIO_CHANNELS)) as u64,
                    AUDIO_SAMPLE_RATE,
                ),
            )?;
            self.transform.ProcessInput(0, &sample, 0)?;
            let mut output = Vec::new();
            loop {
                let info = self.transform.GetOutputStreamInfo(0)?;
                let sample = if info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0 {
                    let sample = MFCreateSample()?;
                    let buffer = MFCreateMemoryBuffer(info.cbSize.max(1))?;
                    sample.AddBuffer(&buffer)?;
                    Some(sample)
                } else {
                    None
                };
                let mut slot = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(sample),
                    ..Default::default()
                };
                let mut status = 0;
                let result =
                    self.transform
                        .ProcessOutput(0, std::slice::from_mut(&mut slot), &mut status);
                let sample = ManuallyDrop::into_inner(std::ptr::read(&slot.pSample));
                drop(ManuallyDrop::into_inner(std::ptr::read(&slot.pEvents)));
                match result {
                    Ok(()) => {
                        if let Some(sample) = sample {
                            output.push(sample);
                        }
                    }
                    Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(error) => return Err(error),
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
        for index in 0..64 {
            let media_type = if input {
                transform.GetInputAvailableType(0, index)
            } else {
                transform.GetOutputAvailableType(0, index)
            };
            let Ok(media_type) = media_type else {
                break;
            };
            if media_type.GetGUID(&MF_MT_SUBTYPE).ok() != Some(subtype) {
                continue;
            }
            media_type.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, AUDIO_SAMPLE_RATE)?;
            media_type.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, AUDIO_CHANNELS as u32)?;
            if input {
                media_type.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
                media_type.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, 4)?;
                media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_SAMPLE_RATE * 4)?;
            } else {
                media_type.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AUDIO_BITRATE / 8)?;
            }
            return Ok(media_type);
        }
        Err(windows::core::Error::from_win32())
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
