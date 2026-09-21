//! The playback engine's worker thread: playlist bookkeeping, the master
//! clock, seek/rate handling, video pacing and audio pumping. Split from
//! `playback.rs`, which keeps the public engine API and shared state.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::ui_state::timeline::{TimelineSegment, TimelineSnapshot, HNS_PER_SECOND};

use super::audio_out::{AudioOut, AUDIO_CHANNELS, AUDIO_RATE};
use super::fetch_gate::FetchGate;
use super::gpu;
use super::mixer;
use super::scale::FrameScaler;
use super::segment_reader::{
    open_segment_with, FetchRequest, FirstFrame, ReaderRetirer, SegmentFetcher, SegmentReader,
};
use super::source::SegmentSource;
use super::stretcher::AudioStretcher;
use super::{
    FullFrame, PlaybackCommand, PlaybackFrame, PlaybackShared, AUDIO_BUFFER, DRIFT_LIMIT_100NS,
    LEASE_WINDOW,
};
use windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager;

/// Shortest interval between two UI wake-ups from a repeated park. A
/// quarter second is imperceptible on a transport that has just stopped,
/// and it puts the ring rate below the UI's own 100ms drain -- so a stuck
/// engine costs the event loop strictly less than it already spends on
/// itself, instead of the ~2,400 rounds a second of 2026-08-14.
const PARK_RING_FLOOR: Duration = Duration::from_millis(250);

/// `AUDIO_BUFFER` on the media timeline, for `drain_continues` (task1880).
const AUDIO_BUFFER_100NS: i64 = AUDIO_BUFFER.as_millis() as i64 * 10_000;

#[derive(Clone)]
struct PlaylistEntry {
    index: u64,
    start_100ns: i64,
    end_100ns: i64,
    /// See `TimelineSegment::audio_offsets_100ns` (task204/task1270), one per
    /// audio track in track order.
    audio_offsets_100ns: Vec<i64>,
}

/// How close to a segment's end the engine starts opening the next one
/// (task203). Short enough that a seek almost never lands inside the window and
/// throws the work away.
///
/// Half a second until task1510, sized against the ~30ms a 2-4MB prefetch cost.
/// A 140Mbps recording writes 28-35MB every two seconds, and reading that,
/// building a Source Reader over it and decoding its first keyframe can run
/// past half a second on its own -- the read-ahead would then still be in
/// flight at the crossing and the tick would fall back to opening it
/// synchronously, which is the stall. A full second is the safe side: the work
/// runs off the playback thread now, so a window that opens early costs idle
/// time on another thread and nothing here.
const PREFETCH_LEAD_100NS: i64 = HNS_PER_SECOND;

/// How much playable media a live park waits for before resuming (task1800).
///
/// Two segments, matching `encoder::SEGMENT_DURATION_100NS` at the 2s cadence
/// segments finalize on. One segment is provably not enough: media arrives at
/// 1.0x and plays at 1.0x, so whatever lead a resume starts with is exactly
/// what it has at the next crossing, minus the finalize latency and the ≤100ms
/// drain poll. Resuming with one segment therefore parks again at every
/// crossing -- the reported stutter -- and no reserve depth changes that, since
/// the reserve moves the edge and the position together.
///
/// Waiting for two turns that into one park per follow session: the transport
/// stops once while the buffer fills, then runs continuously with the lead
/// oscillating between two segments and one. The cost is sitting about 4-6s
/// behind the newest playable frame instead of on top of it, which is the
/// trade the task accepted.
const LIVE_RESUME_LEAD_100NS: i64 = 2 * crate::encoder::SEGMENT_DURATION_100NS;

/// How late a frame has to be before it is thrown away instead of shown
/// (task630). One 60fps frame, the same figure `frames_late` counts at: below
/// it the frame is merely jittery and dropping it would cost picture for
/// nothing.
const LATE_ENOUGH_TO_DROP: Duration = Duration::from_millis(17);

/// How many frames in a row may be dropped before one is shown regardless.
///
/// Without a cap, a rate the machine cannot decode at would show nothing at
/// all: the clock would be perfect and the picture frozen. Four is ~130ms of
/// stillness at 30fps content, which reads as a slow rate rather than a hang.
const MAX_CONSECUTIVE_DROPS: u32 = 4;

/// Whether a frame this far past its deadline should be dropped rather than
/// converted and shown (task630).
///
/// Only the *decode* has been paid at this point; the RGBA swizzle is the
/// expensive half (5.87ms a 1080p frame, task200), and skipping it is what lets
/// x2 keep the clock true on a machine that cannot decode twice as fast.
/// `droppable` is false for a crossing's prefetched first frame -- dropping it
/// would still the picture exactly at the seam -- and for the first frame after
/// a seek, where a still picture is the user's own gesture answering back.
fn should_drop_frame(late: Duration, droppable: bool, consecutive_drops: u32) -> bool {
    droppable && late >= LATE_ENOUGH_TO_DROP && consecutive_drops < MAX_CONSECUTIVE_DROPS
}

/// Whether a frame due before the display's next refresh should be skipped
/// rather than converted (t260913-3842).
///
/// A 120fps recording played on a 60Hz stage has the engine convert and shrink
/// 120 frames a second so the UI can show 60 of them; at x2 task578a measured
/// 237 built against 136 taken. Nothing on screen changes by not building the
/// other half -- the display cannot show it -- and on the GPU path each one
/// costs a `Map(READ)` that waits for the blit (t260913-2527: 0.6-1.1ms).
///
/// `since` is measured between *deadlines*, not between wake times: the
/// deadline comes straight off the media timestamps and the anchor, while a
/// wake time carries the sleep's overshoot. With wake times a 120fps frame
/// landing 16.6ms after a previous frame that overslept reads as "too early",
/// so the beat alternates 16.7/25ms and the visible rate falls to 40 -- which
/// is the stutter this task must not introduce.
///
/// `step` is one source frame interval in wall time, and the comparison picks
/// the frame *nearest* the refresh rather than the first one past it: without
/// it a 120fps step that rounds to 16_666_600ns loses to a 16_666_666ns period
/// by 66ns and the same 40fps beat comes back through arithmetic instead of
/// through jitter. `droppable` is false for a crossing's prefetched first
/// frame and for the frame a seek asked for, so neither is ever skipped.
fn should_skip_frame(
    droppable: bool,
    since: Option<Duration>,
    step: Duration,
    period: Option<Duration>,
) -> bool {
    let (Some(since), Some(period)) = (since, period) else {
        return false;
    };
    droppable && since + step / 2 < period
}

/// When the frame at `absolute` is due, from the clock anchor
/// (`(wall, media)`) and the playback rate.
///
/// `max(0)`: a frame *behind* the anchor -- the keyframe a coarse seek lands
/// on, decoded before the target -- is due now rather than at a negative
/// offset, which `Duration::from_secs_f64` would refuse anyway.
fn frame_deadline(anchor_wall: Instant, anchor_media: i64, absolute: i64, rate: f64) -> Instant {
    let due_in_media = (absolute - anchor_media).max(0);
    anchor_wall + Duration::from_secs_f64(due_in_media as f64 / HNS_PER_SECOND as f64 / rate)
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn the_rate_divides_the_wait() {
        let now = Instant::now();
        let due = frame_deadline(now, 0, HNS_PER_SECOND, 2.0);
        assert_eq!((due - now).as_millis(), 500);
    }

    #[test]
    fn a_frame_behind_the_anchor_is_due_now() {
        let now = Instant::now();
        assert_eq!(frame_deadline(now, HNS_PER_SECOND, 0, 1.0), now);
    }
}

/// Which segment, if any, is worth opening ahead of the crossing (task203),
/// given the caller's index and where playback actually is.
///
/// Every reason to decline lives here rather than as early returns in
/// `prefetch_next`, because the reasons are the part worth testing and the
/// caller is not testable: the in-memory worker harness has no segment files
/// behind it, so `open_segment_at` fails regardless and asserting on
/// `worker.next` passes whether the rules hold or not. That vacuum is what let
/// task740's bug sit under a green test.
///
/// The rules, in order:
///
/// - nothing follows the last segment;
/// - `playing` -- the segment the *position* is in -- is the honest answer to
///   "where are we", not `playlist_index` and not `self.current`. During a
///   crossing the caller's index is a tick stale (`tick` binds it once and
///   never rebinds it) and `current` still points at the outgoing segment. A
///   target at or behind where playback already is means the stale index just
///   named the segment now playing; reading *that* ahead parks it in
///   `self.next` and blocks the real read-ahead until the following crossing
///   discards it as a mismatch. Hit, poisoned, miss, clean, hit -- the measured
///   rate sat at exactly 50% at every speed until task740. `None` (parked past
///   the live edge, no segment under the position) says nothing either way, so
///   it declines nothing.
/// - too far from the crossing to be worth it: task203 sized
///   `PREFETCH_LEAD_100NS` so a seek rarely lands inside the window and throws
///   the work away. This is media time, so it does not shrink with the rate --
///   unlike the wall-slack gate task630 removed, which at x1.5 and above never
///   opened at all.
fn prefetch_target(
    playlist_index: usize,
    playlist_len: usize,
    playing: Option<usize>,
    remaining_100ns: i64,
) -> Option<usize> {
    let next = playlist_index + 1;
    if next >= playlist_len {
        return None;
    }
    if playing.is_some_and(|playing| next <= playing) {
        return None;
    }
    (remaining_100ns <= PREFETCH_LEAD_100NS).then_some(next)
}

/// How far into the playlist the transport is allowed to play (task1530).
///
/// While the session is still recording, the newest finalized segment is held
/// in reserve and never crossed into. Without the reserve, live follow has no
/// read-ahead window at all: the transport parks on the end of the newest
/// segment, and the moment the next one finalizes the playlist grows and the
/// crossing happens on the very next tick -- `prefetch_next` is not even
/// reached, because a tick that crosses returns before it. `PREFETCH_LEAD_100NS`
/// cannot help, since the two conditions it needs (the segment exists; the
/// crossing is within a second) become true at the same instant.
///
/// Holding one segment back is the smallest lead the encoder's ~2s cadence
/// offers, and it is enough: the transport then always plays a segment whose
/// successor has been in the playlist for a whole segment's worth of time.
/// The cost is that live follow runs about one segment behind the newest
/// finalized frame instead of on top of it.
///
/// A finished session reserves nothing -- its last segment is the end of the
/// recording, and refusing to play it would simply lose it.
fn crossable_len(playlist_len: usize, live: bool) -> usize {
    if live && playlist_len >= 2 {
        playlist_len - 1
    } else {
        playlist_len
    }
}

/// Whether the transport is as close to the live edge as the reserve lets it
/// get -- the LIVE chip's meaning after task1530.
///
/// It used to be "parked on the live edge", which live follow reached on every
/// crossing. With the reserve the transport *plays* continuously one segment
/// behind instead, and `write_status`'s old `playing => not at the live edge`
/// would have kept the chip dark for the whole of it. Being on the last
/// segment the reserve allows is the same claim the chip always made: there is
/// nothing newer this transport can show.
fn follows_live(playing: Option<usize>, playlist_len: usize, live: bool) -> bool {
    live && playing.is_some_and(|index| index + 1 >= crossable_len(playlist_len, live))
}

/// What becomes of a read-ahead that has just come back from the fetch thread
/// (task1510).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PrefetchVerdict {
    /// Still the segment the transport is heading for: keep the reader and its
    /// first frame for the crossing.
    Accept,
    /// Still wanted, but the read failed: the fetch gate has to hear about it,
    /// or a genuinely unreadable segment would be retried every tick.
    Failed,
    /// Nobody wants it any more. The reader goes to the retirer and **an error
    /// is dropped without a word**: routing a stale failure into the gate would
    /// close it on a segment nothing asked for, and the next real fetch of it
    /// would be refused. That is what would leave a scrub storm parked.
    Discard,
}

/// Whether a response is still the read-ahead the transport wants.
///
/// Extracted for the same reason `prefetch_target` was (task740): the in-memory
/// worker harness has no segment files behind it, so nothing ever comes back
/// through the real path and an assertion on `worker.next` would pass with
/// these rules deleted.
///
/// - `in_flight` mismatch covers both staleness axes at once. A seek moves the
///   generation on and clears the slot, so a response from before it can never
///   equal what is in flight -- not even when a *new* read-ahead for the same
///   playlist index is already on its way.
/// - `slot_filled`: `self.next` was filled some other way (a synchronous
///   `ensure_segment` handed its reader over) while this was in the air.
/// - at or behind `playing`: the same rule `prefetch_target` applies on the way
///   out, applied again on the way back, because a read that outlives a
///   crossing returns naming a segment that is now current or already past.
///   Storing that in `self.next` is exactly task740's poisoned slot. It is also
///   the known benign race: the crossing's synchronous fallback re-windows the
///   lease, the in-flight read fails on the lease it was given, and the failure
///   arrives here -- to be discarded rather than charged to the gate.
fn prefetch_verdict(
    response: (u64, usize),
    in_flight: Option<(u64, usize)>,
    ok: bool,
    slot_filled: bool,
    playing: Option<usize>,
) -> PrefetchVerdict {
    if in_flight != Some(response)
        || slot_filled
        || playing.is_some_and(|playing| response.1 <= playing)
    {
        return PrefetchVerdict::Discard;
    }
    if ok {
        PrefetchVerdict::Accept
    } else {
        PrefetchVerdict::Failed
    }
}

/// The crossing's audio seam (task640), logged from `pump_audio_inner` when the
/// first block after a crossing is queued.
///
/// What it measures, settled by task1860 over 6,961 crossings
/// (evidence/1860-audio-hole-index2):
/// - A non-zero `hole_ms` is always a whole number of AAC access units (21.33ms
///   each), and the material is *skipped*, never replaced by silence: `write`
///   only appends. Never the incoming segment's head: `first_audio_start -
///   next_start` is the grid phase and stays under one unit, so a skipped head
///   unit would push it over.
/// - The margin is one access unit: a healthy crossing ends the outgoing audio
///   ~1 unit past `previous_end` (`tail_ms` < 0). `tail_ms` > 0 is the loss.
///
/// 21/42/64 -- the outgoing segment's last 1-3 units, still unread when the
/// video ran out and `current` was swapped -- was task1880: they went with the
/// retired reader, invisibly, since units dropped before `block_fate` cannot
/// move `drift_corrections` either. `drain_outgoing_audio` now reads them out
/// first, so a crossing of contiguous segments is expected to be exactly 0. A
/// *negative* `hole_ms` on such a crossing is the drain overrunning into the
/// incoming segment, not noise.
///
/// Still blind to what `flush_audio` throws away (task1840's blind spot): the
/// reader keeps `last_audio_end_100ns`, so a park or a seek discards queued
/// audio with no hole to show for it. The drain does not change that -- it runs
/// on the crossing path only.
#[allow(clippy::too_many_arguments)]
fn log_audio_seam(
    seam: Option<(u64, i64, i64, bool)>,
    absolute: i64,
    rate: f64,
    queued_ms: u64,
    last_audio_end_100ns: Option<i64>,
    drain_dropped: u32,
    drain_dropped_100ns: i64,
) {
    let Some((index, previous_end, next_start, prefetched)) = seam else {
        return;
    };
    {
        tracing::info!(
            target: "task640_audio_seam",
            index,
            rate,
            prefetched,
            previous_end,
            next_start,
            last_audio_end = ?last_audio_end_100ns,
            first_audio_start = absolute,
            hole_ms = last_audio_end_100ns.map(|end| (absolute - end) / 10_000),
            tail_ms = last_audio_end_100ns.map(|end| (previous_end - end) / 10_000),
            queued_ms = queued_ms,
            // Task1900: the half `hole_ms` cannot see any more. The
            // drain reads the outgoing tail out either way, so a
            // crossing that stranded 700ms and one that stranded
            // nothing both report `hole_ms = 0`; these two say how much
            // of what it read was already too old to play.
            drain_dropped = drain_dropped,
            drain_dropped_ms = drain_dropped_100ns / 10_000,
            "audio across a segment crossing"
        );
    }
}

/// What a block's fate costs, and whether it is played at all.
///
/// `Realign` is audio from before a seek target: the seek already flushed the
/// endpoint, so nothing of it was ever heard, and walking past it is the reader
/// arriving rather than the clock running away (task1840). `Drift` is behind
/// the clock -- dropped and counted, the video is the truth. On the crossing
/// drain the drop is counted apart from `drift_corrections`, which other things
/// move too: there it is material the outgoing segment held and nobody heard
/// (task1900).
#[allow(clippy::too_many_arguments)]
fn accept_block(
    absolute_end_100ns: i64,
    master_100ns: i64,
    block_100ns: i64,
    drain: bool,
    align: &mut Option<i64>,
    realigned: &mut u64,
    (drain_dropped, drain_dropped_100ns): (&mut u32, &mut i64),
    status: &std::sync::Mutex<super::PlaybackStatus>,
) -> bool {
    match block_fate(absolute_end_100ns, master_100ns, *align) {
        BlockFate::Realign => {
            // Audio from before the seek target. The seek already
            // flushed the endpoint, so nothing of it was ever heard;
            // walking past it is the reader arriving, not the clock
            // running away (task1840).
            *realigned += 1;
            return false;
        }
        BlockFate::Drift => {
            // Behind the clock: drop and count, the video is the truth.
            *align = None;
            if drain {
                // Counted apart from `drift_corrections`, which other
                // things move too: on the crossing path this is
                // material the outgoing segment held and nobody heard
                // (task1900).
                *drain_dropped += 1;
                *drain_dropped_100ns += block_100ns;
            }
            let mut status = status.lock().unwrap();
            status.drift_corrections += 1;
            return false;
        }
        BlockFate::Play => *align = None,
    }
    true
}

/// What one audio block is mixed from: the reader the extra tracks are decoded
/// out of, the entry that dates them, the per-track buffers they land in, and
/// the levels applied on the way (task1270).
struct MixInputs<'a> {
    current: &'a mut SegmentReader,
    entry: &'a PlaylistEntry,
    extra_audio: &'a mut Vec<mixer::TrackAudioBuffer>,
    track_volumes: &'a [(i64, bool)],
    volume_scale: f32,
    tracks: usize,
}

impl MixInputs<'_> {
    /// Applies track 0's own level and the master, then mixes every other
    /// track into the same block.
    fn apply(&mut self, absolute: i64, block_100ns: i64, samples: &mut [f32]) {
        // Track 0's own level and the master, applied before anything is
        // added to it: every track is scaled first, and the *sum* is what
        // gets clipped. Applied to the input rather than the stretched
        // output because a scalar gain passes through analysis/synthesis
        // unchanged, which keeps the 1x bypass and the stretched path
        // identical (task155).
        let target_scale = self.volume_scale * track_scale_of(self.track_volumes, 0);
        if target_scale != 1.0 {
            for sample in samples.iter_mut() {
                *sample *= target_scale;
            }
        }
        // The extra tracks, mixed into this block (task1270). Each is
        // decoded up to the end of the block first, so a track whose
        // decoder runs a little behind still lands where it belongs
        // rather than a block late.
        if self.tracks > 1 {
            let block_end = absolute + block_100ns;
            for track in 1..self.tracks {
                let buffer = &mut self.extra_audio[track - 1];
                while buffer.end_100ns().is_none_or(|end| end < block_end) {
                    let Some((local, block)) = self.current.next_audio_on(track) else {
                        break;
                    };
                    let offset = self
                        .entry
                        .audio_offsets_100ns
                        .get(track)
                        .copied()
                        .unwrap_or(0);
                    if local < offset {
                        // This track's priming unit (task204), same rule
                        // as track 0's above.
                        continue;
                    }
                    buffer.push(self.entry.start_100ns + local - offset, &block);
                }
                let scale = self.volume_scale * track_scale_of(self.track_volumes, track);
                buffer.mix_into(absolute, samples, scale);
            }
            mixer::clip(samples);
        }
    }
}

/// A frame decoded and waiting for its deadline (task630).
///
/// One shape on purpose (t260914-8d69): a crossing's prefetched first frame
/// used to arrive here already swizzled, which put that one frame per crossing
/// on the CPU resample while every other frame went through `gpu_frame`.
/// Whether a frame may be thrown away for being late is `must_show_next`'s
/// business, not the queue's.
enum Pending {
    /// Decoded, not yet swizzled. The costly half is still unpaid, so this one
    /// can be thrown away if it turns out to be late.
    Sample(i64, windows::Win32::Media::MediaFoundation::IMFSample),
}

impl Pending {
    fn absolute(&self) -> i64 {
        let Pending::Sample(absolute, _) = self;
        *absolute
    }
}

/// What `tick` ended up with, once the frame is known to be worth showing.
///
/// Two shapes because the GPU path (t260913-2527) produces the finished,
/// stage-sized frame in one go, while the CPU fallback hands over
/// recording-sized RGBA that `publish_frame` still has to resample.
enum Prepared {
    Decoded(Vec<u8>, u32, u32),
    Scaled(PlaybackFrame),
}

/// A frame handed to the GPU whose picture has not been read back yet
/// (t260915-8ea7).
///
/// The sizes travel with the planes rather than being re-read when the picture
/// arrives: by then the stage may be a different size and the pipeline rebuilt,
/// and a frame described by anybody else's dimensions is the bug this task's
/// AC2/AC3 are about.
struct GpuPending {
    nv12: Vec<u8>,
    /// The recording's size -- what `PlaybackFrame::full` is measured in.
    source: (u32, u32),
    /// The stage's size -- what the picture read back is measured in.
    scaled: (u32, u32),
}

impl GpuPending {
    /// This frame's planes and sizes paired with the picture just read back.
    /// Pure, and the one place the pairing is made, so a test can hold it to
    /// "N-1's planes come out with N-1's pixels" without a device.
    fn into_frame(self, rgba: Vec<u8>) -> PlaybackFrame {
        PlaybackFrame {
            width: self.scaled.0,
            height: self.scaled.1,
            rgba,
            full: Some((self.source.0, self.source.1, FullFrame::nv12(self.nv12))),
        }
    }
}

/// What `gpu_frame` had for the caller.
///
/// `Warming` is separate from `Unavailable` on purpose (t260915-8ea7 AC1):
/// running the CPU pair for a frame whose blit is already in flight would add
/// the work this path exists to remove, so the tick publishes nothing instead
/// and the picture arrives on the next one.
enum GpuFrame {
    Ready(PlaybackFrame),
    Warming,
    Unavailable,
}

/// The next segment, opened ahead of the crossing (task203).
struct Prefetched {
    playlist_index: usize,
    reader: SegmentReader,
    /// Its first frame, decoded but not converted: (absolute 100ns, sample).
    /// The conversion is the crossing tick's, so the seam frame takes the same
    /// route as every other one (t260914-8d69).
    first: Option<FirstFrame>,
}

/// Whether a crossing keeps the master clock or re-anchors on the new segment.
///
/// Contiguous segments are one continuous recording split into files, so the
/// clock must run straight through -- re-anchoring there is what made every
/// seam visible, since it silently granted the crossing however long it had
/// just taken. A real gap (the recording genuinely stopped) is the opposite:
/// waiting it out in real time would freeze the picture for its whole length,
/// so the clock restarts at the new segment's own start.
fn keeps_anchor(previous_end_100ns: i64, next_start_100ns: i64) -> bool {
    next_start_100ns - previous_end_100ns < crate::ring_buffer::GAP_THRESHOLD_100NS
}

/// The playlist entry playback runs from at `time_100ns`: the covering segment,
/// or the next one after a gap. `None` is past the end.
fn playlist_position(playlist: &[PlaylistEntry], time_100ns: i64) -> Option<usize> {
    match playlist.binary_search_by(|entry| {
        if time_100ns < entry.start_100ns {
            std::cmp::Ordering::Greater
        } else if time_100ns >= entry.end_100ns {
            std::cmp::Ordering::Less
        } else {
            std::cmp::Ordering::Equal
        }
    }) {
        Ok(index) => Some(index),
        Err(insertion) => (insertion < playlist.len()).then_some(insertion),
    }
}

/// Where the master clock may be anchored when playback starts from
/// `position_100ns`: the position itself when a segment covers it, otherwise
/// the start of the segment `playlist_position` will snap forward to.
///
/// The deadline of every frame is `anchor_wall + (absolute - anchor_media)`, so
/// an anchor on a position no segment covers charges the wait *to the segment*
/// against the first frame. A live session loaded before its first segment
/// existed is the pathological case (task1630): it sits at 0 -- the park leaves
/// it at -1 -- while the timeline it grows into is absolute 100ns ticks since
/// boot, so the first frame comes out ~9.6 hours late and `tick` sleeps that
/// long. The transport's commands are only read between ticks, which is what
/// left the review pane taking no input after a recording stopped (task1620
/// measured the same sleep from the other end, where it hung the UI's join).
/// Resuming inside a gap is the same shape, one gap wide.
fn anchor_position(playlist: &[PlaylistEntry], position_100ns: i64) -> i64 {
    match playlist_position(playlist, position_100ns) {
        Some(index) => position_100ns.max(playlist[index].start_100ns),
        None => position_100ns,
    }
}

/// How long a mixed block of interleaved stereo samples lasts.
fn block_span_100ns(samples: usize) -> i64 {
    (samples as i64 / i64::from(AUDIO_CHANNELS)) * HNS_PER_SECOND / i64::from(AUDIO_RATE)
}

/// What `pump_audio` does with a decoded block (task1840).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockFate {
    /// Queue it.
    Play,
    /// Discard it: a seek landed after it, so it is audio from before the
    /// target that `SetCurrentPosition` handed back because the source seeks
    /// to the keyframe, not to the frame. Not drift -- the clock is fine, the
    /// reader simply has to walk forward to where the seek pointed.
    Realign,
    /// Discard it: it is genuinely behind the clock by more than the limit.
    Drift,
}

/// Separated from `pump_audio` so the rule is testable without a WASAPI
/// endpoint or a decoder (task1840). `align_100ns` is the position the last
/// seek asked for, live until the first block that reaches it.
///
/// `Realign` is checked first on purpose: right after a seek every pre-target
/// block is *also* far behind the clock, and counting those as drift is what
/// made `drift_corrections` step by 21-75 on every seek while five clean rate
/// changes moved it by 0.
fn block_fate(block_end_100ns: i64, master_100ns: i64, align_100ns: Option<i64>) -> BlockFate {
    // `<=`, not `<`: a block that ends exactly on the target contains none of
    // it. The block that straddles the target is kept -- it is the first audio
    // the listener is owed.
    if align_100ns.is_some_and(|align| block_end_100ns <= align) {
        return BlockFate::Realign;
    }
    if block_end_100ns < master_100ns - DRIFT_LIMIT_100NS {
        return BlockFate::Drift;
    }
    BlockFate::Play
}

/// Whether the crossing drain (task1880) keeps reading after this block.
///
/// The drain runs with the endpoint full -- that is the case it exists for --
/// so nothing else bounds it: what the endpoint refuses piles into the carry,
/// and the carry is playback latency. A healthy crossing leaves at most three
/// access units (64ms), but a recording whose video stops early (task480/410)
/// can leave a whole segment of audio behind, and queueing all of it would push
/// the playback that much later.
///
/// This is the upper bound only. The lower one is `block_fate`'s drift rule,
/// which the drain runs with `previous_end` as its master: together they hold
/// the drain to `[previous_end - DRIFT_LIMIT, previous_end + limit]`.
fn drain_continues(block_end_100ns: i64, previous_end_100ns: i64, limit_100ns: i64) -> bool {
    block_end_100ns - previous_end_100ns < limit_100ns
}

pub(super) struct Worker {
    /// Where the container bytes come from: the ring buffer behind a lease, or
    /// one exported clip on disk (task3500). The `Worker` knows nothing else
    /// about either.
    source: Arc<dyn SegmentSource>,
    session_id: String,
    playlist: Vec<PlaylistEntry>,
    live_edge_100ns: i64,
    /// Whether the session under the transport is the one still recording
    /// (task1530), which is what arms the one-segment reserve. Cached rather
    /// than asked per frame: `refresh_live` runs at the three places that
    /// decide anything with it -- a crossing, a seek, and a playlist growth --
    /// so while playing it is never more than one segment stale.
    live: bool,
    shared: Arc<PlaybackShared>,
    notify: Box<dyn Fn() + Send>,

    current: Option<SegmentReader>,
    /// Where finished readers go to be released (task246). Releasing one costs
    /// 29-34ms of COM teardown, and every place below that replaces `current`
    /// or discards `next` used to pay it on whichever thread the user was
    /// waiting on.
    retirer: ReaderRetirer,
    /// Resamples each frame to the size the stage draws it at (task990). Kept
    /// for the session so its convolution buffers are grown once, not per
    /// frame.
    scaler: FrameScaler,
    /// The GPU half of the same job (t260913-2527): NV12 -> RGBA and the
    /// downscale in one `VideoProcessorBlt`, with `scaler` above still there as
    /// the fallback. Built on the first frame that would use it -- a session
    /// played at or above the recording's size never wants one, and building it
    /// stands up a D3D11 device.
    gpu: Option<gpu::GpuScaler>,
    /// Whether `gpu` has been attempted. `false` with `gpu: None` means "not
    /// yet"; `true` with `None` means this machine will not give one.
    gpu_tried: bool,
    /// The frame whose blit is in flight (t260915-8ea7): its planes and the two
    /// sizes they were blitted at, waiting for the call that maps the picture
    /// back. `None` means the GPU pipeline has nothing to hand over.
    ///
    /// It is held *here* rather than beside the staging texture because it is
    /// what the picture has to come out paired with: the frame `PlaybackFrame`
    /// publishes carries the NV12 a screenshot reads, and the whole hazard of
    /// this change is publishing N's planes with N-1's pixels.
    gpu_pending: Option<GpuPending>,
    /// Whether the slow path is already the one being taken: set by
    /// [`Worker::cpu_convert`] when it converts a decoder texture -- the
    /// synchronous read-back that method names (t260915-e6c8 follow-up) -- and
    /// cleared by `gpu_frame` on any `Converted` but `Failed`, so that leaving
    /// the slow path and falling back into it warns again. Only the *edge* is
    /// logged, so this is the whole of the state that needs.
    cpu_readback: bool,
    /// The following segment, opened during a sleep `tick` would otherwise
    /// spend idle (task203). Dropped on any seek: it belongs to a position the
    /// transport is no longer heading for.
    next: Option<Prefetched>,
    /// Reads the following segment on its own thread (task1510). Its request
    /// carries a lease this thread acquired; `self.lease` and `fetch_gate`
    /// never leave the worker.
    fetcher: SegmentFetcher,
    /// Moves on whenever the transport stops heading where a read-ahead in
    /// flight was aimed. Responses carry the generation they were asked under,
    /// so a stale one is recognised without having to cancel anything.
    prefetch_generation: u64,
    /// The one read-ahead allowed in the air, as (generation, playlist index).
    /// One at a time: a second would only ever be for a segment two crossings
    /// away, and would keep another 35MB pinned to prove it.
    prefetch_in_flight: Option<(u64, usize)>,
    lease: Option<String>,
    audio: Option<AudioOut>,

    playing: bool,
    position_100ns: i64,
    rate: f64,
    volume_scale: f32,
    /// Per-track level and mute, in track order (task1270). Session-lived:
    /// nothing writes it to disk, so a recording opens sounding the way it was
    /// recorded. A track nothing has set is full volume, unmuted.
    track_volumes: Vec<(i64, bool)>,
    /// Decoded audio for tracks 1.. waiting for the target track's block that
    /// covers it. Index 0 here is track 1.
    extra_audio: Vec<mixer::TrackAudioBuffer>,
    /// (wall anchor, media anchor): master clock while playing.
    anchor: Option<(Instant, i64)>,
    /// Decoded but not yet due.
    pending_frame: Option<Pending>,
    /// Frames dropped in a row for being late (task630), and whether the next
    /// one must be shown regardless: a seek just asked for it, or it is the
    /// head a crossing carried over from the prefetch (t260914-8d69), where
    /// dropping it would still the picture exactly at the seam.
    consecutive_drops: u32,
    must_show_next: bool,
    /// Deadline of the last frame actually published (t260913-3842), against
    /// which `should_skip_frame` measures the display period. The scheduled
    /// instant rather than the wake time -- see that function for why.
    last_publish_deadline: Option<Instant>,
    /// Segment crossings and how many of them landed on a prefetch. Reported
    /// with the 1Hz playback line rather than one `debug!` per crossing, so the
    /// hit rate task209 was decided on is readable at any rate from the shipped
    /// log (task630 needed it at x1.5 and x2).
    crossings: u64,
    crossings_prefetched: u64,
    /// The same two numbers restricted to crossings made *at the live edge* --
    /// the incoming segment was the newest one the playlist had (task1530).
    /// Task1510's 15/22 was the two kinds added together, which hid that the
    /// misses were all of one kind.
    crossings_live: u64,
    crossings_live_prefetched: u64,
    /// How many times playback ran out of playlist and parked (task1530). The
    /// number that says whether live follow is riding the edge itself or
    /// trailing it: a transport with any lead at all never parks.
    parks: u64,
    frames_dropped: u64,
    /// Frames skipped for landing inside a refresh the display is already
    /// showing something in (t260913-3842). Deliberately *not* folded into
    /// `frames_dropped`: that number is the reactive late-frame drop, and a
    /// run whose two are added together reads as if the machine were failing
    /// to keep up.
    frames_skipped: u64,
    /// Absolute end of the last audio block handed to the endpoint, and the
    /// crossing waiting to be reported against it (task640): the outgoing
    /// segment's declared end, the incoming one's start, and whether that tick
    /// got to pump audio at all.
    last_audio_end_100ns: Option<i64>,
    pending_seam: Option<(u64, i64, i64, bool)>,
    /// What the last crossing drain threw away as `BlockFate::Drift`: blocks
    /// and their span. Task1900's detector -- since task1880 the outgoing
    /// segment's stranded tail is *read* before the reader retires, so
    /// `hole_ms` reads 0 whether the audio played or was discarded on the way
    /// out. This pair is what still tells the two apart. Reset by the drain,
    /// read by the seam line the next pump emits.
    drain_dropped: u32,
    drain_dropped_100ns: i64,
    /// Lowest the endpoint's queue has been since the last 1Hz report
    /// (task640). A seam that is only measured *at* the seam cannot tell a
    /// crossing that starved the endpoint from one that merely dipped, and 0
    /// here is the only thing that actually sounds like a dropout.
    ///
    /// Sampled *before* each fill, not after (task1840). Sampling after the
    /// write meant the mark could never read 0 however dry the endpoint had
    /// run -- the sample always included the block just written, so a starved
    /// endpoint reported one block's worth instead of nothing.
    audio_queued_min_ms: u32,
    /// Where a seek left the audio to realign to (task1840). `SetCurrentPosition`
    /// moves the whole source to the keyframe before the target, so the audio
    /// stream resumes up to a second and a half *before* the frame the seek
    /// landed on. Those blocks have to go, but they are the seek doing its job,
    /// not the clock drifting away from them.
    audio_align_100ns: Option<i64>,
    /// How many blocks that realignment has discarded, so the tick line shows
    /// where the old `drift` count went instead of it merely vanishing.
    audio_realigned: u64,
    /// Mixed blocks the endpoint has already been given, still recent enough
    /// that the clock has not passed them (task1840).
    ///
    /// A rate change across 1x flushes the endpoint, but nothing rewinds the
    /// reader -- `SetCurrentPosition` would drag the video back with it. The
    /// media that was queued is therefore never heard, the reader is left that
    /// far ahead of the clock, and the segment's audio ends early by exactly
    /// that much: silence at the segment tail until the next crossing
    /// re-anchors. `pause` documents the same failure from the same call.
    /// Keeping the blocks lets the crossing re-render them through the new
    /// path instead of destroying them.
    ///
    /// Bounded by the read-ahead: at most `AUDIO_BUFFER * rate` of media plus
    /// the drift slack, so ~326KB at 4x.
    audio_recent: VecDeque<(i64, Vec<f32>)>,
    /// Backs off and de-duplicates failing segment fetches.
    fetch_gate: FetchGate,
    /// Whether the park `tick` is about to do is worth waking the UI for.
    /// Cleared only by a fetch failure the gate swallowed: that park is the
    /// previous one repeating, and ringing for it as fast as it happens is
    /// what closed the engine <-> range-repeat feedback loop on 2026-08-14.
    /// Only meaningful immediately after an `ensure_segment` that returned
    /// false -- `do_seek` writes it too, and its value is stale by the next
    /// loop iteration.
    report_park: bool,
    /// When the UI was last rung for a park, for `PARK_RING_FLOOR`.
    last_park_ring: Option<Instant>,
    /// Set only by a park that a *playing* transport fell into, which is the
    /// one an `ExtendPlaylist` may resume (task161). A user pause at the live
    /// edge leaves it false, so growing the playlist never starts playback the
    /// user didn't ask for -- `at_live_edge` alone cannot tell the two apart
    /// (a paused seek to the end sets it too).
    parked_from_play: bool,
    /// Produced but not yet accepted by the endpoint (task154), interleaved
    /// stereo like everything else on this path. `AudioOut::write` clamps to
    /// the room actually left in the WASAPI buffer, and the overflow has to
    /// wait here for the next tick rather than be dropped -- at 1x the fill
    /// loop keeps this almost always empty, but under time-stretch one input
    /// block can expand past the room available.
    pending_out: Vec<f32>,
    /// Built the first time a non-1x block needs it (task155). 1x never
    /// touches it, so normal playback pays neither the CPU nor the latency.
    stretcher: Option<AudioStretcher>,

    fps_window: Instant,
    fps_frames: u32,
    /// Disconnects when the engine is dropped (task1620). Nothing is ever sent
    /// on it -- it exists so `nap` can be woken, see the comment there.
    stop_rx: Receiver<()>,
    /// When `publish_prepared` last handed the UI a frame (t260917-efbe):
    /// the clock `release_idle_spares` reads to decide the engine has gone
    /// quiet and its spare buffers are only holding memory.
    last_publish: Instant,
    /// What the last `do_seek` decoded and dropped, and the absolute time of
    /// the frame it published (t260913-b4a6). The `skipped` of `task128_seek`,
    /// kept where a test can read it without parsing the log.
    #[cfg(test)]
    last_seek: (u32, Option<i64>),
    /// How many times `do_seek` asked the source to seek, so a test can tell a
    /// resumed precise seek from one that went back to the keyframe.
    #[cfg(test)]
    source_seeks: u32,
}

/// How long the engine must have published nothing before its spare buffers
/// go back to the allocator (t260917-efbe).
///
/// Long enough that playing and scrubbing never see it -- both publish many
/// times a second, and each publish restarts the clock -- and short enough that
/// a pause, the end of a clip or a park at the live edge stops holding a 4K
/// frame's worth of buffers almost at once. What a resume after it pays is one
/// fresh allocation per slot on its first frame: task1640 measured 1171us
/// against 428us for a reused 7.8MB buffer, once, not per frame.
pub(super) const SPARE_IDLE: Duration = Duration::from_secs(2);

/// Waits out a frame's deadline, but no longer than the engine lives
/// (task1620).
///
/// `thread::sleep` here was the UI deadlock of 2026-08-25: `PlaybackEngine::drop`
/// joins this thread from the UI thread (task1520 needs that join synchronous),
/// so a worker asleep on a deadline the master clock put hours out froze the
/// window until the process was killed. `recv_timeout` on a channel nobody
/// sends to is the same sleep, except that dropping the engine ends it.
fn nap(stop_rx: &Receiver<()>, duration: Duration) {
    // Both arms mean "stop waiting": `Disconnected` is the engine going away,
    // `Timeout` is the deadline arriving as it always did.
    let _ = stop_rx.recv_timeout(duration);
}

#[cfg(test)]
mod nap_tests {
    use super::*;

    /// The property the whole fix rests on: a nap far longer than any join is
    /// willing to wait ends the moment the engine's sender goes.
    #[test]
    fn dropping_the_engine_ends_a_long_nap() {
        let (stop_tx, stop_rx) = crossbeam_channel::unbounded::<()>();
        let started = Instant::now();
        let napper = std::thread::spawn(move || nap(&stop_rx, Duration::from_secs(30)));
        drop(stop_tx);
        napper.join().expect("the napper thread");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the nap outlived its sender by {:?}",
            started.elapsed()
        );
    }

    /// And without that, it still is a sleep: a nap nobody interrupts runs its
    /// duration out rather than returning immediately.
    #[test]
    fn an_uninterrupted_nap_waits_for_its_deadline() {
        let (_stop_tx, stop_rx) = crossbeam_channel::unbounded::<()>();
        let started = Instant::now();
        nap(&stop_rx, Duration::from_millis(50));
        assert!(
            started.elapsed() >= Duration::from_millis(45),
            "the nap returned after only {:?}",
            started.elapsed()
        );
    }
}

impl Worker {
    // Eight distinct, unrelated values with no shared owner to group them under:
    // bundling them into a struct would only move the same list one layer out.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        source: Arc<dyn SegmentSource>,
        snapshot: TimelineSnapshot,
        initial_position_100ns: i64,
        volume_percent: i64,
        muted: bool,
        shared: Arc<PlaybackShared>,
        notify: Box<dyn Fn() + Send>,
        stop_rx: Receiver<()>,
    ) -> Self {
        let audio = match AudioOut::new() {
            Ok(audio) => Some(audio),
            Err(error) => {
                // No render device: video-only, never fatal.
                tracing::warn!(?error, "playback: audio output unavailable");
                None
            }
        };
        let mut worker = Self::assemble(
            source,
            snapshot,
            initial_position_100ns,
            volume_percent,
            muted,
            shared,
            notify,
            audio,
            stop_rx,
        );
        worker.publish_status();
        // Show the first frame immediately, like the <video> element's poster.
        worker.do_seek(initial_position_100ns, true);
        worker
    }

    /// The bookkeeping half of `new`, with the endpoint handed in. Split out so
    /// unit tests can drive playlist/park logic without WASAPI: a test thread
    /// has no COM apartment of its own, and an `IAudioClient` released on one
    /// is an intermittent access violation in the test binary.
    #[allow(clippy::too_many_arguments)]
    fn assemble(
        source: Arc<dyn SegmentSource>,
        snapshot: TimelineSnapshot,
        initial_position_100ns: i64,
        volume_percent: i64,
        muted: bool,
        shared: Arc<PlaybackShared>,
        notify: Box<dyn Fn() + Send>,
        audio: Option<AudioOut>,
        stop_rx: Receiver<()>,
    ) -> Self {
        let playlist = snapshot
            .segments
            .iter()
            .map(|segment| PlaylistEntry {
                index: segment.index,
                start_100ns: segment.start_100ns,
                end_100ns: segment.end_100ns,
                audio_offsets_100ns: segment.audio_offsets_100ns.clone(),
            })
            .collect();
        Self {
            source,
            session_id: snapshot.session_id.clone(),
            playlist,
            live_edge_100ns: snapshot.live_edge_100ns,
            // False until something asks: a session that is recording says so
            // on its first `ExtendPlaylist`, ~100ms away, and starting without
            // the reserve only means the first crossing behaves as it did
            // before task1530.
            live: false,
            shared,
            notify,
            current: None,
            retirer: ReaderRetirer::spawn(),
            scaler: FrameScaler::default(),
            gpu: None,
            gpu_tried: false,
            gpu_pending: None,
            cpu_readback: false,
            next: None,
            fetcher: SegmentFetcher::spawn(),
            prefetch_generation: 0,
            prefetch_in_flight: None,
            lease: None,
            audio,
            playing: false,
            position_100ns: initial_position_100ns,
            rate: 1.0,
            volume_scale: volume_scale(volume_percent, muted),
            track_volumes: Vec::new(),
            extra_audio: Vec::new(),
            anchor: None,
            pending_frame: None,
            consecutive_drops: 0,
            must_show_next: false,
            last_publish_deadline: None,
            crossings: 0,
            crossings_prefetched: 0,
            crossings_live: 0,
            crossings_live_prefetched: 0,
            parks: 0,
            frames_dropped: 0,
            frames_skipped: 0,
            last_audio_end_100ns: None,
            pending_seam: None,
            drain_dropped: 0,
            drain_dropped_100ns: 0,
            audio_queued_min_ms: u32::MAX,
            audio_align_100ns: None,
            audio_realigned: 0,
            audio_recent: VecDeque::new(),
            fetch_gate: FetchGate::default(),
            report_park: true,
            last_park_ring: None,
            parked_from_play: false,
            pending_out: Vec::new(),
            stretcher: None,
            fps_window: Instant::now(),
            fps_frames: 0,
            stop_rx,
            last_publish: Instant::now(),
            #[cfg(test)]
            last_seek: (0, None),
            #[cfg(test)]
            source_seeks: 0,
        }
    }

    pub(super) fn run(&mut self, rx: Receiver<PlaybackCommand>) {
        loop {
            // Every pass, playing or not: the idle branch below comes back
            // here at least every 250ms, and a park at the live edge is
            // `playing` with nothing published (t260917-efbe).
            self.release_idle_spares(Instant::now());
            // Coalesce: only the newest queued seek survives, so a scrub can
            // never build a backlog (the React engine's fetch window could).
            let mut pending_seek: Option<(i64, bool)> = None;
            let mut shutdown = false;
            let handle = |worker: &mut Self,
                          command: PlaybackCommand,
                          pending_seek: &mut Option<(i64, bool)>,
                          shutdown: &mut bool| {
                match command {
                    PlaybackCommand::Play => worker.play(),
                    PlaybackCommand::Pause => worker.pause(),
                    PlaybackCommand::Seek {
                        target_100ns,
                        precise,
                    } => *pending_seek = Some((target_100ns, precise)),
                    PlaybackCommand::SetRate(rate) => worker.set_rate(rate),
                    PlaybackCommand::SetVolume { percent, muted } => {
                        worker.volume_scale = volume_scale(percent, muted);
                    }
                    PlaybackCommand::SetTrackVolume {
                        track,
                        percent,
                        muted,
                    } => {
                        // Grown on demand: the engine does not know how many
                        // tracks a session has until a segment is open, and a
                        // track nothing has touched is full volume anyway.
                        if worker.track_volumes.len() <= track {
                            worker.track_volumes.resize(track + 1, (100, false));
                        }
                        worker.track_volumes[track] = (percent, muted);
                    }
                    PlaybackCommand::ExtendPlaylist {
                        segments,
                        live_edge_100ns,
                    } => worker.extend_playlist(&segments, live_edge_100ns),
                    PlaybackCommand::Shutdown => *shutdown = true,
                }
            };
            if self.playing {
                while let Ok(command) = rx.try_recv() {
                    handle(self, command, &mut pending_seek, &mut shutdown);
                }
            } else {
                // Idle: block until something arrives.
                match rx.recv_timeout(Duration::from_millis(250)) {
                    Ok(command) => {
                        handle(self, command, &mut pending_seek, &mut shutdown);
                        while let Ok(command) = rx.try_recv() {
                            handle(self, command, &mut pending_seek, &mut shutdown);
                        }
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                        self.poll_parked_resume();
                        continue;
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                }
            }
            if shutdown {
                self.release_lease();
                return;
            }
            if let Some((target, precise)) = pending_seek {
                self.do_seek(target, precise);
                // Echo the target back so the UI knows the position it is
                // about to read belongs to this seek and not the one before
                // it (task2910). Unconditional, and only here: an internal
                // seek (the poster frame, play-from-head) answers no request
                // the UI is holding for, and a seek that could not run --
                // no playlist entry, unreadable segment -- must still answer,
                // or the UI keeps its position forever.
                self.shared.status.lock().unwrap().acked_seek_100ns = Some(target);
                (self.notify)();
            }
            if self.playing {
                self.tick();
            }
        }
    }

    /// Grows the playlist of a session that is still recording (task161). The
    /// engine is never rebuilt for this: segments finalize about every two
    /// seconds, and restarting would re-establish WASAPI and drop frames that
    /// often. Appending is all a monotonically growing session ever needs.
    fn extend_playlist(&mut self, segments: &[TimelineSegment], live_edge_100ns: i64) {
        let added = append_new_segments(&mut self.playlist, segments);
        // An extend is itself proof the session is recording (task1530):
        // `follow_live_edge` only sends one when the manifest it polled is
        // still this session's. Asking the controller here would cost a lock
        // for an answer already in hand.
        self.live = true;
        let advanced = live_edge_100ns > self.live_edge_100ns;
        if advanced {
            self.live_edge_100ns = live_edge_100ns;
        }
        if added == 0 && !advanced {
            return;
        }
        if self.parked_from_play {
            // Playback ran out of playlist while playing; there is more now --
            // but while recording, "more" arriving is not on its own a reason
            // to move (task1800).
            //
            // An extend advances the playable edge by exactly one segment, so
            // resuming on it leaves the transport playing the *last* segment
            // the reserve allows -- and crossing out of that one is precisely
            // what `tick` refuses, so it parks again at its end, two seconds
            // later. Media arrives at 1.0x and plays at 1.0x, so that lead is
            // conserved rather than rebuilt: a park per crossing, forever.
            // Deepening the reserve moves the edge and the position together
            // and changes nothing.
            //
            // So a live park waits for real lead instead. One park per follow
            // session while the buffer fills, then continuous playback with a
            // whole segment of crossing room.
            // Not on a cold start, though: a session opened before anything
            // finalized parks *before* its own first segment, and the stage
            // stays black until that park resumes. There is no frame to hold
            // and nothing to stutter, so the first segment plays on arrival --
            // the hysteresis is for a transport that caught up to media it was
            // already showing.
            let head = self.playlist.first().map(|s| s.start_100ns).unwrap_or(0);
            if self.live
                && self.position_100ns >= head
                && self.playable_edge_100ns() - self.position_100ns < LIVE_RESUME_LEAD_100NS
            {
                self.write_status();
                return;
            }
            // Re-anchors the master clock and clears the park flag -- and
            // keeps the parked position rather than restarting from the head,
            // which is the whole difference between this and `play`.
            self.resume_from_park();
            return;
        }
        if !self.playing {
            // A paused transport keeps its position, but the live edge moving
            // past it means it is no longer *at* the edge.
            let mut status = self.shared.status.lock().unwrap();
            status.at_live_edge = self.position_100ns >= self.playable_edge_100ns() - 1;
        }
        self.publish_status();
    }

    /// Whether the session under the transport is the one still recording
    /// (task1530). Exact -- the source's own answer -- rather than a
    /// timeout on the extend cadence, which would guess wrong for as long as
    /// the timeout lasts every time a recording stops. A clip is never live.
    fn refresh_live(&mut self) {
        self.live = self.source.is_live(&self.session_id);
    }

    /// The furthest the transport may go: the end of the last segment
    /// `crossable_len` allows, which while recording is one behind the newest
    /// (task1530). Park position and seek clamp both use it, so nothing can
    /// land inside the reserved segment and then have to jump back out of it.
    fn playable_edge_100ns(&self) -> i64 {
        let crossable = self.crossable();
        if crossable < self.playlist.len() {
            self.playlist[crossable - 1].end_100ns
        } else {
            self.live_edge_100ns
        }
    }

    /// Where `play` may not resume from. Off the end is the `<video>` rule --
    /// pressing play on a finished clip starts it over. *Before* the head is
    /// the same problem from the other side: a session loaded before it had
    /// recorded anything sits at 0 while the timeline it grows into is
    /// absolute 100ns ticks, so every tick would park on a position no segment
    /// covers and the stage would never draw.
    fn restarts_from_head(position_100ns: i64, head_100ns: i64, live_edge_100ns: i64) -> bool {
        position_100ns < head_100ns || position_100ns >= live_edge_100ns - 1
    }

    fn play(&mut self) {
        self.play_from_here(true);
    }

    /// The engine's own resume out of `park_at_live_edge`, which is not the
    /// user pressing play (task161's contract, stated at the park itself).
    ///
    /// It must not take the head-restart branch. The park deliberately sits on
    /// the last playable instant, so it satisfies `restarts_from_head` by
    /// construction -- the rule is about a transport that ran *off* the end,
    /// and this one was put there. Measuring it against the playable edge
    /// rather than the live one does not help: the two coincide exactly in the
    /// cases that matter (a one-segment live playlist, where the reserve is
    /// off), so the resume would restart a live-follow session from the top
    /// instead of continuing. The distinction is the caller, not the ruler.
    fn resume_from_park(&mut self) {
        self.play_from_here(false);
    }

    /// The way out of a live park when the recording it is waiting on stops
    /// (task1800).
    ///
    /// A live park holds until `LIVE_RESUME_LEAD_100NS` of playable media
    /// exists, and the only thing that delivers media is an extend -- which
    /// `follow_live_edge` sends only while the manifest is still growing. So a
    /// recording that stops while the transport is short of that lead would
    /// leave it parked for good, a few seconds shy of its own end. The idle
    /// loop's 250ms wake is what notices: once the session is no longer the one
    /// recording, the reserve turns off, the last segment becomes playable, and
    /// there is nothing left to wait for.
    fn poll_parked_resume(&mut self) {
        if !self.parked_from_play {
            return;
        }
        self.refresh_live();
        if !self.live {
            self.resume_from_park();
        }
    }

    fn play_from_here(&mut self, may_restart_from_head: bool) {
        self.parked_from_play = false;
        if self.playing {
            return;
        }
        // Playing off the live edge restarts from the top, like pressing play
        // on an ended <video>. So does playing from *before* the first segment:
        // a session loaded with nothing recorded yet starts at 0 while the
        // timeline it grows into is absolute 100ns ticks -- leaving the
        // transport playing a position no segment covers, which parks on every
        // tick and never draws a frame.
        let head = self.playlist.first().map(|s| s.start_100ns).unwrap_or(0);
        if may_restart_from_head
            && Self::restarts_from_head(self.position_100ns, head, self.live_edge_100ns)
        {
            self.do_seek(head, true);
        }
        self.playing = true;
        // Onto recorded time before the clock is anchored to it: see
        // `anchor_position`. The park's resume is the case that needs it --
        // it deliberately does not seek, so nothing else moves the position
        // out of the "before the first segment" hole.
        self.position_100ns = anchor_position(&self.playlist, self.position_100ns);
        self.anchor = Some((Instant::now(), self.position_100ns));
        if let Some(audio) = &mut self.audio {
            audio.resume();
        }
        self.publish_status();
    }

    fn pause(&mut self) {
        self.parked_from_play = false;
        self.playing = false;
        self.anchor = None;
        // Stop the endpoint, keep everything queued behind it (carry and
        // stretcher analysis included): resuming continues the same stream from
        // the same position. Flushing here instead used to advance the audio
        // reader an `AUDIO_BUFFER` past the clock on every pause, which the
        // segment's audio paid for by ending that much early -- the dropout
        // heard at the next crossing.
        if let Some(audio) = &mut self.audio {
            audio.pause();
        }
        // Publishing the frame in flight rings the status bell itself
        // (`publish_prepared` ends in `publish_status`), so the pause's own
        // publish is the *else* of it rather than a second one. `playing` is
        // already false above, so either way the status published says paused.
        if !self.flush_gpu_pending() {
            self.publish_status();
        }
    }

    fn set_rate(&mut self, rate: f64) {
        let previous = self.rate;
        // Upper bound follows `ui_state::timeline::PLAYBACK_RATES`, which
        // round5 §4-C extended to 4x (task171). Clamping at 2.0 while the menu
        // offered 4x would have played the 4x row at half its own label.
        self.rate = rate.clamp(0.25, 4.0);
        // Re-anchor so the rate change applies from here, not from play start.
        if self.playing {
            self.anchor = Some((Instant::now(), self.position_100ns));
        }
        // Crossing 1x swaps the whole audio path -- bypass on one side, the
        // stretcher on the other -- so what is already queued no longer belongs
        // to it. Flush and start clean, at the cost of a few tens of ms.
        // Between two non-1x rates only the length ratio moves and the
        // stretcher keeps its analysis, which is what makes that change
        // glitch-free (task155).
        let crossed_1x = (previous == 1.0) != (self.rate == 1.0);
        // Taken *before* the flush, which clears it. These are the blocks the
        // endpoint was already holding: without them the flush silently eats
        // that span of media and leaves the reader ahead of the clock by it
        // (task1840).
        let retained = if crossed_1x {
            std::mem::take(&mut self.audio_recent)
        } else {
            VecDeque::new()
        };
        if crossed_1x {
            self.flush_audio();
        }
        if let Some(stretcher) = self.stretcher.as_mut() {
            stretcher.set_rate(self.rate);
        }
        if crossed_1x {
            self.requeue_audio(retained);
            // And put it out now rather than leaving the endpoint stopped
            // until the next frame tick gets round to it.
            if self.playing {
                self.pump_audio(self.position_100ns);
            }
        }
    }

    /// Re-render blocks the 1x crossing's flush destroyed, through whichever
    /// audio path is now current, into the carry -- which `pump_audio` already
    /// drains with the right backpressure, so a 0.25x crossing that turns
    /// 200ms of media into 800ms of output needs no special case (task1840).
    ///
    /// task155 holds: these are blocks at the *current* position (a rate change
    /// does not move it), re-rendered by the path that is now correct for it.
    /// Nothing from an old position can reach here -- `flush_audio` clears the
    /// retention, so a seek or a park leaves nothing to replay.
    fn requeue_audio(&mut self, retained: VecDeque<(i64, Vec<f32>)>) {
        let position = self.position_100ns;
        for (absolute, samples) in retained {
            // Same straddler rule as the seek realignment: a block that ends
            // at or before the clock has already been heard.
            if block_fate(
                absolute + block_span_100ns(samples.len()),
                position,
                Some(position),
            ) != BlockFate::Play
            {
                continue;
            }
            if self.rate == 1.0 {
                self.pending_out.extend_from_slice(&samples);
            } else {
                let rate = self.rate;
                let out = self
                    .stretcher
                    .get_or_insert_with(|| AudioStretcher::new(rate))
                    .feed(&samples);
                self.pending_out.extend_from_slice(out);
            }
        }
    }

    /// How far into the playlist this transport may play, live reserve
    /// included. See `crossable_len`.
    fn crossable(&self) -> usize {
        crossable_len(self.playlist.len(), self.live)
    }

    fn playlist_position(&self, time_100ns: i64) -> Option<usize> {
        playlist_position(&self.playlist, time_100ns)
    }

    fn ensure_segment(&mut self, playlist_index: usize) -> bool {
        if self
            .current
            .as_ref()
            .is_some_and(|current| current.playlist_index == playlist_index)
        {
            return true;
        }
        // Already opened ahead of time (task203). Its decoded first frame is
        // only useful to the crossing in `tick`, which takes the whole
        // `Prefetched` itself; reaching here means something else asked for
        // this segment, so the reader is kept and the frame discarded.
        if self
            .next
            .as_ref()
            .is_some_and(|next| next.playlist_index == playlist_index)
        {
            let prefetched = self.next.take().expect("checked");
            self.retirer.retire(self.current.replace(prefetched.reader));
            return true;
        }
        match self.open_segment_at(playlist_index) {
            Some(reader) => {
                self.retirer.retire(self.current.replace(reader));
                true
            }
            None => false,
        }
    }

    /// Leases, reads and opens one playlist entry, reporting failures through
    /// the fetch gate. Shared by the synchronous path and the prefetch, so a
    /// segment opened early is opened exactly the same way.
    fn open_segment_at(&mut self, playlist_index: usize) -> Option<SegmentReader> {
        crate::insight_scope!("playback_open_segment");
        let entry_index = self.playlist[playlist_index].index;
        let lease = self.lease_segment(playlist_index)?;
        // Scoped apart from the read inside it: `playback_open_segment` covers
        // lease + bytes + decoder, `playback_open_bytes` (raised by the source)
        // covers the read alone, and only the split says which one a slow seek
        // was in (task3460).
        let source = self.source.clone();
        let manager = self.decoder_manager();
        let opened = {
            crate::insight_scope!("playback_open_reader");
            open_segment_with(playlist_index, manager.as_ref(), || {
                source.byte_stream(&lease, entry_index, playlist_index)
            })
        };
        match opened {
            Ok(reader) => {
                self.note_fetch_success(entry_index);
                Some(reader)
            }
            Err(error) => {
                self.note_fetch_failure(entry_index, "playback: segment open failed", error);
                None
            }
        }
    }

    /// The gate check and the lease window, which is everything a fetch needs
    /// before the bytes are touched. Split out for task1510: the read itself
    /// runs on the fetch thread, but neither the gate nor `self.lease` may
    /// leave this one -- both are worker-only state, and both are cheap.
    fn lease_segment(&mut self, playlist_index: usize) -> Option<String> {
        let entry_index = self.playlist[playlist_index].index;
        // Before the lease, not after it: a gated attempt has to cost
        // nothing, and `acquire_review_lease`/`release_review_lease` log a
        // line each. Two of every three records the 2026-08-14 storm wrote
        // were that pair, not the warn below.
        if !self.fetch_gate.ready(entry_index, Instant::now()) {
            self.report_park = false;
            return None;
        }
        // Window the lease over what is about to be read, acquiring the new
        // window before releasing the old so prune never sees a hole.
        let window: Vec<u64> = self
            .playlist
            .iter()
            .skip(playlist_index)
            .take(LEASE_WINDOW)
            .map(|entry| entry.index)
            .collect();
        let new_lease = self.source.acquire_lease(&self.session_id, window);
        let lease = match new_lease {
            Ok(lease) => lease,
            Err(error) => {
                self.note_fetch_failure(entry_index, "playback: lease acquisition failed", error);
                return None;
            }
        };
        if let Some(old) = self.lease.replace(lease.clone()) {
            self.source.release_lease(&old);
        }
        Some(lease)
    }

    /// Clears the gate's backoff for an index that just read cleanly. Shared
    /// with the fetch thread's responses (task1510) so an asynchronous recovery
    /// reports exactly like a synchronous one.
    fn note_fetch_success(&mut self, index: u64) {
        if let Some(suppressed) = self.fetch_gate.succeed(index) {
            tracing::info!(index, suppressed, "playback: segment fetch recovered");
        }
    }

    /// Records a failed fetch against the gate, warning only for the ones
    /// it says are news. Every failure arm goes through here so none of
    /// them can retry-and-log without a backoff behind it.
    fn note_fetch_failure(&mut self, index: u64, message: &'static str, error: String) {
        let report = self.fetch_gate.fail(index, Instant::now());
        self.report_park = report.log;
        if report.log {
            tracing::warn!(
                error,
                index,
                suppressed = report.suppressed,
                retry_in_ms = report.retry_in.as_millis() as u64,
                "{message}"
            );
        }
    }

    fn do_seek(&mut self, target_100ns: i64, precise: bool) {
        crate::insight_scope!("playback_seek");
        // The whole call, including the part before `started` below. `took_ms`
        // keeps its old boundary so it still compares against every log since
        // task128; `pre_ms` is what that boundary was hiding (task3460).
        let entered = Instant::now();
        // Read before `abandon_prefetch` clears it: near the live edge the
        // fetch thread is reading the next segment out of the same 20GB
        // container this seek is about to read synchronously, and that read
        // cannot be called back (task1510). Whether the two overlap is the
        // last unexplained candidate for the near-edge excess (task3460).
        let prefetch_in_flight = self.prefetch_in_flight.is_some();
        // Whatever the transport was doing before it parked, it is not doing
        // it from here: an explicit seek owns the position now.
        self.parked_from_play = false;
        // Same for the frame whose blit is in flight (t260915-8ea7): it is the
        // picture from before the seek, and publishing it a tick later is the
        // stale frame AC3 forbids.
        self.drop_gpu_pending();
        // The segment read ahead belongs to where playback *was* heading
        // (task203). `ensure_segment` below may well want a different one, and
        // keeping a stale reader around would hand it to the wrong crossing.
        // The same goes for one still being read on the fetch thread, which
        // cannot be called back -- the generation is how its response is
        // recognised as belonging to nowhere (task1510).
        self.abandon_prefetch();
        self.refresh_live();
        // Almost all of this is `refresh_live` waiting on the `sessions`
        // mutex, which the index writer holds across its container appends,
        // prune and checkpoint -- every 2s while recording (task3460).
        let pre_ms = entered.elapsed().as_millis() as u64;
        let started = Instant::now();
        // `.max(low)`: an empty playlist has `live_edge_100ns == 0`, and
        // `clamp` panics when its bounds invert. A zero-segment (but
        // readable) manifest is loadable, so degrade to a zero-width range.
        let low = self.playlist.first().map(|s| s.start_100ns).unwrap_or(0);
        // The clamp follows the reserve while recording (task1530): a seek to
        // the far right that landed *inside* the held-back segment would play
        // a segment the crossing rule refuses to leave, and the park at its
        // end would throw the position back where it came from.
        let target = target_100ns.clamp(low, (self.playable_edge_100ns() - 1).max(low));
        let Some(playlist_index) = self.playlist_position(target) else {
            return;
        };
        let entry = self.playlist[playlist_index].clone();
        let target = target.max(entry.start_100ns);
        // A precise seek the clamp put on the edge lands on the keyframe the
        // source returns instead of decoding to the edge (t260913-b4a6). The
        // last frame before the edge is the one frame a precise seek has to
        // decode the entire last GOP to reach and then drop -- task3460
        // measured End at `skipped` 60-111 against Home's 0 -- and nobody asked
        // for that frame in particular: End and a scrub pinned to the right
        // both mean "the end". The position follows the frame shown.
        let edge_seek = precise && target == self.playable_edge_100ns() - 1;
        if !self.ensure_segment(playlist_index) {
            return;
        }
        let ensured_ms = started.elapsed().as_millis() as u64;
        // A scrub is two seeks to the same spot -- coarse, then precise -- and
        // the 2026-09-15 split priced the second at `SetCurrentPosition` ~10ms
        // plus a ~20ms decode floor before the first frame comes out, on top
        // of the frames it skips. The coarse seek left the reader just past
        // the keyframe before `target`, which is exactly where a fresh precise
        // seek would start decoding; so when the frame to show lies ahead of
        // the last one handed out, decode forward from here instead.
        //
        // Exact only when `last + 1/60 < target`: the frame a precise seek
        // shows is the first with `abs + 1/60 >= target`, and the reader cannot
        // give back one it already passed. Paused only: while playing the audio
        // streams have run on past the keyframe, and the realignment armed
        // below walks forward, never back.
        // ponytail: the 0.5s bound stands in for "fewer frames than a fresh
        // seek's own GOP walk"; the player does not know the GOP.
        let resume = precise
            && !edge_seek
            && !self.playing
            && self.current.as_ref().is_some_and(|current| {
                !current.video_done()
                    && current.last_video_local().is_some_and(|local| {
                        let last = entry.start_100ns + local;
                        last + HNS_PER_SECOND / 60 < target && target - last < HNS_PER_SECOND / 2
                    })
            });
        self.pending_frame = None;
        // The frame the user just asked to see is shown whatever the clock
        // says about it (task630): a seek that lands on a still picture reads
        // as the seek not working.
        self.must_show_next = true;
        self.consecutive_drops = 0;
        // The three halves `took_ms` never split (2026-09-15): a seek with
        // `ensured_ms` 2 and `skipped` 0 still took 55-111ms on a 1080p
        // release build, against 2620's 34ms spec. Each is scoped on its
        // own so the next `--insight` log says which one it is.
        {
            crate::insight_scope!("playback_seek_flush_audio");
            self.flush_audio();
        }
        // The source seeks to the keyframe before `target`, so the audio
        // stream resumes there too -- up to 1.6s early in run5. Arm the
        // realignment so those blocks are walked past rather than counted as
        // the clock drifting (task1840).
        self.audio_align_100ns = Some(target);
        let current = self.current.as_mut().expect("segment ensured");
        // An edge seek asks for a position 1/60s *inside* the edge, because the
        // edge itself is a position no source will take: a manifest declares a
        // segment's end as `exclusive_segment_end` (last sample + nominal
        // 1/fps), so `end - 1` rounds onto the track's own duration in its
        // ticks and `SetCurrentPosition` refuses it -- measured on production
        // writer segments at 60 and 120fps, evenly and unevenly paced, by
        // `a_production_segment_refuses_a_seek_to_its_own_end` (t260913-e5ce).
        // The shift changes nothing about where it lands: anywhere in the last
        // GOP seeks to that GOP's keyframe, so no frame rate and no keyframe
        // table is needed, and 1/60s clears one tick of any usable timescale.
        // Only the position handed to the source moves -- `target` still
        // decides `edge_seek`, the position and the audio realignment.
        let local_target =
            target - entry.start_100ns - if edge_seek { HNS_PER_SECOND / 60 } else { 0 };
        let accepted = if resume {
            true
        } else {
            #[cfg(test)]
            {
                self.source_seeks += 1;
            }
            crate::insight_scope!("playback_seek_source");
            current.seek_local(local_target)
        };
        if !accepted {
            tracing::warn!(target, "playback: source refused seek");
        }
        // Only a seek the source performed has a keyframe to land on. A refused
        // one leaves the reader wherever it stood -- mid-GOP on a reader already
        // in use -- and its next sample is just the next frame, so it decodes
        // forward to the target as it always did.
        let lands_on_keyframe = edge_seek && accepted;
        let decodes_to_target = precise && !lands_on_keyframe;
        // Coarse: the first decoded sample (the nearest clean point) is the
        // scrub preview. Precise: decode forward to the exact frame.
        // Only the frame that gets shown is converted to RGBA. The ones a
        // precise seek passes on its way to the target are decoded (there is
        // no skipping that) and then dropped where they are -- converting them
        // was ~4.7ms apiece spent on pixels nobody ever sees (task207).
        // The position is the seek target either way -- except an edge
        // landing, below -- and a decode that finds no frame to show still
        // leaves the transport where the user asked.
        self.position_100ns = target;
        let mut skipped = 0u32;
        #[cfg(test)]
        let mut shown = None;
        // Broken out of the loop rather than published inside it
        // (t260914-8d69): `gpu_frame` takes `&mut self` and reads `self.current`
        // itself, so the reader borrow has to be finished before the poster
        // frame can go down the same path as every played frame.
        let landing = {
            // Every `ReadSample` of the seek, the keyframe and the frames a
            // precise seek passes alike: `skipped` in the log divides the two.
            crate::insight_scope!("playback_seek_decode");
            loop {
                let current = self.current.as_mut().expect("segment ensured");
                let Some((local, sample)) = current.next_video_sample() else {
                    break None;
                };
                let dimensions = (current.width, current.height);
                let absolute = entry.start_100ns + local;
                if decodes_to_target && absolute + HNS_PER_SECOND / 60 < target {
                    skipped += 1;
                    continue;
                }
                break Some((absolute, sample, dimensions));
            }
        };
        if let Some((absolute, sample, (width, height))) = landing {
            // The same convert-and-scale blit the played frames take
            // (t260913-2527). It used to be the CPU resample, which made the
            // poster -- including the one `Worker::new` shows at startup -- the
            // one frame per seek on a different scaler from its neighbours.
            //
            // Read back on the spot rather than pipelined (t260915-8ea7): a
            // seek may well be a paused scrub, and then there is no next tick to
            // map the picture on -- the stage would sit on the frame before the
            // seek for as long as the user keeps dragging. One stall per seek,
            // which is what this frame always cost.
            let prepared = match self.gpu_frame(&sample, width, height, gpu::Readback::Now) {
                GpuFrame::Ready(frame) => Some(Prepared::Scaled(frame)),
                // Unreachable for `Readback::Now`, which always reads what it
                // just wrote; publishing nothing is still the honest answer.
                GpuFrame::Warming => None,
                GpuFrame::Unavailable => {
                    // One frame per seek: not worth reaching for a spare
                    // (task1640).
                    self.cpu_convert(&sample, None)
                        .map(|rgba| Prepared::Decoded(rgba, width, height))
                }
            };
            // Nothing below runs for a frame that never became a picture --
            // the position, the realignment and `last_seek` all describe what
            // is on screen.
            if let Some(prepared) = prepared {
                if lands_on_keyframe {
                    self.position_100ns = absolute;
                    // Realign to the landing, not the edge: armed on the edge,
                    // a playing transport would walk past the very audio of the
                    // GOP it is about to show (task1840's `Realign`).
                    self.audio_align_100ns = Some(absolute);
                }
                #[cfg(test)]
                {
                    shown = Some(absolute);
                }
                match prepared {
                    Prepared::Decoded(rgba, width, height) => {
                        self.publish_frame(rgba, width, height)
                    }
                    Prepared::Scaled(frame) => self.publish_prepared(frame),
                }
            }
        }
        #[cfg(test)]
        {
            self.last_seek = (skipped, shown);
        }
        if self.playing {
            self.anchor = Some((Instant::now(), self.position_100ns));
            // Refill here rather than leaving the endpoint empty until the
            // next frame tick gets round to it (task1840). The flush above
            // stopped the endpoint dead; every frame interval between that and
            // the first write is silence on top of the seek's own cost.
            self.pump_audio(self.position_100ns);
        }
        {
            let mut status = self.shared.status.lock().unwrap();
            status.last_seek_ms = started.elapsed().as_millis() as u64;
            // A paused seek away from the live edge must drop the LIVE
            // indicator too; `publish_status` only clears it while playing.
            // An edge landing sits up to a GOP short of the edge and is still
            // the edge seek it was (t260913-b4a6).
            status.at_live_edge =
                lands_on_keyframe || self.position_100ns >= self.playable_edge_100ns() - 1;
        }
        tracing::info!(
            target: "task128_seek",
            precise,
            target_s = target / HNS_PER_SECOND,
            took_ms = started.elapsed().as_millis() as u64,
            pre_ms,
            prefetch_in_flight,
            // The two halves of a seek, so a slow one says which half was slow
            // without a rebuild: fetching and opening the segment, against
            // decoding forward to the target frame (task207).
            ensured_ms,
            skipped,
            resumed = resume,
            "seek finished"
        );
        self.publish_status();
    }

    /// Crosses into the segment after `playlist_index`, once the current
    /// one has no video left. Returns false when the tick has nothing more
    /// to do: the transport parked, or the crossing left no frame decoded
    /// and the next tick reads the new segment's first one.
    fn cross_to_next(&mut self, playlist_index: usize) -> bool {
        let next = playlist_index + 1;
        // Once per crossing is often enough to notice the recording stopping,
        // and it is the only place the answer changes what happens next
        // (task1530).
        self.refresh_live();
        if next >= self.crossable() {
            self.park_at_live_edge(true);
            return false;
        }
        let previous_end = self.playlist[playlist_index].end_100ns;
        let entry = self.playlist[next].clone();
        // A prefetch (task203) carries its first frame with it, so a crossing
        // that hit one costs nothing here and the frame below is paced against
        // the same clock as every other.
        let prefetched = self
            .next
            .take()
            .filter(|ready| ready.playlist_index == next);
        // How many crossings land on a prefetch -- the number task209 was
        // decided on (96% of 79). `debug!` like `task128_tick` above: a line
        // per 2s segment is noise in the shipped log, but the measurement has
        // to stay repeatable without editing code again.
        tracing::debug!(
            target: "task209_crossing",
            prefetched = prefetched.is_some(),
            index = next,
            "segment crossing"
        );
        self.crossings += 1;
        // The frame in flight belongs to the segment being left (t260915-8ea7).
        // Where the two segments differ in size the pipeline rebuild would drop
        // it anyway; this is the case where they do not, and it costs the seam
        // one picture rather than letting the outgoing segment's last frame
        // arrive after the incoming one's clock.
        self.drop_gpu_pending();
        // At the live edge the incoming segment is the newest one the
        // transport may play, which is the case task1530 is about: before the
        // reserve it had been appended moments ago and nothing had time to
        // read it ahead. Counted apart from the catch-up crossings so the hit
        // rate says *which* kind is missing.
        if next + 1 >= self.crossable() {
            self.crossings_live += 1;
            if prefetched.is_some() {
                self.crossings_live_prefetched += 1;
            }
        }
        // Task1880: the outgoing reader still holds the audio the video ran
        // out ahead of -- 1-3 access units on a healthy crossing, and they
        // used to go with the retired reader (task1860 measured the loss at
        // 0.26% of crossings). Ordered deliberately: before `pending_seam` is
        // armed, so the seam line still belongs to the incoming segment's
        // first block rather than being eaten by a drained one; and before
        // either retire path below, which is the last moment `self.current` is
        // the outgoing reader. Not in `ensure_segment` -- that also retires on
        // seeks, where draining would replay the audio the seek left behind.
        self.drain_outgoing_audio(previous_end);
        // Task640: what the audio did across this seam. Filled in by the next
        // block `pump_audio` emits, so the two halves (what the outgoing
        // segment stopped at, what the incoming one starts at) are one line.
        self.pending_seam = Some((
            entry.index,
            previous_end,
            entry.start_100ns,
            prefetched.is_some(),
        ));
        match prefetched {
            Some(ready) => {
                self.crossings_prefetched += 1;
                // Before the queue is filled: `tick` reads `self.current` for
                // the frame's dimensions and for `nv12_planes`, and the sample
                // below came out of *this* reader on the fetch thread.
                self.retirer.retire(self.current.replace(ready.reader));
                self.pending_frame = ready
                    .first
                    .map(|(absolute, sample)| Pending::Sample(absolute, sample));
                // The seam frame is shown however the clock feels about it.
                // It used to be exempt by being a `Pending::Ready`; now that it
                // is queued like every other frame (t260914-8d69), the exemption
                // is the same flag a seek uses. Only when there is a frame: a
                // crossing that missed the prefetch was never protected.
                self.must_show_next |= self.pending_frame.is_some();
            }
            None => {
                if !self.ensure_segment(next) {
                    self.park_at_live_edge(self.report_park);
                    return false;
                }
            }
        }
        self.position_100ns = entry.start_100ns;
        // Contiguous segments are one recording split into files: holding the
        // clock is what makes the seam invisible. Only a real gap restarts it,
        // so the silence is skipped rather than waited out.
        if !keeps_anchor(previous_end, entry.start_100ns) {
            self.anchor = Some((Instant::now(), entry.start_100ns));
        }
        if self.pending_frame.is_none() {
            // Nothing decoded ahead; the next tick reads the new segment's
            // first frame, as it always did.
            return false;
        }
        true
    }

    /// Sleeps until this frame is due, and says so out loud when the wait is
    /// long enough to have emptied the audio endpoint (task1900).
    ///
    /// Measured apart from the rest of the tick (task210): this wait is the
    /// frame's pacing, so counting it as tick cost put a scope that is doing
    /// nothing at the top of every insight run.
    fn wait_for_deadline(
        &self,
        deadline: Instant,
        absolute: i64,
        due_in_media: i64,
        anchor_media: i64,
    ) {
        let now = Instant::now();
        if deadline > now {
            // Measured apart from the rest of the tick (task210): this wait is
            // the frame's pacing, so counting it as tick cost put a scope that
            // is doing nothing at the top of every insight run.
            crate::insight_scope!("playback_sleep");
            let requested = deadline - now;
            // Not `thread::sleep`: the UI thread joins this one when the engine
            // is swapped, so this wait has to end when the engine does
            // (task1620).
            nap(&self.stop_rx, requested);
            // Task1900: a wait this long empties the endpoint, so it is exactly
            // the wait that used to cost the outgoing segment's audio. Which
            // half produced it is the question 1890 left open, and both halves
            // are here: `step_ms` (this frame's PTS against the last one) says
            // the material or the reader handed up a jump, `slept_ms` against
            // `wait_ms` says the wait itself overshot. `info!` because the file
            // layer is INFO-filtered and this is too rare to be noise -- a
            // frame interval is ~17ms and the bar is `AUDIO_BUFFER`.
            if requested > AUDIO_BUFFER {
                tracing::info!(
                    target: "task1900_long_wait",
                    wait_ms = requested.as_secs_f64() * 1_000.0,
                    slept_ms = now.elapsed().as_secs_f64() * 1_000.0,
                    step_ms = (absolute - self.position_100ns) / 10_000,
                    due_in_media_ms = due_in_media / 10_000,
                    absolute,
                    anchor_media,
                    rate = self.rate,
                    "a frame deadline past the audio buffer"
                );
            }
        }
    }

    /// One playing iteration: decode the next due frame, keep audio fed,
    /// sleep to the frame's deadline, publish.
    fn tick(&mut self) {
        // One pass of the playback engine: decode, present, audio, prefetch
        // (task205). The scopes below it say which of those is the cost --
        // including `playback_sleep`, which is the wait for the frame's
        // deadline, so tick minus sleep is the work (task210).
        crate::insight_scope!("playback_tick");
        // Before `ensure_segment`, so a read-ahead that finished between two
        // ticks is in `self.next` in time for a crossing this tick makes
        // (task1510).
        self.take_prefetch_responses();
        let Some(playlist_index) = self.playlist_position(self.position_100ns) else {
            self.park_at_live_edge(true);
            return;
        };
        if !self.ensure_segment(playlist_index) {
            // Parking is what stops this loop -- but only until the UI arms
            // it again, which range repeat does on every drain tick. Which
            // is why a failure the gate already reported once parks under
            // `PARK_RING_FLOOR`: the ring is the other half of the loop, and
            // throttling it is what keeps the pair from running away
            // (2026-08-14).
            self.park_at_live_edge(self.report_park);
            return;
        }
        // Decode the next frame if none is waiting.
        let decode_started = Instant::now();
        if self.pending_frame.is_none() {
            let entry = self.playlist[playlist_index].clone();
            let current = self.current.as_mut().expect("segment ensured");
            // Decode only (task630). The RGBA swizzle is deferred to the
            // deadline check below, so a frame that turns out to be late costs
            // the decode and nothing more.
            // Task1580: the steady-state read, which had no scope at all --
            // which is what a "the stage and the decoder are fighting over the
            // GPU" hypothesis has to be tested against. The fetch thread's own
            // first-frame read is `playback_prefetch_first` (t260914-8d69 left
            // `playback_decode_video` to the tests, the last callers of
            // `next_video`).
            let read = {
                crate::insight_scope!("playback_read_sample");
                current.next_video_sample()
            };
            match read {
                Some((local, sample)) => {
                    self.pending_frame = Some(Pending::Sample(entry.start_100ns + local, sample));
                }
                None => {
                    if !self.cross_to_next(playlist_index) {
                        return;
                    }
                }
            }
        }
        let absolute = self
            .pending_frame
            .as_ref()
            .expect("decoded above")
            .absolute();
        let (anchor_wall, anchor_media) = *self
            .anchor
            .get_or_insert_with(|| (Instant::now(), self.position_100ns));
        let due_in_media = absolute - anchor_media;
        let deadline = frame_deadline(anchor_wall, anchor_media, absolute, self.rate);
        let decode_us = decode_started.elapsed().as_micros();
        let audio_started = Instant::now();
        self.pump_audio(absolute);
        let audio_us = audio_started.elapsed().as_micros();
        tracing::debug!(target: "task128_tick", decode_us, audio_us, "tick cost");
        // Spend the wait opening the next segment rather than sleeping through
        // it (task203). Whatever is left is still slept, so at x1 this costs
        // idle time and nothing else. It used to be skipped when the frame
        // budget looked too tight, which at x1.5 and above meant always
        // (task630); overrunning now costs a dropped frame instead of a shown
        // one, and that is the cheaper half of the trade.
        self.prefetch_next(playlist_index);
        self.wait_for_deadline(deadline, absolute, due_in_media, anchor_media);
        let late = Instant::now().saturating_duration_since(deadline);
        let pending = self.pending_frame.take().expect("still queued");
        // Every queued frame is now an unconverted sample (t260914-8d69), so
        // the flag is the whole rule: a seek's poster and a crossing's head set
        // it, and nothing else is exempt.
        let droppable = !self.must_show_next;
        // t260913-3842: before the reactive drop, and before either converter.
        // `self.position_100ns` still holds the previous frame's timestamp
        // here -- every arm below writes it -- so its deadline is one source
        // frame back and the difference is the step `should_skip_frame` needs.
        let step = deadline.saturating_duration_since(frame_deadline(
            anchor_wall,
            anchor_media,
            self.position_100ns,
            self.rate,
        ));
        let since = self
            .last_publish_deadline
            .map(|last| deadline.saturating_duration_since(last));
        if should_skip_frame(droppable, since, step, self.shared.display_period()) {
            // Same exit as the drop below -- the clock advances and the audio
            // has already been pumped -- but none of its counters: this frame
            // was never going to be seen, so it is not late, it does not count
            // against `MAX_CONSECUTIVE_DROPS`, and folding it into
            // `frames_dropped` would make a healthy run look like a failing one.
            self.position_100ns = pending.absolute();
            self.frames_skipped += 1;
            return;
        }
        if should_drop_frame(late, droppable, self.consecutive_drops) {
            // Throw it away *before* the swizzle, which is the whole point
            // (task630): the clock still advances, so x2 stays x2 even when the
            // machine cannot convert frames twice as fast. `pump_audio` already
            // ran above, so sound never depends on whether a picture was shown.
            self.position_100ns = pending.absolute();
            self.consecutive_drops += 1;
            self.frames_dropped += 1;
            return;
        }
        self.consecutive_drops = 0;
        self.must_show_next = false;
        let Pending::Sample(absolute, sample) = pending;
        let (width, height) = {
            let current = self.current.as_ref().expect("segment ensured");
            (current.width, current.height)
        };
        // t260913-2527: both steps in one GPU blit when there is a stage
        // small enough to be worth scaling to. What comes back is already
        // stage-sized, so it skips `publish_frame`'s resample rather than
        // feeding it. `None` is every other case -- no stage, the 11b4 band,
        // a BGRA segment, no device -- and each falls through to the CPU pair
        // below unchanged.
        let prepared = match self.gpu_frame(&sample, width, height, gpu::Readback::Pipelined) {
            GpuFrame::Ready(frame) => Some(Prepared::Scaled(frame)),
            // t260915-8ea7: this frame's blit is in flight and its picture is
            // the next tick's. Nothing is published and the CPU pair does *not*
            // run -- it would redo on this thread the work that is already on
            // the GPU. The clock below still advances, so the frame is not lost,
            // only shown one tick late.
            GpuFrame::Warming => None,
            GpuFrame::Unavailable => {
                // Written into the buffer the UI handed back, when it handed
                // one back (task1640) -- a frame's worth of page faults the
                // engine thread would otherwise pay for a brand new 8.3MB
                // `Vec`.
                let spare = self.shared.take_spare(width as usize * height as usize * 4);
                // The swizzle task200 measured at 5.87ms, now paid only for
                // frames that are actually going to be seen.
                let Some(rgba) = ({
                    crate::insight_scope!("playback_convert_video");
                    self.cpu_convert(&sample, spare)
                }) else {
                    return;
                };
                Some(Prepared::Decoded(rgba, width, height))
            }
        };
        self.position_100ns = absolute;
        // t260913-3842: what the next tick's skip measures its period from.
        // Set whichever way the frame was prepared, and for a crossing's first
        // frame too, so it re-phases the period instead of being invisible
        // to it.
        self.last_publish_deadline = Some(deadline);
        let Some(prepared) = prepared else {
            // Warming: the counters below describe a frame that was shown, and
            // this one was not.
            return;
        };
        match prepared {
            Prepared::Decoded(rgba, width, height) => self.publish_frame(rgba, width, height),
            Prepared::Scaled(frame) => self.publish_prepared(frame),
        }
        let mut status = self.shared.status.lock().unwrap();
        status.frames_shown += 1;
        if late > Duration::from_millis(17) {
            status.frames_late += 1;
        }
        drop(status);
        self.fps_frames += 1;
        if self.fps_window.elapsed() >= Duration::from_secs(1) {
            self.log_playback_window();
        }
    }

    /// The once-a-second diagnostic line, and the counters it resets
    /// (task128). Emitted from `tick` after a frame is shown.
    fn log_playback_window(&mut self) {
        let mut status = self.shared.status.lock().unwrap();
        status.actual_fps = self.fps_frames;
        let snapshot = *status;
        drop(status);
        // The AC's numbers: displayed fps, cumulative drops, and the audio
        // drift corrections the Verification section asks to compare against
        // the recording-side diagnostic.
        tracing::info!(
            target: "task128_playback",
            fps = snapshot.actual_fps,
            shown = snapshot.frames_shown,
            late = snapshot.frames_late,
            drift = snapshot.drift_corrections,
            audio = snapshot.audio_enabled,
            position_s = snapshot.position_100ns / HNS_PER_SECOND,
            // Task630: the two numbers a rate change is judged on. Dropped
            // frames are the deliberate cost of keeping the clock true, and
            // the prefetch hit rate is what says whether the crossings are
            // still free -- both were only visible at `debug!` before, which
            // the shipped log never records.
            rate = self.rate,
            dropped = self.frames_dropped,
            // t260913-3842: frames never built because the display was still
            // showing the previous one. Separate from `dropped` on purpose --
            // adding the two would read as the machine failing to keep up.
            skipped = self.frames_skipped,
            crossings = self.crossings,
            prefetched = self.crossings_prefetched,
            // Task1530: the live-edge subset of the two above. A hit rate that
            // only moves here is the one this task changed.
            crossings_live = self.crossings_live,
            prefetched_live = self.crossings_live_prefetched,
            parks = self.parks,
            // How far the picture is behind the newest finalized frame
            // (task1530). The reserve buys its hit rate with exactly this, so
            // it is the number the trade is judged on.
            behind_ms = (self.live_edge_100ns - self.position_100ns) / 10_000,
            // Task640: the endpoint's low-water mark over the last second. A
            // dropout is this reaching 0; anything else is headroom. `None`
            // when nothing was written at all -- a paused or parked second, or
            // a session with no audio -- rather than the sentinel, which reads
            // as a nonsense 4294967295.
            queued_min_ms = ?(self.audio_queued_min_ms != u32::MAX)
                .then_some(self.audio_queued_min_ms),
            // Task1840: blocks walked past because a seek landed after them.
            // Every `drift` step in run5 was this wearing drift's name, so the
            // two have to be readable side by side to tell a fixed count from
            // a hidden one.
            realigned = self.audio_realigned,
            "playback tick"
        );
        self.audio_queued_min_ms = u32::MAX;
        self.fps_frames = 0;
        self.fps_window = Instant::now();
    }

    /// Asks the fetch thread for the segment after `playlist_index`, if the
    /// crossing is close enough to be worth it (task203).
    ///
    /// What is left here is the cheap half (task1510): the fetch gate, the
    /// lease window, and a `send`. The read, the Source Reader and the first
    /// decode happen on `SegmentFetcher`'s thread and come back through
    /// `take_prefetch_responses`. Failures are left to the normal path: the
    /// fetch gate has already recorded them, and the crossing will report the
    /// park.
    fn prefetch_next(&mut self, playlist_index: usize) {
        // The slack gate is gone (task630). It compared wall time, which shrank
        // with the rate while the ~30ms a prefetch costs did not: at x1.5 the
        // whole per-frame budget is about 11ms wall, so the gate almost never
        // opened and every crossing paid the full open-and-decode instead --
        // the stutter this task was filed for. Scaling it into media time was
        // tried first and only took the x1.5 hit rate from 22% to 56%, still
        // far short.
        //
        // Removing it is safe now in a way it was not when `PREFETCH_MIN_SLACK`
        // was written: the cost of overrunning a frame's deadline used to be a
        // frame shown late, and is now a frame *dropped before the swizzle* by
        // the code above.
        //
        // One read-ahead at a time, whether it is already sitting in `next` or
        // still being read (task1510) -- re-sending while one is in flight
        // would pile up 35MB reads for the same crossing.
        if self.next.is_some() || self.prefetch_in_flight.is_some() {
            return;
        }
        let Some(next) = prefetch_target(
            playlist_index,
            self.playlist.len(),
            self.playlist_position(self.position_100ns),
            self.playlist[playlist_index].end_100ns - self.position_100ns,
        ) else {
            return;
        };
        let Some(lease) = self.lease_segment(next) else {
            return;
        };
        let generation = self.prefetch_generation;
        // Before the borrow of `self.fetcher`, not inside the literal: this
        // builds the scaler on its first call.
        let manager = self.decoder_manager();
        let sent = self.fetcher.request(FetchRequest {
            generation,
            playlist_index: next,
            segment_index: self.playlist[next].index,
            start_100ns: self.playlist[next].start_100ns,
            lease,
            source: self.source.clone(),
            manager,
        });
        if sent {
            self.prefetch_in_flight = Some((generation, next));
        }
    }

    /// Takes whatever the fetch thread has finished (task1510).
    ///
    /// A drain rather than one `try_recv`: a seek can leave a response in the
    /// channel *and* put a second read in flight before the first is looked at,
    /// so two can be waiting. Called at the top of every tick -- early enough
    /// that a response landing between two ticks is in `self.next` before the
    /// crossing that wants it -- and again after a seek, so the reader it was
    /// carrying is retired instead of sitting on 35MB until playback resumes.
    fn take_prefetch_responses(&mut self) {
        while let Some(response) = self.fetcher.try_recv() {
            let key = (response.generation, response.playlist_index);
            let verdict = prefetch_verdict(
                key,
                self.prefetch_in_flight,
                response.result.is_ok(),
                self.next.is_some(),
                self.playlist_position(self.position_100ns),
            );
            // Whatever the verdict, the slot this response occupies is free
            // again -- including on `Discard`, or the read-ahead would never
            // restart.
            if self.prefetch_in_flight == Some(key) {
                self.prefetch_in_flight = None;
            }
            match (verdict, response.result) {
                (PrefetchVerdict::Accept, Ok((reader, first))) => {
                    self.note_fetch_success(response.segment_index);
                    self.next = Some(Prefetched {
                        playlist_index: response.playlist_index,
                        reader,
                        first,
                    });
                }
                (PrefetchVerdict::Failed, Err(error)) => {
                    self.note_fetch_failure(
                        response.segment_index,
                        "playback: segment prefetch failed",
                        error,
                    );
                }
                // Stale. The reader goes off this thread to be released, the
                // error goes nowhere at all.
                (_, Ok((reader, _))) => self.retirer.retire(Some(reader)),
                (_, Err(_)) => {}
            }
        }
    }

    /// Retires the read-ahead and disowns anything still in flight: whatever
    /// was being read belongs to a position the transport is no longer heading
    /// for (task203/task1510). The generation is what makes the disowning work
    /// without a way to cancel the read itself.
    fn abandon_prefetch(&mut self) {
        self.retirer
            .retire(self.next.take().map(|ready| ready.reader));
        self.prefetch_generation += 1;
        self.prefetch_in_flight = None;
        self.take_prefetch_responses();
    }

    /// Every flush point has to take the carry with it (task154): the samples
    /// waiting there belong to the position that is being left behind, and
    /// writing them after a seek would leak a moment of the old audio.
    fn flush_audio(&mut self) {
        // A pending realignment belongs to the seek that armed it; anything
        // that flushes afterwards has already thrown that audio away.
        self.audio_align_100ns = None;
        // And the retained blocks belong to the position being left. Clearing
        // them here is what keeps task155 airtight: only `set_rate` ever
        // replays them, and it takes them *before* flushing, at a position
        // that has not moved. A seek or a park can never reach them.
        self.audio_recent.clear();
        self.pending_out.clear();
        // The extra tracks hold audio for the position being left; mixing it
        // into the new one would smear the old moment into it, the same
        // reason the stretcher resets below (task1270).
        for track in &mut self.extra_audio {
            track.clear();
        }
        if let Some(stretcher) = &mut self.stretcher {
            // Its analysis window still holds the audio around the position
            // being left; emitting that after a seek would smear the old
            // moment into the new one (task155). A pause leaves the position
            // alone and so does not come through here.
            stretcher.reset();
        }
        if let Some(audio) = &mut self.audio {
            audio.flush();
        }
    }

    /// Keeps up to `AUDIO_BUFFER` of decoded audio queued, dropping blocks
    /// that fall behind the master clock by more than the drift limit.
    ///
    /// Track 0 -- the capture target -- drives the timeline; every other track
    /// is decoded into its own buffer and mixed into the block track 0 just
    /// produced (task1270). See `mixer` for why that direction.
    fn pump_audio(&mut self, master_100ns: i64) {
        self.pump_audio_inner(master_100ns, false);
    }

    /// Reads the audio the outgoing segment still holds, before the crossing
    /// retires its reader (task1880). Called with `self.current` still the
    /// outgoing reader and `self.position_100ns` still inside the outgoing
    /// segment, which is the only window where those blocks are reachable.
    ///
    /// `previous_end` is both the master clock and the bound: the tail block a
    /// healthy crossing leaves ends about one access unit *past* the segment's
    /// end, so `previous_end` is the one master that keeps it out of
    /// `BlockFate::Drift` while still discarding anything genuinely stale.
    fn drain_outgoing_audio(&mut self, previous_end_100ns: i64) {
        self.drain_dropped = 0;
        self.drain_dropped_100ns = 0;
        self.pump_audio_inner(previous_end_100ns, true);
    }

    /// `drain` is the crossing drain (task1880): it reads the outgoing reader
    /// dry instead of stopping at what the endpoint will take. Every place the
    /// normal pump stops because the endpoint is full is skipped, and what
    /// `write` refuses goes to the carry -- which the next pump writes before
    /// any of the incoming segment's audio, so the order survives.
    fn pump_audio_inner(&mut self, master_100ns: i64, drain: bool) {
        crate::insight_scope!("playback_pump_audio");
        let Some(playlist_index) = self.playlist_position(self.position_100ns) else {
            return;
        };
        let entry = self.playlist[playlist_index].clone();
        let Some(current) = self.current.as_mut() else {
            return;
        };
        if current.audio_streams.is_empty() {
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        let carry = &mut self.pending_out;
        let stretcher = &mut self.stretcher;
        let rate = self.rate;
        // Carry first, and nothing else until it is gone: queueing a new block
        // over a half-written one would splice the stream out of order.
        if !carry.is_empty() {
            let written = audio.write(carry);
            // Draining does not get to give up here: the endpoint being full is
            // exactly the state that loses the tail (task1860 measured
            // `hole>0` at 17.5% for `queued_ms > 190`). The blocks below append
            // to the carry behind what is already in it instead.
            if retain_unwritten(carry, written) && !drain {
                return;
            }
        }
        let tracks = current.audio_streams.len();
        while self.extra_audio.len() + 1 < tracks {
            self.extra_audio.push(mixer::TrackAudioBuffer::new());
        }
        let extra_audio = &mut self.extra_audio;
        let track_volumes = &self.track_volumes;
        let align = &mut self.audio_align_100ns;
        loop {
            // One `GetCurrentPadding` per round, read *before* the fill and
            // used for both the stop condition and the low-water mark
            // (task1840). The mark used to be taken after the write, which is
            // the one moment the endpoint is guaranteed not to be empty.
            let queued = audio.queued();
            self.audio_queued_min_ms = self.audio_queued_min_ms.min(queued.as_millis() as u32);
            if queued >= AUDIO_BUFFER && !drain {
                break;
            }
            let Some((local, mut samples)) = current.next_audio() else {
                break;
            };
            // The priming access unit (task204) is a copy of the previous
            // segment's last block, written at local 0 only so this segment's
            // decoder has the predecessor AAC overlap-adds with. Every real
            // block was shifted past it, so "before the shift" identifies it
            // exactly -- and says nothing after a seek into the middle of a
            // segment, where no priming unit is read at all.
            let target_offset = entry.audio_offsets_100ns.first().copied().unwrap_or(0);
            if local < target_offset {
                continue;
            }
            let absolute = entry.start_100ns + local - target_offset;
            let block_100ns = block_span_100ns(samples.len());
            if !accept_block(
                absolute + block_100ns,
                master_100ns,
                block_100ns,
                drain,
                align,
                &mut self.audio_realigned,
                (&mut self.drain_dropped, &mut self.drain_dropped_100ns),
                &self.shared.status,
            ) {
                continue;
            }
            MixInputs {
                current,
                entry: &entry,
                extra_audio,
                track_volumes,
                volume_scale: self.volume_scale,
                tracks,
            }
            .apply(absolute, block_100ns, &mut samples);
            // 1x never touches the stretcher -- no latency, no CPU, no change
            // to the audio that has always worked (task155).
            let block: &[f32] = if rate == 1.0 {
                &samples
            } else {
                stretcher
                    .get_or_insert_with(|| AudioStretcher::new(rate))
                    .feed(&samples)
            };
            // Offered to the endpoint only while the carry is empty: writing
            // over a carry that is still waiting would splice the stream out of
            // order. Outside a drain the carry always *is* empty here (the
            // early return above and the break below both see to it), so this
            // is the same single `write` the pump has always done.
            let written = if carry.is_empty() {
                audio.write(block)
            } else {
                0
            };
            carry_tail(carry, block, written);
            let queued = audio.queued();
            log_audio_seam(
                self.pending_seam.take(),
                absolute,
                rate,
                queued.as_millis() as u64,
                self.last_audio_end_100ns,
                self.drain_dropped,
                self.drain_dropped_100ns,
            );
            self.last_audio_end_100ns = Some(absolute + block_100ns);
            // Keep the mixed block, not the stretched output: a 1x crossing
            // has to re-render it through the *other* path (task1840). Held
            // only until the clock passes it, which bounds this by the
            // read-ahead.
            let recent = &mut self.audio_recent;
            recent.push_back((absolute, samples));
            while recent.front().is_some_and(|(start, block)| {
                start + block_span_100ns(block.len()) <= master_100ns - DRIFT_LIMIT_100NS
            }) {
                recent.pop_front();
            }
            {
                let mut status = self.shared.status.lock().unwrap();
                status.audio_queued_ms = queued.as_millis() as u32;
                // Frames the endpoint actually took, not frames offered: the
                // remainder is still waiting in the carry.
                status.audio_frames_written += written as u64;
            }
            if drain {
                if !drain_continues(absolute + block_100ns, master_100ns, AUDIO_BUFFER_100NS) {
                    tracing::debug!(
                        target: "task1880_drain",
                        previous_end = master_100ns,
                        block_end = absolute + block_100ns,
                        "crossing drain stopped at the limit"
                    );
                    break;
                }
                continue;
            }
            if !carry.is_empty() {
                break;
            }
        }
    }

    /// `notify` false is a park that repeats one the UI has already been
    /// told about. It still writes the shared status -- and still rings the
    /// UI once `PARK_RING_FLOOR` has passed, because `play()` may have
    /// published `playing = true` in between and nothing else would take it
    /// back -- but it cannot ring as fast as the caller can park. That floor
    /// is what caps the engine <-> range-repeat feedback loop: on
    /// 2026-08-14 every park rang, every ring pulled the UI round to arm the
    /// engine again, and the pair ran at ~2,400 rounds a second.
    fn park_at_live_edge(&mut self, notify: bool) {
        // Only `tick` parks, and only a playing transport ticks: reaching here
        // always means playback stopped for want of playlist, not for want of
        // the user. An `ExtendPlaylist` resumes exactly this state (task161).
        self.parked_from_play = true;
        self.parks += 1;
        self.playing = false;
        self.anchor = None;
        // Nothing ticks from here, so a frame still in flight would be published
        // by whatever happens next -- a resume, a seek -- however much later that
        // is (t260915-8ea7).
        self.drop_gpu_pending();
        // Not `live_edge_100ns` since task1530: while recording, that is inside
        // the reserved segment, and parking there would deadlock the reserve
        // against itself -- every resumed tick would refuse to cross out of a
        // segment it had just been parked into.
        self.position_100ns = self.playable_edge_100ns() - 1;
        self.flush_audio();
        let mut status = self.shared.status.lock().unwrap();
        status.at_live_edge = true;
        drop(status);
        let due = self
            .last_park_ring
            .is_none_or(|last| last.elapsed() >= PARK_RING_FLOOR);
        if notify || due {
            self.last_park_ring = Some(Instant::now());
            self.publish_status();
        } else {
            self.write_status();
        }
    }

    /// The decoded sample converted and downscaled in one GPU blit
    /// (t260913-2527), or `None` when the caller should run the CPU pair.
    ///
    /// `None` covers both "not worth it" and "not possible": no stage size yet,
    /// a stage inside t260913-11b4's near-1:1 band (where nothing is resampled
    /// and the GPU would only add a round trip), a segment that decoded to BGRA,
    /// a machine with no video device, or a blit that failed. Every one of them
    /// leaves the old path byte for byte as it was.
    ///
    /// What it keeps for [`PlaybackFrame::full`] is the NV12 the decoder handed
    /// over rather than recording-sized RGBA: the blit reads back the stage
    /// only, and converting the whole frame again just so a screenshot *could*
    /// ask for it would put back most of what this removes.
    /// The one-shot construction of the GPU scaler, hoisted out of
    /// [`Self::gpu_frame`] (t260915-e6c8). The scaler now also owns the device
    /// manager a segment has to be *opened* with, so it has to exist before the
    /// first reader does rather than before the first frame.
    fn ensure_gpu(&mut self) {
        if !self.gpu_tried {
            self.gpu_tried = true;
            self.gpu = gpu::GpuScaler::new();
        }
    }

    /// The device to open the next reader's decoder onto, or `None` on a
    /// machine with no GPU scaler -- where playback stays on exactly the
    /// software decoder it always had.
    fn decoder_manager(&mut self) -> Option<IMFDXGIDeviceManager> {
        self.ensure_gpu();
        self.gpu.as_ref().map(|gpu| gpu.manager().clone())
    }

    /// [`PlaybackShared::recycle_nv12`] minus the empty buffer the
    /// decoder-texture path carries (t260915-e6c8): a zero-length `Vec` in the
    /// spare slot is one every `take_nv12_spare` rejects, and putting it there
    /// displaces a real one.
    fn recycle_planes(&self, planes: Vec<u8>) {
        if !planes.is_empty() {
            self.shared.recycle_nv12(planes);
        }
    }

    /// The CPU pair -- `SegmentReader::convert` -- with the one thing it was
    /// missing: a name for what it costs when the sample is still a decoder
    /// texture. Then `convert` is `IMF2DBuffer2::Lock2D` pulling the frame off
    /// the GPU, the synchronous read-back measured at 12ms that t260915-e6c8
    /// exists to avoid and that `.agents/docs/spike-seek-decode.md` rejected.
    ///
    /// Named rather than refused, deliberately. Both routes that reach it need
    /// a picture and have no second way to make one: `display_target() ==
    /// None` is every process's first poster (`PlaybackShared::display_target`
    /// is still zero before any stage has been laid out, and `Worker::new`
    /// decodes one there), and `gpu::Converted::Failed` is a driver that will
    /// refuse every frame. A `dxgi_texture().is_some()` guard in front of
    /// either would drop the frame instead of slowing it down.
    ///
    /// The edge, not once per process: that startup poster takes this path on
    /// purpose, so a `Once` would be spent on the expected case and a later
    /// `Converted::Failed` -- the one worth hearing about -- would be silent.
    /// One `warn!` per *entry* into the slow path instead, which is also what
    /// says how often a session is falling into it.
    fn cpu_convert(
        &mut self,
        sample: &windows::Win32::Media::MediaFoundation::IMFSample,
        spare: Option<Vec<u8>>,
    ) -> Option<Vec<u8>> {
        let from_texture = self
            .current
            .as_ref()
            .expect("segment ensured")
            .dxgi_texture(sample)
            .is_some();
        if from_texture && !self.cpu_readback {
            tracing::warn!(
                event = "playback_cpu_readback",
                display_target = ?self.shared.display_target(),
                "a decoder texture is going back through the CPU pair; Lock2D reads it off the GPU synchronously"
            );
        }
        self.cpu_readback = from_texture;
        self.current
            .as_ref()
            .expect("segment ensured")
            .convert(sample, spare)
    }

    fn gpu_frame(
        &mut self,
        sample: &windows::Win32::Media::MediaFoundation::IMFSample,
        width: u32,
        height: u32,
        readback: gpu::Readback,
    ) -> GpuFrame {
        self.ensure_gpu();
        if self.gpu.is_none() {
            return GpuFrame::Unavailable;
        }
        let Some(target) = self.shared.display_target() else {
            return GpuFrame::Unavailable;
        };
        let texture = self
            .current
            .as_ref()
            .expect("segment ensured")
            .dxgi_texture(sample);
        // The near-1:1 band leaves the resample to skia (t260913-11b4), and for
        // a frame already in system memory that is the cheaper path. For one
        // still on the GPU it is not: the CPU route would have to pull it back
        // with `IMF2DBuffer2::Lock2D`, the synchronous readback t260915-e6c8
        // exists to avoid. So a decoder texture is blitted 1:1 instead -- at
        // scale 1.0 the resample shader is a pass-through, every tap but the
        // centre weighing zero.
        let scaled = match super::scale::scaled_dims((width, height), target) {
            Some(scaled) => scaled,
            None if texture.is_some() => (width, height),
            None => return GpuFrame::Unavailable,
        };
        let spare = self
            .shared
            .take_nv12_spare(gpu::nv12_bytes((width, height)));
        // The system-memory path reads the planes *in*. The decoder-texture
        // path gets them *out* of the blit, a tick later, paired with the
        // picture they belong to -- so in both cases what ends up in a
        // `GpuPending` is the NV12 of the frame whose picture it is handed out
        // with.
        let mut planes = match texture {
            Some(_) => spare.unwrap_or_default(),
            None => {
                crate::insight_scope!("playback_gpu_read_planes");
                let current = self.current.as_ref().expect("segment ensured");
                match current.nv12_planes(sample, spare) {
                    Some(nv12) => nv12,
                    None => return GpuFrame::Unavailable,
                }
            }
        };
        let bytes = scaled.0 as usize * scaled.1 as usize * 4;
        let mut rgba = self
            .shared
            .take_scaled_spare(bytes)
            .unwrap_or_else(|| vec![0u8; bytes]);
        let gpu = self.gpu.as_mut().expect("checked above");
        let converted = match &texture {
            Some((texture, slice)) => gpu.convert_texture_and_scale(
                (texture, *slice),
                (width, height),
                scaled,
                &mut rgba,
                &mut planes,
                readback,
            ),
            None => gpu.convert_and_scale(&planes, (width, height), scaled, &mut rgba, readback),
        };
        // Anything but `Failed` means the GPU pair is working -- it produced a
        // picture, or one is in flight -- so the next fall through to
        // [`Worker::cpu_convert`] is a *fresh* entry into the slow path and
        // earns its own line. Without this the flag latches on the startup
        // poster (the first frame of every process, before any stage exists)
        // and a later `Converted::Failed` stays silent, which is exactly the
        // `Once` behaviour `cpu_convert` says it is not. Left set on `Failed`
        // so a driver refusing every frame says so once, not per frame.
        if !matches!(converted, gpu::Converted::Failed) {
            self.cpu_readback = false;
        }
        let from_texture = texture.is_some();
        let mut arriving = GpuPending {
            nv12: if from_texture {
                Vec::new()
            } else {
                std::mem::take(&mut planes)
            },
            source: (width, height),
            scaled,
        };
        match converted {
            gpu::Converted::Failed => {
                // Both buffers go straight back, each into its own slot: the CPU
                // path is about to ask for a recording-sized RGBA one and the
                // next GPU frame for these planes again, and before t260914-862b
                // the planes landed in the RGBA slot and both asks missed.
                self.shared.recycle_scaled(rgba);
                self.recycle_planes(std::mem::take(&mut arriving.nv12));
                self.recycle_planes(planes);
                // The failure dropped the pipeline, and the cycle went with it,
                // so only the planes are left to return.
                self.recycle_gpu_pending();
                GpuFrame::Unavailable
            }
            gpu::Converted::Warming => {
                self.shared.recycle_scaled(rgba);
                // Nothing was read back, so on the decoder-texture path `planes`
                // is still the empty spare it was handed in as; on the other one
                // it was moved into `arriving` above and this is a no-op.
                self.recycle_planes(planes);
                // A pipeline with nothing to hand back is a pipeline that was
                // just built: anything still held here was blitted at the sizes
                // that rebuild replaced (AC3), so it goes back rather than out.
                // The planes only -- the cycle is holding the frame that just
                // went in, and telling it to forget that one would warm forever.
                self.recycle_gpu_pending();
                self.gpu_pending = Some(arriving);
                GpuFrame::Warming
            }
            gpu::Converted::This => {
                // `Readback::Now` emptied the cycle itself; whatever was in
                // flight was abandoned there, so its planes go back here.
                self.recycle_gpu_pending();
                if from_texture {
                    // Read back out of the slot this frame's picture came from,
                    // so these are this frame's own planes.
                    arriving.nv12 = planes;
                }
                GpuFrame::Ready(arriving.into_frame(rgba))
            }
            gpu::Converted::Previous => {
                let Some(mut previous) = self.gpu_pending.replace(arriving) else {
                    // Unreachable: the GPU only reports a previous frame for one
                    // this thread put in flight, and `drop_gpu_pending` empties
                    // both sides together. Handing the picture over with no
                    // planes to pair it with is the one outcome this task must
                    // not have, so it is dropped instead.
                    debug_assert!(false, "the GPU read back a frame nothing was pending for");
                    self.shared.recycle_scaled(rgba);
                    self.recycle_planes(planes);
                    return GpuFrame::Warming;
                };
                debug_assert_eq!(
                    previous.scaled, scaled,
                    "a live pipeline cannot change size under the frame in flight"
                );
                if from_texture {
                    // The picture and these planes came out of the same cycle
                    // slot, so they are the same frame's -- the previous one's.
                    self.recycle_planes(std::mem::replace(&mut previous.nv12, planes));
                }
                GpuFrame::Ready(previous.into_frame(rgba))
            }
        }
    }

    /// Drops the frame whose blit is in flight, on **both** sides: the planes
    /// here and the picture the GPU is holding for them.
    ///
    /// Called wherever the next picture must not be the one before it: a seek,
    /// a park, a crossing. Dropping only the planes is what the first version of
    /// this change did, and the cycle then handed the next call a picture with
    /// nothing to pair it with -- on every same-size crossing, which is every
    /// two seconds of playback.
    fn drop_gpu_pending(&mut self) {
        self.recycle_gpu_pending();
        if let Some(gpu) = self.gpu.as_mut() {
            gpu.discard_in_flight();
        }
    }

    /// The planes only, for the places inside `gpu_frame` where the GPU has
    /// already said what it did with the cycle: it must **not** be told to
    /// forget a frame it has just accepted.
    fn recycle_gpu_pending(&mut self) {
        if let Some(pending) = self.gpu_pending.take() {
            self.shared.recycle_nv12(pending.nv12);
        }
    }

    /// Reads back the frame whose blit is in flight and publishes it; says
    /// whether it did (t260915-7088).
    ///
    /// `pause()` is the one drop point that stops **at** the frame in flight:
    /// `position_100ns` already points at it, so the stage and
    /// `save_current_frame` have to settle on it rather than on the one before.
    /// A seek, a park and a crossing all move the position off it instead,
    /// which is why they call `drop_gpu_pending` and this does not.
    ///
    /// No frame is dropped either way: the planes either come out paired with
    /// the picture or go back to the spare slot, never nowhere. With nothing
    /// pending -- the CPU path, a machine with no video device, a pipeline
    /// still warming -- this does nothing at all.
    fn flush_gpu_pending(&mut self) -> bool {
        let Some(mut pending) = self.gpu_pending.take() else {
            return false;
        };
        let bytes = pending.scaled.0 as usize * pending.scaled.1 as usize * 4;
        let mut rgba = self
            .shared
            .take_scaled_spare(bytes)
            .unwrap_or_else(|| vec![0u8; bytes]);
        // Empty planes are the decoder-texture path's marker (t260915-e6c8):
        // its NV12 lives in the GPU's own staging pair, not here, so it has to
        // come back out with the picture or `full` would be published empty.
        let planes = pending.nv12.is_empty().then_some(&mut pending.nv12);
        let read = self
            .gpu
            .as_mut()
            .is_some_and(|gpu| gpu.read_in_flight(&mut rgba, planes));
        if !read {
            self.shared.recycle_scaled(rgba);
            self.recycle_planes(pending.nv12);
            return false;
        }
        self.publish_prepared(pending.into_frame(rgba));
        true
    }

    fn publish_frame(&mut self, rgba: Vec<u8>, width: u32, height: u32) {
        // Task1660: the tick's biggest cost had no scope at all. `playback_tick`
        // minus `playback_sleep` came to 8.3ms a tick while decode + swizzle +
        // audio summed to 2.9ms, and the missing 5.4ms is all in here -- the
        // stage downscale and the buffer it allocates for the result.
        crate::insight_scope!("playback_publish_frame");
        // Resampled here rather than on the UI thread (task990): whatever the
        // UI spends in `pump_playback` is time it is not drawing, which is the
        // same reason task205 moved work off it.
        let target = self.shared.display_target();
        // Task1690: the destination the last stage-sized frame was resampled
        // into, when the UI has handed it back and the stage is still the size
        // it was. Asked for only when the resize is actually going to happen,
        // so a fullscreen stage never takes a buffer it has no use for.
        let spare = target
            .and_then(|target| super::scale::scaled_dims((width, height), target))
            .and_then(|(scaled_width, scaled_height)| {
                self.shared
                    .take_scaled_spare(scaled_width as usize * scaled_height as usize * 4)
            });
        let frame = {
            crate::insight_scope!("playback_scale_stage");
            super::scale::frame_for_stage(&mut self.scaler, rgba, (width, height), target, spare)
        };
        self.publish_prepared(frame);
    }

    /// The half of `publish_frame` after the resample: hand the frame to the
    /// mailbox, take back whatever the UI never came for, ring the bell. Called
    /// on its own by the GPU path, whose frame arrives already stage-sized.
    fn publish_prepared(&mut self, frame: PlaybackFrame) {
        self.last_publish = Instant::now();
        let evicted = {
            let mut slot = self.shared.frame.lock().unwrap();
            slot.replace(frame)
        };
        // A frame the UI never took (the slot is latest-wins, and task1580's
        // stage gate turns frames away on purpose) is a buffer nobody else ever
        // saw, so it goes back into the spare slot exactly like one the UI
        // returns -- otherwise a gated run would allocate every frame after all.
        if let Some(evicted) = evicted {
            match evicted.full {
                Some((_, _, full)) => {
                    self.shared.recycle_scaled(evicted.rgba);
                    self.shared.recycle_full(full);
                }
                None => self.shared.recycle(evicted.rgba),
            }
        }
        self.publish_status();
    }

    /// Gives the three spare slots back once nothing has been published for
    /// [`SPARE_IDLE`] (t260917-efbe). Runs every loop pass; after the first
    /// release the slots are empty and this is three `try_lock`s that find
    /// nothing, so the log line is once per quiet period. A buffer the UI
    /// hands back late is simply released on the next pass.
    fn release_idle_spares(&mut self, now: Instant) {
        let idle = now.saturating_duration_since(self.last_publish);
        if idle < SPARE_IDLE {
            return;
        }
        let [spare, nv12, scaled] = self.shared.release_spares();
        let total = spare + nv12 + scaled;
        if total == 0 {
            return;
        }
        tracing::info!(
            event = "playback_spares_released",
            spare,
            nv12,
            scaled,
            total,
            idle_ms = idle.as_millis() as u64,
            engines_alive = super::live_engines(),
            "playback spares released"
        );
    }

    fn publish_status(&self) {
        self.write_status();
        (self.notify)();
    }

    /// The shared-state half of `publish_status`, without the bell.
    fn write_status(&self) {
        // The retire thread must not tear a decoder down while this one is
        // decoding on a deadline (task246): same toll, worse timing.
        self.retirer.set_busy(self.playing);
        let mut status = self.shared.status.lock().unwrap();
        status.playing = self.playing;
        status.position_100ns = self.position_100ns;
        if self.playing {
            // Live follow *plays* since task1530 instead of parking on every
            // crossing, so a flat `false` here would keep the LIVE chip dark
            // exactly while the transport is following live.
            status.at_live_edge = follows_live(
                self.playlist_position(self.position_100ns),
                self.playlist.len(),
                self.live,
            );
        }
        status.audio_enabled = self.audio.is_some();
        // How many tracks the open segment carries, for the mixer panel to
        // draw rows for (task1270). Their names come from the manifest, not
        // from here.
        status.audio_tracks = self
            .current
            .as_ref()
            .map(|reader| reader.audio_streams.len())
            .unwrap_or_default();
    }

    fn release_lease(&mut self) {
        if let Some(lease) = self.lease.take() {
            self.source.release_lease(&lease);
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.release_lease();
    }
}

/// One track's own scalar, defaulting to full volume for a track nothing has
/// set yet (task1270). The master is a separate multiplication.
fn track_scale_of(volumes: &[(i64, bool)], track: usize) -> f32 {
    let (percent, muted) = volumes.get(track).copied().unwrap_or((100, false));
    mixer::track_scale(percent, muted)
}

pub(super) fn volume_scale(percent: i64, muted: bool) -> f32 {
    if muted {
        0.0
    } else {
        (percent.clamp(0, 100) as f32) / 100.0
    }
}

/// Drops what the endpoint accepted off the front of the carry, and reports
/// whether anything is still waiting. Split out from `pump_audio` so the
/// bookkeeping is testable without a WASAPI endpoint (task154).
fn retain_unwritten(pending: &mut Vec<f32>, written_frames: usize) -> bool {
    let written = (written_frames * AUDIO_CHANNELS as usize).min(pending.len());
    pending.drain(..written);
    !pending.is_empty()
}

/// Appends the entries past the playlist's tail and reports how many landed
/// (task161). Segments finalize in ascending index order and never change once
/// finalized, so anything at or before the tail is a resend of what the engine
/// already has -- ignored rather than merged, which keeps the playlist sorted
/// by construction and `playlist_position`'s binary search valid.
fn append_new_segments(playlist: &mut Vec<PlaylistEntry>, incoming: &[TimelineSegment]) -> usize {
    debug_assert!(
        incoming
            .windows(2)
            .all(|pair| pair[0].index < pair[1].index),
        "extend segments must arrive in ascending index order"
    );
    let mut added = 0;
    for segment in incoming {
        if playlist
            .last()
            .is_some_and(|last| segment.index <= last.index)
        {
            continue;
        }
        playlist.push(PlaylistEntry {
            index: segment.index,
            start_100ns: segment.start_100ns,
            end_100ns: segment.end_100ns,
            audio_offsets_100ns: segment.audio_offsets_100ns.clone(),
        });
        added += 1;
    }
    added
}

/// Appends whatever the endpoint refused from a freshly produced block.
fn carry_tail(pending: &mut Vec<f32>, block: &[f32], written_frames: usize) {
    let written = (written_frames * AUDIO_CHANNELS as usize).min(block.len());
    pending.extend_from_slice(&block[written..]);
}

/// LiveReview playlist growth (task161). Built through `assemble` with no
/// endpoint and no priming seek, so what runs here is playlist, live edge and
/// park bookkeeping only -- no MF, no WASAPI, no COM.
#[cfg(test)]
mod extend_tests {
    use super::*;
    use crate::capture::CaptureController;
    use crate::playback::source::ControllerSource;

    fn segment(index: u64, start_s: i64, end_s: i64) -> TimelineSegment {
        TimelineSegment {
            index,
            start_100ns: start_s * HNS_PER_SECOND,
            end_100ns: end_s * HNS_PER_SECOND,
            audio_offsets_100ns: vec![0],
        }
    }

    fn worker_with(segments: Vec<TimelineSegment>) -> Worker {
        let live_edge_100ns = segments.last().map(|s| s.end_100ns).unwrap_or(0);
        Worker::assemble(
            Arc::new(ControllerSource::new(CaptureController::new())),
            TimelineSnapshot {
                audio_tracks: Vec::new(),
                session_id: "task161-extend-test".into(),
                segments,
                gaps: Vec::new(),
                live_edge_100ns,
                target_title: None,
                target_executable: None,
                target_executable_path: None,
            },
            0,
            0,
            true,
            Arc::new(PlaybackShared::default()),
            Box::new(|| {}),
            None,
            // Never napped on here: `assemble` runs no ticks (task1620).
            crossbeam_channel::unbounded().1,
        )
    }

    fn indexes(worker: &Worker) -> Vec<u64> {
        worker.playlist.iter().map(|entry| entry.index).collect()
    }

    /// Feeds one segment per extend until the parked transport resumes, and
    /// returns how many that took. The count is not a fixed number: the live
    /// reserve holds one segment back, so the lead the hysteresis waits for
    /// arrives a segment later than the arithmetic on its own suggests, and a
    /// test that hardcoded it would break on the reserve rather than on the
    /// contract it means to check.
    fn extends_until_resumed(worker: &mut Worker, first_index: u64) -> u64 {
        for sent in 1..=8 {
            let index = first_index + sent - 1;
            let start = index as i64 * 2;
            worker.extend_playlist(
                &[segment(index, start, start + 2)],
                (start + 2) * HNS_PER_SECOND,
            );
            if worker.playing {
                return sent;
            }
        }
        panic!("the park never resumed after 8 extends");
    }

    /// Task1840: run5 charged every `drift` step to a seek -- 51, 75, 54, 21,
    /// 30, 6 corrections, each matching the seek's skipped-frame span to
    /// within a block. That audio is not late, it is audio from before the
    /// target that the source handed back because it seeks to the keyframe.
    #[test]
    fn a_seek_realigns_instead_of_calling_it_drift() {
        let block = HNS_PER_SECOND / 50; // 20ms, about one AAC block
        let target = 100 * HNS_PER_SECOND;
        // A block a second before the target, with the clock already there:
        // behind the drift limit on both counts, but the seek owns it.
        assert_eq!(
            block_fate(target - HNS_PER_SECOND, target, Some(target)),
            BlockFate::Realign
        );
        // The one that ends exactly on the target holds none of it.
        assert_eq!(block_fate(target, target, Some(target)), BlockFate::Realign);
        // The one straddling it is the first audio the listener is owed.
        assert_eq!(
            block_fate(target + block, target, Some(target)),
            BlockFate::Play
        );
        // Without a seek armed, the same block is drift again -- the
        // realignment must not become a blanket amnesty.
        assert_eq!(
            block_fate(target - HNS_PER_SECOND, target, None),
            BlockFate::Drift
        );
    }

    /// The drift rule itself is unchanged: `DRIFT_LIMIT_100NS` of slack, then
    /// the block goes, because the video is the truth (task128).
    #[test]
    fn drift_still_bites_exactly_at_the_limit() {
        let master = 100 * HNS_PER_SECOND;
        assert_eq!(
            block_fate(master - DRIFT_LIMIT_100NS, master, None),
            BlockFate::Play
        );
        assert_eq!(
            block_fate(master - DRIFT_LIMIT_100NS - 1, master, None),
            BlockFate::Drift
        );
        // Ahead of the clock is always fine: that is the queue doing its job.
        assert_eq!(
            block_fate(master + HNS_PER_SECOND, master, None),
            BlockFate::Play
        );
    }

    /// Task1880: the crossing drain reads past what the endpoint will take, so
    /// the only two things that stop it are the reader running out (which is
    /// `next_audio()` returning `None`, not this rule) and this bound.
    #[test]
    fn the_crossing_drain_stops_at_the_limit() {
        let previous_end = 100 * HNS_PER_SECOND;
        // One AAC access unit.
        let unit = HNS_PER_SECOND * 1024 / 48_000;
        // The healthy case: the tail ends about one unit past the segment, far
        // inside the bound, so the drain is stopped by the reader instead.
        assert!(drain_continues(
            previous_end + unit,
            previous_end,
            AUDIO_BUFFER_100NS
        ));
        // One tick short of the bound still continues.
        assert!(drain_continues(
            previous_end + AUDIO_BUFFER_100NS - 1,
            previous_end,
            AUDIO_BUFFER_100NS
        ));
        // Exactly the bound stops -- `AUDIO_BUFFER` past the end is already as
        // much carry as the endpoint itself would hold.
        assert!(!drain_continues(
            previous_end + AUDIO_BUFFER_100NS,
            previous_end,
            AUDIO_BUFFER_100NS
        ));
        assert!(!drain_continues(
            previous_end + AUDIO_BUFFER_100NS + unit,
            previous_end,
            AUDIO_BUFFER_100NS
        ));
    }

    /// Task154: the carry holds samples the endpoint has not taken yet, and
    /// they belong to the position being left. A flush that kept them would
    /// play a moment of the old audio over the new one.
    #[test]
    fn a_flush_drops_the_carry() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.pending_out = vec![0.5; 480];
        worker.stretcher = Some(AudioStretcher::new(2.0));
        worker.audio_align_100ns = Some(HNS_PER_SECOND);
        worker.flush_audio();
        assert!(
            worker.pending_out.is_empty(),
            "the carry survived a flush: {} samples",
            worker.pending_out.len()
        );
        // Task1840: a realignment armed by an earlier seek must not survive
        // either -- once flushed there is nothing left to walk past, and a
        // stale one would excuse real drift later.
        assert_eq!(worker.audio_align_100ns, None);
    }

    /// The other side of it: a pause does not leave the position, so the carry
    /// is still the audio that comes next. Dropping it would put the segment
    /// reader that much ahead of the clock, and the segment's audio would end
    /// early by exactly the discarded amount -- silence at the next crossing.
    #[test]
    fn a_pause_keeps_the_carry() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.pending_out = vec![0.5; 480];
        worker.playing = true;
        worker.pause();
        assert_eq!(worker.pending_out.len(), 480, "the pause dropped the carry");
    }

    /// Task155: 1x bypasses the stretcher, so crossing it swaps the whole audio
    /// path and what is queued no longer belongs to it. Moving between two
    /// stretched rates keeps the analysis window, which is what makes that
    /// change glitch-free -- so it must *not* flush.
    #[test]
    fn only_crossing_1x_flushes_the_audio_path() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);

        worker.pending_out = vec![0.5; 480];
        worker.set_rate(2.0);
        assert!(worker.pending_out.is_empty(), "1x -> 2x must flush");

        worker.pending_out = vec![0.5; 480];
        worker.set_rate(0.5);
        assert_eq!(
            worker.pending_out.len(),
            480,
            "2x -> 0.5x stays on the stretcher and must keep the carry"
        );

        worker.set_rate(1.0);
        assert!(worker.pending_out.is_empty(), "0.5x -> 1x must flush");
    }

    /// Task1840: the flush above is right, but on its own it *destroys* the
    /// media the endpoint was holding -- nothing rewinds the reader, so that
    /// span is never heard and the segment's audio ends early by exactly it.
    /// `pause` documents the same failure from the same call. The crossing has
    /// to re-render those blocks through the new path instead.
    #[test]
    fn a_1x_crossing_re_renders_what_it_flushed() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        let ten_ms = 2 * (AUDIO_RATE as usize / 100);
        worker.position_100ns = HNS_PER_SECOND;
        worker.rate = 2.0;
        worker.stretcher = Some(AudioStretcher::new(2.0));
        worker.audio_recent = VecDeque::from(vec![
            // Already played: ends before the clock.
            (HNS_PER_SECOND - HNS_PER_SECOND / 100, vec![0.25; ten_ms]),
            // Still owed: starts at the clock.
            (HNS_PER_SECOND, vec![0.5; ten_ms]),
        ]);

        worker.set_rate(1.0);

        // Bypass on the far side, so the owed block comes back verbatim and
        // the heard one does not come back at all.
        assert_eq!(
            worker.pending_out,
            vec![0.5; ten_ms],
            "the 2x -> 1x crossing did not re-render exactly the audio still owed"
        );
        assert!(
            worker.audio_recent.is_empty(),
            "the retention outlived the crossing that consumed it"
        );

        // A change between two stretched rates keeps its analysis window and
        // never flushed, so there is nothing to replay.
        worker.set_rate(2.0);
        worker.pending_out.clear();
        worker.audio_recent = VecDeque::from(vec![(HNS_PER_SECOND, vec![0.5; ten_ms])]);
        worker.set_rate(4.0);
        assert_eq!(
            worker.audio_recent.len(),
            1,
            "2x -> 4x flushed something it should have left alone"
        );
    }

    /// Task203: a recording split into files must play as one. Re-anchoring at
    /// every crossing is what made the seams visible -- it silently forgave the
    /// crossing however long it had just taken, so the picture stalled and then
    /// resumed a beat late. Only a real gap restarts the clock.
    #[test]
    fn only_a_real_gap_restarts_the_master_clock() {
        let second = HNS_PER_SECOND;
        // Back-to-back segments, and the small overshoot task201 leaves
        // between an exclusive end and the next keyframe.
        assert!(keeps_anchor(2 * second, 2 * second));
        assert!(keeps_anchor(2 * second, 2 * second + second / 4));
        // A hole the timeline itself calls a gap.
        assert!(!keeps_anchor(
            2 * second,
            2 * second + crate::ring_buffer::GAP_THRESHOLD_100NS
        ));
        assert!(!keeps_anchor(2 * second, 60 * second));
    }

    /// Task1630: the anchor must sit on recorded time. A live session loaded
    /// before its first segment existed starts at 0 (the park leaves -1) while
    /// the segments it grows into carry absolute ticks since boot -- anchoring
    /// there charged ~9.6 hours to the first frame's deadline and the tick
    /// slept through every transport command.
    #[test]
    fn the_anchor_lands_on_recorded_time() {
        let second = HNS_PER_SECOND;
        let entry = |start: i64, end: i64| PlaylistEntry {
            index: 0,
            start_100ns: start,
            end_100ns: end,
            audio_offsets_100ns: vec![],
        };
        // Absolute timestamps, as a real recording has: 9.6 hours in.
        let head = 345_600 * second;
        let playlist = vec![
            entry(head, head + 2 * second),
            // A gap, then a second segment.
            entry(head + 60 * second, head + 62 * second),
        ];

        // The pathological case: the pre-segment sentinel.
        assert_eq!(anchor_position(&playlist, -1), head);
        assert_eq!(anchor_position(&playlist, 0), head);
        // Inside a segment: left alone.
        assert_eq!(anchor_position(&playlist, head + second), head + second);
        // Inside the gap: forward to the segment playback will snap to, so the
        // clock does not wait the gap out in real time.
        assert_eq!(
            anchor_position(&playlist, head + 30 * second),
            head + 60 * second
        );
        // Past the end there is nothing to snap to; the park owns that case.
        let past = head + 120 * second;
        assert_eq!(anchor_position(&playlist, past), past);
        // An empty playlist is the load's first instant, before any extend.
        assert_eq!(anchor_position(&[], 0), 0);
    }

    // The two tests below assert on `prefetch_target` rather than on
    // `worker.next` after a `prefetch_next` call. That is deliberate: this
    // harness has no segment files behind it, so `open_segment_at` fails and
    // `next` stays `None` whichever way the decision goes. The assertions they
    // replace were written against the worker and were therefore vacuous --
    // they passed with the rules deleted, which is how task740's bug sat under
    // a green test for a whole task. `playlist_position` is still exercised
    // through the real worker, since "which segment is the position in" is the
    // input the rules turn on.

    /// Task203: the prefetch only runs near a crossing, so a seek does not keep
    /// throwing the work away. The "and only if the frame budget has room for
    /// it" half went with task630 -- it was wall-time slack, which shrinks with
    /// the rate while the work does not, so at x1.5 and above it never opened;
    /// overrunning now costs a dropped frame rather than a shown-late one.
    #[test]
    fn a_prefetch_only_runs_near_a_crossing() {
        let two_segments = 2;
        let playing = Some(0);

        // Too early in the segment: nothing to gain, and a seek would only
        // throw the work away. Two seconds of a two-second segment left.
        assert_eq!(
            prefetch_target(0, two_segments, playing, 2 * HNS_PER_SECOND),
            None,
            "far from the crossing"
        );

        // Inside the lead window, which is where the work belongs.
        assert_eq!(
            prefetch_target(0, two_segments, playing, PREFETCH_LEAD_100NS),
            Some(1),
            "at the edge of the lead window"
        );
        assert_eq!(
            prefetch_target(0, two_segments, playing, PREFETCH_LEAD_100NS / 2),
            Some(1),
            "inside the lead window"
        );

        // The last segment has nothing after it to read ahead, however close
        // the crossing is.
        assert_eq!(
            prefetch_target(1, two_segments, Some(1), 0),
            None,
            "no segment follows the last one"
        );
    }

    /// The engine's resume out of a park keeps the parked position; only the
    /// user pressing play may restart from the head.
    ///
    /// The park sits on the last playable instant by construction, so it
    /// satisfies `restarts_from_head` every time -- the assertion below says so
    /// outright. Routing the resume through `play` therefore left a live-follow
    /// session one condition away from jumping to its own beginning.
    ///
    /// **This test does not distinguish the two routings**, and no test here
    /// can: the head branch is unreachable from the only sender, so both spell
    /// the same behaviour today. It pins the shape the reasoning rests on --
    /// that the park lands where the head rule would fire, and that a resume
    /// comes back at that same position with its read-ahead intact -- so that a
    /// change to either end shows up here rather than in live follow.
    #[test]
    fn an_extend_resumes_where_the_park_left_off() {
        // Two segments and recording: the reserve holds the newest back, so the
        // park lands short of the live edge.
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.live = true;
        worker.park_at_live_edge(false);
        let parked = worker.position_100ns;
        assert_eq!(parked, 2 * HNS_PER_SECOND - 1);

        // Two extends, not one: a live park waits for `LIVE_RESUME_LEAD_100NS`
        // before resuming (task1800), because resuming on one segment of lead
        // spends it before the next crossing and parks again. What task161's
        // contract still guarantees -- and what this asserts -- is that the
        // resume, when it comes, keeps the parked position instead of
        // restarting from the head.
        worker.extend_playlist(&[segment(2, 4, 6)], 6 * HNS_PER_SECOND);
        assert!(!worker.playing, "one segment of lead is not enough");
        worker.extend_playlist(&[segment(3, 6, 8)], 8 * HNS_PER_SECOND);
        assert!(worker.playing);
        assert_eq!(worker.position_100ns, parked, "the resume kept its place");

        // The case the split is for: one live segment, where `crossable_len`
        // turns the reserve off and the playable edge *is* the live edge. The
        // park sits on `live_edge - 1`, and appending a segment does not move
        // the playable edge -- the new one is what the reserve now holds. Under
        // the old routing this is the shape that restarts from the head.
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.live = true;
        assert_eq!(worker.playable_edge_100ns(), worker.live_edge_100ns);
        worker.park_at_live_edge(false);
        let parked = worker.position_100ns;
        assert_eq!(parked, 2 * HNS_PER_SECOND - 1);
        let head = worker.playlist[0].start_100ns;
        assert!(
            Worker::restarts_from_head(parked, head, worker.playable_edge_100ns()),
            "the parked position satisfies the head rule -- that is the point"
        );

        // What keeps this from firing today is the *order* inside
        // `extend_playlist`: the edge is taken up before the resume runs, and
        // the only sender's edge is the last segment's end -- so by the time
        // anything looks, the parked position is behind it. That is a property
        // of the sender, not of the transport, which is why the resume no
        // longer asks the question at all.
        let generation = worker.prefetch_generation;
        extends_until_resumed(&mut worker, 1);
        assert!(worker.playing);
        assert_eq!(worker.position_100ns, parked, "the resume kept its place");
        // A seek would have thrown the read-ahead away on its way past.
        assert_eq!(worker.prefetch_generation, generation);

        // A user-initiated play keeps the <video> rule it always had: pressing
        // play on a transport sitting off the end starts it over.
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.position_100ns = worker.live_edge_100ns - 1;
        let head = worker.playlist[0].start_100ns;
        assert!(Worker::restarts_from_head(
            worker.position_100ns,
            head,
            worker.live_edge_100ns
        ));
    }

    /// The fix: a live park resumes only once real lead exists, so the cycle
    /// above collapses to a single park per follow session (task1800).
    #[test]
    fn a_live_park_waits_for_two_segments_of_lead_before_resuming() {
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.live = true;
        worker.position_100ns = worker.playable_edge_100ns() - 1;
        worker.park_at_live_edge(false);
        assert!(!worker.playing, "caught up to the playable edge");

        // One extend is what the old resume accepted, and what the arithmetic
        // says is spent before the next crossing arrives: the transport would
        // be playing the last segment the reserve allows, and crossing out of
        // it is exactly what parks.
        worker.extend_playlist(&[segment(2, 4, 6)], 6 * HNS_PER_SECOND);
        assert!(
            !worker.playing,
            "one segment of lead is not enough to resume on"
        );
        assert!(worker.parked_from_play, "still parked, not cancelled");

        let extends = extends_until_resumed(&mut worker, 3);
        assert!(extends >= 1, "the wait ends once the lead is there");
        assert!(
            worker.playable_edge_100ns() - worker.position_100ns >= LIVE_RESUME_LEAD_100NS,
            "the resume starts with the lead it waited for"
        );

        // And it keeps running: further extends find it already playing, so
        // `parks` stays at the one stop the buffer fill cost.
        for index in 4..12u64 {
            let start = index as i64 * 2;
            worker.extend_playlist(
                &[segment(index, start, start + 2)],
                (start + 2) * HNS_PER_SECOND,
            );
        }
        assert_eq!(
            worker.parks, 1,
            "one park per follow session, not one per crossing"
        );
    }

    /// The escape hatch for the hysteresis: a recording that stops while the
    /// transport is still short of its lead sends no further extends, so
    /// nothing would ever resume it. The idle wake is what does (task1800).
    #[test]
    fn a_park_waiting_for_lead_resumes_once_the_recording_stops() {
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.live = true;
        worker.position_100ns = worker.playable_edge_100ns() - 1;
        worker.park_at_live_edge(false);
        worker.extend_playlist(&[segment(2, 4, 6)], 6 * HNS_PER_SECOND);
        assert!(!worker.playing, "still short of the lead it waits for");

        // `poll_parked_resume` asks the controller rather than trusting the
        // `live` an extend asserts. The harness's controller has no active
        // session, which is exactly the "recording stopped" answer.
        worker.poll_parked_resume();
        assert!(
            worker.playing,
            "a stopped recording leaves nothing to wait for"
        );
        assert!(
            !worker.live,
            "and the reserve is off, so the tail is playable"
        );
    }

    /// The same wake must not disturb a transport the user paused, or a
    /// deliberate pause would undo itself every 250ms.
    #[test]
    fn the_idle_wake_leaves_a_user_pause_alone() {
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.playing = false;
        worker.parked_from_play = false;
        worker.poll_parked_resume();
        assert!(!worker.playing, "a pause is not a park");
    }

    /// Task1530: while recording, the newest finalized segment is held back so
    /// the crossing into it is never the tick that first learns it exists.
    /// A finished session holds nothing back -- its last segment is the end of
    /// the recording -- and neither does a session with only one segment,
    /// where the reserve would leave nothing to play.
    #[test]
    fn recording_holds_the_newest_segment_in_reserve() {
        assert_eq!(crossable_len(3, true), 2);
        assert_eq!(crossable_len(3, false), 3);
        assert_eq!(crossable_len(1, true), 1);
        assert_eq!(crossable_len(0, true), 0);

        // The LIVE chip follows the reserve, not the park: the last segment the
        // transport may play *is* as live as it gets.
        assert!(follows_live(Some(1), 3, true));
        assert!(!follows_live(Some(0), 3, true));
        // Nothing under the position says nothing, and a finished session is
        // never "live" however close to its end the playhead sits.
        assert!(!follows_live(None, 3, true));
        assert!(!follows_live(Some(2), 3, false));
    }

    /// The case task1530 was filed for: playback has reached the end of the
    /// playlist while the session is still recording, and `extend_playlist`
    /// arrives with the next segment.
    ///
    /// Before the reserve the park sat on `live_edge - 1` -- inside the newest
    /// segment -- and the crossing out of it happened on the tick right after
    /// the growth, with no read-ahead window at all. With the reserve the park
    /// sits a segment earlier, so the segment the transport crosses into has
    /// been in the playlist the whole time and `prefetch_target` has something
    /// to say before the crossing arrives.
    #[test]
    fn a_growth_at_the_live_edge_leaves_room_to_read_ahead() {
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        // `worker_with` builds on an idle controller, so the reserve is off
        // until something says the session is recording. An `ExtendPlaylist`
        // is that something -- here, straight to the field it sets.
        worker.live = true;

        worker.park_at_live_edge(false);
        assert_eq!(
            worker.position_100ns,
            2 * HNS_PER_SECOND - 1,
            "parked at the end of segment 0, with segment 1 still in reserve"
        );

        let playing = worker.playlist_position(worker.position_100ns);
        assert_eq!(playing, Some(0));
        assert_eq!(
            prefetch_target(0, worker.playlist.len(), playing, 1),
            Some(1),
            "the segment the crossing is about to need can be read ahead"
        );

        // The growth moves the reserve along rather than releasing it: the
        // transport crosses into segment 1 and segment 2 becomes the new
        // reserve.
        worker.extend_playlist(&[segment(2, 4, 6)], 6 * HNS_PER_SECOND);
        assert_eq!(crossable_len(worker.playlist.len(), worker.live), 2);
        assert_eq!(worker.live_edge_100ns, 6 * HNS_PER_SECOND);

        // A finished session gives the reserved segment back, or its tail
        // would never play.
        worker.live = false;
        worker.park_at_live_edge(false);
        assert_eq!(worker.position_100ns, 6 * HNS_PER_SECOND - 1);
    }

    /// Task740: the position, not the caller's index, decides where playback
    /// is. This is the state a crossing leaves behind -- the position has moved
    /// into segment 1 while the caller still says 0 -- and it is the one the
    /// lead gate would otherwise wave through, since `remaining` is negative.
    #[test]
    fn a_stale_index_does_not_read_ahead_into_the_segment_already_playing() {
        let worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4), segment(2, 4, 6)]);
        let playing = worker.playlist_position(2 * HNS_PER_SECOND);
        assert_eq!(
            playing,
            Some(1),
            "the position is inside the second segment"
        );

        // Exactly what a crossing tick asks: index 0, position already in 1,
        // so `remaining` has gone negative.
        assert_eq!(
            prefetch_target(0, 3, playing, -1),
            None,
            "a crossing's stale index reads ahead into the segment already playing"
        );

        // The honest call one segment later, which must still go through.
        assert_eq!(
            prefetch_target(1, 3, playing, 0),
            Some(2),
            "the real next segment"
        );

        // Parked past the live edge: nothing is known about where playback is,
        // so this rule declines nothing.
        assert_eq!(prefetch_target(0, 3, None, 0), Some(1), "position unknown");
    }

    /// Task1510: the same reasoning one step later. `prefetch_target` decides
    /// what to ask the fetch thread for; this decides what to do with what
    /// comes back, and it is extracted for the identical reason -- no segment
    /// files behind this harness, so nothing ever travels the real path.
    #[test]
    fn only_a_response_the_transport_still_wants_is_taken() {
        let in_flight = Some((7, 3));
        let playing = Some(2);

        assert_eq!(
            prefetch_verdict((7, 3), in_flight, true, false, playing),
            PrefetchVerdict::Accept,
            "the read-ahead that was asked for, still ahead of the position"
        );
        // A seek moved the generation on: this belongs to a position the
        // transport left, whatever its index says.
        assert_eq!(
            prefetch_verdict((6, 3), in_flight, true, false, playing),
            PrefetchVerdict::Discard,
            "an older generation is nobody's"
        );
        assert_eq!(
            prefetch_verdict((7, 4), in_flight, true, false, playing),
            PrefetchVerdict::Discard,
            "a different segment than the one in flight"
        );
        assert_eq!(
            prefetch_verdict((7, 3), None, true, false, playing),
            PrefetchVerdict::Discard,
            "nothing is in flight at all"
        );
        assert_eq!(
            prefetch_verdict((7, 3), in_flight, true, true, playing),
            PrefetchVerdict::Discard,
            "the slot was filled some other way while this was in the air"
        );

        // The failure paths, which are the ones the fetch gate hears about --
        // and only when the response is still wanted.
        assert_eq!(
            prefetch_verdict((7, 3), in_flight, false, false, playing),
            PrefetchVerdict::Failed,
            "a real failure of a segment still wanted goes to the gate"
        );
        assert_eq!(
            prefetch_verdict((6, 3), in_flight, false, false, playing),
            PrefetchVerdict::Discard,
            "a stale failure must not close the gate on a segment nobody asked for"
        );
    }

    /// The read that outlived its crossing (task1510). The synchronous
    /// fallback already opened the segment, so the response names one that is
    /// current or behind -- storing it in `next` is task740's poisoned slot,
    /// and the lease-race failure that usually comes with it must not be
    /// charged to the gate.
    #[test]
    fn a_response_at_or_behind_the_playing_segment_is_never_taken() {
        let in_flight = Some((7, 3));
        assert_eq!(
            prefetch_verdict((7, 3), in_flight, true, false, Some(3)),
            PrefetchVerdict::Discard,
            "the crossing already opened this one synchronously"
        );
        assert_eq!(
            prefetch_verdict((7, 3), in_flight, true, false, Some(4)),
            PrefetchVerdict::Discard,
            "two crossings went by while this was being read"
        );
        assert_eq!(
            prefetch_verdict((7, 3), in_flight, false, false, Some(3)),
            PrefetchVerdict::Discard,
            "the benign lease race: its failure is dropped, not reported"
        );
        // Parked past the live edge says nothing about where playback is, so
        // this rule declines nothing -- same as `prefetch_target`.
        assert_eq!(
            prefetch_verdict((7, 3), in_flight, true, false, None),
            PrefetchVerdict::Accept,
            "position unknown"
        );
    }

    /// A seek disowns the read in flight so its response cannot be taken, and
    /// leaves the slot free for the next one (task1510).
    #[test]
    fn a_seek_disowns_the_read_in_flight() {
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        let generation = worker.prefetch_generation;
        worker.prefetch_in_flight = Some((generation, 1));

        worker.abandon_prefetch();

        assert_eq!(worker.prefetch_in_flight, None, "the slot is free again");
        assert!(worker.prefetch_generation > generation);
        assert_eq!(
            prefetch_verdict(
                (generation, 1),
                worker.prefetch_in_flight,
                true,
                false,
                Some(0)
            ),
            PrefetchVerdict::Discard,
            "the response of the disowned read is nobody's"
        );
    }

    #[test]
    fn extend_appends_new_segments_and_grows_the_live_edge() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.extend_playlist(&[segment(1, 2, 4), segment(2, 4, 6)], 6 * HNS_PER_SECOND);
        assert_eq!(indexes(&worker), vec![0, 1, 2]);
        assert_eq!(worker.live_edge_100ns, 6 * HNS_PER_SECOND);
    }

    #[test]
    fn extend_resumes_a_park_that_playback_fell_into() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.playing = true;
        worker.park_at_live_edge(false);
        assert!(!worker.playing, "parking stops the transport");
        assert_eq!(worker.position_100ns, 2 * HNS_PER_SECOND - 1);

        // This park is a catch-up -- the transport was playing media it had --
        // so it waits for `LIVE_RESUME_LEAD_100NS` rather than resuming onto a
        // lead it would spend before the next crossing (task1800). The part of
        // task161's contract that matters here is unchanged: the resume, when
        // it comes, picks up where the park left off.
        let extends = extends_until_resumed(&mut worker, 1);
        assert!(
            extends > 1,
            "a live park must not resume on the first extend"
        );
        assert_eq!(
            worker.position_100ns,
            2 * HNS_PER_SECOND - 1,
            "resuming picks up where the park left off, it does not rewind"
        );
    }

    #[test]
    fn extend_does_not_start_a_transport_the_user_paused() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.playing = true;
        worker.park_at_live_edge(false);
        // The user pauses while parked at the live edge: the pending resume is
        // theirs to cancel, and the next segment must not undo that.
        worker.pause();
        worker.extend_playlist(&[segment(1, 2, 4)], 4 * HNS_PER_SECOND);
        assert!(!worker.playing);
        assert_eq!(indexes(&worker), vec![0, 1]);
    }

    /// Starting a capture now opens the review on the session it just started,
    /// so a manifest with nothing finalized yet is the *normal* first state --
    /// not the rare one it was when only the hotkey could reach it.
    #[test]
    fn a_session_with_nothing_recorded_yet_takes_its_first_segment() {
        let mut worker = worker_with(Vec::new());
        assert!(indexes(&worker).is_empty());
        assert_eq!(worker.live_edge_100ns, 0);
        // `load_review` asks an empty live session to play, which parks it
        // -- and the park is what the first segment has to resume, or the
        // stage stays black for the whole recording.
        worker.playing = true;
        worker.park_at_live_edge(false);
        assert!(!worker.playing);

        worker.extend_playlist(&[segment(0, 0, 2)], 2 * HNS_PER_SECOND);
        assert_eq!(indexes(&worker), vec![0]);
        assert_eq!(worker.live_edge_100ns, 2 * HNS_PER_SECOND);
        assert!(worker.playing, "the first segment starts the playback");
    }

    /// Segment times are absolute 100ns ticks, so the 0 an empty load starts
    /// at is *before* the first segment, not at it.
    #[test]
    fn playing_from_outside_the_playlist_restarts_at_the_head() {
        let head = 7000 * HNS_PER_SECOND;
        let edge = 7002 * HNS_PER_SECOND;
        assert!(
            Worker::restarts_from_head(0, head, edge),
            "a position before the head has no segment to decode"
        );
        assert!(Worker::restarts_from_head(edge - 1, head, edge));
        assert!(!Worker::restarts_from_head(head, head, edge));
        assert!(!Worker::restarts_from_head(head + 1, head, edge));
    }

    #[test]
    fn a_repeated_or_empty_extend_changes_nothing() {
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.playing = true;
        worker.park_at_live_edge(false);

        worker.extend_playlist(&[], 4 * HNS_PER_SECOND);
        worker.extend_playlist(&[segment(1, 2, 4)], 4 * HNS_PER_SECOND);
        // A stale resend must not rewind the edge either.
        worker.extend_playlist(&[segment(0, 0, 2)], 2 * HNS_PER_SECOND);

        assert_eq!(indexes(&worker), vec![0, 1]);
        assert_eq!(worker.live_edge_100ns, 4 * HNS_PER_SECOND);
        assert!(!worker.playing, "nothing new means nothing to resume for");
    }
}

#[cfg(test)]
mod carry_tests {
    use super::*;

    #[test]
    fn a_fully_accepted_block_leaves_nothing_behind() {
        let mut pending = Vec::new();
        carry_tail(&mut pending, &[0.1, 0.2, 0.3, 0.4], 2);
        assert!(pending.is_empty());
    }

    #[test]
    fn a_partly_accepted_block_carries_the_tail_in_order() {
        let mut pending = Vec::new();
        // Four frames offered, two taken: the last two frames wait.
        carry_tail(&mut pending, &[1.0, 1.1, 2.0, 2.1, 3.0, 3.1, 4.0, 4.1], 2);
        assert_eq!(pending, vec![3.0, 3.1, 4.0, 4.1]);
    }

    #[test]
    fn draining_the_carry_consumes_from_the_front() {
        let mut pending = vec![1.0, 1.1, 2.0, 2.1, 3.0, 3.1];
        assert!(retain_unwritten(&mut pending, 1));
        assert_eq!(pending, vec![2.0, 2.1, 3.0, 3.1]);
        assert!(!retain_unwritten(&mut pending, 2));
        assert!(pending.is_empty());
    }

    #[test]
    fn an_endpoint_that_took_nothing_leaves_the_carry_intact() {
        let mut pending = vec![1.0, 1.1];
        assert!(retain_unwritten(&mut pending, 0));
        assert_eq!(pending, vec![1.0, 1.1]);
    }

    #[test]
    fn an_over_report_cannot_drain_past_the_end() {
        // Defensive: the endpoint reports frames, the carry holds samples, and
        // a mismatch must not panic on an out-of-range drain.
        let mut pending = vec![1.0, 1.1];
        assert!(!retain_unwritten(&mut pending, 99));
        assert!(pending.is_empty());

        let mut pending = Vec::new();
        carry_tail(&mut pending, &[1.0, 1.1], 99);
        assert!(pending.is_empty());
    }
}

#[cfg(test)]
mod task630_tests {
    use super::*;

    /// Task630. x2 on a machine that cannot decode twice as fast used to keep
    /// every frame and let the clock fall behind, so "x2" was neither twice as
    /// fast nor smooth. Dropping the late ones before the swizzle is what makes
    /// the clock true; the guards are what stop it from freezing the picture.
    #[test]
    fn a_late_frame_is_dropped_but_only_within_the_guards() {
        let on_time = Duration::from_millis(0);
        let barely = LATE_ENOUGH_TO_DROP - Duration::from_millis(1);
        let late = LATE_ENOUGH_TO_DROP;
        let very_late = Duration::from_millis(500);

        assert!(
            !should_drop_frame(on_time, true, 0),
            "a frame on time is shown"
        );
        assert!(
            !should_drop_frame(barely, true, 0),
            "jitter under one 60fps frame is not worth losing a picture over"
        );
        assert!(should_drop_frame(late, true, 0), "a whole frame late goes");
        assert!(should_drop_frame(very_late, true, 0));

        assert!(
            !should_drop_frame(very_late, false, 0),
            "a must-show frame -- a crossing's first, or the one a seek asked for -- is shown however late"
        );
        assert!(
            should_drop_frame(late, true, MAX_CONSECUTIVE_DROPS - 1),
            "the last drop the cap allows"
        );
        assert!(
            !should_drop_frame(very_late, true, MAX_CONSECUTIVE_DROPS),
            "at the cap one frame is shown regardless, or the picture just stops"
        );
    }
}

#[cfg(test)]
mod t260913_3842_tests {
    use super::*;

    /// 60Hz, the rate this desk's stage runs at.
    const PERIOD: Duration = Duration::from_nanos(1_000_000_000 / 60);
    /// One 120fps frame -- the recording rate task578a delivered.
    const STEP_120: Duration = Duration::from_nanos(1_000_000_000 / 120);

    /// The whole point: a 120fps recording on a 60Hz stage builds 60 frames a
    /// second instead of 120, and the ones it stops building are exactly the
    /// ones that land inside a refresh already showing a picture.
    #[test]
    fn a_frame_inside_the_displayed_refresh_is_skipped() {
        assert!(
            should_skip_frame(true, Some(STEP_120), STEP_120, Some(PERIOD)),
            "the frame between two refreshes is never seen, so it is never built"
        );
        assert!(
            !should_skip_frame(true, Some(PERIOD), STEP_120, Some(PERIOD)),
            "the frame the refresh is waiting for is shown"
        );
        assert!(
            !should_skip_frame(true, Some(PERIOD * 3), STEP_120, Some(PERIOD)),
            "a frame long past its refresh is shown"
        );
    }

    /// The reason the comparison carries `step / 2` instead of being a bare
    /// `since < period`. 100ns timestamps do not divide into a 60Hz period, so
    /// two 120fps steps land 66ns short of it; a bare compare calls that
    /// "early", skips it, shows the one after, and the visible rate is 40fps.
    #[test]
    fn a_period_the_timestamps_miss_by_rounding_is_still_shown() {
        let step = Duration::from_nanos(8_333_300);
        let since = step * 2;
        assert!(
            since < PERIOD,
            "the case only exists because the pair falls short of the period"
        );
        assert!(
            !should_skip_frame(true, Some(since), step, Some(PERIOD)),
            "66ns of rounding must not cost a frame"
        );
    }

    /// A 60fps recording on a 60Hz stage loses nothing -- including when the
    /// clock anchor puts a frame slightly under the period.
    #[test]
    fn matching_rates_skip_nothing() {
        assert!(!should_skip_frame(true, Some(PERIOD), PERIOD, Some(PERIOD)));
        assert!(
            !should_skip_frame(
                true,
                Some(PERIOD - Duration::from_millis(2)),
                PERIOD,
                Some(PERIOD)
            ),
            "a frame 2ms under the period is the nearest one to the refresh"
        );
    }

    /// The two frames a skip would be visible on: a crossing's prefetched
    /// first frame and the one a seek asked for. Both arrive with `droppable`
    /// false, which is the same guard `should_drop_frame` uses -- and since
    /// t260914-8d69 both get there the same way, through `must_show_next`.
    #[test]
    fn the_crossing_head_and_the_seek_poster_are_never_skipped() {
        assert!(
            !should_skip_frame(false, Some(Duration::ZERO), STEP_120, Some(PERIOD)),
            "a must_show_next frame is shown however early"
        );
    }

    /// No answer from the display, and the engine behaves exactly as it did
    /// before this task -- which is what every existing playback test relies
    /// on, `set_display_refresh_hz` being called only from the bin.
    #[test]
    fn without_a_period_nothing_is_skipped() {
        assert!(!should_skip_frame(
            true,
            Some(Duration::ZERO),
            STEP_120,
            None
        ));
        assert!(
            !should_skip_frame(true, None, STEP_120, Some(PERIOD)),
            "the first frame of a session has nothing to measure against"
        );
    }

    /// The display does not always answer -- `display_refresh_hz` returns 0
    /// when Windows will not say, and documents 1 as "the hardware default".
    /// A period built from one of those is wrong the *low* way, which would
    /// skip 119 frames in 120 and freeze the picture, so it is refused.
    #[test]
    fn an_implausible_refresh_rate_is_no_period_at_all() {
        let shared = PlaybackShared::default();
        assert_eq!(shared.display_period(), None, "before the UI has answered");
        shared.set_display_refresh_hz(60);
        assert_eq!(shared.display_period(), Some(PERIOD));
        shared.set_display_refresh_hz(144);
        assert_eq!(
            shared.display_period(),
            Some(Duration::from_nanos(1_000_000_000 / 144))
        );
        for refused in [0, 1, 19, 1001] {
            shared.set_display_refresh_hz(refused);
            assert_eq!(
                shared.display_period(),
                None,
                "{refused}Hz is not a believable answer from a display"
            );
        }
    }
}

/// t260913-b4a6: a precise seek clamped to the playable edge used to decode the
/// whole last GOP and drop it (task3460: `skipped` 60-111 on End). It lands on
/// the keyframe the source returns instead.
///
/// Runs a real H.264 segment through the real `do_seek`: the export fixture's
/// keyframe at 0 and delta at 1/30s, written by the production segment writer
/// and served through the review path's `memory_byte_stream`. Two frames make
/// the smallest GOP a target can sit inside, which is all either branch needs.
#[cfg(test)]
mod edge_seek_tests {
    use super::super::source::memory_byte_stream;
    use super::*;
    use windows::Win32::Media::MediaFoundation::IMFByteStream;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    pub(super) struct BytesSource(pub(super) Vec<u8>);

    impl SegmentSource for BytesSource {
        fn acquire_lease(&self, _session_id: &str, _window: Vec<u64>) -> Result<String, String> {
            Ok(String::new())
        }

        fn release_lease(&self, _lease: &str) {}

        fn byte_stream(
            &self,
            _lease: &str,
            _segment_index: u64,
            playlist_index: usize,
        ) -> Result<IMFByteStream, String> {
            memory_byte_stream(&self.0, playlist_index)
        }

        fn is_live(&self, _session_id: &str) -> bool {
            false
        }
    }

    /// The scrub pair (2026-09-15): coarse to a spot, then precise to the same
    /// spot. The precise seek decodes forward from where the coarse one left
    /// the reader instead of asking the source to seek again -- unless the
    /// frame it has to show is one the reader already handed out.
    #[test]
    fn a_precise_seek_after_a_coarse_one_resumes_from_the_frame_it_left() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let directory =
            std::env::temp_dir().join(format!("livia-scrub-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let record = crate::export::tests::fixtures::write_video_only_segment(&directory);
        let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
        let delta_100ns = record.end_100ns / 2;
        let (mut worker, shared) = worker_over(&bytes, &record, record.end_100ns);
        worker.playing = false;
        let take_frame = || shared.frame.lock().unwrap().take().is_some();

        // Coarse: the keyframe is shown, the reader sits just past it.
        worker.do_seek(delta_100ns, false);
        assert_eq!(worker.last_seek, (0, Some(record.start_100ns)));
        assert_eq!(worker.source_seeks, 1);
        assert!(take_frame());

        // Precise to the same spot: the delta frame, with no second source
        // seek and nothing dropped on the way (the keyframe was already
        // passed on the coarse seek).
        worker.do_seek(delta_100ns, true);
        assert_eq!(
            worker.last_seek,
            (0, Some(delta_100ns)),
            "the precise seek lands on its frame by decoding forward"
        );
        assert_eq!(
            worker.source_seeks, 1,
            "the source was not asked to seek again"
        );
        assert_eq!(worker.position_100ns, delta_100ns);
        assert!(take_frame());

        // Coarse again, then precise to a spot *before* the delta frame: the
        // frame to show is the keyframe the reader already handed out, so the
        // source has to seek -- resuming would show the delta frame instead.
        worker.do_seek(delta_100ns, false);
        assert_eq!(worker.source_seeks, 2);
        assert!(take_frame());
        worker.do_seek(HNS_PER_SECOND / 100, true);
        assert_eq!(
            worker.last_seek,
            (0, Some(record.start_100ns)),
            "a target the reader already passed goes back through the source"
        );
        assert_eq!(worker.source_seeks, 3);
        assert!(take_frame());

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }

    #[test]
    fn a_seek_clamped_to_the_edge_lands_on_its_keyframe_without_decoding_the_gop() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        // Held for the whole test, as the export fixtures do: the writer's own
        // runtime must not take the MF refcount to zero before the reads.
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let directory =
            std::env::temp_dir().join(format!("livia-t260913-b4a6-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let record = crate::export::tests::fixtures::write_video_only_segment(&directory);
        let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
        // The delta frame: one frame interval after the keyframe at 0.
        let delta_100ns = record.end_100ns / 2;
        // Declared one track tick (timescale 30000) short of the file, so that
        // `end - 1` sits inside the track duration even before `do_seek` moves
        // it (t260913-b4a6's original landing case). The manifest's own end --
        // the declaration a recording actually carries, which rounds onto the
        // duration and is refused unshifted -- is the second half of this test.
        let end_100ns = record.end_100ns - HNS_PER_SECOND / 30_000;

        let (mut worker, shared) = worker_over(&bytes, &record, end_100ns);
        let edge = worker.playable_edge_100ns() - 1;
        let take_frame = || shared.frame.lock().unwrap().take().is_some();

        // Control first: a precise seek into the middle of the GOP still
        // decodes forward to the exact frame, dropping the keyframe on the way.
        // Without it, a `skipped` of 0 below could be a harness that never
        // counts anything.
        worker.do_seek(delta_100ns, true);
        assert_eq!(
            worker.last_seek,
            (1, Some(delta_100ns)),
            "a mid-GOP precise seek lands on its target frame past the keyframe"
        );
        assert_eq!(worker.position_100ns, delta_100ns);
        assert!(take_frame(), "the mid-GOP seek published its frame");
        assert!(!shared.status.lock().unwrap().at_live_edge);

        // Past the end, so the target is the clamp's, the way End's
        // `live_edge - 1` arrives -- then exactly on the edge, as a scrub to it.
        for target in [end_100ns + HNS_PER_SECOND, edge] {
            worker.do_seek(target, true);
            let (skipped, shown) = worker.last_seek;
            assert_eq!(
                skipped, 0,
                "the edge seek decoded and dropped {skipped} frame(s)"
            );
            assert!(take_frame(), "the edge seek published a frame");
            assert_eq!(
                shown,
                Some(worker.position_100ns),
                "the position is the frame on screen, not the edge"
            );
            assert!(
                worker.position_100ns < edge,
                "landed on {} rather than before the edge {edge}",
                worker.position_100ns
            );
            // The keyframe the source seeks to -- the fixture has one, at 0.
            assert_eq!(worker.position_100ns, record.start_100ns);
            assert!(
                shared.status.lock().unwrap().at_live_edge,
                "an edge seek still reports the edge"
            );
            // The audio realignment follows the landing, or a playing edge seek
            // would walk past the very audio of the GOP it is about to show.
            assert_eq!(worker.audio_align_100ns, Some(worker.position_100ns));
        }

        drop(worker);

        // The shape a recording actually has: the file's own end, which is what
        // a manifest carries (`exclusive_segment_end` of the last frame).
        // `end - 1` rounds onto the track duration in the track's own ticks and
        // Media Foundation refuses it outright (measured for the production
        // writer at 60 and 120fps by
        // `a_production_segment_refuses_a_seek_to_its_own_end`), which used to
        // leave the reader wherever it stood and decode forward from there --
        // the whole landing above never ran on a real segment. `do_seek` now
        // hands the source a position 1/60s inside the edge, which is still in
        // the last GOP, so the landing is the same keyframe and the refusal
        // never happens.
        let (mut worker, shared) = worker_over(&bytes, &record, record.end_100ns);
        let edge = worker.playable_edge_100ns() - 1;
        worker.do_seek(record.start_100ns, true);
        assert_eq!(worker.last_seek, (0, Some(record.start_100ns)));
        worker.do_seek(edge, true);
        assert_ne!(
            worker.last_seek.1,
            Some(delta_100ns),
            "the edge seek published the next delta as its landing"
        );
        // `shown` is `Some` only for a frame this seek published, and `skipped`
        // 0 with the landing on the keyframe is reachable only through
        // `lands_on_keyframe`, which needs `accepted`. So this pair is also the
        // assertion that `playback: source refused seek` did not fire.
        assert_eq!(
            worker.last_seek,
            (0, Some(record.start_100ns)),
            "the edge seek must land on the keyframe the source returns"
        );
        assert_eq!(worker.position_100ns, record.start_100ns);
        assert!(
            worker.position_100ns < edge,
            "landed on {} rather than before the edge {edge}",
            worker.position_100ns
        );
        assert_eq!(worker.audio_align_100ns, Some(worker.position_100ns));
        assert!(shared.status.lock().unwrap().at_live_edge);

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// t260913-e5ce step 1: a segment written by the production
    /// `Mp4SegmentWriter` refuses a seek to `end - 1`, and accepts the same
    /// seek moved 1/60s inside.
    ///
    /// `end` here is the manifest's own (`exclusive_segment_end`, the last
    /// sample plus the nominal `10^7 / fps`), which is exactly what the clamp
    /// in `do_seek` takes `playable_edge_100ns` from. MF converts the position
    /// to the track's own ticks, where `end - 1` rounds onto the track duration
    /// and `SetCurrentPosition` refuses it -- so without the shift the whole
    /// edge-seek landing of t260913-b4a6 never runs on a real recording.
    ///
    /// Both reader states are measured because they behave differently on a
    /// refusal (b4a6): a fresh reader stays at 0 and a used one carries on from
    /// where it stood. Refused is refused either way.
    #[test]
    fn a_production_segment_refuses_a_seek_to_its_own_end() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let directory =
            std::env::temp_dir().join(format!("livia-t260913-e5ce-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        // 178_571 is one frame of the 56fps WGC actually supplies whatever the
        // configured rate is (KNOWLEDGE: `MinUpdateInterval`), so the uneven
        // rows are the pacing a real recording writes at either setting.
        for (frame_rate, delta_100ns, pacing) in [
            (60u8, 10_000_000 / 60, "nominal"),
            (60u8, 178_571, "uneven56"),
            (120u8, 10_000_000 / 120, "nominal"),
            (120u8, 178_571, "uneven56"),
        ] {
            let case = directory.join(format!("{frame_rate}-{pacing}"));
            let record = crate::export::tests::fixtures::write_video_only_segment_at(
                &case,
                frame_rate,
                delta_100ns,
            );
            let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
            let video = crate::encoder::dump_segment_track_summary(&record.path)
                .expect("track summary reads")
                .into_iter()
                .find(|track| !track.is_audio)
                .expect("the fixture has a video track");
            let edge = record.end_100ns - 1 - record.start_100ns;
            let inside = edge - HNS_PER_SECOND / 60;

            let open = || {
                super::super::segment_reader::open_segment(&bytes, 0).expect("the segment opens")
            };
            let used = || {
                let mut reader = open();
                reader.next_video_sample().expect("the keyframe reads");
                reader
            };
            let fresh_edge = open().seek_local(edge);
            let used_edge = used().seek_local(edge);
            let fresh_inside = open().seek_local(inside);
            let used_inside = used().seek_local(inside);
            println!(
                "t260913-e5ce fps={frame_rate} {pacing}: timescale={} duration_ticks={} \
                 start={} end={} end-start={} | end-1 fresh={fresh_edge} used={used_edge} \
                 | shifted({inside}) fresh={fresh_inside} used={used_inside}{}",
                video.timescale,
                video.total_sample_duration_ticks,
                record.start_100ns,
                record.end_100ns,
                record.end_100ns - record.start_100ns,
                if inside <= 0 {
                    " [shift fell before 0: this two-sample track is shorter than 1/60s, \
                     so that column is a seek to 0, not a measurement]"
                } else {
                    ""
                }
            );
            assert!(
                !fresh_edge && !used_edge,
                "fps={frame_rate} {pacing}: the source accepted a seek to the manifest's own \
                 end, so the shift this task adds is unnecessary here"
            );
            assert!(
                fresh_inside && used_inside,
                "fps={frame_rate} {pacing}: the shifted edge seek was refused too"
            );
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    pub(super) fn worker_over(
        bytes: &[u8],
        record: &crate::ring_buffer::SegmentRecord,
        end_100ns: i64,
    ) -> (Worker, Arc<PlaybackShared>) {
        let shared = Arc::new(PlaybackShared::default());
        let worker = Worker::assemble(
            Arc::new(BytesSource(bytes.to_vec())),
            TimelineSnapshot {
                audio_tracks: Vec::new(),
                session_id: "t260913-b4a6".into(),
                segments: vec![TimelineSegment {
                    index: record.index,
                    start_100ns: record.start_100ns,
                    end_100ns,
                    audio_offsets_100ns: vec![0],
                }],
                gaps: Vec::new(),
                live_edge_100ns: end_100ns,
                target_title: None,
                target_executable: None,
                target_executable_path: None,
            },
            0,
            0,
            true,
            shared.clone(),
            Box::new(|| {}),
            None,
            crossbeam_channel::unbounded().1,
        );
        (worker, shared)
    }
}

/// t260914-1627: what `tick` does once `should_skip_frame` has said yes.
///
/// t260913-3842 tested the predicate alone (six cases over
/// `should_skip_frame`); the five lines of `tick` behind it -- advance the
/// clock, count a skip, count no drop -- had no assertion at all. Those three
/// belong in one tick's result, so they are asserted together rather than
/// spread over three tests.
///
/// 120fps material on a 60Hz stage is the case 3842 was written for. The delta
/// frame is 8.333ms after the keyframe, so `since + step / 2` is 12.5ms against
/// a 16.667ms period -- 4ms of headroom either side, unlike 30fps against 20Hz
/// which comes out 1ns from the boundary. And the comparison is between
/// *deadlines*, not wake times, so the test thread's jitter cannot turn the
/// skip into a drop.
#[cfg(test)]
mod t260914_1627_tests {
    use super::edge_seek_tests::worker_over;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    /// One 120fps frame interval: also the delta frame's timestamp, since the
    /// fixture writes its two samples at 0 and `delta_100ns`.
    const DELTA_100NS: i64 = 83_333;

    fn fixture(
        slug: &str,
    ) -> (
        std::path::PathBuf,
        crate::ring_buffer::SegmentRecord,
        Vec<u8>,
    ) {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let directory =
            std::env::temp_dir().join(format!("livia-t260914-1627-{slug}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let record = crate::export::tests::fixtures::write_video_only_segment_at(
            &directory,
            120,
            DELTA_100NS,
        );
        let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
        (directory, record, bytes)
    }

    #[test]
    fn a_skipped_frame_advances_the_clock_and_counts_as_neither_shown_nor_dropped() {
        // Held for the whole test, as the fixtures do: the writer's own runtime
        // must not take the MF refcount to zero before the reads.
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let (directory, record, bytes) = fixture("skip");
        let (mut worker, shared) = worker_over(&bytes, &record, record.end_100ns);
        shared.set_display_refresh_hz(60);
        assert!(
            shared.display_period().is_some(),
            "60Hz has to land, or the skip below is measuring nothing"
        );

        // The positive control, first: `last_publish_deadline` is `None` on a
        // fresh `assemble`, so this tick cannot skip and the keyframe is shown.
        // Without it a `frames_skipped` of 0 on tick two would read as "did not
        // skip" when it actually meant "nothing ran".
        worker.tick();
        assert_eq!(
            shared.status.lock().unwrap().frames_shown,
            1,
            "the first tick has to publish, or the scaffold never got going"
        );
        assert!(shared.frame.lock().unwrap().take().is_some());
        assert_eq!(worker.position_100ns, record.start_100ns);
        assert_eq!(worker.frames_skipped, 0, "the first frame is never skipped");
        let dropped_before = worker.frames_dropped;

        worker.tick();
        assert_eq!(
            worker.position_100ns,
            record.start_100ns + DELTA_100NS,
            "a skipped frame still moves the clock, or x2 stops being x2"
        );
        assert_eq!(
            worker.frames_dropped, dropped_before,
            "a skip is not a late drop: folding it in makes a healthy run look like a failing one"
        );
        assert_eq!(worker.frames_skipped, 1);
        assert_eq!(
            shared.status.lock().unwrap().frames_shown,
            1,
            "the skipped frame was never built"
        );

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The control for the one above: the same material and the same two ticks
    /// with no refresh rate declared. `display_period()` is `None`, so nothing
    /// is skipped and both frames are shown.
    #[test]
    fn without_a_known_refresh_rate_the_same_two_ticks_show_both_frames() {
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let (directory, record, bytes) = fixture("noskip");
        let (mut worker, shared) = worker_over(&bytes, &record, record.end_100ns);
        assert!(shared.display_period().is_none());

        worker.tick();
        worker.tick();
        assert_eq!(
            worker.frames_skipped, 0,
            "no display period, nothing to skip against"
        );
        assert_eq!(
            worker.frames_dropped, 0,
            "the second frame was dropped as late, so `frames_shown` below says \
             nothing about skipping"
        );
        assert_eq!(shared.status.lock().unwrap().frames_shown, 2);
        assert_eq!(worker.position_100ns, record.start_100ns + DELTA_100NS);

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }
}

#[cfg(test)]
mod t260914_8d69_tests {
    use super::edge_seek_tests::BytesSource;
    use super::*;
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

    /// The frame a crossing carries over from the prefetch is queued the way
    /// every other frame is -- an unconverted sample -- so the tick puts it
    /// through `gpu_frame` rather than `publish_frame`'s CPU resample, which
    /// used to make it the one frame per crossing on a different scaler.
    ///
    /// Routing, not pixels, on purpose: whether `gpu_frame` answers depends on
    /// the machine's device and on the stage size, so an assert on the
    /// published bytes would read one way here and the other on the next
    /// machine and fix nothing. What is fixed here is that the crossing hands
    /// the tick the same thing a steady read does, plus the exemption that
    /// used to come free with the pre-converted variant: the seam frame is
    /// still shown however late or early it turns out to be.
    #[test]
    fn a_crossing_queues_its_head_as_an_unconverted_sample() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        }
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let directory =
            std::env::temp_dir().join(format!("livia-t260914-8d69-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let record = crate::export::tests::fixtures::write_video_only_segment(&directory);
        let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
        let span = record.end_100ns - record.start_100ns;
        // The same fixture twice, back to back, so the crossing is contiguous
        // -- `BytesSource` hands out the bytes whatever segment is asked for.
        let second_start = record.end_100ns;

        let worker_over_two = || {
            let segment = |index: u64, start: i64| TimelineSegment {
                index,
                start_100ns: start,
                end_100ns: start + span,
                audio_offsets_100ns: vec![0],
            };
            Worker::assemble(
                Arc::new(BytesSource(bytes.clone())),
                TimelineSnapshot {
                    audio_tracks: Vec::new(),
                    session_id: "t260914-8d69".into(),
                    segments: vec![
                        segment(record.index, record.start_100ns),
                        segment(record.index + 1, second_start),
                    ],
                    gaps: Vec::new(),
                    live_edge_100ns: second_start + span,
                    target_title: None,
                    target_executable: None,
                    target_executable_path: None,
                },
                0,
                0,
                true,
                Arc::new(PlaybackShared::default()),
                Box::new(|| {}),
                None,
                crossbeam_channel::unbounded().1,
            )
        };

        // Control first, and on its own worker because `must_show_next` only
        // ever goes up here: a crossing whose prefetch carried no frame arms
        // nothing and queues nothing, exactly as before. Without it the
        // assertions below would also pass if the flag were simply set on
        // every crossing.
        let mut barren = worker_over_two();
        barren.next = Some(Prefetched {
            playlist_index: 1,
            reader: super::super::segment_reader::open_segment(&bytes, 1)
                .expect("the segment opens"),
            first: None,
        });
        assert!(
            !barren.cross_to_next(0),
            "a crossing with nothing decoded ahead leaves the frame to the next tick"
        );
        assert!(barren.pending_frame.is_none());
        assert!(
            !barren.must_show_next,
            "a crossing that missed the prefetch was never protected"
        );
        drop(barren);

        let mut worker = worker_over_two();
        let mut ahead =
            super::super::segment_reader::open_segment(&bytes, 1).expect("the segment opens");
        let (local, sample) = ahead
            .next_video_sample()
            .expect("the incoming segment's first frame decodes");
        let expected = second_start + local;
        worker.next = Some(Prefetched {
            playlist_index: 1,
            reader: ahead,
            first: Some((expected, sample)),
        });

        assert!(worker.cross_to_next(0), "the crossing has a frame to show");
        assert_eq!(
            worker.crossings_prefetched, 1,
            "the assertions below say nothing unless this crossing took the prefetch"
        );
        let Some(Pending::Sample(queued, _)) = worker.pending_frame.as_ref() else {
            panic!("the crossing queued no frame");
        };
        assert_eq!(
            *queued, expected,
            "the queued sample is the incoming segment's first frame"
        );
        assert!(
            worker.must_show_next,
            "the seam frame must survive the drop and skip guards"
        );
        assert_eq!(worker.position_100ns, second_start);

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }
}

/// t260915-8ea7: the pairing the double-buffered readback can get wrong.
///
/// No device and no sample -- `GpuPending::into_frame` is deliberately the one
/// place the picture meets the planes, so the hazard is testable on any machine.
#[cfg(test)]
mod t260915_8ea7_tests {
    use super::*;

    /// The frame published carries N-1's planes, N-1's recording size and N-1's
    /// stage size -- never the frame that was handed in alongside it.
    ///
    /// A screenshot reads `full` and the stage draws `rgba`, so mixing the two
    /// is two pictures of different moments claiming to be one frame.
    #[test]
    fn the_picture_is_published_with_its_own_planes_and_sizes() {
        let previous = GpuPending {
            nv12: vec![0xAA; 16 * 8 * 3 / 2],
            source: (16, 8),
            scaled: (8, 4),
        };
        // What the *next* frame would have been, had the pairing come from the
        // call rather than from the frame in flight: a different recording size
        // and a different stage size, so either mistake is visible.
        let arriving = GpuPending {
            nv12: vec![0xBB; 32 * 16 * 3 / 2],
            source: (32, 16),
            scaled: (16, 8),
        };
        let rgba = vec![0x11; 8 * 4 * 4];
        let frame = previous.into_frame(rgba);
        assert_eq!((frame.width, frame.height), (8, 4));
        let (width, height, full) = frame.full.expect("the GPU path always carries the planes");
        assert_eq!((width, height), (16, 8));
        assert!(
            full.rgba(width, height).len() == 16 * 8 * 4,
            "the planes converted at their own size, not the arriving frame's"
        );
        // The control: the arriving frame's own pairing differs in every field,
        // so the assertions above cannot pass by coincidence.
        let other = arriving.into_frame(vec![0x22; 16 * 8 * 4]);
        assert_ne!((other.width, other.height), (frame.width, frame.height));
    }
}

#[cfg(test)]
mod t260915_7088_tests {
    use super::edge_seek_tests::BytesSource;
    use super::*;

    /// A worker with nothing to play: `pause()` touches no segment, so the
    /// playlist can be empty and no Media Foundation fixture is needed.
    pub(super) fn paused_worker(shared: Arc<PlaybackShared>) -> Worker {
        Worker::assemble(
            Arc::new(BytesSource(Vec::new())),
            TimelineSnapshot {
                audio_tracks: Vec::new(),
                session_id: "t260915-7088".into(),
                segments: Vec::new(),
                gaps: Vec::new(),
                live_edge_100ns: 0,
                target_title: None,
                target_executable: None,
                target_executable_path: None,
            },
            0,
            0,
            true,
            shared,
            Box::new(|| {}),
            None,
            crossbeam_channel::unbounded().1,
        )
    }

    /// Tightly packed NV12 of one flat luma with neutral chroma: two calls with
    /// different `luma` give two pictures that differ in every pixel.
    fn nv12_flat(source: (u32, u32), luma: u8) -> Vec<u8> {
        let mut planes = vec![luma; source.0 as usize * source.1 as usize];
        planes.resize(gpu::nv12_bytes(source), 0x80);
        planes
    }

    fn marker_frame(width: u32, height: u32, fill: u8) -> PlaybackFrame {
        PlaybackFrame {
            width,
            height,
            rgba: vec![fill; width as usize * height as usize * 4],
            full: None,
        }
    }

    /// Pausing publishes the frame whose blit is in flight, so the picture the
    /// stage freezes on -- and the PNG `save_current_frame` writes from it --
    /// is the frame `position_100ns` points at, not the one before it.
    ///
    /// Three parts in one test on purpose. The two negatives (nothing pending,
    /// and pending on a machine with no scaler) are what a machine without a
    /// video device runs, and they are also the control for the positive: an
    /// assert that the stage did not change passes for free unless something in
    /// the same test proves this path can change it.
    #[test]
    fn pausing_publishes_the_frame_in_flight() {
        // Nothing in flight: the CPU path, and every machine with no GPU.
        let shared = Arc::new(PlaybackShared::default());
        *shared.frame.lock().unwrap() = Some(marker_frame(2, 1, 9));
        let mut idle = paused_worker(shared.clone());
        assert!(idle.gpu_pending.is_none());
        idle.pause();
        {
            let slot = shared.frame.lock().unwrap();
            let held = slot.as_ref().expect("the paused stage keeps its frame");
            assert_eq!((held.width, held.height), (2, 1));
            assert_eq!(
                held.rgba,
                vec![9u8; 8],
                "with nothing in flight there is nothing to publish"
            );
        }

        // Pending planes but no scaler to read them back with: nothing is
        // published and the planes go back to the spare slot rather than
        // nowhere.
        let source = (8u32, 6u32);
        let scaled = (4u32, 3u32);
        idle.gpu_pending = Some(GpuPending {
            nv12: nv12_flat(source, 0x40),
            source,
            scaled,
        });
        idle.gpu_tried = true;
        assert!(idle.gpu.is_none(), "this half runs without a device");
        idle.pause();
        assert!(
            idle.gpu_pending.is_none(),
            "the flush takes the planes whether or not it can read a picture"
        );
        assert!(
            shared.take_nv12_spare(gpu::nv12_bytes(source)).is_some(),
            "the planes were recycled rather than dropped"
        );
        {
            let slot = shared.frame.lock().unwrap();
            let held = slot.as_ref().expect("the paused stage keeps its frame");
            assert_eq!(held.rgba, vec![9u8; 8], "no picture, no publish");
        }
        drop(idle);

        let Some(mut scaler) = gpu::GpuScaler::new() else {
            eprintln!("no D3D11 video device on this machine; the flush's GPU half is untested");
            return;
        };
        let source = (64u32, 64u32);
        let scaled = (40u32, 40u32);
        let bytes = scaled.0 as usize * scaled.1 as usize * 4;
        let (dark, light) = (nv12_flat(source, 0x30), nv12_flat(source, 0xd0));
        // What each frame looks like on its own, and the proof they differ at
        // all -- without it "the stage shows N" could not fail.
        let mut previous = vec![0u8; bytes];
        let mut in_flight = vec![0u8; bytes];
        assert_eq!(
            scaler.convert_and_scale(&dark, source, scaled, &mut previous, gpu::Readback::Now),
            gpu::Converted::This
        );
        assert_eq!(
            scaler.convert_and_scale(&light, source, scaled, &mut in_flight, gpu::Readback::Now),
            gpu::Converted::This
        );
        assert_ne!(
            previous, in_flight,
            "N-1 and N have to be different pictures"
        );

        // The state a tick leaves behind: N-1's picture handed out and on the
        // stage, N's blit submitted and its planes held.
        let mut handed_out = vec![0u8; bytes];
        assert_eq!(
            scaler.convert_and_scale(
                &dark,
                source,
                scaled,
                &mut handed_out,
                gpu::Readback::Pipelined
            ),
            gpu::Converted::Warming
        );
        assert_eq!(
            scaler.convert_and_scale(
                &light,
                source,
                scaled,
                &mut handed_out,
                gpu::Readback::Pipelined
            ),
            gpu::Converted::Previous
        );
        assert_eq!(handed_out, previous, "the stage starts out showing N-1");

        let shared = Arc::new(PlaybackShared::default());
        *shared.frame.lock().unwrap() = Some(PlaybackFrame {
            width: scaled.0,
            height: scaled.1,
            rgba: handed_out,
            full: None,
        });
        let mut worker = paused_worker(shared.clone());
        worker.gpu_tried = true;
        worker.gpu = Some(scaler);
        worker.gpu_pending = Some(GpuPending {
            nv12: light.clone(),
            source,
            scaled,
        });

        worker.pause();

        let slot = shared.frame.lock().unwrap();
        let held = slot.as_ref().expect("the pause published a frame");
        assert_eq!(
            held.rgba, in_flight,
            "the paused stage has to settle on N, the frame the position points at"
        );
        assert_ne!(held.rgba, previous, "and not on N-1, which it was showing");
        assert_eq!((held.width, held.height), scaled);
        let (full_width, full_height, _) = held
            .full
            .as_ref()
            .expect("N's picture came out paired with N's planes");
        assert_eq!(
            (*full_width, *full_height),
            source,
            "the planes published with it are N's own, at the recording's size"
        );
        assert!(worker.gpu_pending.is_none(), "nothing is left in flight");
    }
}

#[cfg(test)]
mod t260917_efbe_tests {
    use super::t260915_7088_tests::paused_worker;
    use super::*;

    const RGBA: usize = 16 * 8 * 4;
    const NV12: usize = 16 * 8 * 3 / 2;
    const SCALED: usize = 8 * 4 * 4;

    fn fill(shared: &PlaybackShared) {
        shared.recycle(vec![0; RGBA]);
        shared.recycle_nv12(vec![0; NV12]);
        shared.recycle_scaled(vec![0; SCALED]);
    }

    fn take_all(shared: &PlaybackShared) -> [Option<usize>; 3] {
        [
            shared.take_spare(RGBA).map(|spare| spare.len()),
            shared.take_nv12_spare(NV12).map(|spare| spare.len()),
            shared.take_scaled_spare(SCALED).map(|spare| spare.len()),
        ]
    }

    /// The worker gives the three spare slots back once it has published
    /// nothing for [`SPARE_IDLE`] -- and not a moment before, because playing
    /// and scrubbing live inside that window and must keep reusing them.
    ///
    /// The "not before" half is the control: an engine that released on every
    /// pass, or kept no spares at all, would pass the "released" half alone.
    #[test]
    fn a_quiet_engine_releases_its_spares_and_a_busy_one_keeps_them() {
        let shared = Arc::new(PlaybackShared::default());
        let mut worker = paused_worker(shared.clone());
        let published = worker.last_publish;

        fill(&shared);
        worker.release_idle_spares(published + SPARE_IDLE - Duration::from_millis(1));
        assert_eq!(
            take_all(&shared),
            [Some(RGBA), Some(NV12), Some(SCALED)],
            "inside the idle window every slot keeps its buffer"
        );

        fill(&shared);
        worker.release_idle_spares(published + SPARE_IDLE);
        assert_eq!(
            take_all(&shared),
            [None, None, None],
            "past the idle window every slot is empty"
        );
    }
}
