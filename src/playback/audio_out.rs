use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use super::AUDIO_BUFFER;

// ---------- WASAPI output ----------

/// 48k stereo f32; `AUTOCONVERTPCM` makes the shared-mode engine accept it
/// regardless of the device's mix format.
pub(super) const AUDIO_RATE: u32 = 48_000;
pub(super) const AUDIO_CHANNELS: u32 = 2;

pub(super) struct AudioOut {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_frames: u32,
    started: bool,
}

impl AudioOut {
    pub(super) fn new() -> Result<Self, windows::core::Error> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let format = WAVEFORMATEX {
                // WAVE_FORMAT_IEEE_FLOAT; the constant lives in a Multimedia
                // feature this crate doesn't otherwise need.
                wFormatTag: 3,
                nChannels: AUDIO_CHANNELS as u16,
                nSamplesPerSec: AUDIO_RATE,
                nAvgBytesPerSec: AUDIO_RATE * AUDIO_CHANNELS * 4,
                nBlockAlign: (AUDIO_CHANNELS * 4) as u16,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                // 100ns units; double the fill target so the buffer never caps it.
                (AUDIO_BUFFER.as_nanos() as i64 / 100) * 2,
                0,
                &format,
                None,
            )?;
            let buffer_frames = client.GetBufferSize()?;
            let render = client.GetService::<IAudioRenderClient>()?;
            Ok(Self {
                client,
                render,
                buffer_frames,
                started: false,
            })
        }
    }

    pub(super) fn queued(&self) -> Duration {
        let padding = unsafe { self.client.GetCurrentPadding().unwrap_or(0) };
        Duration::from_secs_f64(f64::from(padding) / f64::from(AUDIO_RATE))
    }

    /// Interleaved stereo f32 frames. Returns how many **frames** were actually
    /// taken, which can be fewer than offered when the endpoint buffer is
    /// nearly full -- the caller has to hold the remainder rather than assume
    /// it landed (task154). At 1x the `AUDIO_BUFFER` fill loop keeps the offer
    /// small enough that this is almost always the whole block; under
    /// time-stretch it stops being true, and a silently dropped tail is a
    /// glitch.
    pub(super) fn write(&mut self, samples: &[f32]) -> usize {
        let frames = samples.len() as u32 / AUDIO_CHANNELS;
        if frames == 0 {
            return 0;
        }
        unsafe {
            let padding = self.client.GetCurrentPadding().unwrap_or(0);
            let available = self.buffer_frames.saturating_sub(padding);
            let frames = frames.min(available);
            if frames == 0 {
                return 0;
            }
            let Ok(target) = self.render.GetBuffer(frames) else {
                return 0;
            };
            std::ptr::copy_nonoverlapping(
                samples.as_ptr().cast::<u8>(),
                target,
                (frames * AUDIO_CHANNELS * 4) as usize,
            );
            let _ = self.render.ReleaseBuffer(frames, 0);
            if !self.started {
                let _ = self.client.Start();
                self.started = true;
            }
            frames as usize
        }
    }

    /// Silence now, but the queued frames stay queued: a user pause is not a
    /// position change, and what is already in the endpoint is exactly the
    /// audio that follows. Discarding it here (as `flush` does) would leave the
    /// segment reader a whole `AUDIO_BUFFER` ahead of the clock, and the
    /// segment's audio would then run dry that much before its video does --
    /// a dropout at the next crossing, one per pause.
    pub(super) fn pause(&mut self) {
        if self.started {
            unsafe {
                let _ = self.client.Stop();
            }
            self.started = false;
        }
    }

    /// Explicit, not left to the next `write`: after a pause the endpoint can
    /// still hold a full buffer, in which case the fill loop never offers
    /// anything and a write-side `Start` would never fire.
    pub(super) fn resume(&mut self) {
        if !self.started {
            unsafe {
                let _ = self.client.Start();
            }
            self.started = true;
        }
    }

    pub(super) fn flush(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
        self.started = false;
    }
}
