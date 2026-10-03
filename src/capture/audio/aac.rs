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

use super::{AUDIO_BITRATE, AUDIO_CHANNELS, AUDIO_SAMPLE_RATE};

pub struct AacEncoder {
    transform: IMFTransform,
    output_type: IMFMediaType,
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

    fn encode_i16(
        &self,
        pcm: &[i16],
        timestamp_100ns: i64,
    ) -> windows::core::Result<Vec<IMFSample>> {
        if pcm.is_empty() {
            return Ok(Vec::new());
        }
        unsafe {
            let sample = MFCreateSample()?;
            let bytes =
                std::slice::from_raw_parts(pcm.as_ptr().cast::<u8>(), std::mem::size_of_val(pcm));
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut dest = std::ptr::null_mut();
            let mut max = 0;
            let mut current = 0;
            buffer.Lock(&mut dest, Some(&mut max), Some(&mut current))?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dest, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(timestamp_100ns)?;
            sample.SetSampleDuration(
                (pcm.len() as i64 / i64::from(AUDIO_CHANNELS)) * 10_000_000
                    / i64::from(AUDIO_SAMPLE_RATE),
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
