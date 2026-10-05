use std::time::Duration;

use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioClient, IAudioRenderClient, IAudioStreamVolume, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use super::AUDIO_BUFFER;

// ---------- WASAPI output ----------

/// 48k stereo f32; `AUTOCONVERTPCM` makes the shared-mode engine accept it
/// regardless of the device's mix format.
pub(super) const AUDIO_RATE: u32 = 48_000;
pub(super) const AUDIO_CHANNELS: u32 = 2;

/// How long a clip's audio takes to come up when it opens, and to go down when
/// another clip replaces it (t260926-2144).
pub(super) const FADE: Duration = Duration::from_millis(80);
/// [`FADE`] in frames at [`AUDIO_RATE`].
pub(super) const FADE_FRAMES: usize = (AUDIO_RATE as u128 * FADE.as_millis() / 1000) as usize;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum FadeDir {
    In,
    Out,
}

/// Equal-power gain at `t` (0..=1) through a fade: `sin` up, `cos` down, so a
/// crossfade's two sides always sum to unit power.
pub(super) fn equal_power(dir: FadeDir, t: f32) -> f32 {
    let angle = t.clamp(0.0, 1.0) * std::f32::consts::FRAC_PI_2;
    match dir {
        FadeDir::In => angle.sin(),
        // `cos` as the mirrored `sin`, so both ends are exact: silence is 0.0.
        FadeDir::Out => (std::f32::consts::FRAC_PI_2 - angle).sin(),
    }
}

/// A per-frame gain ramp over the samples as they go to the device, so it
/// lasts [`FADE`] of real time whatever the playback rate (t260926-2144).
#[derive(Clone, Copy, Debug)]
pub(super) struct Ramp {
    dir: FadeDir,
    pos: usize,
    len: usize,
}

impl Ramp {
    pub(super) fn fade_in(len: usize) -> Self {
        Self {
            dir: FadeDir::In,
            pos: 0,
            len,
        }
    }

    fn gain(&self) -> f32 {
        equal_power(self.dir, self.pos as f32 / self.len.max(1) as f32)
    }

    /// Past the fade-in: the samples pass untouched.
    fn is_unity(&self) -> bool {
        self.dir == FadeDir::In && self.pos >= self.len
    }

    /// Nothing written through it yet, so the next write starts the fade-in.
    fn starts_now(&self) -> bool {
        self.dir == FadeDir::In && self.pos == 0
    }

    pub(super) fn is_silent(&self) -> bool {
        self.dir == FadeDir::Out && self.pos >= self.len
    }

    /// Turns around from wherever the gain is now: `cos` at `len - pos` is
    /// the `sin` at `pos`, so a fade-in cut short goes down without a step.
    pub(super) fn fade_out(&mut self) {
        if self.dir == FadeDir::In {
            self.pos = self.len - self.pos.min(self.len);
            self.dir = FadeDir::Out;
        }
    }

    /// Scales interleaved `samples`, one gain per frame across `channels`.
    pub(super) fn apply(&mut self, samples: &mut [f32], channels: usize) {
        for frame in samples.chunks_mut(channels) {
            let gain = self.gain();
            frame.iter_mut().for_each(|sample| *sample *= gain);
            self.pos = (self.pos + 1).min(self.len);
        }
    }
}

pub(super) struct AudioOut {
    client: IAudioClient,
    render: IAudioRenderClient,
    volume: IAudioStreamVolume,
    buffer_frames: u32,
    started: bool,
    /// Armed as a fade-in at construction: the first audio an engine ever
    /// writes comes up over [`FADE`]. Seeks, pauses and flushes never re-arm it.
    ramp: Ramp,
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
            let volume = client.GetService::<IAudioStreamVolume>()?;
            Ok(Self {
                client,
                render,
                volume,
                buffer_frames,
                started: false,
                ramp: Ramp::fade_in(FADE_FRAMES),
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
            if !self.ramp.is_unity() {
                if self.ramp.starts_now() {
                    tracing::info!(
                        event = "audio_fade",
                        dir = "in",
                        frames = FADE_FRAMES as u64,
                        "audio fade"
                    );
                }
                let taken = std::slice::from_raw_parts_mut(
                    target.cast::<f32>(),
                    (frames * AUDIO_CHANNELS) as usize,
                );
                self.ramp.apply(taken, AUDIO_CHANNELS as usize);
            }
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

    /// The master level, applied by the engine as it *consumes* the endpoint
    /// rather than baked into the samples we write: the up-to-`AUDIO_BUFFER`
    /// a pause keeps queued was scaled at the old level, so a mute while
    /// paused used to let that much through on the next play.
    pub(super) fn set_volume(&self, scale: f32) {
        unsafe {
            let channels = self.volume.GetChannelCount().unwrap_or(AUDIO_CHANNELS);
            let _ = self.volume.SetAllVolumes(&vec![scale; channels as usize]);
        }
    }

    /// Starts the fade-out on the next frames written (t260926-2144). What the
    /// endpoint already holds still plays at the old gain first.
    pub(super) fn fade_out(&mut self) {
        self.ramp.fade_out();
    }

    /// The whole fade-out has been written; everything after it is silence.
    pub(super) fn faded_out(&self) -> bool {
        self.ramp.is_silent()
    }

    pub(super) fn flush(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = self.client.Reset();
        }
        self.started = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fade_is_3840_frames_at_48k() {
        assert_eq!(FADE_FRAMES, 3840);
    }

    #[test]
    fn equal_power_gains_meet_their_endpoints() {
        assert_eq!(equal_power(FadeDir::In, 0.0), 0.0);
        assert!((equal_power(FadeDir::In, 1.0) - 1.0).abs() < 1e-6);
        assert_eq!(equal_power(FadeDir::Out, 0.0), 1.0);
        assert!(equal_power(FadeDir::Out, 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_crossfade_keeps_unit_power_all_the_way() {
        for step in 0..=20 {
            let t = step as f32 / 20.0;
            let power = equal_power(FadeDir::In, t).powi(2) + equal_power(FadeDir::Out, t).powi(2);
            assert!((power - 1.0).abs() < 1e-5, "t={t} power={power}");
        }
        // Somewhere strictly inside, so a ramp that jumps 0 -> 1 fails too.
        assert!((equal_power(FadeDir::In, 0.5) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-6);
    }

    #[test]
    fn a_ramp_fades_in_turns_around_without_a_step_and_ends_silent() {
        let mut ramp = Ramp::fade_in(8);
        let mut samples = vec![1.0f32; 2 * 3];
        ramp.apply(&mut samples, 2);
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[0], samples[1], "both channels share a gain");
        let before = ramp.gain();
        ramp.fade_out();
        assert!(
            (ramp.gain() - before).abs() < 1e-6,
            "turned around with a step"
        );
        let mut rest = vec![1.0f32; 2 * 10];
        ramp.apply(&mut rest, 2);
        assert!(ramp.is_silent());
        assert_eq!(rest[rest.len() - 1], 0.0);
    }
}
