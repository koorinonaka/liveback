//! Mixing several audio tracks down to the one stream WASAPI gets (task1270).
//!
//! A recording can carry the capture target plus up to three other
//! applications, each as its own AAC track (task1260). Playback decodes them
//! all and adds them together here.
//!
//! **The target's track is the timeline.** Its blocks are what the existing
//! pump already aligns, drift-corrects and hands to the stretcher; the extra
//! tracks are buffered and mixed into whatever region of that timeline the
//! current block covers. Doing it the other way -- an independent clock per
//! track -- would need per-track drift correction, and dropping a block on one
//! track and not the others is exactly how tracks drift apart.
//!
//! Everything here is arithmetic on `f32` slices, so it is tested without
//! Media Foundation, a device, or a file.

use std::collections::VecDeque;

use super::audio_out::{AUDIO_CHANNELS, AUDIO_RATE};
use crate::ui_state::timeline::HNS_PER_SECOND;

/// Frames (not samples) in an interleaved buffer of `len` values.
fn frames(len: usize) -> i64 {
    (len / AUDIO_CHANNELS as usize) as i64
}

fn frames_to_100ns(frames: i64) -> i64 {
    frames * HNS_PER_SECOND / i64::from(AUDIO_RATE)
}

/// Rounded, not truncated: a frame is 208.33 units, so truncation loses a
/// frame on almost every conversion and the tracks slide apart one block at a
/// time.
fn duration_to_frames(duration_100ns: i64) -> i64 {
    let rate = i64::from(AUDIO_RATE);
    if duration_100ns >= 0 {
        (duration_100ns * rate + HNS_PER_SECOND / 2) / HNS_PER_SECOND
    } else {
        (duration_100ns * rate - HNS_PER_SECOND / 2) / HNS_PER_SECOND
    }
}

/// Decoded audio for one non-target track, held until the target track's block
/// that covers it comes past.
#[derive(Default)]
pub(super) struct TrackAudioBuffer {
    /// Absolute time of the first sample still held, or `None` when empty.
    start_100ns: Option<i64>,
    samples: VecDeque<f32>,
}

impl TrackAudioBuffer {
    pub(super) fn new() -> Self {
        Self::default()
    }

    pub(super) fn clear(&mut self) {
        self.start_100ns = None;
        self.samples.clear();
    }

    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Just past the end of what is held, for deciding whether to decode more.
    pub(super) fn end_100ns(&self) -> Option<i64> {
        self.start_100ns
            .map(|start| start + frames_to_100ns(frames(self.samples.len())))
    }

    /// Appends a decoded block that starts at `absolute_100ns`.
    ///
    /// A hole between this block and what is already held is zero-filled, so
    /// the buffer stays a contiguous stretch of the timeline -- a track that
    /// went quiet must not pull its later audio earlier. A block that arrives
    /// *behind* what is held is dropped: it is already mixed or already past.
    pub(super) fn push(&mut self, absolute_100ns: i64, block: &[f32]) {
        let Some(end) = self.end_100ns() else {
            self.start_100ns = Some(absolute_100ns);
            self.samples.extend(block.iter().copied());
            return;
        };
        let gap_frames = duration_to_frames(absolute_100ns - end);
        if gap_frames < 0 {
            // Overlapping or stale. Trimming would need the overlap to be an
            // exact frame count; dropping loses at most one block of an extra
            // track, which is inaudible next to the target.
            return;
        }
        self.samples.extend(std::iter::repeat_n(
            0.0,
            gap_frames as usize * AUDIO_CHANNELS as usize,
        ));
        self.samples.extend(block.iter().copied());
    }

    /// Adds this track into `out`, which is the interleaved block starting at
    /// `out_start_100ns`, and consumes everything up to the end of it.
    ///
    /// Anything older than `out_start_100ns` is discarded rather than mixed
    /// late: the target's timeline has already moved past it.
    pub(super) fn mix_into(&mut self, out_start_100ns: i64, out: &mut [f32], gain: f32) {
        let Some(start) = self.start_100ns else {
            return;
        };
        let channels = AUDIO_CHANNELS as usize;
        // Discard the part that is already behind the output block.
        if start < out_start_100ns {
            let stale = duration_to_frames(out_start_100ns - start) as usize * channels;
            let stale = stale.min(self.samples.len());
            self.samples.drain(..stale);
            self.start_100ns = Some(start + frames_to_100ns((stale / channels) as i64));
            if self.samples.is_empty() {
                self.start_100ns = None;
                return;
            }
        }
        let start = self.start_100ns.expect("set above");
        let lead_frames = duration_to_frames(start - out_start_100ns);
        let out_frames = frames(out.len());
        if lead_frames >= out_frames {
            // Its audio belongs to a later block; leave it queued.
            return;
        }
        let offset = lead_frames.max(0) as usize * channels;
        let available = self.samples.len();
        let usable = available.min(out.len().saturating_sub(offset));
        if gain != 0.0 {
            for (index, sample) in self.samples.iter().take(usable).enumerate() {
                out[offset + index] += sample * gain;
            }
        }
        // Consumed whether or not it was audible: a muted track still moves
        // past, or unmuting would replay everything it held.
        self.samples.drain(..usable);
        self.start_100ns = if self.samples.is_empty() {
            None
        } else {
            Some(start + frames_to_100ns((usable / channels) as i64))
        };
    }
}

/// Keeps the sum inside what the endpoint accepts. Four tracks at full volume
/// add up past 1.0 easily, and float PCM past ±1.0 is a click.
pub(super) fn clip(out: &mut [f32]) {
    for sample in out {
        *sample = sample.clamp(-1.0, 1.0);
    }
}

/// The scalar one track contributes: its own volume, muted or not. The master
/// volume is applied once to the mix, not here (task1270).
pub(super) fn track_scale(percent: i64, muted: bool) -> f32 {
    if muted {
        return 0.0;
    }
    (percent.clamp(0, 100) as f32) / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(value: f32, frames: usize) -> Vec<f32> {
        vec![value; frames * AUDIO_CHANNELS as usize]
    }

    /// One frame is 1/48000s; the helpers have to agree on that in both
    /// directions or every alignment below is meaningless.
    #[test]
    fn frames_and_durations_round_trip() {
        assert_eq!(duration_to_frames(frames_to_100ns(480)), 480);
        assert_eq!(frames_to_100ns(48_000), HNS_PER_SECOND);
    }

    #[test]
    fn a_track_starting_with_the_block_is_added_sample_for_sample() {
        let mut track = TrackAudioBuffer::new();
        track.push(0, &block(0.25, 4));
        let mut out = block(0.5, 4);
        track.mix_into(0, &mut out, 1.0);
        assert!(out.iter().all(|sample| (*sample - 0.75).abs() < 1e-6));
        assert!(track.is_empty(), "everything it covered is consumed");
    }

    #[test]
    fn gain_scales_the_track_and_mute_contributes_nothing() {
        let mut track = TrackAudioBuffer::new();
        track.push(0, &block(0.4, 2));
        let mut out = block(0.0, 2);
        track.mix_into(0, &mut out, track_scale(50, false));
        assert!(out.iter().all(|sample| (*sample - 0.2).abs() < 1e-6));

        let mut track = TrackAudioBuffer::new();
        track.push(0, &block(0.4, 2));
        let mut out = block(0.1, 2);
        track.mix_into(0, &mut out, track_scale(100, true));
        assert!(out.iter().all(|sample| (*sample - 0.1).abs() < 1e-6));
        // Muted still consumes: unmuting must not replay what was held.
        assert!(track.is_empty());
    }

    /// The whole point of buffering: a track whose block starts halfway
    /// through the target's block lands halfway through, not at the head.
    #[test]
    fn a_late_track_lands_at_its_own_offset_in_the_block() {
        let mut track = TrackAudioBuffer::new();
        let half = frames_to_100ns(2);
        track.push(half, &block(1.0, 2));
        let mut out = block(0.0, 4);
        track.mix_into(0, &mut out, 1.0);
        let channels = AUDIO_CHANNELS as usize;
        assert!(out[..2 * channels].iter().all(|sample| *sample == 0.0));
        assert!(out[2 * channels..].iter().all(|sample| *sample == 1.0));
    }

    #[test]
    fn audio_older_than_the_block_is_dropped_rather_than_mixed_late() {
        let mut track = TrackAudioBuffer::new();
        track.push(0, &block(1.0, 4));
        let mut out = block(0.0, 2);
        // The output has moved on two frames past this track's start.
        track.mix_into(frames_to_100ns(2), &mut out, 1.0);
        assert!(out.iter().all(|sample| *sample == 1.0));
        assert!(track.is_empty());
    }

    #[test]
    fn a_track_that_is_still_ahead_stays_queued() {
        let mut track = TrackAudioBuffer::new();
        track.push(frames_to_100ns(100), &block(1.0, 4));
        let mut out = block(0.0, 4);
        track.mix_into(0, &mut out, 1.0);
        assert!(out.iter().all(|sample| *sample == 0.0));
        assert!(!track.is_empty(), "its audio belongs to a later block");
    }

    /// A gap in an extra track is silence in place, not its later audio
    /// arriving early.
    #[test]
    fn a_hole_between_blocks_is_zero_filled() {
        let mut track = TrackAudioBuffer::new();
        track.push(0, &block(1.0, 2));
        track.push(frames_to_100ns(4), &block(1.0, 2));
        let mut out = block(0.0, 6);
        track.mix_into(0, &mut out, 1.0);
        let channels = AUDIO_CHANNELS as usize;
        assert!(out[..2 * channels].iter().all(|sample| *sample == 1.0));
        assert!(out[2 * channels..4 * channels]
            .iter()
            .all(|sample| *sample == 0.0));
        assert!(out[4 * channels..].iter().all(|sample| *sample == 1.0));
    }

    #[test]
    fn a_block_that_arrives_behind_what_is_held_is_dropped() {
        let mut track = TrackAudioBuffer::new();
        track.push(frames_to_100ns(10), &block(1.0, 2));
        track.push(0, &block(0.5, 2));
        assert_eq!(track.start_100ns, Some(frames_to_100ns(10)));
        assert_eq!(frames(track.samples.len()), 2);
    }

    #[test]
    fn the_sum_is_clipped_to_what_the_endpoint_accepts() {
        let mut out = vec![1.5, -1.5, 0.5, -0.5];
        clip(&mut out);
        assert_eq!(out, vec![1.0, -1.0, 0.5, -0.5]);
    }
}
