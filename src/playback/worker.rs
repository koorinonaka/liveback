//! The playback engine's worker thread: playlist bookkeeping, the master
//! clock, seek/rate handling, video pacing and audio pumping. Split from
//! `playback.rs`, which keeps the public engine API and shared state.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;

use crate::ui_state::timeline::{TimelineSegment, TimelineSnapshot, HNS_PER_SECOND};

use super::audio_out::{AudioOut, FADE_FRAMES};
use super::fetch_gate::FetchGate;
use super::fetcher::{FetchRequest, FetchResponse, FirstFrame, SegmentFetcher};
use super::gpu;
use super::mixer;
use super::retirer::ReaderRetirer;
use super::scale::FrameScaler;
use super::segment_reader::{open_segment_with, SegmentReader};
use super::source::SegmentSource;
use super::stretcher::AudioStretcher;
use super::{
    Counters, EngineParams, FullFrame, PlaybackCommand, PlaybackFrame, PlaybackShared,
    AUDIO_BUFFER, DRIFT_LIMIT_100NS, LEASE_WINDOW,
};
use crate::encoder::aac::AUDIO_CHANNELS;
use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, IMFSample};

/// Shortest interval between two UI wake-ups from a repeated park. A
/// quarter second is imperceptible on a transport that has just stopped,
/// and it puts the ring rate below the UI's own 100ms drain -- so a stuck
/// engine costs the event loop strictly less than it already spends on
/// itself, instead of the ~2,400 rounds a second of 2026-08-14.
const PARK_RING_FLOOR: Duration = Duration::from_millis(250);

/// `AUDIO_BUFFER` on the media timeline, for `drain_continues` (task1880).
const AUDIO_BUFFER_100NS: i64 = AUDIO_BUFFER.as_millis() as i64 * 10_000;

/// m, what a live follow keeps in hand beyond one segment (t261012-5b97): the
/// newest segment has to be opened, and its arrival can be late, inside it.
/// Measured parts (t261012-18e8): opening a ~12MB segment from memory and
/// decoding its first frame 114-117ms, finalize jitter ~30ms, the UI's drain
/// poll ≤100ms. Not derived from D -- none of the three scales with it, though
/// a longer segment is more bytes to open; `playback_prefetch`'s max is what
/// says if it ran short. Public because the rejoin threshold
/// (`ui_state::playback::follow_rejoin_lag_100ns()`) is built on it.
pub const LIVE_FOLLOW_MARGIN_100NS: i64 = 2_500_000;

/// How much playable media a live park waits for before resuming (task1800).
///
/// One segment (D, `encoder::segment_duration_100ns()`) plus
/// [`LIVE_FOLLOW_MARGIN_100NS`] (t261012-5b97). One segment alone is provably
/// not enough: media arrives at 1.0x and plays at 1.0x, so whatever lead a
/// resume starts with is exactly what it has at the next crossing, minus the
/// finalize latency and the ≤100ms drain poll -- a lead of exactly D parks
/// again at every crossing, the reported stutter. m is what covers that.
///
/// A resume therefore needs two arrivals after a park at the edge (one gives D,
/// short of D + m), and then runs continuously with the lead oscillating
/// between D + m and m: each segment arrives m before the transport reaches
/// its start. That is only safe because the newest segment is read from memory
/// (`capture::indexer::RecentSegments`) -- a HDD read (p99 1.8 s on one test desk) does
/// not fit in m, which is why this was 2D behind a one-segment reserve until
/// t261012-5b97.
fn live_resume_lead_100ns() -> i64 {
    crate::encoder::segment_duration_100ns() + LIVE_FOLLOW_MARGIN_100NS
}

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

/// The QPC clock in 100ns, the timebase capture stamps media with (WGC's
/// `SystemRelativeTime`), for `task128_playback`'s `wall_behind_ms`
/// (t261012-5b97). Split before scaling: `qpc * 10^7` overflows i64 after
/// about a month of uptime at a 10MHz QPC.
/// t260924-327f: how long one step of a crossing -- `refresh_live`, an inline
/// reader release, the crossing as a whole -- may take before it gets a line
/// of its own in `liveback.log`. A crossing is every 2s, so only the slow ones
/// are written; a frame is ~17ms and 200ms is a visible freeze.
pub(super) const SLOW_STEP: std::time::Duration = std::time::Duration::from_millis(200);

/// The one comparison every t260924-327f line is gated on, split out so the
/// boundary has a test.
pub(super) fn deserves_a_line(
    elapsed: std::time::Duration,
    threshold: std::time::Duration,
) -> bool {
    elapsed >= threshold
}

/// 100ns units to whole milliseconds, for log fields.
fn ms(hns: i64) -> i64 {
    hns / 10_000
}

fn qpc_now_100ns() -> i64 {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    let (mut count, mut frequency) = (0i64, 0i64);
    // SAFETY: both write one i64 through a valid pointer; they cannot fail
    // on Windows XP and later.
    unsafe {
        let _ = QueryPerformanceCounter(&mut count);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    if frequency <= 0 {
        return 0;
    }
    count / frequency * HNS_PER_SECOND + count % frequency * HNS_PER_SECOND / frequency
}

/// Which segment, if any, is worth opening ahead of the crossing (task203),
/// given the caller's index and where playback actually is.
///
/// Every reason to decline lives here rather than as early returns in
/// `prefetch_next`, because the reasons are the part worth testing and the
/// caller is not testable: the in-memory worker harness has no segment files
/// behind it, so `open_segment_at` fails regardless and asserting on
/// `worker.prefetch.next` passes whether the rules hold or not. That vacuum is what let
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
///   `self.prefetch.next` and blocks the real read-ahead until the following crossing
///   discards it as a mismatch. Hit, poisoned, miss, clean, hit -- the measured
///   rate sat at exactly 50% at every speed until task740. `None` (parked past
///   the live edge, no segment under the position) says nothing either way, so
///   it declines nothing.
/// - the window is the playing entry's own length (`lead_100ns`,
///   t261016-153c): the read-ahead starts as the transport enters a segment.
///   task203 had it at one second so a seek would rarely throw the work away,
///   but on a HDD a 25MB read alone takes about a second, so the read was
///   still in flight at the crossing 268 times in a week (10-09..15 on one test desk).
///   The length comes from the entry, not the encoder's current setting,
///   because recordings of two- and one-second segments both get played back.
///   The comparison is not vacuous: `playlist_position` snaps forward across a
///   gap, so a position still short of the entry's start leaves more than its
///   length remaining and reads nothing ahead yet. This is media time, so it
///   does not shrink with the rate -- unlike the wall-slack gate task630
///   removed, which at x1.5 and above never opened at all.
fn prefetch_target(
    playlist_index: usize,
    playlist_len: usize,
    playing: Option<usize>,
    remaining_100ns: i64,
    lead_100ns: i64,
) -> Option<usize> {
    let next = playlist_index + 1;
    if next >= playlist_len {
        return None;
    }
    if playing.is_some_and(|playing| next <= playing) {
        return None;
    }
    (remaining_100ns <= lead_100ns).then_some(next)
}

/// Whether the transport is on the newest segment of a recording -- what
/// `status.at_live_edge` says while playing.
///
/// task1530 made this the LIVE chip's meaning; t260926-cc7e's Sync Mode
/// replaced it there, and the flag is now only the mode's "arrived" input
/// (`ui_state::playback::follow_arrives`). Until t261012-5b97 the newest
/// segment was held in reserve and this asked about the one before it, which
/// a steady follow two to three segments behind never reached. With no
/// reserve and a lead of D + m, a steady follow plays between D + m and m
/// behind the edge, so it is on the newest segment -- and this is true -- for
/// about (D - m) / D of the time.
fn follows_live(playing: Option<usize>, playlist_len: usize, live: bool) -> bool {
    live && playing.is_some_and(|index| index + 1 >= playlist_len)
}

/// Where following a recording lands (t260926-cc7e): where a live park that
/// resumed at the last segment's arrival would be now. Landing on the edge
/// itself (`live_edge - 1`, the End key's old target) parks at once and sits
/// still for the lead (D + m) while it builds (task1800); landing here
/// plays straight on at the steady D + m to m behind.
///
/// The park resumes `live_resume_lead_100ns()` short of the live edge at the
/// instant a segment arrives, and has played on since. `since_growth` is
/// that instant's age as the caller saw it -- without it a landing made late
/// in a segment's cadence starts that much further back, and the lag then
/// peaks at the lead *plus* the phase (measured 6.2 s on the first build, at
/// 2 s segments and the old 3D), which runs toward the rejoin
/// (`ui_state::playback::follow_rejoin_lag_100ns()`). Capped at one segment: past
/// that the next arrival is late, not further away.
///
/// The edge is the live edge itself since t261012-5b97: the newest segment is
/// no longer held in reserve, because it is read from memory
/// (`capture::indexer::RecentSegments`). task1530 held it back so a crossing
/// would never be the first tick to learn of its segment; the lead does that
/// now -- a segment arrives m before the transport reaches it, and the
/// read-ahead (`prefetch_target`) asks for it as soon as it exists.
///
/// Public so the UI can send it as an ordinary seek and still recognise the
/// engine's echo (`owns_position`) -- the lead is written once, here. `None`
/// for a session with nothing recorded yet.
pub fn live_follow_landing_100ns(
    segments: &[TimelineSegment],
    live_edge_100ns: i64,
    since_growth: Duration,
) -> Option<i64> {
    let head = segments.first()?.start_100ns;
    let phase = i64::try_from(since_growth.as_nanos() / 100)
        .unwrap_or(i64::MAX)
        .min(crate::encoder::segment_duration_100ns());
    Some((live_edge_100ns - live_resume_lead_100ns() + phase).max(head))
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
/// through the real path and an assertion on `worker.prefetch.next` would pass with
/// these rules deleted.
///
/// - `in_flight` mismatch covers both staleness axes at once. A seek moves the
///   generation on and clears the slot, so a response from before it can never
///   equal what is in flight -- not even when a *new* read-ahead for the same
///   playlist index is already on its way.
/// - `slot_filled`: `self.prefetch.next` was filled some other way (a synchronous
///   `ensure_segment` handed its reader over) while this was in the air.
/// - at or behind `playing`: the same rule `prefetch_target` applies on the way
///   out, applied again on the way back, because a read that outlives a
///   crossing returns naming a segment that is now current or already past.
///   Storing that in `self.prefetch.next` is exactly task740's poisoned slot. It is also
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
/// A crossing whose audio is still to be logged (task640): where the outgoing
/// segment's media stopped and the incoming one's starts, filled in by the
/// next block `pump_audio` emits so both halves are one line.
struct Seam {
    index: u64,
    previous_end: i64,
    next_start: i64,
    prefetched: bool,
}

fn log_audio_seam(
    seam: Option<Seam>,
    absolute: i64,
    rate: f64,
    queued_ms: u64,
    last_audio_end_100ns: Option<i64>,
    drain_dropped: u32,
    drain_dropped_100ns: i64,
) {
    let Some(Seam {
        index,
        previous_end,
        next_start,
        prefetched,
    }) = seam
    else {
        return;
    };
    tracing::info!(
        target: "task640_audio_seam",
        index,
        rate,
        prefetched,
        previous_end,
        next_start,
        last_audio_end = ?last_audio_end_100ns,
        first_audio_start = absolute,
        hole_ms = last_audio_end_100ns.map(|end| ms(absolute - end)),
        tail_ms = last_audio_end_100ns.map(|end| ms(previous_end - end)),
        queued_ms = queued_ms,
        // Task1900: the half `hole_ms` cannot see any more. The
        // drain reads the outgoing tail out either way, so a
        // crossing that stranded 700ms and one that stranded
        // nothing both report `hole_ms = 0`; these two say how much
        // of what it read was already too old to play.
        drain_dropped = drain_dropped,
        drain_dropped_ms = ms(drain_dropped_100ns),
        "audio across a segment crossing"
    );
}

/// The audio half of the worker (t261025-6699): the endpoint, and everything a
/// block passes through between the reader and it. Its own struct so a pump
/// can hold `&mut` to it next to `&mut` to the reader, which is what the
/// borrow-by-borrow split of `pump_audio_inner` used to stand in for.
pub struct AudioPump {
    audio: Option<AudioOut>,
    /// Per-track level and mute, in track order (task1270). Session-lived:
    /// nothing writes it to disk, so a recording opens sounding the way it was
    /// recorded. A track nothing has set is full volume, unmuted.
    track_volumes: Vec<(i64, bool)>,
    /// Decoded audio for tracks 1.. waiting for the target track's block that
    /// covers it. Index 0 here is track 1.
    extra_audio: Vec<mixer::TrackAudioBuffer>,
    /// Absolute end of the last audio block handed to the endpoint, and the
    /// crossing waiting to be reported against it (task640): the outgoing
    /// segment's declared end, the incoming one's start, and whether that tick
    /// got to pump audio at all.
    last_audio_end_100ns: Option<i64>,
    pending_seam: Option<Seam>,
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
    /// Blocks the endpoint has already been given, still recent enough that
    /// the clock has not passed them (task1840). Each is its tracks unscaled,
    /// track 0 first (see `AudioPump::gather`), so a track level change can
    /// re-mix them.
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
    /// the drift slack, so ~326KB per track at 4x.
    audio_recent: VecDeque<(i64, Vec<Vec<f32>>)>,
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
}

impl AudioPump {
    fn new() -> Self {
        Self {
            audio: None,
            track_volumes: Vec::new(),
            extra_audio: Vec::new(),
            last_audio_end_100ns: None,
            pending_seam: None,
            drain_dropped: 0,
            drain_dropped_100ns: 0,
            audio_queued_min_ms: u32::MAX,
            audio_align_100ns: None,
            audio_realigned: 0,
            audio_recent: VecDeque::new(),
            pending_out: Vec::new(),
            stretcher: None,
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
    fn accept_block(
        &mut self,
        absolute_end_100ns: i64,
        master_100ns: i64,
        block_100ns: i64,
        drain: bool,
        counters: &mut Counters,
    ) -> bool {
        match block_fate(absolute_end_100ns, master_100ns, self.audio_align_100ns) {
            BlockFate::Realign => {
                // Audio from before the seek target. The seek already
                // flushed the endpoint, so nothing of it was ever heard;
                // walking past it is the reader arriving, not the clock
                // running away (task1840).
                self.audio_realigned += 1;
                return false;
            }
            BlockFate::Drift => {
                // Behind the clock: drop and count, the video is the truth.
                self.audio_align_100ns = None;
                if drain {
                    // Counted apart from `drift_corrections`, which other
                    // things move too: on the crossing path this is
                    // material the outgoing segment held and nobody heard
                    // (task1900).
                    self.drain_dropped += 1;
                    self.drain_dropped_100ns += block_100ns;
                }
                counters.drift_corrections += 1;
                return false;
            }
            BlockFate::Play => self.audio_align_100ns = None,
        }
        true
    }

    /// Track 0's block followed by every other track's share of the same span,
    /// all unscaled: `mix_tracks` applies the levels, so a level change can
    /// re-mix a block the endpoint already holds. The extra tracks are decoded
    /// out of `current` and dated by `entry` (task1270).
    fn gather(
        &mut self,
        current: &mut SegmentReader,
        entry: &TimelineSegment,
        absolute: i64,
        block_100ns: i64,
        samples: Vec<f32>,
    ) -> Vec<Vec<f32>> {
        let len = samples.len();
        let mut tracks = vec![samples];
        // The extra tracks, lined up under this block (task1270). Each is
        // decoded up to the end of the block first, so a track whose
        // decoder runs a little behind still lands where it belongs
        // rather than a block late.
        let block_end = absolute + block_100ns;
        for track in 1..current.audio_streams.len() {
            let buffer = &mut self.extra_audio[track - 1];
            while buffer.end_100ns().is_none_or(|end| end < block_end) {
                let Some((local, block)) = current.next_audio_on(track) else {
                    break;
                };
                let offset = entry.audio_offsets_100ns.get(track).copied().unwrap_or(0);
                if local < offset {
                    // This track's priming unit (task204), same rule
                    // as track 0's above.
                    continue;
                }
                buffer.push(entry.start_100ns + local - offset, &block);
            }
            let mut share = vec![0.0; len];
            buffer.mix_into(absolute, &mut share, 1.0);
            tracks.push(share);
        }
        tracks
    }

    /// `Worker::pump_audio_inner`'s loop: `current` is the reader the playhead
    /// is in and `entry` its playlist entry.
    fn pump(
        &mut self,
        current: &mut SegmentReader,
        entry: &TimelineSegment,
        rate: f64,
        master_100ns: i64,
        drain: bool,
        counters: &mut Counters,
    ) {
        if current.audio_streams.is_empty() {
            return;
        }
        let Some(audio) = self.audio.as_mut() else {
            return;
        };
        // Carry first, and nothing else until it is gone: queueing a new block
        // over a half-written one would splice the stream out of order.
        if !self.pending_out.is_empty() {
            let written = audio.write(&self.pending_out);
            // Draining does not get to give up here: the endpoint being full is
            // exactly the state that loses the tail (task1860 measured
            // `hole>0` at 17.5% for `queued_ms > 190`). The blocks below append
            // to the carry behind what is already in it instead.
            if retain_unwritten(&mut self.pending_out, written) && !drain {
                return;
            }
        }
        let tracks = current.audio_streams.len();
        while self.extra_audio.len() + 1 < tracks {
            self.extra_audio.push(mixer::TrackAudioBuffer::new());
        }
        loop {
            // One `GetCurrentPadding` per round, read *before* the fill and
            // used for both the stop condition and the low-water mark
            // (task1840). The mark used to be taken after the write, which is
            // the one moment the endpoint is guaranteed not to be empty.
            let Some(audio) = self.audio.as_ref() else {
                return;
            };
            let queued = audio.queued();
            self.audio_queued_min_ms = self.audio_queued_min_ms.min(queued.as_millis() as u32);
            if queued >= AUDIO_BUFFER && !drain {
                break;
            }
            let Some((local, samples)) = current.next_audio() else {
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
            let block_100ns = mixer::samples_to_100ns(samples.len());
            if !self.accept_block(
                absolute + block_100ns,
                master_100ns,
                block_100ns,
                drain,
                counters,
            ) {
                continue;
            }
            let unscaled = self.gather(current, entry, absolute, block_100ns, samples);
            let samples = mix_tracks(&self.track_volumes, &unscaled);
            // 1x never touches the stretcher -- no latency, no CPU, no change
            // to the audio that has always worked (task155).
            let block: &[f32] = if rate == 1.0 {
                &samples
            } else {
                self.stretcher
                    .get_or_insert_with(|| AudioStretcher::new(rate))
                    .feed(&samples)
            };
            let Some(audio) = self.audio.as_mut() else {
                return;
            };
            // Offered to the endpoint only while the carry is empty: writing
            // over a carry that is still waiting would splice the stream out of
            // order. Outside a drain the carry always *is* empty here (the
            // early return above and the break below both see to it), so this
            // is the same single `write` the pump has always done.
            let written = if self.pending_out.is_empty() {
                audio.write(block)
            } else {
                0
            };
            carry_tail(&mut self.pending_out, block, written);
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
            // Keep the unscaled tracks, not the stretched output: a 1x
            // crossing has to re-render them through the *other* path
            // (task1840), and a track level change has to re-mix them. Held
            // only until the clock passes them, which bounds this by the
            // read-ahead.
            let recent = &mut self.audio_recent;
            recent.push_back((absolute, unscaled));
            while recent.front().is_some_and(|(start, tracks)| {
                start + mixer::samples_to_100ns(tracks[0].len()) <= master_100ns - DRIFT_LIMIT_100NS
            }) {
                recent.pop_front();
            }
            counters.audio_queued_ms = queued.as_millis() as u32;
            // Frames the endpoint actually took, not frames offered: the
            // remainder is still waiting in the carry.
            counters.audio_frames_written += written as u64;
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
            if !self.pending_out.is_empty() {
                break;
            }
        }
    }
}

/// Applies each track's own level and sums them into the block the endpoint
/// gets. Every track is scaled first, and the *sum* is what gets clipped.
/// Applied to the input rather than the stretched output because a scalar gain
/// passes through analysis/synthesis unchanged, which keeps the 1x bypass and
/// the stretched path identical (task155). The master is not here:
/// `AudioOut::set_volume` applies it at the endpoint.
fn mix_tracks(volumes: &[(i64, bool)], tracks: &[Vec<f32>]) -> Vec<f32> {
    let scale = track_scale_of(volumes, 0);
    let mut out: Vec<f32> = tracks[0].iter().map(|sample| sample * scale).collect();
    if tracks.len() > 1 {
        for (track, share) in tracks.iter().enumerate().skip(1) {
            let scale = track_scale_of(volumes, track);
            for (sample, add) in out.iter_mut().zip(share) {
                *sample += add * scale;
            }
        }
        // ponytail: the master now scales after this clamp, so a loud
        // multi-track sum at a low master clips where it used to fit.
        // Clamp to ±1/master if that is ever heard.
        mixer::clip(&mut out);
    }
    out
}

/// A frame decoded and waiting for its deadline (task630).
///
/// One shape on purpose (t260914-8d69): a crossing's prefetched first frame
/// used to arrive here already swizzled, which put that one frame per crossing
/// on the CPU resample while every other frame went through `gpu_frame`.
/// Whether a frame may be thrown away for being late is `must_show_next`'s
/// business, not the queue's.
///
/// Decoded, not yet swizzled: the costly half is still unpaid, so it can be
/// thrown away if it turns out to be late.
struct Pending {
    absolute: i64,
    sample: IMFSample,
}

/// The GPU scaler, or what is known about not having one: `Untried` until
/// the first reader or frame asks, `Absent` once this machine has said it
/// will not give one.
// One per engine, built once and never moved on a hot path: the same size as
// the `Option<GpuScaler>` it replaced, so boxing would only add an allocation.
#[allow(clippy::large_enum_variant)]
enum GpuSlot {
    Untried,
    Absent,
    Ready(gpu::GpuScaler),
}

impl GpuSlot {
    fn get(&self) -> Option<&gpu::GpuScaler> {
        match self {
            GpuSlot::Ready(gpu) => Some(gpu),
            _ => None,
        }
    }

    fn get_mut(&mut self) -> Option<&mut gpu::GpuScaler> {
        match self {
            GpuSlot::Ready(gpu) => Some(gpu),
            _ => None,
        }
    }

    /// The scaler, leaving `Absent` behind.
    fn take(&mut self) -> Option<gpu::GpuScaler> {
        match std::mem::replace(self, GpuSlot::Absent) {
            GpuSlot::Ready(gpu) => Some(gpu),
            _ => None,
        }
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
    /// The GPU blit is in flight and its picture is the next tick's
    /// (t260915-8ea7): nothing to publish yet.
    Warming,
}

/// One seek as `do_seek`'s stages learn it, in the order they do, for the
/// `task128_seek` line the last stage writes.
struct Seek {
    precise: bool,
    /// Clamped into the playlist, then onto the target segment's start.
    target: i64,
    started: Instant,
    pre_ms: u64,
    prefetch_in_flight: bool,
    in_flight_index: Option<usize>,
    /// Whether the read-ahead is the target's own segment and so is kept.
    reuse: bool,
    playlist_index: usize,
    waited_prefetch_ms: u64,
    reused_prefetch: bool,
    edge_seek: bool,
    /// The position handed to the source, inside the segment.
    local_target: i64,
    cut: bool,
    ensured_ms: u64,
    resume: bool,
    skipped: u32,
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
    /// `gpu` is the same picture when it was left on the GPU (t260917-66c2),
    /// and `rgba` is then empty.
    fn into_frame(self, rgba: Vec<u8>, gpu: Option<super::SharedPicture>) -> PlaybackFrame {
        PlaybackFrame {
            width: self.scaled.0,
            height: self.scaled.1,
            rgba,
            gpu,
            // Empty planes are the decoder-texture path playing (t261016-97b7):
            // its NV12 stayed in the GPU's staging slot, and only the size goes
            // out -- the stage aspect still needs it every frame.
            full: Some((
                self.source.0,
                self.source.1,
                if self.nv12.is_empty() {
                    FullFrame::Unread
                } else {
                    FullFrame::nv12(self.nv12)
                },
            )),
        }
    }
}

/// The size `gpu_frame` blits a frame to, or `None` for the CPU pair.
///
/// The near-1:1 band leaves the resample to skia (t260913-11b4), and for a
/// frame already in system memory that is the cheaper path. So is having no
/// target yet -- the process's first poster, decoded before any stage has been
/// laid out. For a frame still on the GPU neither is: the CPU route would have
/// to pull it back with `IMF2DBuffer2::Lock2D`, the synchronous readback
/// t260915-e6c8 exists to avoid. So a decoder texture is blitted 1:1 in both
/// cases (t260917-d792) -- at scale 1.0 the resample shader is a pass-through,
/// every tap but the centre weighing zero.
fn blit_dims(source: (u32, u32), target: Option<(u32, u32)>, texture: bool) -> Option<(u32, u32)> {
    match target.and_then(|target| super::scale::scaled_dims(source, target)) {
        Some(scaled) => Some(scaled),
        None if texture => Some(source),
        None => None,
    }
}

/// [`Worker::cpu_convert`]'s half of the `cpu_readback` edge: records whether
/// this conversion is a decoder texture going back through `Lock2D`, and
/// returns whether that is a fresh entry into the slow path (warn now).
fn readback_edge(latched: &mut bool, from_texture: bool) -> bool {
    let entering = from_texture && !*latched;
    *latched = from_texture;
    entering
}

/// `gpu_frame`'s half of the `cpu_readback` edge. Anything but `Failed` means
/// the GPU pair is working -- it produced a picture, or one is in flight -- so
/// the next fall through to [`Worker::cpu_convert`] is a *fresh* entry into
/// the slow path and earns its own line. Left set on `Failed` so a driver
/// refusing every frame says so once, not per frame.
fn clear_readback_edge(latched: &mut bool, converted: &gpu::Converted) {
    if !matches!(converted, gpu::Converted::Failed) {
        *latched = false;
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
fn playlist_position(playlist: &[TimelineSegment], time_100ns: i64) -> Option<usize> {
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
fn anchor_position(playlist: &[TimelineSegment], position_100ns: i64) -> i64 {
    match playlist_position(playlist, position_100ns) {
        Some(index) => position_100ns.max(playlist[index].start_100ns),
        None => position_100ns,
    }
}

/// How long a mixed block of interleaved stereo samples lasts.
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

/// The picture half of the worker (t261025-6699): the CPU scaler, the GPU
/// one, and the frame in flight between them.
struct GpuStage {
    /// Resamples each frame to the size the stage draws it at (task990). Kept
    /// for the session so its convolution buffers are grown once, not per
    /// frame.
    scaler: FrameScaler,
    /// The GPU half of the same job (t260913-2527): NV12 -> RGBA and the
    /// downscale in one `VideoProcessorBlt`, with `scaler` above still there as
    /// the fallback. Built on the first frame that would use it -- a session
    /// played at or above the recording's size never wants one, and building it
    /// stands up a D3D11 device.
    gpu: GpuSlot,
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
}

/// The read-ahead (t261025-6699): the segment after the playhead, the thread
/// that reads it, and which request is the live one.
struct Prefetch {
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
    generation: u64,
    /// The one read-ahead allowed in the air, as (generation, playlist index).
    /// One at a time: a second would only ever be for a segment two crossings
    /// away, and would keep another 35MB pinned to prove it.
    in_flight: Option<(u64, usize)>,
}
pub(super) struct Worker {
    /// Where the container bytes come from: the ring buffer behind a lease, or
    /// one exported clip on disk (task3500). The `Worker` knows nothing else
    /// about either.
    source: Arc<dyn SegmentSource>,
    session_id: String,
    playlist: Vec<TimelineSegment>,
    live_edge_100ns: i64,
    /// Whether the session under the transport is the one still recording
    /// (task1530), which is what makes a park wait for the resume lead
    /// (task1800; the one-segment reserve it armed went in t261012-5b97). Cached rather
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
    /// The CPU and GPU scalers and the frame in flight (t261025-6699).
    gpu_stage: GpuStage,
    /// The read-ahead (t261025-6699).
    prefetch: Prefetch,
    /// Readers opened from a seek target's GOP rather than whole
    /// (t261017-3f94); `task128_seek`'s `cut` is whether a seek moved it.
    cut_reads: u64,
    lease: Option<String>,
    /// The endpoint and the blocks on their way to it (t261025-6699).
    audio_pump: AudioPump,
    /// What the 1Hz playback line and the measurements count (t261025-6699).
    counters: Counters,

    playing: bool,
    position_100ns: i64,
    rate: f64,
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
    /// A live cold start is showing its still and waiting for the lead
    /// (`cold_start`). Cleared by whatever ends the park: a resume, a pause, a
    /// seek.
    cold_start_still: bool,

    fps_window: Instant,
    fps_frames: u32,
    /// Disconnects when the engine is dropped (task1620). Nothing is ever sent
    /// on it -- it exists so `nap` can be woken, see the comment there.
    stop_rx: Receiver<()>,
    /// When `publish_prepared` last handed the UI a frame (t260917-efbe):
    /// the clock `release_idle_spares` reads to decide the engine has gone
    /// quiet and its spare buffers are only holding memory.
    last_publish: Instant,
    /// The bar the t260924-327f crossing and `refresh_live` lines are written
    /// from: `SLOW_STEP`, or zero in the test that proves they are emitted.
    stall_threshold: Duration,
    /// When the last tick of an uninterrupted playing stretch ended
    /// (t260924-327f). `None` while stopped, so a pause is never read as a
    /// gap between ticks.
    last_tick_end: Option<Instant>,
    /// Why and when `playing` last went false, for the line its resume writes.
    stopped: Option<(Instant, &'static str)>,
    /// `FadeOutAndShutdown` in progress (t260926-2144): when it started, and
    /// the hard deadline past which the worker shuts down regardless.
    fading: Option<(Instant, Instant)>,
    /// The t260924-327f stages this worker wrote a line for, in order.
    #[cfg(test)]
    stall_lines: Vec<&'static str>,
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
const SPARE_IDLE: Duration = Duration::from_secs(2);

/// How long a playing transport may go between the end of one tick and the
/// start of the next before `run` says so (t260924-327f). Nothing between two
/// ticks is meant to take longer than a command or two, and 2s is a segment.
const TICK_GAP: Duration = Duration::from_secs(2);

/// How long a `FadeOutAndShutdown` may take in all (t260926-2144): the 80ms
/// ramp behind up to twice `AUDIO_BUFFER` already in the endpoint, with room
/// to spare. Past it the worker shuts down anyway.
const FADE_DEADLINE: Duration = Duration::from_secs(1);

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

impl Worker {
    pub(super) fn new(
        source: Arc<dyn SegmentSource>,
        snapshot: TimelineSnapshot,
        params: EngineParams,
        shared: Arc<PlaybackShared>,
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
        if let Some(audio) = &audio {
            audio.set_volume(mixer::track_scale(params.volume_percent, params.muted));
        }
        let initial_position_100ns = params.initial_position_100ns;
        let mut worker = Self::assemble(source, snapshot, params, shared, stop_rx);
        worker.audio_pump.audio = audio;
        worker.publish_status();
        // Show the first frame immediately, like the <video> element's poster.
        worker.do_seek(initial_position_100ns, true);
        worker
    }

    /// The bookkeeping half of `new`, with no endpoint. Split out so
    /// unit tests can drive playlist/park logic without WASAPI: a test thread
    /// has no COM apartment of its own, and an `IAudioClient` released on one
    /// is an intermittent access violation in the test binary.
    fn assemble(
        source: Arc<dyn SegmentSource>,
        snapshot: TimelineSnapshot,
        params: EngineParams,
        shared: Arc<PlaybackShared>,
        stop_rx: Receiver<()>,
    ) -> Self {
        // The volume is the endpoint's, which `new` sets; the stage size went
        // to `shared` before this thread existed (`start_with`).
        let EngineParams {
            initial_position_100ns,
            notify,
            ..
        } = params;
        let playlist = snapshot.segments.clone();
        Self {
            source,
            session_id: snapshot.session_id.clone(),
            playlist,
            live_edge_100ns: snapshot.live_edge_100ns,
            // False until something asks: a session that is recording says so
            // on its first `ExtendPlaylist`, ~100ms away, and until then a park
            // only resumes without waiting for the lead.
            live: false,
            shared,
            notify,
            current: None,
            retirer: ReaderRetirer::spawn(),
            gpu_stage: GpuStage {
                scaler: FrameScaler::default(),
                gpu: GpuSlot::Untried,
                gpu_pending: None,
                cpu_readback: false,
            },
            prefetch: Prefetch {
                next: None,
                fetcher: SegmentFetcher::spawn(),
                generation: 0,
                in_flight: None,
            },
            cut_reads: 0,
            lease: None,
            audio_pump: AudioPump::new(),
            playing: false,
            position_100ns: initial_position_100ns,
            rate: 1.0,
            anchor: None,
            pending_frame: None,
            consecutive_drops: 0,
            must_show_next: false,
            last_publish_deadline: None,
            counters: Counters::default(),
            fetch_gate: FetchGate::default(),
            report_park: true,
            last_park_ring: None,
            parked_from_play: false,
            cold_start_still: false,
            fps_window: Instant::now(),
            fps_frames: 0,
            stop_rx,
            last_publish: Instant::now(),
            stall_threshold: SLOW_STEP,
            last_tick_end: None,
            stopped: None,
            fading: None,
            #[cfg(test)]
            stall_lines: Vec::new(),
            #[cfg(test)]
            last_seek: (0, None),
            #[cfg(test)]
            source_seeks: 0,
        }
    }

    pub(super) fn run(&mut self, rx: Receiver<PlaybackCommand>) {
        loop {
            // At the top, so the idle branch's `continue` cannot skip it.
            if self.fade_finished() {
                self.release_lease();
                return;
            }
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
                    PlaybackCommand::Pause { reason } => worker.pause(reason),
                    PlaybackCommand::Seek {
                        target_100ns,
                        precise,
                    } => *pending_seek = Some((target_100ns, precise)),
                    PlaybackCommand::SetRate(rate) => worker.set_rate(rate),
                    PlaybackCommand::SetVolume { percent, muted } => {
                        if let Some(audio) = &worker.audio_pump.audio {
                            audio.set_volume(mixer::track_scale(percent, muted));
                        }
                    }
                    PlaybackCommand::SetTrackVolume {
                        track,
                        percent,
                        muted,
                    } => {
                        // Grown on demand: the engine does not know how many
                        // tracks a session has until a segment is open, and a
                        // track nothing has touched is full volume anyway.
                        if worker.audio_pump.track_volumes.len() <= track {
                            worker
                                .audio_pump
                                .track_volumes
                                .resize(track + 1, (100, false));
                        }
                        worker.audio_pump.track_volumes[track] = (percent, muted);
                        worker.remix_queued_audio();
                    }
                    PlaybackCommand::ExtendPlaylist {
                        segments,
                        live_edge_100ns,
                    } => worker.extend_playlist(&segments, live_edge_100ns),
                    PlaybackCommand::Screenshot(reply) => {
                        // The UI may have given up waiting; nothing to do then.
                        let _ = reply.send(worker.screenshot());
                    }
                    PlaybackCommand::Shutdown => *shutdown = true,
                    PlaybackCommand::FadeOutAndShutdown => {
                        if !worker.begin_fade_out() {
                            *shutdown = true;
                        }
                    }
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
                // t260924-327f: a playing transport that went this long
                // without a tick was held up *between* ticks -- the part of
                // this loop no insight scope covers.
                if let Some(ended) = self.last_tick_end {
                    let gap = ended.elapsed();
                    if deserves_a_line(gap, TICK_GAP) {
                        tracing::info!(
                            target: "task327f_stall",
                            stage = "tick_gap",
                            gap_ms = gap.as_millis() as u64,
                            seek = pending_seek.is_some(),
                            "a playing transport went without a tick"
                        );
                    }
                }
                self.tick();
                self.last_tick_end = self.playing.then(Instant::now);
            }
        }
    }

    /// Starts the fade-out a clip switch asks for (t260926-2144). False when
    /// nothing is sounding -- paused, no device, no audio track -- and the
    /// command is then a plain `Shutdown`.
    fn begin_fade_out(&mut self) -> bool {
        let sounding = self.playing
            && self
                .current
                .as_ref()
                .is_some_and(|current| !current.audio_streams.is_empty());
        let Some(audio) = self.audio_pump.audio.as_mut().filter(|_| sounding) else {
            return false;
        };
        let queued_ms = audio.queued().as_millis() as u64;
        audio.fade_out();
        tracing::info!(
            event = "audio_fade",
            dir = "out",
            frames = FADE_FRAMES as u64,
            queued_ms,
            "audio fade"
        );
        // The UI has already let go of this engine; nothing it publishes from
        // here on may wake it (t260918-3153).
        self.notify = Box::new(|| {});
        let now = Instant::now();
        self.fading = Some((now, now + FADE_DEADLINE));
        true
    }

    /// The fade-out is over once its last ramped frame has been written *and*
    /// played: dropping the endpoint earlier cuts the ramp off with the
    /// audio still queued in front of it. Waited out here, without ticking, so
    /// the silence after the ramp does not keep the endpoint full.
    fn fade_finished(&mut self) -> bool {
        let Some((started, deadline)) = self.fading else {
            return false;
        };
        let written = self
            .audio_pump
            .audio
            .as_ref()
            .is_none_or(|audio| audio.faded_out());
        if self.playing && !written && Instant::now() < deadline {
            return false;
        }
        if let Some(audio) = &self.audio_pump.audio {
            while !audio.queued().is_zero() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        tracing::info!(
            event = "audio_fade",
            dir = "out_done",
            written,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "audio fade"
        );
        true
    }

    /// `playing` is about to go false (t260924-327f): says why, once per
    /// stop, so a long gap in `task128_playback` reads as a pause or a park
    /// rather than as nothing at all. A UI pause carries its route's word in
    /// `PlaybackCommand::Pause { reason }` (t260925-8bc3: `offscreen`,
    /// `range_end`, `toggle`, `clip_*` ...); the park says `park`.
    fn note_stopped(&mut self, reason: &'static str) {
        if !self.playing {
            return;
        }
        self.stopped = Some((Instant::now(), reason));
        self.last_tick_end = None;
        tracing::info!(
            target: "task327f_stall",
            stage = "stopped",
            reason,
            position_100ns = self.position_100ns,
            "playback stopped"
        );
        #[cfg(test)]
        self.stall_lines.push("stopped");
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
            // An extend advances the live edge by exactly one segment, so
            // resuming on it leaves the transport playing the newest segment
            // -- and crossing out of that one is precisely what `tick`
            // refuses, so it parks again at its end, one segment later. Media
            // arrives at 1.0x and plays at 1.0x, so that lead is conserved
            // rather than rebuilt: a park per crossing, forever.
            //
            // So a live park waits for real lead instead: one park per follow
            // session while the buffer fills, then continuous playback with
            // each segment arriving m (`LIVE_FOLLOW_MARGIN_100NS`) before the
            // transport reaches it.
            //
            // A cold start -- a session opened before anything finalized,
            // parked *before* its own first segment -- waits too, from a still
            // (`cold_start`).
            let head = self.head_100ns();
            if self.cold_start_still || self.position_100ns < head {
                self.cold_start(head);
                return;
            }
            if self.live && self.live_edge_100ns - self.position_100ns < live_resume_lead_100ns() {
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
            self.set_at_live_edge(self.at_edge());
        }
        self.publish_status();
    }

    /// The cold start's way out of its park (edge-start-stutter, 2026-10-26).
    /// Until then the first segment played on arrival, from its head: the
    /// transport ran out of it before the next one was in hand, parked there
    /// for want of the lead anyway, and resumed 2D behind the edge -- past
    /// the UI's rejoin lag, so a seek of D - m followed. Now it waits for
    /// `live_resume_lead_100ns()` like any live park and starts where a follow
    /// at that steady lag would be: `edge - lead`.
    ///
    /// Meanwhile the stage is not left black: the paused seek's frame is the
    /// still, m short of the edge -- which is `edge - lead` once a segment of
    /// D more has landed, so the resume plays on from the very frame shown.
    fn cold_start(&mut self, head: i64) {
        let edge = self.live_edge_100ns;
        let lead = live_resume_lead_100ns();
        if edge - head < lead {
            if self.cold_start_still {
                self.write_status();
                return;
            }
            let still = (edge - LIVE_FOLLOW_MARGIN_100NS).max(head);
            // Set ahead of the seek so a read that fails still leaves the
            // transport where the follow will start, not before the head.
            self.position_100ns = still;
            self.do_seek(still, true);
            // The seek ended the park; this is still one.
            self.parked_from_play = true;
            self.cold_start_still = true;
            // `do_seek` published before the flag was set; the UI hides the
            // knob on it.
            self.publish_status();
            return;
        }
        let start = edge - lead;
        // In place when the still is already about there (the segment after
        // it was D long); a seek otherwise -- no still yet, or a segment that
        // was not D.
        if (start - self.position_100ns).abs() > LIVE_FOLLOW_MARGIN_100NS {
            self.position_100ns = start;
            self.do_seek(start, true);
        }
        self.resume_from_park();
    }

    /// Whether the session under the transport is the one still recording
    /// (task1530). Exact -- the source's own answer -- rather than a
    /// timeout on the extend cadence, which would guess wrong for as long as
    /// the timeout lasts every time a recording stops. A clip is never live.
    ///
    /// Timed (t260924-327f): the answer comes from the capture controller's
    /// locks, and a wait on them had no scope. `caller` names the road in.
    fn refresh_live(&mut self, caller: &'static str) -> Duration {
        let started = Instant::now();
        {
            crate::insight_scope!("playback_refresh_live");
            self.live = self.source.is_live(&self.session_id);
        }
        let took = started.elapsed();
        if deserves_a_line(took, self.stall_threshold) {
            tracing::info!(
                target: "task327f_stall",
                stage = "refresh_live",
                caller,
                ms = took.as_millis() as u64,
                live = self.live,
                "refresh_live waited"
            );
            #[cfg(test)]
            self.stall_lines.push("refresh_live");
        }
        took
    }

    /// Whether the transport stands on the furthest it may go: the live edge
    /// (park position, seek clamp). task1530 held that one segment short
    /// while recording; the reserve went in t261012-5b97, when the newest
    /// segment started being read from memory (`live_follow_landing_100ns`).
    fn at_edge(&self) -> bool {
        self.position_100ns >= self.live_edge_100ns - 1
    }

    /// Where the playlist starts: its first segment, or 0 with none yet.
    fn head_100ns(&self) -> i64 {
        self.playlist.first().map_or(0, |s| s.start_100ns)
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
    /// rather than the live one does not help: the two are the same edge since
    /// t261012-5b97 (and coincided before it on a one-segment live playlist),
    /// so the resume would restart a live-follow session from the top
    /// instead of continuing. The distinction is the caller, not the ruler.
    fn resume_from_park(&mut self) {
        self.play_from_here(false);
    }

    /// The way out of a live park when the recording it is waiting on stops
    /// (task1800).
    ///
    /// A live park holds until `live_resume_lead_100ns()` of playable media
    /// exists, and the only thing that delivers media is an extend -- which
    /// `follow_live_edge` sends only while the manifest is still growing. So a
    /// recording that stops while the transport is short of that lead would
    /// leave it parked for good, a few seconds shy of its own end. The idle
    /// loop's 250ms wake is what notices: once the session is no longer the one
    /// recording, there is nothing left to wait for.
    fn poll_parked_resume(&mut self) {
        if !self.parked_from_play {
            return;
        }
        self.refresh_live("parked");
        if !self.live {
            self.resume_from_park();
        }
    }

    fn play_from_here(&mut self, may_restart_from_head: bool) {
        self.parked_from_play = false;
        self.cold_start_still = false;
        if self.playing {
            return;
        }
        // The other half of `note_stopped` (t260924-327f): how long the stop
        // lasted and what ended it, so a stall reads start to finish.
        if let Some((since, reason)) = self.stopped.take() {
            tracing::info!(
                target: "task327f_stall",
                stage = "resumed",
                after = reason,
                stopped_ms = since.elapsed().as_millis() as u64,
                via = if may_restart_from_head {
                    "play_command"
                } else {
                    "park_resume"
                },
                "playback resumed"
            );
            #[cfg(test)]
            self.stall_lines.push("resumed");
        }
        // Playing off the live edge restarts from the top, like pressing play
        // on an ended <video>. So does playing from *before* the first segment:
        // a session loaded with nothing recorded yet starts at 0 while the
        // timeline it grows into is absolute 100ns ticks -- leaving the
        // transport playing a position no segment covers, which parks on every
        // tick and never draws a frame.
        let head = self.head_100ns();
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
        if let Some(audio) = &mut self.audio_pump.audio {
            audio.resume();
        }
        self.publish_status();
    }

    fn pause(&mut self, reason: &'static str) {
        // t260917-1a00: a pause that lands on a park is otherwise silent --
        // `note_stopped` already fired for the park -- yet it is what ends the
        // park's own resume: with `parked_from_play` cleared, no extend wakes
        // the transport again and only a `Play` does. The occlusion gate's
        // `offscreen` pause landing on a live session's cold-start park is how
        // a covered stage showed zero ticks for a whole recording.
        if self.parked_from_play {
            tracing::info!(
                target: "task327f_stall",
                stage = "park_cancelled",
                reason,
                position_100ns = self.position_100ns,
                "a pause cancelled the park's resume"
            );
            #[cfg(test)]
            self.stall_lines.push("park_cancelled");
        }
        self.note_stopped(reason);
        self.parked_from_play = false;
        self.cold_start_still = false;
        self.playing = false;
        self.anchor = None;
        // Stop the endpoint, keep everything queued behind it (carry and
        // stretcher analysis included): resuming continues the same stream from
        // the same position. Flushing here instead used to advance the audio
        // reader an `AUDIO_BUFFER` past the clock on every pause, which the
        // segment's audio paid for by ending that much early -- the dropout
        // heard at the next crossing.
        if let Some(audio) = &mut self.audio_pump.audio {
            audio.pause();
        }
        // t261012-5b97: `write_status` stops updating `at_live_edge` once
        // paused, and a steady follow now plays with it up most of the time.
        // Left there, a paused transport would read as parked to the range
        // guard and `arms_end_hold`, so it gets the park test `extend_playlist`
        // applies to a stopped transport.
        self.set_at_live_edge(self.at_edge());
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
            std::mem::take(&mut self.audio_pump.audio_recent)
        } else {
            VecDeque::new()
        };
        if crossed_1x {
            self.flush_audio();
        }
        if let Some(stretcher) = self.audio_pump.stretcher.as_mut() {
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
    fn requeue_audio(&mut self, retained: VecDeque<(i64, Vec<Vec<f32>>)>) {
        let position = self.position_100ns;
        for (absolute, tracks) in retained {
            let samples = mix_tracks(&self.audio_pump.track_volumes, &tracks);
            // Same straddler rule as the seek realignment: a block that ends
            // at or before the clock has already been heard.
            if block_fate(
                absolute + mixer::samples_to_100ns(samples.len()),
                position,
                Some(position),
            ) != BlockFate::Play
            {
                continue;
            }
            if self.rate == 1.0 {
                self.audio_pump.pending_out.extend_from_slice(&samples);
            } else {
                let rate = self.rate;
                let out = self
                    .audio_pump
                    .stretcher
                    .get_or_insert_with(|| AudioStretcher::new(rate))
                    .feed(&samples);
                self.audio_pump.pending_out.extend_from_slice(out);
            }
        }
    }

    /// Puts a track level change into the audio already queued, not only
    /// what is decoded after it. The endpoint holds up to `AUDIO_BUFFER` mixed
    /// at the old levels, and a pause keeps it there, so a track muted while
    /// paused used to play its old level for that long on resume. The master
    /// does not need this: `AudioOut::set_volume` scales at the endpoint.
    ///
    /// Narrower than `flush_audio`: the position does not move, so the extra
    /// tracks' decoded read-ahead -- all of it past what is queued -- stays.
    /// A block straddling the clock is re-mixed whole, the same straddler
    /// rule `requeue_audio` has for the 1x crossing.
    fn remix_queued_audio(&mut self) {
        let retained = std::mem::take(&mut self.audio_pump.audio_recent);
        self.audio_pump.pending_out.clear();
        if let Some(stretcher) = &mut self.audio_pump.stretcher {
            // Its analysis already holds the retained blocks at the old
            // levels; they are fed again below.
            stretcher.reset();
        }
        if let Some(audio) = &mut self.audio_pump.audio {
            audio.flush();
        }
        self.requeue_audio(retained);
        if self.playing {
            self.pump_audio(self.position_100ns);
        }
    }

    fn playlist_position(&self, time_100ns: i64) -> Option<usize> {
        playlist_position(&self.playlist, time_100ns)
    }

    /// Makes `playlist_index` the current reader. `from_100ns` is a seek's
    /// segment-local target (t261017-3f94): the open may then read only from
    /// that GOP on, and a current reader that was itself cut after the target
    /// is opened again, whole -- it cannot seek back past its own start.
    fn ensure_segment(&mut self, playlist_index: usize, from_100ns: Option<i64>) -> bool {
        if self.current.as_ref().is_some_and(|current| {
            current.playlist_index == playlist_index
                && from_100ns.is_none_or(|from| from >= current.readable_from)
        }) {
            return true;
        }
        let reopen = self
            .current
            .as_ref()
            .is_some_and(|current| current.playlist_index == playlist_index);
        // Already opened ahead of time (task203). Its decoded first frame is
        // only useful to the crossing in `tick`, which takes the whole
        // `Prefetched` itself; reaching here means something else asked for
        // this segment, so the reader is kept and the frame discarded.
        if self
            .prefetch
            .next
            .as_ref()
            .is_some_and(|next| next.playlist_index == playlist_index)
        {
            let prefetched = self.prefetch.next.take().expect("checked");
            let outgoing = self.current.replace(prefetched.reader);
            self.retirer.retire(outgoing, "ensure_prefetched");
            return true;
        }
        match self.open_segment_at(playlist_index, from_100ns.filter(|_| !reopen)) {
            Some(reader) => {
                let outgoing = self.current.replace(reader);
                self.retirer.retire(outgoing, "ensure_open");
                true
            }
            None => false,
        }
    }

    /// Leases, reads and opens one playlist entry, reporting failures through
    /// the fetch gate. Shared by the synchronous path and the prefetch, so a
    /// segment opened early is opened exactly the same way.
    ///
    /// `from_100ns` is passed by a seek only: the source may then cut the
    /// segment to that time's GOP (t261017-3f94). Both calls the RGB32
    /// fallback makes ask for the same time, and the reader keeps what the
    /// source said it can serve from.
    fn open_segment_at(
        &mut self,
        playlist_index: usize,
        from_100ns: Option<i64>,
    ) -> Option<SegmentReader> {
        crate::insight_scope!("playback_open_segment");
        let segment_index = self.playlist[playlist_index].index;
        let lease = self.lease_segment(playlist_index)?;
        // Scoped apart from the read inside it: `playback_open_segment` covers
        // lease + bytes + decoder, `playback_open_bytes` (raised by the source)
        // covers the read alone, and only the split says which one a slow seek
        // was in (task3460).
        let source = self.source.clone();
        let manager = self.decoder_manager();
        let readable_from = std::cell::Cell::new(0);
        let opened = {
            crate::insight_scope!("playback_open_reader");
            open_segment_with(playlist_index, manager.as_ref(), || match from_100ns {
                Some(from) => source
                    .byte_stream_from(&lease, segment_index, playlist_index, from)
                    .map(|(stream, served_from)| {
                        readable_from.set(served_from);
                        stream
                    }),
                None => source.byte_stream(&lease, segment_index, playlist_index),
            })
        };
        match opened {
            Ok(mut reader) => {
                self.note_fetch_success(segment_index);
                reader.readable_from = readable_from.get();
                if reader.readable_from > 0 {
                    self.cut_reads += 1;
                }
                Some(reader)
            }
            Err(error) => {
                self.note_fetch_failure(segment_index, "playback: segment open failed", error);
                None
            }
        }
    }

    /// The gate check and the lease window, which is everything a fetch needs
    /// before the bytes are touched. Split out for task1510: the read itself
    /// runs on the fetch thread, but neither the gate nor `self.lease` may
    /// leave this one -- both are worker-only state, and both are cheap.
    fn lease_segment(&mut self, playlist_index: usize) -> Option<String> {
        let segment_index = self.playlist[playlist_index].index;
        // Before the lease, not after it: a gated attempt has to cost
        // nothing, and `acquire_review_lease`/`release_review_lease` log a
        // line each. Two of every three records the 2026-08-14 storm wrote
        // were that pair, not the warn below.
        if !self.fetch_gate.ready(segment_index, Instant::now()) {
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
                self.note_fetch_failure(segment_index, "playback: lease acquisition failed", error);
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
        let Some(mut seek) = self.seek_clamp(target_100ns, precise) else {
            return;
        };
        let Some(entry_start_100ns) = self.seek_ensure(&mut seek) else {
            return;
        };
        let lands_on_keyframe = self.seek_source(&seek);
        let landing = self.seek_decode(&mut seek, entry_start_100ns, lands_on_keyframe);
        self.seek_show(&seek, landing, lands_on_keyframe);
        self.seek_finish(&seek, lands_on_keyframe);
    }

    /// `do_seek`'s first stage: the target clamped into the playlist and the
    /// read-ahead kept or dropped. `None` when no segment holds the target.
    fn seek_clamp(&mut self, target_100ns: i64, precise: bool) -> Option<Seek> {
        // The whole call, including the part before `started` below. `took_ms`
        // keeps its old boundary so it still compares against every log since
        // task128; `pre_ms` is what that boundary was hiding (task3460).
        let entered = Instant::now();
        // Read before `abandon_prefetch` clears it: near the live edge the
        // fetch thread is reading the next segment out of the same 20GB
        // container this seek is about to read synchronously, and that read
        // cannot be called back (task1510). Whether the two overlap is the
        // last unexplained candidate for the near-edge excess (task3460).
        let in_flight_index = self.prefetch.in_flight.map(|(_, index)| index);
        let prefetch_in_flight = in_flight_index.is_some();
        // Whatever the transport was doing before it parked, it is not doing
        // it from here: an explicit seek owns the position now.
        self.parked_from_play = false;
        self.cold_start_still = false;
        // Same for the frame whose blit is in flight (t260915-8ea7): it is the
        // picture from before the seek, and publishing it a tick later is the
        // stale frame AC3 forbids.
        self.drop_gpu_pending();
        // Ahead of the target (a park after the seek waits for the lead only
        // while live; the clamp no longer depends on it since t261012-5b97). It touches
        // neither `self.prefetch.next` nor the in-flight slot, so the prefetch decision
        // can wait until the target is known (t261017-cd44).
        self.refresh_live("seek");
        // `.max(low)`: an empty playlist has `live_edge_100ns == 0`, and
        // `clamp` panics when its bounds invert. A zero-segment (but
        // readable) manifest is loadable, so degrade to a zero-width range.
        let low = self.head_100ns();
        let target = target_100ns.clamp(low, (self.live_edge_100ns - 1).max(low));
        let target_index = self.playlist_position(target);
        // The segment read ahead belongs to where playback *was* heading
        // (task203). `ensure_segment` below may well want a different one, and
        // keeping a stale reader around would hand it to the wrong crossing.
        // The same goes for one still being read on the fetch thread, which
        // cannot be called back -- the generation is how its response is
        // recognised as belonging to nowhere (task1510).
        //
        // Unless the seek lands in that very segment (t261017-cd44): then the
        // read-ahead is exactly what `ensure_segment` would read again, and on
        // On an HDD buffer 26% of the 10-16 seeks arrived with one in flight. Kept,
        // and waited for below instead of read a second time.
        let reuse = target_index.is_some_and(|index| {
            in_flight_index == Some(index)
                || self
                    .prefetch
                    .next
                    .as_ref()
                    .is_some_and(|next| next.playlist_index == index)
        });
        if !reuse {
            self.abandon_prefetch();
        }
        // This used to be almost all `refresh_live` waiting on the `sessions`
        // mutex, which the index writer holds across its container appends,
        // prune and checkpoint -- every 2s while recording (task3460).
        // `refresh_live` reads `active` alone since t260924-327f.
        let pre_ms = entered.elapsed().as_millis() as u64;
        let started = Instant::now();
        Some(Seek {
            precise,
            target,
            started,
            pre_ms,
            prefetch_in_flight,
            in_flight_index,
            reuse,
            playlist_index: target_index?,
            waited_prefetch_ms: 0,
            reused_prefetch: false,
            edge_seek: false,
            local_target: 0,
            cut: false,
            ensured_ms: 0,
            resume: false,
            skipped: 0,
        })
    }

    /// The second stage: the target segment's reader, taken from the
    /// read-ahead or opened, and whether the seek can decode on from where the
    /// last one left it. `None` when the segment cannot be opened; otherwise
    /// the segment's start.
    fn seek_ensure(&mut self, seek: &mut Seek) -> Option<i64> {
        let playlist_index = seek.playlist_index;
        // Inside `ensured_ms`, where the synchronous read it replaces was.
        if seek.reuse {
            seek.waited_prefetch_ms = self
                .await_prefetch(playlist_index)
                .map_or(0, |waited| waited.as_millis() as u64);
        }
        // False when the wait came back empty-handed (a failed read): then
        // `ensure_segment` reads it itself, as before.
        seek.reused_prefetch = self
            .prefetch
            .next
            .as_ref()
            .is_some_and(|next| next.playlist_index == playlist_index);
        let entry_start = self.playlist[playlist_index].start_100ns;
        let target = seek.target.max(entry_start);
        seek.target = target;
        // A precise seek the clamp put on the edge lands on the keyframe the
        // source returns instead of decoding to the edge (t260913-b4a6). The
        // last frame before the edge is the one frame a precise seek has to
        // decode the entire last GOP to reach and then drop -- task3460
        // measured End at `skipped` 60-111 against Home's 0 -- and nobody asked
        // for that frame in particular: End and a scrub pinned to the right
        // both mean "the end". The position follows the frame shown.
        seek.edge_seek = seek.precise && target == self.live_edge_100ns - 1;
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
        // Ahead of `ensure_segment`, which may read only from its GOP on
        // (t261017-3f94).
        seek.local_target = target
            - entry_start
            - if seek.edge_seek {
                HNS_PER_SECOND / 60
            } else {
                0
            };
        let cut_reads = self.cut_reads;
        if !self.ensure_segment(playlist_index, Some(seek.local_target)) {
            return None;
        }
        seek.cut = self.cut_reads != cut_reads;
        seek.ensured_ms = seek.started.elapsed().as_millis() as u64;
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
        seek.resume = seek.precise
            && !seek.edge_seek
            && !self.playing
            && self.current.as_ref().is_some_and(|current| {
                !current.video_done()
                    && current.last_video_local().is_some_and(|local| {
                        let last = entry_start + local;
                        last + HNS_PER_SECOND / 60 < target && target - last < HNS_PER_SECOND / 2
                    })
            });
        Some(entry_start)
    }

    /// The third stage: the audio flushed and realigned, and the source moved
    /// to the target unless the seek resumes. Whether the seek lands on the
    /// keyframe the source returned (an edge seek the source accepted).
    fn seek_source(&mut self, seek: &Seek) -> bool {
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
        self.audio_pump.audio_align_100ns = Some(seek.target);
        let current = self.current.as_mut().expect("segment ensured");
        let accepted = if seek.resume {
            true
        } else {
            #[cfg(test)]
            {
                self.source_seeks += 1;
            }
            crate::insight_scope!("playback_seek_source");
            current.seek_local(seek.local_target)
        };
        if !accepted {
            tracing::warn!(target = seek.target, "playback: source refused seek");
        }
        // Only a seek the source performed has a keyframe to land on. A refused
        // one leaves the reader wherever it stood -- mid-GOP on a reader already
        // in use -- and its next sample is just the next frame, so it decodes
        // forward to the target as it always did.
        seek.edge_seek && accepted
    }

    /// The fourth stage: the frame to show, decoded. Coarse: the first
    /// decoded sample (the nearest clean point) is the scrub preview. Precise:
    /// decode forward to the exact frame.
    fn seek_decode(
        &mut self,
        seek: &mut Seek,
        entry_start_100ns: i64,
        lands_on_keyframe: bool,
    ) -> Option<(i64, IMFSample, (u32, u32))> {
        let decodes_to_target = seek.precise && !lands_on_keyframe;
        // Only the frame that gets shown is converted to RGBA. The ones a
        // precise seek passes on its way to the target are decoded (there is
        // no skipping that) and then dropped where they are -- converting them
        // was ~4.7ms apiece spent on pixels nobody ever sees (task207).
        // The position is the seek target either way -- except an edge
        // landing, below -- and a decode that finds no frame to show still
        // leaves the transport where the user asked.
        self.position_100ns = seek.target;
        // Broken out of the loop rather than published inside it
        // (t260914-8d69): `gpu_frame` takes `&mut self` and reads `self.current`
        // itself, so the reader borrow has to be finished before the poster
        // frame can go down the same path as every played frame.
        //
        // Every `ReadSample` of the seek, the keyframe and the frames a
        // precise seek passes alike: `skipped` in the log divides the two.
        crate::insight_scope!("playback_seek_decode");
        loop {
            let current = self.current.as_mut().expect("segment ensured");
            let (local, sample) = current.next_video_sample()?;
            let dimensions = (current.width, current.height);
            let absolute = entry_start_100ns + local;
            if decodes_to_target && absolute + HNS_PER_SECOND / 60 < seek.target {
                seek.skipped += 1;
                continue;
            }
            return Some((absolute, sample, dimensions));
        }
    }

    /// The fifth stage: the landing frame converted and published.
    fn seek_show(
        &mut self,
        #[cfg_attr(not(test), allow(unused_variables))] seek: &Seek,
        landing: Option<(i64, IMFSample, (u32, u32))>,
        lands_on_keyframe: bool,
    ) {
        #[cfg(test)]
        let mut shown = None;
        if let Some((absolute, sample, dimensions)) = landing {
            // The same convert-and-scale blit the played frames take
            // (t260913-2527). It used to be the CPU resample, which made the
            // poster -- including the one `Worker::new` shows at startup -- the
            // one frame per seek on a different scaler from its neighbours.
            //
            // Read back on the spot rather than pipelined (t260915-8ea7): a
            // seek may well be a paused scrub, and then there is no next tick to
            // map the picture on -- the stage would sit on the frame before the
            // seek for as long as the user keeps dragging. One stall per seek,
            // which is what this frame always cost. `Warming` is unreachable
            // for `Readback::Now`, which always reads what it just wrote;
            // publishing nothing is still the honest answer. No spare: one frame
            // per seek is not worth reaching for one (task1640).
            let prepared = self
                .prepare_frame(&sample, dimensions, gpu::Readback::Now, false)
                .filter(|prepared| !matches!(prepared, Prepared::Warming));
            // Nothing below runs for a frame that never became a picture --
            // the position, the realignment and `last_seek` all describe what
            // is on screen.
            if let Some(prepared) = prepared {
                if lands_on_keyframe {
                    self.position_100ns = absolute;
                    // Realign to the landing, not the edge: armed on the edge,
                    // a playing transport would walk past the very audio of the
                    // GOP it is about to show (task1840's `Realign`).
                    self.audio_pump.audio_align_100ns = Some(absolute);
                }
                #[cfg(test)]
                {
                    shown = Some(absolute);
                }
                self.publish(prepared);
            }
        }
        #[cfg(test)]
        {
            self.last_seek = (seek.skipped, shown);
        }
    }

    /// The last stage: the clock restarted, the status written and the
    /// `task128_seek` line.
    fn seek_finish(&mut self, seek: &Seek, lands_on_keyframe: bool) {
        if self.playing {
            self.anchor = Some((Instant::now(), self.position_100ns));
            // Refill here rather than leaving the endpoint empty until the
            // next frame tick gets round to it (task1840). The flush above
            // stopped the endpoint dead; every frame interval between that and
            // the first write is silence on top of the seek's own cost.
            self.pump_audio(self.position_100ns);
        }
        self.counters.last_seek_ms = seek.started.elapsed().as_millis() as u64;
        // A paused seek away from the live edge must drop the LIVE
        // indicator too; `publish_status` only clears it while playing.
        // An edge landing sits up to a GOP short of the edge and is still
        // the edge seek it was (t260913-b4a6).
        self.set_at_live_edge(lands_on_keyframe || self.at_edge());
        tracing::info!(
            target: "task128_seek",
            precise = seek.precise,
            target_s = seek.target / HNS_PER_SECOND,
            took_ms = seek.started.elapsed().as_millis() as u64,
            pre_ms = seek.pre_ms,
            prefetch_in_flight = seek.prefetch_in_flight,
            // t261017-cd44: whether the seek landed in the segment being read
            // ahead, and so took that read instead of reading it again.
            target_index = seek.playlist_index,
            in_flight_index = ?seek.in_flight_index,
            reused_prefetch = seek.reused_prefetch,
            // t261017-3f94: whether this seek read its segment from the
            // target's GOP on rather than whole.
            cut = seek.cut,
            waited_prefetch_ms = seek.waited_prefetch_ms,
            // The two halves of a seek, so a slow one says which half was slow
            // without a rebuild: fetching and opening the segment, against
            // decoding forward to the target frame (task207).
            ensured_ms = seek.ensured_ms,
            skipped = seek.skipped,
            resumed = seek.resume,
            "seek finished"
        );
        self.publish_status();
    }

    /// The sample as a picture to publish: the GPU's convert-and-scale blit
    /// when there is one (t260913-2527), else the CPU pair. `None` only when
    /// the CPU conversion failed and there is nothing to show. `spare` reaches
    /// for the buffer the UI handed back (task1640) for the CPU pair.
    fn prepare_frame(
        &mut self,
        sample: &IMFSample,
        (width, height): (u32, u32),
        readback: gpu::Readback,
        spare: bool,
    ) -> Option<Prepared> {
        match self.gpu_frame(sample, width, height, readback) {
            GpuFrame::Ready(frame) => Some(Prepared::Scaled(frame)),
            GpuFrame::Warming => Some(Prepared::Warming),
            GpuFrame::Unavailable => {
                // Written into the buffer the UI handed back, when it handed
                // one back (task1640) -- a frame's worth of page faults the
                // engine thread would otherwise pay for a brand new 8.3MB
                // `Vec`.
                let spare = spare
                    .then(|| self.shared.take_spare(super::rgba_bytes(width, height)))
                    .flatten();
                // The swizzle task200 measured at 5.87ms, now paid only for
                // frames that are actually going to be seen (t260925-8bc3: one
                // scope for the tick's and the seek's).
                crate::insight_scope!("playback_convert_video");
                self.cpu_convert(sample, spare)
                    .map(|rgba| Prepared::Decoded(rgba, width, height))
            }
        }
    }

    /// Puts a prepared picture on the stage. `false` for `Warming`, which has
    /// none yet.
    fn publish(&mut self, prepared: Prepared) -> bool {
        match prepared {
            Prepared::Decoded(rgba, width, height) => self.publish_frame(rgba, width, height),
            Prepared::Scaled(frame) => self.publish_prepared(frame),
            Prepared::Warming => return false,
        }
        true
    }

    /// Crosses into the segment after `playlist_index`, once the current
    /// one has no video left. Returns false when the tick has nothing more
    /// to do: the transport parked, or the crossing left no frame decoded
    /// and the next tick reads the new segment's first one.
    fn cross_to_next(&mut self, playlist_index: usize) -> bool {
        // t260924-327f: the crossing had no scope of its own, so a 9s wait in
        // it read as "somewhere in the tick". One line when it is slow, naming
        // the two steps in it that wait on something outside this thread.
        crate::insight_scope!("playback_cross");
        let started = Instant::now();
        // Once per crossing is often enough to notice the recording stopping,
        // and it is the only place the answer changes what happens next
        // (task1530).
        let refresh = self.refresh_live("cross");
        let (crossed, retire_inline, waited_prefetch) = self.cross_after_refresh(playlist_index);
        let total = started.elapsed();
        if deserves_a_line(total, self.stall_threshold) {
            tracing::info!(
                target: "task327f_stall",
                stage = "cross",
                index = playlist_index + 1,
                total_ms = total.as_millis() as u64,
                refresh_live_ms = refresh.as_millis() as u64,
                retire_inline_ms = retire_inline.map(|took| took.as_millis() as u64),
                // t261016-153c: a slow crossing that waited out the read-ahead
                // in flight, as opposed to one that read the segment itself.
                waited_prefetch_ms = waited_prefetch.map(|took| took.as_millis() as u64),
                crossed,
                "a slow segment crossing"
            );
            #[cfg(test)]
            self.stall_lines.push("cross");
        }
        crossed
    }

    /// `cross_to_next` after its `refresh_live`, returning the crossing's
    /// answer, how long the outgoing reader's inline release took (`None`
    /// when it was queued or there was none), and how long the crossing waited
    /// for a read-ahead still in flight (`None` when there was none).
    fn cross_after_refresh(
        &mut self,
        playlist_index: usize,
    ) -> (bool, Option<Duration>, Option<Duration>) {
        let next = playlist_index + 1;
        if next >= self.playlist.len() {
            self.park_at_live_edge(true);
            return (false, None, None);
        }
        let mut retire_inline = None;
        let previous_end = self.playlist[playlist_index].end_100ns;
        let entry = self.playlist[next].clone();
        let waited_prefetch = self.await_prefetch(next);
        // A prefetch (task203) carries its first frame with it, so a crossing
        // that hit one costs nothing here and the frame below is paced against
        // the same clock as every other.
        let prefetched = self
            .prefetch
            .next
            .take()
            .filter(|ready| ready.playlist_index == next);
        // How many crossings land on a prefetch -- the number task209 was
        // decided on (96% of 79). `debug!` like `task128_tick` above: a line
        // per segment is noise in the shipped log, but the measurement has
        // to stay repeatable without editing code again.
        tracing::debug!(
            target: "task209_crossing",
            prefetched = prefetched.is_some(),
            index = next,
            "segment crossing"
        );
        self.counters.crossings += 1;
        // The frame in flight belongs to the segment being left (t260915-8ea7).
        // Where the two segments differ in size the pipeline rebuild would drop
        // it anyway; this is the case where they do not, and it costs the seam
        // one picture rather than letting the outgoing segment's last frame
        // arrive after the incoming one's clock.
        self.drop_gpu_pending();
        // At the live edge the incoming segment is the newest one, which is
        // the case task1530 is about: it was appended moments ago (m before
        // the crossing since t261012-5b97), so its read-ahead has the least
        // time. Counted apart from the catch-up crossings so the hit rate
        // says *which* kind is missing.
        if next + 1 >= self.playlist.len() {
            self.counters.crossings_live += 1;
            if prefetched.is_some() {
                self.counters.crossings_live_prefetched += 1;
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
        self.audio_pump.pending_seam = Some(Seam {
            index: entry.index,
            previous_end,
            next_start: entry.start_100ns,
            prefetched: prefetched.is_some(),
        });
        match prefetched {
            Some(ready) => {
                self.counters.crossings_prefetched += 1;
                // Before the queue is filled: `tick` reads `self.current` for
                // the frame's dimensions and for `nv12_planes`, and the sample
                // below came out of *this* reader on the fetch thread.
                let outgoing = self.current.replace(ready.reader);
                retire_inline = self.retirer.retire(outgoing, "cross");
                self.pending_frame = ready
                    .first
                    .map(|(absolute, sample)| Pending { absolute, sample });
                // The seam frame is shown however the clock feels about it.
                // It used to be exempt by being a `Pending::Ready`; now that it
                // is queued like every other frame (t260914-8d69), the exemption
                // is the same flag a seek uses. Only when there is a frame: a
                // crossing that missed the prefetch was never protected.
                self.must_show_next |= self.pending_frame.is_some();
            }
            None => {
                if !self.ensure_segment(next, None) {
                    self.park_at_live_edge(self.report_park);
                    return (false, None, waited_prefetch);
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
            return (false, retire_inline, waited_prefetch);
        }
        (true, retire_inline, waited_prefetch)
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
                    step_ms = ms(absolute - self.position_100ns),
                    due_in_media_ms = ms(due_in_media),
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
        let Some((playlist_index, decode_started)) = self.tick_decode() else {
            return;
        };
        let (deadline, anchor) = self.tick_wait(playlist_index, decode_started);
        let Some((late, absolute, sample)) = self.tick_keep(deadline, anchor) else {
            return;
        };
        let (width, height) = {
            let current = self.current.as_ref().expect("segment ensured");
            (current.width, current.height)
        };
        // t260913-2527: both steps in one GPU blit when there is a stage
        // small enough to be worth scaling to. What comes back is already
        // stage-sized, so it skips `publish_frame`'s resample rather than
        // feeding it. `Unavailable` is every other case -- no stage, the 11b4
        // band, a BGRA segment, no device -- and each falls through to the CPU
        // pair unchanged.
        //
        // t260915-8ea7: `Warming` is this frame's blit in flight, its picture
        // the next tick's. Nothing is published and the CPU pair does *not*
        // run -- it would redo on this thread the work that is already on the
        // GPU. The clock below still advances, so the frame is not lost, only
        // shown one tick late.
        let Some(prepared) =
            self.prepare_frame(&sample, (width, height), gpu::Readback::Pipelined, true)
        else {
            return;
        };
        self.position_100ns = absolute;
        // t260913-3842: what the next tick's skip measures its period from.
        // Set whichever way the frame was prepared, and for a crossing's first
        // frame too, so it re-phases the period instead of being invisible
        // to it.
        self.last_publish_deadline = Some(deadline);
        self.tick_show(prepared, late);
    }

    /// The tick's first stage: the segment under the transport ensured, and
    /// the next frame decoded into `pending_frame` if none is waiting. `None`
    /// when the tick ends here -- parked, or a crossing that left no frame
    /// decoded. Otherwise the segment and when the decode started.
    fn tick_decode(&mut self) -> Option<(usize, Instant)> {
        // Before `ensure_segment`, so a read-ahead that finished between two
        // ticks is in `self.prefetch.next` in time for a crossing this tick makes
        // (task1510).
        self.take_prefetch_responses();
        let Some(playlist_index) = self.playlist_position(self.position_100ns) else {
            self.park_at_live_edge(true);
            return None;
        };
        if !self.ensure_segment(playlist_index, None) {
            // Parking is what stops this loop -- but only until the UI arms
            // it again, which range repeat does on every drain tick. Which
            // is why a failure the gate already reported once parks under
            // `PARK_RING_FLOOR`: the ring is the other half of the loop, and
            // throttling it is what keeps the pair from running away
            // (2026-08-14).
            self.park_at_live_edge(self.report_park);
            return None;
        }
        // Decode the next frame if none is waiting.
        let decode_started = Instant::now();
        if self.pending_frame.is_none() {
            let start_100ns = self.playlist[playlist_index].start_100ns;
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
                    self.pending_frame = Some(Pending {
                        absolute: start_100ns + local,
                        sample,
                    });
                }
                None => {
                    if !self.cross_to_next(playlist_index) {
                        return None;
                    }
                }
            }
        }
        Some((playlist_index, decode_started))
    }

    /// The second stage: the queued frame's deadline, the audio pumped up to
    /// it, the next segment read ahead, and the wait. The deadline and the
    /// anchor it was measured from.
    fn tick_wait(
        &mut self,
        playlist_index: usize,
        decode_started: Instant,
    ) -> (Instant, (Instant, i64)) {
        let absolute = self.pending_frame.as_ref().expect("decoded above").absolute;
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
        (deadline, (anchor_wall, anchor_media))
    }

    /// The third stage: whether the frame is shown at all. `None` for a frame
    /// skipped or dropped -- the clock has advanced past it -- otherwise how
    /// late it is, its timestamp and its sample.
    fn tick_keep(
        &mut self,
        deadline: Instant,
        (anchor_wall, anchor_media): (Instant, i64),
    ) -> Option<(Duration, i64, IMFSample)> {
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
            self.position_100ns = pending.absolute;
            self.counters.frames_skipped += 1;
            return None;
        }
        if should_drop_frame(late, droppable, self.consecutive_drops) {
            // Throw it away *before* the swizzle, which is the whole point
            // (task630): the clock still advances, so x2 stays x2 even when the
            // machine cannot convert frames twice as fast. `pump_audio` already
            // ran above, so sound never depends on whether a picture was shown.
            self.position_100ns = pending.absolute;
            self.consecutive_drops += 1;
            self.counters.frames_dropped += 1;
            return None;
        }
        self.consecutive_drops = 0;
        self.must_show_next = false;
        let Pending { absolute, sample } = pending;
        Some((late, absolute, sample))
    }

    /// The last stage: the frame published and counted.
    fn tick_show(&mut self, prepared: Prepared, late: Duration) {
        if !self.publish(prepared) {
            // Warming: the counters below describe a frame that was shown, and
            // this one was not.
            return;
        }
        self.counters.frames_shown += 1;
        if late > LATE_ENOUGH_TO_DROP {
            self.counters.frames_late += 1;
        }
        self.fps_frames += 1;
        if self.fps_window.elapsed() >= Duration::from_secs(1) {
            self.log_playback_window();
        }
    }

    /// The once-a-second diagnostic line, and the counters it resets
    /// (task128). Emitted from `tick` after a frame is shown.
    fn log_playback_window(&mut self) {
        self.counters.actual_fps = self.fps_frames;
        let snapshot = self.counters;
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
            position_s = self.position_100ns / HNS_PER_SECOND,
            // Task630: the two numbers a rate change is judged on. Dropped
            // frames are the deliberate cost of keeping the clock true, and
            // the prefetch hit rate is what says whether the crossings are
            // still free -- both were only visible at `debug!` before, which
            // the shipped log never records.
            rate = self.rate,
            dropped = self.counters.frames_dropped,
            // t260913-3842: frames never built because the display was still
            // showing the previous one. Separate from `dropped` on purpose --
            // adding the two would read as the machine failing to keep up.
            skipped = self.counters.frames_skipped,
            crossings = self.counters.crossings,
            prefetched = self.counters.crossings_prefetched,
            // Task1530: the live-edge subset of the two above. A hit rate that
            // only moves here is the one this task changed.
            crossings_live = self.counters.crossings_live,
            prefetched_live = self.counters.crossings_live_prefetched,
            parks = self.counters.parks,
            // How far the picture is behind the newest finalized frame
            // (task1530): the resume lead's price, D + m down to m on a steady
            // follow since t261012-5b97.
            behind_ms = ms(self.live_edge_100ns - self.position_100ns),
            // t261012-5b97: how far the picture is behind the wall clock. Media
            // times are WGC's `SystemRelativeTime`, QPC in 100ns, so this is
            // `behind_ms` plus the finalize-and-append latency. A measurement
            // only; `None` unless live. After `behind_ms`, so the
            // `parks=N behind_ms=M` match below still holds.
            wall_behind_ms = ?self
                .live
                .then(|| ms(qpc_now_100ns() - self.position_100ns)),
            // Task640: the endpoint's low-water mark over the last second. A
            // dropout is this reaching 0; anything else is headroom. `None`
            // when nothing was written at all -- a paused or parked second, or
            // a session with no audio -- rather than the sentinel, which reads
            // as a nonsense 4294967295.
            queued_min_ms = ?(self.audio_pump.audio_queued_min_ms != u32::MAX)
                .then_some(self.audio_pump.audio_queued_min_ms),
            // Task1840: blocks walked past because a seek landed after them.
            // Every `drift` step in run5 was this wearing drift's name, so the
            // two have to be readable side by side to tell a fixed count from
            // a hidden one.
            realigned = self.audio_pump.audio_realigned,
            // t261017-cd44: read-aheads thrown away, cumulative like
            // `crossings`. `stale_prefetch` came back to nobody (a seek
            // disowned it mid-read), `abandoned` was ready and dropped by one.
            // Last, so `parks=N behind_ms=M` stays adjacent for the evidence
            // scripts that match on it (t261012-38b1, t261016-3fbd).
            stale_prefetch = self.counters.prefetches_stale,
            abandoned = self.counters.prefetches_abandoned,
            "playback tick"
        );
        self.audio_pump.audio_queued_min_ms = u32::MAX;
        self.fps_frames = 0;
        self.fps_window = Instant::now();
    }

    /// Asks the fetch thread for the segment after `playlist_index`, if the
    /// transport has entered the segment it follows (task203, window widened by
    /// t261016-153c -- see `prefetch_target`).
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
        if self.prefetch.next.is_some() || self.prefetch.in_flight.is_some() {
            return;
        }
        let Some(next) = prefetch_target(
            playlist_index,
            self.playlist.len(),
            self.playlist_position(self.position_100ns),
            self.playlist[playlist_index].end_100ns - self.position_100ns,
            self.playlist[playlist_index].end_100ns - self.playlist[playlist_index].start_100ns,
        ) else {
            return;
        };
        let Some(lease) = self.lease_segment(next) else {
            return;
        };
        let generation = self.prefetch.generation;
        // Before the borrow of `self.prefetch.fetcher`, not inside the literal: this
        // builds the scaler on its first call.
        let manager = self.decoder_manager();
        let sent = self.prefetch.fetcher.request(FetchRequest {
            generation,
            playlist_index: next,
            segment_index: self.playlist[next].index,
            start_100ns: self.playlist[next].start_100ns,
            lease,
            source: self.source.clone(),
            manager,
        });
        if sent {
            self.prefetch.in_flight = Some((generation, next));
        }
    }

    /// Takes whatever the fetch thread has finished (task1510).
    ///
    /// A drain rather than one `try_recv`: a seek can leave a response in the
    /// channel *and* put a second read in flight before the first is looked at,
    /// so two can be waiting. Called at the top of every tick -- early enough
    /// that a response landing between two ticks is in `self.prefetch.next` before the
    /// crossing that wants it -- and again after a seek, so the reader it was
    /// carrying is retired instead of sitting on 35MB until playback resumes.
    fn take_prefetch_responses(&mut self) {
        while let Some(response) = self.prefetch.fetcher.try_recv() {
            self.take_prefetch_response(response);
        }
    }

    /// Waits out a read-ahead of `next` that is still in flight when the
    /// crossing wants it (t261016-153c), and returns how long that took (`None`
    /// when there was nothing to wait for). Falling through to `ensure_segment`
    /// instead read the same segment a second time while the first read was
    /// still going -- on a HDD both take about a second and fight each other.
    /// Waiting blocks this thread no longer than that synchronous read did.
    /// Responses for anything else that arrive first go through the same
    /// verdict as at the top of a tick; a failed read-ahead leaves `self.prefetch.next`
    /// empty and the crossing falls back to `ensure_segment` as before.
    fn await_prefetch(&mut self, next: usize) -> Option<Duration> {
        let wanted = |worker: &Self| {
            worker.prefetch.next.is_none()
                && worker
                    .prefetch
                    .in_flight
                    .is_some_and(|(_, index)| index == next)
        };
        if !wanted(self) {
            return None;
        }
        let started = Instant::now();
        while wanted(self) {
            let Some(response) = self.prefetch.fetcher.recv() else {
                // The fetch thread is gone: nothing in flight will answer.
                self.prefetch.in_flight = None;
                break;
            };
            self.take_prefetch_response(response);
        }
        Some(started.elapsed())
    }

    fn take_prefetch_response(&mut self, response: FetchResponse) {
        let key = (response.generation, response.playlist_index);
        let verdict = prefetch_verdict(
            key,
            self.prefetch.in_flight,
            response.result.is_ok(),
            self.prefetch.next.is_some(),
            self.playlist_position(self.position_100ns),
        );
        // Whatever the verdict, the slot this response occupies is free
        // again -- including on `Discard`, or the read-ahead would never
        // restart.
        if self.prefetch.in_flight == Some(key) {
            self.prefetch.in_flight = None;
        }
        match (verdict, response.result) {
            (PrefetchVerdict::Accept, Ok((reader, first))) => {
                self.note_fetch_success(response.segment_index);
                self.prefetch.next = Some(Prefetched {
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
            (_, Ok((reader, _))) => {
                self.counters.prefetches_stale += 1;
                self.retirer.retire(Some(reader), "stale_prefetch");
            }
            (_, Err(_)) => {}
        }
    }

    /// Retires the read-ahead and disowns anything still in flight: whatever
    /// was being read belongs to a position the transport is no longer heading
    /// for (task203/task1510). The generation is what makes the disowning work
    /// without a way to cancel the read itself.
    fn abandon_prefetch(&mut self) {
        if let Some(ready) = self.prefetch.next.take() {
            self.counters.prefetches_abandoned += 1;
            self.retirer.retire(Some(ready.reader), "abandon");
        }
        self.prefetch.generation += 1;
        self.prefetch.in_flight = None;
        self.take_prefetch_responses();
    }

    /// Every flush point has to take the carry with it (task154): the samples
    /// waiting there belong to the position that is being left behind, and
    /// writing them after a seek would leak a moment of the old audio.
    fn flush_audio(&mut self) {
        // A pending realignment belongs to the seek that armed it; anything
        // that flushes afterwards has already thrown that audio away.
        self.audio_pump.audio_align_100ns = None;
        // And the retained blocks belong to the position being left. Clearing
        // them here is what keeps task155 airtight: only `set_rate` ever
        // replays them, and it takes them *before* flushing, at a position
        // that has not moved. A seek or a park can never reach them.
        self.audio_pump.audio_recent.clear();
        self.audio_pump.pending_out.clear();
        // The extra tracks hold audio for the position being left; mixing it
        // into the new one would smear the old moment into it, the same
        // reason the stretcher resets below (task1270).
        for track in &mut self.audio_pump.extra_audio {
            track.clear();
        }
        if let Some(stretcher) = &mut self.audio_pump.stretcher {
            // Its analysis window still holds the audio around the position
            // being left; emitting that after a seek would smear the old
            // moment into the new one (task155). A pause leaves the position
            // alone and so does not come through here.
            stretcher.reset();
        }
        if let Some(audio) = &mut self.audio_pump.audio {
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
        self.audio_pump.drain_dropped = 0;
        self.audio_pump.drain_dropped_100ns = 0;
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
        let Some(current) = self.current.as_mut() else {
            return;
        };
        self.audio_pump.pump(
            current,
            &self.playlist[playlist_index],
            self.rate,
            master_100ns,
            drain,
            &mut self.counters,
        );
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
        self.note_stopped("park");
        self.parked_from_play = true;
        self.counters.parks += 1;
        self.playing = false;
        self.anchor = None;
        // Nothing ticks from here, so a frame still in flight would be published
        // by whatever happens next -- a resume, a seek -- however much later that
        // is (t260915-8ea7).
        self.drop_gpu_pending();
        self.position_100ns = self.live_edge_100ns - 1;
        self.flush_audio();
        self.set_at_live_edge(true);
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

    /// The one-shot construction of the GPU scaler, hoisted out of
    /// [`Self::gpu_frame`] (t260915-e6c8). The scaler now also owns the device
    /// manager a segment has to be *opened* with, so it has to exist before the
    /// first reader does rather than before the first frame.
    fn ensure_gpu(&mut self) {
        if matches!(self.gpu_stage.gpu, GpuSlot::Untried) {
            self.gpu_stage.gpu =
                gpu::GpuScaler::take_or_new().map_or(GpuSlot::Absent, GpuSlot::Ready);
        }
    }

    /// The device to open the next reader's decoder onto, or `None` on a
    /// machine with no GPU scaler -- where playback stays on exactly the
    /// software decoder it always had.
    fn decoder_manager(&mut self) -> Option<IMFDXGIDeviceManager> {
        self.ensure_gpu();
        self.gpu_stage.gpu.get().map(|gpu| gpu.manager().clone())
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
    /// Named rather than refused, deliberately. Both routes that still bring a
    /// decoder texture here need a picture and have no second way to make one:
    /// no GPU scaler at all (`gpu.is_none()`), and `gpu::Converted::Failed`, a
    /// driver that will refuse every frame. A `dxgi_texture().is_some()` guard
    /// in front of either would drop the frame instead of slowing it down.
    /// (The process's first poster, decoded before any stage exists, used to
    /// be a third; t260917-d792 blits it 1:1 instead -- see [`blit_dims`].)
    ///
    /// Reachable, then, only on these conditions (t260918-8ffa): no GPU scaler
    /// (`GpuScaler::new` failed, so the reader stays on the software decoder
    /// and every frame lands here); a sample in system memory that `gpu_frame`
    /// declines -- no stage, the near-1:1 band, or an RGB32 fallback segment
    /// with no NV12 planes; and `Converted::Failed`. On a machine whose
    /// hardware decoder hands out textures none of those hold, so this is not
    /// dead code but it is not reached either. In the tests only the
    /// `gpu = None` injection (t260914-1627) reaches it: `edge_seek_tests` and
    /// `clip_engine_tests` hand it decoder textures with no stage and arrive
    /// here only when [`blit_dims`]'s texture arm is hollowed (t260925-dbf8).
    /// The window size stopped
    /// selecting it at t260915-e6c8 (`66e1e66a`, the near-1:1 band blitted
    /// 1:1 for a texture), the startup poster at t260917-d792 (`5784d387`).
    /// Kept because t260913-2527 requires the no-GPU fallback.
    ///
    /// The edge, not once per process: GPU work can recover between failures,
    /// and each fresh entry into the slow path is worth hearing about. One
    /// `warn!` per *entry* ([`readback_edge`]), which is also what says how
    /// often a session is falling into it.
    fn cpu_convert(&mut self, sample: &IMFSample, spare: Option<Vec<u8>>) -> Option<Vec<u8>> {
        let from_texture = self
            .current
            .as_ref()
            .expect("segment ensured")
            .dxgi_texture(sample)
            .is_some();
        if readback_edge(&mut self.gpu_stage.cpu_readback, from_texture) {
            tracing::warn!(
                event = "playback_cpu_readback",
                display_target = ?self.shared.display_target(),
                "a decoder texture is going back through the CPU pair; Lock2D reads it off the GPU synchronously"
            );
        }
        self.current
            .as_ref()
            .expect("segment ensured")
            .convert(sample, spare)
    }

    /// The GPU convert-and-scale for one frame. `Unavailable` -- the only
    /// answer that sends a caller to [`Self::cpu_convert`] -- comes from four
    /// places: no GPU scaler, [`blit_dims`] declining a system-memory frame
    /// (no stage or the near-1:1 band), a system-memory sample with no NV12
    /// planes (the RGB32 fallback), and `Converted::Failed`. A decoder texture
    /// never takes the second or third (t260918-8ffa). Every one of them leaves
    /// the CPU path byte for byte as it was.
    ///
    /// What it keeps for [`PlaybackFrame::full`] is the NV12 the decoder handed
    /// over rather than recording-sized RGBA: the blit reads back the stage
    /// only, and converting the whole frame again just so a screenshot *could*
    /// ask for it would put back most of what this removes.
    fn gpu_frame(
        &mut self,
        sample: &IMFSample,
        width: u32,
        height: u32,
        readback: gpu::Readback,
    ) -> GpuFrame {
        self.ensure_gpu();
        if self.gpu_stage.gpu.get().is_none() {
            return GpuFrame::Unavailable;
        }
        let texture = self
            .current
            .as_ref()
            .expect("segment ensured")
            .dxgi_texture(sample);
        let Some(scaled) = blit_dims(
            (width, height),
            self.shared.display_target(),
            texture.is_some(),
        ) else {
            return GpuFrame::Unavailable;
        };
        // The system-memory path reads the planes *in*. The decoder-texture
        // path gets them *out* of the blit only on `Readback::Now`; playing,
        // it leaves them in the GPU's staging slot (t261016-97b7) and the
        // frame goes out as `FullFrame::Unread`.
        let mut planes = match texture {
            Some(_) if readback == gpu::Readback::Pipelined => Vec::new(),
            Some(_) => self
                .shared
                .take_nv12_spare(gpu::nv12_bytes((width, height)))
                .unwrap_or_default(),
            None => {
                let spare = self
                    .shared
                    .take_nv12_spare(gpu::nv12_bytes((width, height)));
                crate::insight_scope!("playback_gpu_read_planes");
                let current = self.current.as_ref().expect("segment ensured");
                match current.nv12_planes(sample, spare) {
                    Some(nv12) => nv12,
                    None => return GpuFrame::Unavailable,
                }
            }
        };
        // Shared (t260917-66c2): the picture stays on the GPU for slint and no
        // stage-sized buffer is filled at all.
        let sharing = {
            let gpu = self.gpu_stage.gpu.get_mut().expect("checked above");
            gpu.set_share(self.shared.gpu_stage());
            gpu.sharing()
        };
        let bytes = super::rgba_bytes(scaled.0, scaled.1);
        let mut rgba = if sharing {
            Vec::new()
        } else {
            super::exact_or_new(self.shared.take_scaled_spare(bytes), bytes)
        };
        let gpu = self.gpu_stage.gpu.get_mut().expect("checked above");
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
        let picture = gpu.take_picture();
        clear_readback_edge(&mut self.gpu_stage.cpu_readback, &converted);
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
                self.gpu_stage.gpu_pending = Some(arriving);
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
                GpuFrame::Ready(arriving.into_frame(rgba, picture))
            }
            gpu::Converted::Previous => {
                let Some(previous) = self.gpu_stage.gpu_pending.replace(arriving) else {
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
                // On the decoder-texture path `previous.nv12` is empty: its
                // planes stayed in the staging slot the picture came out of
                // (t261016-97b7), and `Worker::screenshot` reads them from there.
                GpuFrame::Ready(previous.into_frame(rgba, picture))
            }
        }
    }

    /// The NV12 of the frame last published off the decoder-texture path, for
    /// a screenshot of a `FullFrame::Unread` (t261016-97b7). Commands are
    /// handled between ticks, so no blit can overwrite the slot mid-read.
    fn screenshot(&mut self) -> Option<(u32, u32, Vec<u8>)> {
        let mut planes = Vec::new();
        let (width, height) = self
            .gpu_stage
            .gpu
            .get_mut()?
            .read_published_nv12(&mut planes)?;
        Some((width, height, planes))
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
        if let Some(gpu) = self.gpu_stage.gpu.get_mut() {
            gpu.discard_in_flight();
        }
    }

    /// The planes only, for the places inside `gpu_frame` where the GPU has
    /// already said what it did with the cycle: it must **not** be told to
    /// forget a frame it has just accepted.
    fn recycle_gpu_pending(&mut self) {
        if let Some(pending) = self.gpu_stage.gpu_pending.take() {
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
        let Some(mut pending) = self.gpu_stage.gpu_pending.take() else {
            return false;
        };
        let bytes = super::rgba_bytes(pending.scaled.0, pending.scaled.1);
        let sharing = self.gpu_stage.gpu.get().is_some_and(|gpu| gpu.sharing());
        let mut rgba = if sharing {
            Vec::new()
        } else {
            super::exact_or_new(self.shared.take_scaled_spare(bytes), bytes)
        };
        // Empty planes are the decoder-texture path's marker (t260915-e6c8):
        // its NV12 lives in the GPU's own staging pair, not here, so it has to
        // come back out with the picture or `full` would be published empty.
        let planes = pending.nv12.is_empty().then_some(&mut pending.nv12);
        let read = self
            .gpu_stage
            .gpu
            .get_mut()
            .is_some_and(|gpu| gpu.read_in_flight(&mut rgba, planes));
        if !read {
            self.shared.recycle_scaled(rgba);
            self.recycle_planes(pending.nv12);
            return false;
        }
        let picture = self
            .gpu_stage
            .gpu
            .get_mut()
            .and_then(|gpu| gpu.take_picture());
        self.publish_prepared(pending.into_frame(rgba, picture));
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
                    .take_scaled_spare(super::rgba_bytes(scaled_width, scaled_height))
            });
        let frame = {
            crate::insight_scope!("playback_scale_stage");
            super::scale::frame_for_stage(
                &mut self.gpu_stage.scaler,
                rgba,
                (width, height),
                target,
                spare,
            )
        };
        self.publish_prepared(frame);
    }

    /// The half of `publish_frame` after the resample: hand the frame to the
    /// mailbox, take back whatever the UI never came for, ring the bell. Called
    /// on its own by the GPU path, whose frame arrives already stage-sized.
    fn publish_prepared(&mut self, frame: PlaybackFrame) {
        // A fading engine's pictures belong to no stage (t260918-3153).
        if self.fading.is_some() {
            return;
        }
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

    /// `at_live_edge` alone, for the stops `write_status` leaves it to: it
    /// only recomputes it while playing.
    fn set_at_live_edge(&self, at_live_edge: bool) {
        self.shared.status.lock().unwrap().at_live_edge = at_live_edge;
    }

    fn publish_status(&mut self) {
        self.write_status();
        (self.notify)();
    }

    /// The shared-state half of `publish_status`, without the bell.
    fn write_status(&mut self) {
        // The retire thread must not tear a decoder down while this one is
        // decoding on a deadline (task246): same toll, worse timing.
        self.retirer.set_busy(self.playing);
        let mut status = self.shared.status.lock().unwrap();
        status.playing = self.playing;
        status.position_100ns = self.position_100ns;
        status.cold_start_still = self.cold_start_still;
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
        drop(status);
        self.counters.audio_enabled = self.audio_pump.audio.is_some();
        // How many tracks the open segment carries, for the mixer panel to
        // draw rows for (task1270). Their names come from the manifest, not
        // from here.
        self.counters.audio_tracks = self
            .current
            .as_ref()
            .map(|reader| reader.audio_streams.len())
            .unwrap_or_default();
        #[cfg(test)]
        {
            *self.shared.counters.lock().unwrap() = self.counters;
        }
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
        // The next engine takes this device and its shared ring (t260930-8bce).
        // Not after a panic: whatever state it was left in is not handed on.
        if !std::thread::panicking() {
            if let Some(gpu) = self.gpu_stage.gpu.take() {
                gpu.park();
            }
        }
    }
}

/// One track's own scalar, defaulting to full volume for a track nothing has
/// set yet (task1270). The master is a separate multiplication.
fn track_scale_of(volumes: &[(i64, bool)], track: usize) -> f32 {
    let (percent, muted) = volumes.get(track).copied().unwrap_or((100, false));
    mixer::track_scale(percent, muted)
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
fn append_new_segments(playlist: &mut Vec<TimelineSegment>, incoming: &[TimelineSegment]) -> usize {
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
        playlist.push(segment.clone());
        added += 1;
    }
    added
}

/// Appends whatever the endpoint refused from a freshly produced block.
fn carry_tail(pending: &mut Vec<f32>, block: &[f32], written_frames: usize) {
    let written = (written_frames * AUDIO_CHANNELS as usize).min(block.len());
    pending.extend_from_slice(&block[written..]);
}

#[cfg(test)]
mod tests;
