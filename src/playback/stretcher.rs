//! Pitch-corrected time stretch for non-1x playback (task155).
//!
//! Everything below is a thin wrapper over one `signalsmith_stretch::Stretch`.
//! Thin on purpose: the crate was chosen for quality but it is a C++ library
//! behind bindgen, and keeping the seam this narrow is what makes swapping it
//! (for the pure-Rust fallback the task named, or anything else) a small change
//! rather than a rewrite of `pump_audio`.
//!
//! The stretch ratio is expressed as a length ratio, which is exactly what
//! `Stretch::process` reads it from: n input frames handed in with n/rate
//! output frames asked for *is* the instruction to stretch. The fractional
//! remainder is accumulated across blocks so a long session cannot drift --
//! at 48 kHz and ~10 ms blocks, dropping the fraction each time would lose
//! most of a frame per block.

use signalsmith_stretch::Stretch;

use super::audio_out::{AUDIO_CHANNELS, AUDIO_RATE};

pub(super) struct AudioStretcher {
    inner: Stretch,
    rate: f64,
    /// Output frames owed but not yet emitted, carried between blocks.
    frac_acc: f64,
    /// Reused so a steady-state block costs no allocation.
    out: Vec<f32>,
}

impl AudioStretcher {
    pub(super) fn new(rate: f64) -> Self {
        let mut stretcher = Self {
            inner: Stretch::preset_default(AUDIO_CHANNELS, AUDIO_RATE),
            rate,
            frac_acc: 0.0,
            out: Vec::new(),
        };
        stretcher.preroll();
        stretcher
    }

    /// Changing the ratio needs no flush: the stretcher keeps its analysis and
    /// only the length ratio moves, which is what makes a rate change between
    /// two non-1x speeds glitch-free.
    pub(super) fn set_rate(&mut self, rate: f64) {
        self.rate = rate;
    }

    /// Back to a clean stream: used at every point the audio path is flushed
    /// (seek, live-edge park, crossing into or out of 1x). A user pause is not
    /// one of them -- it keeps the analysis, because it resumes at the same
    /// position the window already holds.
    pub(super) fn reset(&mut self) {
        self.inner.reset();
        self.frac_acc = 0.0;
        self.preroll();
    }

    /// Primes the internal buffers so the first real block does not come back
    /// as the analysis window filling up. Silence rather than the audio that
    /// actually precedes the position: at a seek there is no decoded past to
    /// hand over, and the task's design allows the zero-fill.
    fn preroll(&mut self) {
        let frames = self.inner.input_latency();
        if frames == 0 {
            return;
        }
        let silence = vec![0.0f32; frames * AUDIO_CHANNELS as usize];
        self.inner.seek(&silence, self.rate);
    }

    /// Interleaved stereo in, interleaved stereo out. The returned slice is
    /// borrowed from the reused buffer and is only valid until the next call.
    pub(super) fn feed(&mut self, input: &[f32]) -> &[f32] {
        let in_frames = input.len() / AUDIO_CHANNELS as usize;
        let owed = self.frac_acc + in_frames as f64 / self.rate;
        let out_frames = owed.max(0.0).floor() as usize;
        self.frac_acc = owed - out_frames as f64;
        self.out.clear();
        self.out.resize(out_frames * AUDIO_CHANNELS as usize, 0.0);
        self.inner.process(input, &mut self.out);
        &self.out
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    const CHANNELS: usize = AUDIO_CHANNELS as usize;

    /// One segment crossing's worth of stretching has to fit in a frame
    /// (task1540).
    ///
    /// The refill is not spread out: the outgoing segment's audio runs out
    /// ~200ms before its video does, so the first tick that can read the
    /// incoming segment tops the endpoint's queue back up to `AUDIO_BUFFER` in
    /// one pass -- about 19 blocks at x2, all of them through here. That lands
    /// on a tick with an 8.3ms budget, and a tick that overruns costs dropped
    /// frames until the clock catches up.
    ///
    /// This is a *build* guard, not an algorithm one. `signalsmith-stretch`
    /// compiles header-only templated C++ through `cc`, which reads its `/O`
    /// flag from the profile's `OPT_LEVEL` -- so without the
    /// `[profile.dev.package.signalsmith-stretch]` override in `Cargo.toml` a
    /// dev build stretches at 1.39ms a block and this burst costs 26ms, which
    /// is what made x1.5/x2 drop ~3 frames at every crossing. With it: 0.11ms a
    /// block, 2.15ms a burst.
    ///
    /// The bound is the x2 frame budget, and the best of *ten* runs is what it
    /// is measured against -- a scheduler hiccup can inflate any single run,
    /// but nothing makes an unoptimized build fast.
    ///
    /// Ten, not three, because the full suite runs three test binaries at once
    /// and each of them thread-parallel: with three tries the odds of the
    /// scheduler stepping on *all* of them stopped being negligible, and this
    /// went flaky (task3230). What moved is the number of tries, never the 8ms
    /// line -- an optimized build needs one clean run out of ten (2.15ms), and
    /// an unoptimized one is 26ms a run, so no number of tries gets it under
    /// the bound. Raising the threshold instead would narrow that gap and let a
    /// broken profile pass by luck.
    ///
    /// Not `#[ignore]`d either: noticing that the build configuration broke is
    /// the entire job, and a test outside the default suite would not.
    #[test]
    fn a_crossings_worth_of_stretching_fits_in_a_frame() {
        const BLOCKS: usize = 19;
        let mut stretcher = AudioStretcher::new(2.0);
        let block = sine(1024, 440.0);
        // Past the preroll, so what is timed is the steady state.
        for _ in 0..BLOCKS {
            stretcher.feed(&block);
        }
        let best = (0..10)
            .map(|_| {
                let started = Instant::now();
                for _ in 0..BLOCKS {
                    stretcher.feed(&block);
                }
                started.elapsed()
            })
            .min()
            .expect("ten runs");
        assert!(
            best < Duration::from_millis(8),
            "{BLOCKS} blocks took {best:?}; a x2 crossing refill has 8.3ms. \
             Is the signalsmith-stretch opt-level override still in Cargo.toml?"
        );
    }

    fn sine(frames: usize, hz: f64) -> Vec<f32> {
        let mut samples = Vec::with_capacity(frames * CHANNELS);
        for frame in 0..frames {
            let value = (std::f64::consts::TAU * hz * frame as f64 / f64::from(AUDIO_RATE)).sin();
            for _ in 0..CHANNELS {
                samples.push(value as f32);
            }
        }
        samples
    }

    /// Positive-going zero crossings of the left channel over `samples`,
    /// converted to a fundamental frequency. Enough to tell 440 Hz from 220 or
    /// 880, which is the whole question a pitch test has to answer.
    fn fundamental_hz(samples: &[f32]) -> f64 {
        let left: Vec<f32> = samples.iter().step_by(CHANNELS).copied().collect();
        let mut crossings = 0usize;
        let mut first = None;
        let mut last = 0usize;
        for index in 1..left.len() {
            if left[index - 1] <= 0.0 && left[index] > 0.0 {
                crossings += 1;
                first.get_or_insert(index);
                last = index;
            }
        }
        if crossings < 2 {
            return 0.0;
        }
        let span = (last - first.unwrap()) as f64;
        (crossings - 1) as f64 * f64::from(AUDIO_RATE) / span
    }

    fn total_output_frames(rate: f64, block_frames: usize, blocks: usize) -> usize {
        let mut stretcher = AudioStretcher::new(rate);
        let block = sine(block_frames, 440.0);
        let mut frames = 0;
        for _ in 0..blocks {
            frames += stretcher.feed(&block).len() / CHANNELS;
        }
        frames
    }

    #[test]
    fn output_length_tracks_the_rate_without_drifting() {
        // The fractional accumulator is the point: 1024/3 is not an integer, so
        // dropping the remainder each block would lose ~0.3 frames x 200.
        for (rate, block, blocks) in [
            (2.0, 1024, 200),
            (0.25, 512, 200),
            (1.5, 480, 300),
            (0.75, 441, 250),
        ] {
            let expected = (block as f64 * blocks as f64 / rate).floor() as usize;
            let actual = total_output_frames(rate, block, blocks);
            let drift = actual.abs_diff(expected);
            assert!(
                drift <= 1,
                "rate {rate} block {block}: expected ~{expected} frames, got {actual}"
            );
        }
    }

    #[test]
    fn a_faster_rate_produces_less_audio_than_a_slower_one() {
        // Directional, so an inverted ratio convention fails loudly rather than
        // passing on an absolute value that happens to look plausible.
        let fast = total_output_frames(2.0, 1024, 50);
        let normal = total_output_frames(1.0, 1024, 50);
        let slow = total_output_frames(0.5, 1024, 50);
        assert!(
            fast < normal,
            "2.0x produced {fast}, 1.0x produced {normal}"
        );
        assert!(
            slow > normal,
            "0.5x produced {slow}, 1.0x produced {normal}"
        );
    }

    #[test]
    fn stretching_does_not_move_the_pitch() {
        // 440 Hz in, 440 Hz out, whatever the rate does to the length. Read off
        // the transport's own list rather than a copy of it (task155), so a rate
        // added to the speed popup cannot quietly go unchecked here. 1.0 is
        // skipped because it never reaches the stretcher.
        for rate in crate::ui_state::timeline::PLAYBACK_RATES
            .into_iter()
            .filter(|rate| *rate != 1.0)
        {
            let mut stretcher = AudioStretcher::new(rate);
            let block = sine(4096, 440.0);
            let mut output = Vec::new();
            for _ in 0..40 {
                output.extend_from_slice(stretcher.feed(&block));
            }
            // Skip the first half: the analysis window and the preroll are
            // still working their way out of the front of the stream.
            let tail = &output[output.len() / 2..];
            let hz = fundamental_hz(tail);
            assert!(
                (hz - 440.0).abs() < 22.0,
                "rate {rate}: expected ~440 Hz, measured {hz:.1} Hz"
            );
        }
    }

    #[test]
    fn a_reset_starts_the_ratio_over() {
        let mut stretcher = AudioStretcher::new(2.0);
        let block = sine(1000, 440.0);
        // 1000/2 = 500 exactly, but 999/2 leaves a half frame behind.
        let odd = sine(999, 440.0);
        let _ = stretcher.feed(&odd);
        stretcher.reset();
        assert_eq!(stretcher.feed(&block).len() / CHANNELS, 500);
    }
}
