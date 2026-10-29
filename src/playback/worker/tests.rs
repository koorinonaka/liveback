use super::*;
use crate::encoder::aac::AUDIO_SAMPLE_RATE;
use crate::playback::source::memory_byte_stream;
use windows::Win32::Media::MediaFoundation::IMFByteStream;

/// Segment bytes served from memory under the lease every test source shares:
/// nothing to lease, never live. `R` is the read -- [`Whole`] hands the bytes
/// out as they are; the slow and the cutting sources bring their own read and
/// change nothing else.
pub(super) struct BytesSource<R = Whole> {
    pub(super) bytes: Vec<u8>,
    pub(super) read: R,
}

impl BytesSource {
    pub(super) fn new(bytes: Vec<u8>) -> Self {
        Self { bytes, read: Whole }
    }
}

/// How a [`BytesSource`] opens its bytes. `byte_stream_from` defaults to the
/// whole segment from its start, as `SegmentSource`'s own default does.
pub(super) trait ReadBytes: Send + Sync {
    fn byte_stream(&self, bytes: &[u8], playlist_index: usize) -> Result<IMFByteStream, String>;

    fn byte_stream_from(
        &self,
        bytes: &[u8],
        playlist_index: usize,
        _from_100ns: i64,
    ) -> Result<(IMFByteStream, i64), String> {
        self.byte_stream(bytes, playlist_index)
            .map(|stream| (stream, 0))
    }
}

/// The plain read: the bytes, whatever segment is asked for.
pub(super) struct Whole;

impl ReadBytes for Whole {
    fn byte_stream(&self, bytes: &[u8], playlist_index: usize) -> Result<IMFByteStream, String> {
        memory_byte_stream(bytes, playlist_index)
    }
}

impl<R: ReadBytes> SegmentSource for BytesSource<R> {
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
        self.read.byte_stream(&self.bytes, playlist_index)
    }

    fn byte_stream_from(
        &self,
        _lease: &str,
        _segment_index: u64,
        playlist_index: usize,
        from_100ns: i64,
    ) -> Result<(IMFByteStream, i64), String> {
        self.read
            .byte_stream_from(&self.bytes, playlist_index, from_100ns)
    }

    fn is_live(&self, _session_id: &str) -> bool {
        false
    }
}

/// A segment with one audio track starting at the segment's own start.
pub(super) fn segment(index: u64, start_100ns: i64, end_100ns: i64) -> TimelineSegment {
    TimelineSegment {
        index,
        start_100ns,
        end_100ns,
        audio_offsets_100ns: vec![0],
    }
}

/// `Worker::assemble` over `source` and `segments`: no endpoint, no priming
/// seek, nothing playing yet, the live edge where the caller puts it.
pub(super) fn worker_over(
    source: Arc<dyn SegmentSource>,
    session_id: &str,
    segments: Vec<TimelineSegment>,
    live_edge_100ns: i64,
    shared: Arc<PlaybackShared>,
) -> Worker {
    Worker::assemble(
        source,
        TimelineSnapshot {
            session_id: session_id.into(),
            segments,
            live_edge_100ns,
            ..TimelineSnapshot::default()
        },
        EngineParams {
            initial_position_100ns: 0,
            volume_percent: 0,
            muted: true,
            display_target: None,
            notify: Box::new(|| {}),
        },
        shared,
        // Never napped on here: `assemble` runs no ticks (task1620).
        crossbeam_channel::unbounded().1,
    )
}

mod stall_tests {
    use super::*;

    /// t260924-327f: the gate every stall line goes through. Inclusive at the
    /// threshold, so a threshold of zero reports everything -- which is how the
    /// worker test proves the lines are actually emitted.
    #[test]
    fn a_stall_line_is_written_from_the_threshold_up() {
        use std::time::Duration;
        let bar = Duration::from_millis(200);
        assert!(!deserves_a_line(Duration::from_millis(199), bar));
        assert!(deserves_a_line(bar, bar));
        assert!(deserves_a_line(Duration::from_secs(22), bar));
        assert!(deserves_a_line(Duration::ZERO, Duration::ZERO));
        assert_eq!(SLOW_STEP, bar, "the value the task's Steps 3 settled on");
    }
}

mod deadline_tests {
    use super::*;

    #[test]
    fn the_rate_divides_the_wait() {
        let now = Instant::now();
        let due = frame_deadline(now, 0, HNS_PER_SECOND, 2.0);
        assert_eq!((due - now).as_millis(), 500);
        // A frame behind the anchor is due now.
        assert_eq!(frame_deadline(now, HNS_PER_SECOND, 0, 1.0), now);
    }
}

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

/// LiveReview playlist growth (task161). Built through `assemble` with no
/// endpoint and no priming seek, so what runs here is playlist, live edge and
/// park bookkeeping only -- no MF, no WASAPI, no COM.
mod extend_tests {
    use super::*;
    use crate::capture::CaptureController;
    use crate::playback::source::ControllerSource;

    fn segment(index: u64, start_s: i64, end_s: i64) -> TimelineSegment {
        super::segment(index, start_s * HNS_PER_SECOND, end_s * HNS_PER_SECOND)
    }

    /// Segment `index` of a recording at the encoder's own cadence:
    /// `[index * D, (index + 1) * D)`, D being `encoder::segment_duration_100ns()`.
    /// The live-park tests are built from it (t261012-38b1) because the lead
    /// they wait for, `live_resume_lead_100ns()`, is counted in D: literal 2 s
    /// segments under a 1 s D hand a park its whole lead in one extend. The
    /// function, not the const, so `LIVEBACK_SEGMENT_SECS=1` (set at User scope
    /// on one test desk) moves both sides together (t261012-5b97).
    fn cadence(index: u64) -> TimelineSegment {
        let d = crate::encoder::segment_duration_100ns();
        super::segment(index, index as i64 * d, (index as i64 + 1) * d)
    }

    fn worker_with(segments: Vec<TimelineSegment>) -> Worker {
        let live_edge_100ns = segments.last().map(|s| s.end_100ns).unwrap_or(0);
        worker_over(
            Arc::new(ControllerSource::new(CaptureController::new())),
            "task161-extend-test",
            segments,
            live_edge_100ns,
            Arc::new(PlaybackShared::default()),
        )
    }

    fn indexes(worker: &Worker) -> Vec<u64> {
        worker.playlist.iter().map(|entry| entry.index).collect()
    }

    /// Feeds one segment per extend until the parked transport resumes, and
    /// returns how many that took. Two from a park at the edge since
    /// t261012-5b97 (one extend gives D, short of the D + m lead), but the
    /// count belongs to the lead's definition, not to the tests that only
    /// need "more than one".
    fn extends_until_resumed(worker: &mut Worker, first_index: u64) -> u64 {
        for sent in 1..=8 {
            let next = cadence(first_index + sent - 1);
            let live_edge_100ns = next.end_100ns;
            worker.extend_playlist(&[next], live_edge_100ns);
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
        worker.audio_pump.pending_out = vec![0.5; 480];
        worker.audio_pump.stretcher = Some(AudioStretcher::new(2.0));
        worker.audio_pump.audio_align_100ns = Some(HNS_PER_SECOND);
        worker.flush_audio();
        assert!(
            worker.audio_pump.pending_out.is_empty(),
            "the carry survived a flush: {} samples",
            worker.audio_pump.pending_out.len()
        );
        // Task1840: a realignment armed by an earlier seek must not survive
        // either -- once flushed there is nothing left to walk past, and a
        // stale one would excuse real drift later.
        assert_eq!(worker.audio_pump.audio_align_100ns, None);
    }

    /// The other side of it: a pause does not leave the position, so the carry
    /// is still the audio that comes next. Dropping it would put the segment
    /// reader that much ahead of the clock, and the segment's audio would end
    /// early by exactly the discarded amount -- silence at the next crossing.
    #[test]
    fn a_pause_keeps_the_carry() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.audio_pump.pending_out = vec![0.5; 480];
        worker.playing = true;
        worker.pause("toggle");
        assert_eq!(
            worker.audio_pump.pending_out.len(),
            480,
            "the pause dropped the carry"
        );
        // t260924-327f: the stop says why, once -- a second pause of a paused
        // transport is the control -- and the resume closes it.
        worker.pause("range_end");
        assert_eq!(worker.stall_lines, vec!["stopped"]);
        // t260925-8bc3: the line's `reason` is the word the first sender
        // passed; the redundant second pause does not overwrite it.
        assert_eq!(worker.stopped.map(|(_, reason)| reason), Some("toggle"));
        worker.play();
        assert_eq!(worker.stall_lines, vec!["stopped", "resumed"]);
    }

    /// t260917-1a00: a pause landing on a park says so -- the park already
    /// wrote its `stopped` line, so without this the pause that ends the
    /// park's resume leaves no trace. A pause of a transport that was not
    /// parked (above) is the control and writes no such line.
    #[test]
    fn a_pause_on_a_park_says_it_cancelled_the_resume() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.playing = true;
        worker.park_at_live_edge(false);
        assert!(worker.parked_from_play);
        worker.pause("offscreen");
        assert!(!worker.parked_from_play);
        assert_eq!(worker.stall_lines, vec!["stopped", "park_cancelled"]);
    }

    /// Task155: 1x bypasses the stretcher, so crossing it swaps the whole audio
    /// path and what is queued no longer belongs to it. Moving between two
    /// stretched rates keeps the analysis window, which is what makes that
    /// change glitch-free -- so it must *not* flush.
    #[test]
    fn only_crossing_1x_flushes_the_audio_path() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);

        worker.audio_pump.pending_out = vec![0.5; 480];
        worker.set_rate(2.0);
        assert!(
            worker.audio_pump.pending_out.is_empty(),
            "1x -> 2x must flush"
        );

        worker.audio_pump.pending_out = vec![0.5; 480];
        worker.set_rate(0.5);
        assert_eq!(
            worker.audio_pump.pending_out.len(),
            480,
            "2x -> 0.5x stays on the stretcher and must keep the carry"
        );

        worker.set_rate(1.0);
        assert!(
            worker.audio_pump.pending_out.is_empty(),
            "0.5x -> 1x must flush"
        );
    }

    /// Task1840: the flush above is right, but on its own it *destroys* the
    /// media the endpoint was holding -- nothing rewinds the reader, so that
    /// span is never heard and the segment's audio ends early by exactly it.
    /// `pause` documents the same failure from the same call. The crossing has
    /// to re-render those blocks through the new path instead.
    #[test]
    fn a_1x_crossing_re_renders_what_it_flushed() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        let ten_ms = 2 * (AUDIO_SAMPLE_RATE as usize / 100);
        worker.position_100ns = HNS_PER_SECOND;
        worker.rate = 2.0;
        worker.audio_pump.stretcher = Some(AudioStretcher::new(2.0));
        worker.audio_pump.audio_recent = VecDeque::from(vec![
            // Already played: ends before the clock.
            (
                HNS_PER_SECOND - HNS_PER_SECOND / 100,
                vec![vec![0.25; ten_ms]],
            ),
            // Still owed: starts at the clock.
            (HNS_PER_SECOND, vec![vec![0.5; ten_ms]]),
        ]);

        worker.set_rate(1.0);

        // Bypass on the far side, so the owed block comes back verbatim and
        // the heard one does not come back at all.
        assert_eq!(
            worker.audio_pump.pending_out,
            vec![0.5; ten_ms],
            "the 2x -> 1x crossing did not re-render exactly the audio still owed"
        );
        assert!(
            worker.audio_pump.audio_recent.is_empty(),
            "the retention outlived the crossing that consumed it"
        );

        // A change between two stretched rates keeps its analysis window and
        // never flushed, so there is nothing to replay.
        worker.set_rate(2.0);
        worker.audio_pump.pending_out.clear();
        worker.audio_pump.audio_recent =
            VecDeque::from(vec![(HNS_PER_SECOND, vec![vec![0.5; ten_ms]])]);
        worker.set_rate(4.0);
        assert_eq!(
            worker.audio_pump.audio_recent.len(),
            1,
            "2x -> 4x flushed something it should have left alone"
        );
    }

    /// A track muted while paused used to play its old level for as long as
    /// the endpoint's queue on resume: that queue was mixed before the mute.
    /// The change has to re-mix what is queued, and only what is still owed.
    #[test]
    fn a_track_level_change_re_mixes_what_is_queued() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        let ten_ms = 2 * (AUDIO_SAMPLE_RATE as usize / 100);
        worker.position_100ns = HNS_PER_SECOND;
        worker.audio_pump.audio_recent = VecDeque::from(vec![
            (
                HNS_PER_SECOND - HNS_PER_SECOND / 100,
                vec![vec![0.25; ten_ms], vec![0.25; ten_ms]],
            ),
            (HNS_PER_SECOND, vec![vec![0.5; ten_ms], vec![0.25; ten_ms]]),
        ]);
        worker.audio_pump.track_volumes = vec![(100, false), (100, true)];

        worker.remix_queued_audio();

        assert_eq!(
            worker.audio_pump.pending_out,
            vec![0.5; ten_ms],
            "the muted track is still in the queued audio"
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
        let entry = |start: i64, end: i64| TimelineSegment {
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
    // `worker.prefetch.next` after a `prefetch_next` call. That is deliberate: this
    // harness has no segment files behind it, so `open_segment_at` fails and
    // `next` stays `None` whichever way the decision goes. The assertions they
    // replace were written against the worker and were therefore vacuous --
    // they passed with the rules deleted, which is how task740's bug sat under
    // a green test for a whole task. `playlist_position` is still exercised
    // through the real worker, since "which segment is the position in" is the
    // input the rules turn on.

    /// Task203 put the prefetch near the crossing only; t261016-153c widened the
    /// window to the playing entry's length, so the read-ahead starts as the
    /// transport enters a segment (a HDD needs about a second for one read). The
    /// "and only if the frame budget has room for it" half went with task630 --
    /// it was wall-time slack, which shrinks with the rate while the work does
    /// not, so at x1.5 and above it never opened.
    #[test]
    fn a_prefetch_starts_as_the_transport_enters_a_segment() {
        let two_segments = 2;
        let playing = Some(0);
        let two_seconds = 2 * HNS_PER_SECOND;
        let one_second = HNS_PER_SECOND;

        // Just entered a two-second segment: the whole segment is the window.
        // Under task203's one-second window this was `None`.
        assert_eq!(
            prefetch_target(0, two_segments, playing, two_seconds, two_seconds),
            Some(1),
            "the first tick of a two-second segment"
        );
        assert_eq!(
            prefetch_target(0, two_segments, playing, one_second, two_seconds),
            Some(1),
            "half way through a two-second segment"
        );
        // A one-second segment's window is one second, not two.
        assert_eq!(
            prefetch_target(0, two_segments, playing, one_second, one_second),
            Some(1),
            "the first tick of a one-second segment"
        );

        // Short of the entry's start (a gap `playlist_position` snapped
        // across): more left than the entry is long, so nothing yet.
        assert_eq!(
            prefetch_target(0, two_segments, playing, 3 * HNS_PER_SECOND, two_seconds),
            None,
            "still in the gap before the segment"
        );

        // The last segment has nothing after it to read ahead, however close
        // the crossing is.
        assert_eq!(
            prefetch_target(1, two_segments, Some(1), 0, two_seconds),
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
        // Two segments and recording: nothing is held in reserve since
        // t261012-5b97, so the park lands on the live edge.
        let mut worker = worker_with(vec![cadence(0), cadence(1)]);
        worker.live = true;
        worker.park_at_live_edge(false);
        let parked = worker.position_100ns;
        assert_eq!(parked, cadence(1).end_100ns - 1);

        // Two extends, not one: a live park waits for `live_resume_lead_100ns()`
        // (D + m) before resuming (task1800), because resuming on one segment of
        // lead spends it before the next crossing and parks again. What task161's
        // contract still guarantees -- and what this asserts -- is that the
        // resume, when it comes, keeps the parked position instead of
        // restarting from the head.
        worker.extend_playlist(&[cadence(2)], cadence(2).end_100ns);
        assert!(!worker.playing, "one segment of lead is not enough");
        worker.extend_playlist(&[cadence(3)], cadence(3).end_100ns);
        assert!(worker.playing);
        assert_eq!(worker.position_100ns, parked, "the resume kept its place");

        // The case the split is for: one live segment. The park sits on
        // `live_edge - 1`, the playable edge. Under the old routing this is the
        // shape that restarts from the head.
        let mut worker = worker_with(vec![cadence(0)]);
        worker.live = true;
        worker.playing = true;
        worker.park_at_live_edge(false);
        assert!(!worker.playing, "parking stops the transport");
        let parked = worker.position_100ns;
        assert_eq!(parked, cadence(0).end_100ns - 1);
        let head = worker.playlist[0].start_100ns;
        assert!(
            Worker::restarts_from_head(parked, head, worker.live_edge_100ns),
            "the parked position satisfies the head rule -- that is the point"
        );

        // What keeps this from firing today is the *order* inside
        // `extend_playlist`: the edge is taken up before the resume runs, and
        // the only sender's edge is the last segment's end -- so by the time
        // anything looks, the parked position is behind it. That is a property
        // of the sender, not of the transport, which is why the resume no
        // longer asks the question at all.
        let generation = worker.prefetch.generation;
        let extends = extends_until_resumed(&mut worker, 1);
        assert!(
            extends > 1,
            "a live park must not resume on the first extend"
        );
        assert!(worker.playing);
        assert_eq!(worker.position_100ns, parked, "the resume kept its place");
        // A seek would have thrown the read-ahead away on its way past.
        assert_eq!(worker.prefetch.generation, generation);

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
        let mut worker = worker_with(vec![cadence(0), cadence(1)]);
        worker.live = true;
        worker.position_100ns = worker.live_edge_100ns - 1;
        worker.park_at_live_edge(false);
        assert!(!worker.playing, "caught up to the playable edge");

        // One extend is what the old resume accepted, and what the arithmetic
        // says is spent before the next crossing arrives: the transport would
        // be playing the newest segment, and crossing out of it is exactly
        // what parks. D of lead is short of D + m.
        worker.extend_playlist(&[cadence(2)], cadence(2).end_100ns);
        assert!(
            !worker.playing,
            "one segment of lead is not enough to resume on"
        );
        assert!(worker.parked_from_play, "still parked, not cancelled");

        let extends = extends_until_resumed(&mut worker, 3);
        assert_eq!(extends, 1, "the second arrival brings 2D >= D + m");
        assert!(
            worker.live_edge_100ns - worker.position_100ns >= live_resume_lead_100ns(),
            "the resume starts with the lead it waited for"
        );

        // And it keeps running: further extends find it already playing, so
        // `parks` stays at the one stop the buffer fill cost.
        for index in 4..12u64 {
            worker.extend_playlist(&[cadence(index)], cadence(index).end_100ns);
        }
        assert_eq!(
            worker.counters.parks, 1,
            "one park per follow session, not one per crossing"
        );
    }

    /// edge-start-stutter (2026-10-26): a cold start no longer plays its
    /// first segment on arrival -- that ran out of it before the next landed,
    /// parked anyway and resumed 2D behind the edge. It waits for the lead
    /// like any live park, on a still m short of the edge, and starts at the
    /// steady lag D + m.
    #[test]
    fn a_cold_start_waits_for_the_lead_and_starts_at_the_steady_lag() {
        let d = crate::encoder::segment_duration_100ns();
        let cold = || {
            let mut worker = worker_with(Vec::new());
            worker.live = true;
            worker.playing = true;
            worker.park_at_live_edge(false);
            assert!(worker.position_100ns < 0, "parked before any segment");
            worker
        };

        let mut worker = cold();
        worker.extend_playlist(&[cadence(0)], cadence(0).end_100ns);
        assert!(!worker.playing, "one segment is short of the lead");
        assert!(
            worker.parked_from_play,
            "still a park the next extend resumes"
        );
        assert_eq!(
            worker.position_100ns,
            d - LIVE_FOLLOW_MARGIN_100NS,
            "the still sits where the follow will start"
        );
        assert!(
            worker.shared.status.lock().unwrap().cold_start_still,
            "the UI is told the still is up, so it hides the knob"
        );
        worker.extend_playlist(&[cadence(1)], cadence(1).end_100ns);
        assert!(worker.playing);
        assert!(
            !worker.shared.status.lock().unwrap().cold_start_still,
            "and told when it is gone"
        );
        assert_eq!(
            worker.position_100ns,
            2 * d - live_resume_lead_100ns(),
            "the steady lag, from the frame shown"
        );
        assert_eq!(worker.counters.parks, 1);

        // The lead in one extend: straight to the steady lag, not the head.
        let mut worker = cold();
        worker.extend_playlist(&[cadence(0), cadence(1)], cadence(1).end_100ns);
        assert!(worker.playing);
        assert_eq!(worker.position_100ns, 2 * d - live_resume_lead_100ns());
    }

    /// The escape hatch for the hysteresis: a recording that stops while the
    /// transport is still short of its lead sends no further extends, so
    /// nothing would ever resume it. The idle wake is what does (task1800).
    #[test]
    fn a_park_waiting_for_lead_resumes_once_the_recording_stops() {
        let mut worker = worker_with(vec![cadence(0), cadence(1)]);
        worker.live = true;
        worker.position_100ns = worker.live_edge_100ns - 1;
        worker.park_at_live_edge(false);
        worker.extend_playlist(&[cadence(2)], cadence(2).end_100ns);
        assert!(!worker.playing, "still short of the lead it waits for");

        // `poll_parked_resume` asks the controller rather than trusting the
        // `live` an extend asserts. The harness's controller has no active
        // session, which is exactly the "recording stopped" answer.
        worker.poll_parked_resume();
        assert!(
            worker.playing,
            "a stopped recording leaves nothing to wait for"
        );
        assert!(!worker.live, "and it no longer waits for the lead");

        // The same wake must not disturb a transport the user paused, or a
        // deliberate pause would undo itself every 250ms.
        let mut worker = worker_with(vec![segment(0, 0, 2), segment(1, 2, 4)]);
        worker.playing = false;
        worker.parked_from_play = false;
        worker.poll_parked_resume();
        assert!(!worker.playing, "a pause is not a park");
    }

    /// t261012-5b97: a recording holds nothing in reserve any more (task1530
    /// held its newest segment back). The newest segment is read from memory,
    /// so the transport may play into it and park on the live edge itself, and
    /// "following live" means playing that newest segment.
    #[test]
    fn recording_holds_nothing_in_reserve() {
        let d = crate::encoder::segment_duration_100ns();
        let mut worker = worker_with(vec![cadence(0), cadence(1), cadence(2)]);
        worker.live = true;
        assert_eq!(worker.live_edge_100ns, 3 * d);
        worker.park_at_live_edge(false);
        assert_eq!(
            worker.playlist_position(worker.position_100ns),
            Some(2),
            "the park is inside the newest segment"
        );
        assert_eq!(worker.position_100ns, worker.live_edge_100ns - 1);

        // Playing the newest segment raises `at_live_edge`; a pause there is
        // not a park, so it must not keep the flag up.
        worker.position_100ns = 2 * d + d / 2;
        worker.playing = true;
        worker.write_status();
        assert!(worker.shared.status.lock().unwrap().at_live_edge);
        worker.pause("toggle");
        assert!(
            !worker.shared.status.lock().unwrap().at_live_edge,
            "a paused transport mid-segment reads as paused, not parked"
        );

        // On the newest segment is as live as it gets; the one before it is a
        // segment behind.
        assert!(follows_live(Some(2), 3, true));
        assert!(!follows_live(Some(1), 3, true));
        // Nothing under the position says nothing, and a finished session is
        // never "live" however close to its end the playhead sits.
        assert!(!follows_live(None, 3, true));
        assert!(!follows_live(Some(2), 3, false));
    }

    /// t260926-cc7e: following lands the lead a live park waits for, D + m,
    /// short of the live edge (t261012-5b97; the end of the segment before the
    /// newest until then) -- and never before the head.
    #[test]
    fn following_lands_a_park_lead_short_of_the_playable_edge() {
        let d = crate::encoder::segment_duration_100ns();
        let lead = d + LIVE_FOLLOW_MARGIN_100NS;
        assert_eq!(live_resume_lead_100ns(), lead);
        let segments = [cadence(0), cadence(1), cadence(2), cadence(3)];
        let live_edge = 4 * d;
        let now = Duration::ZERO;
        assert_eq!(
            live_follow_landing_100ns(&segments, live_edge, now),
            Some(4 * d - lead)
        );
        // Half a segment after the last arrival: where the park that resumed
        // then has got to.
        let half = Duration::from_nanos(u64::try_from(d / 2 * 100).unwrap());
        assert_eq!(
            live_follow_landing_100ns(&segments, live_edge, half),
            Some(4 * d - lead + d / 2)
        );
        // A late arrival counts one segment, never more: the landing is then m
        // short of the edge, where the next arrival is due.
        assert_eq!(
            live_follow_landing_100ns(&segments, live_edge, Duration::from_secs(60)),
            Some(4 * d - LIVE_FOLLOW_MARGIN_100NS)
        );
        // A young session lands on its head, not before it.
        assert_eq!(live_follow_landing_100ns(&segments[..1], d, now), Some(0));
        assert_eq!(live_follow_landing_100ns(&[], 0, now), None);
    }

    /// The case task1530 was filed for: playback has reached the end of the
    /// playlist while the session is still recording, and `extend_playlist`
    /// arrives with the next segment.
    ///
    /// Parking on `live_edge - 1` and resuming on the next growth crosses out
    /// of the newest segment on the tick right after it, with no read-ahead
    /// window at all. task1530 answered that with a one-segment reserve; since
    /// t261012-5b97 the park sits on the live edge itself and the resume lead
    /// does the same job: it waits for D + m, so the transport plays on with
    /// the segment it crosses into next already in the playlist, and
    /// `prefetch_target` has something to say before the crossing arrives.
    #[test]
    fn a_growth_at_the_live_edge_leaves_room_to_read_ahead() {
        let d = crate::encoder::segment_duration_100ns();
        let mut worker = worker_with(vec![cadence(0), cadence(1)]);
        // `worker_with` builds on an idle controller, so the park does not
        // wait for the lead until something says the session is recording. An
        // `ExtendPlaylist` is that something -- here, straight to the field it
        // sets.
        worker.live = true;

        worker.park_at_live_edge(false);
        assert_eq!(
            worker.position_100ns,
            2 * d - 1,
            "parked on the live edge, nothing held in reserve"
        );

        worker.extend_playlist(&[cadence(2)], 3 * d);
        assert!(!worker.playing, "D of lead is short of D + m");
        worker.extend_playlist(&[cadence(3)], 4 * d);
        assert!(worker.playing, "2D is not");

        let playing = worker.playlist_position(worker.position_100ns);
        assert_eq!(playing, Some(1));
        assert_eq!(
            prefetch_target(1, worker.playlist.len(), playing, 1, d),
            Some(2),
            "the segment the crossing is about to need can be read ahead"
        );

        // A finished session parks on its own end the same way.
        worker.live = false;
        worker.park_at_live_edge(false);
        assert_eq!(worker.position_100ns, 4 * d - 1);
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
            prefetch_target(0, 3, playing, -1, 2 * HNS_PER_SECOND),
            None,
            "a crossing's stale index reads ahead into the segment already playing"
        );

        // The honest call one segment later, which must still go through.
        assert_eq!(
            prefetch_target(1, 3, playing, 0, 2 * HNS_PER_SECOND),
            Some(2),
            "the real next segment"
        );

        // Parked past the live edge: nothing is known about where playback is,
        // so this rule declines nothing.
        assert_eq!(
            prefetch_target(0, 3, None, 0, 2 * HNS_PER_SECOND),
            Some(1),
            "position unknown"
        );
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
        let generation = worker.prefetch.generation;
        worker.prefetch.in_flight = Some((generation, 1));

        worker.abandon_prefetch();

        assert_eq!(worker.prefetch.in_flight, None, "the slot is free again");
        assert!(worker.prefetch.generation > generation);
        assert_eq!(
            prefetch_verdict(
                (generation, 1),
                worker.prefetch.in_flight,
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
    fn extend_does_not_start_a_transport_the_user_paused() {
        let mut worker = worker_with(vec![segment(0, 0, 2)]);
        worker.playing = true;
        worker.park_at_live_edge(false);
        // The user pauses while parked at the live edge: the pending resume is
        // theirs to cancel, and the next segment must not undo that.
        worker.pause("test");
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
        // Not necessarily on arrival since edge-start-stutter: short of the
        // lead it shows a still and waits
        // (`a_cold_start_waits_for_the_lead_and_starts_at_the_steady_lag`).
        // What this guards is that the park survives for an extend to resume.
        assert!(
            worker.playing || worker.parked_from_play,
            "the first segment starts the playback or keeps its park"
        );
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

mod carry_tests {
    use super::*;

    #[test]
    fn a_partly_accepted_block_carries_the_tail_in_order() {
        let mut pending = Vec::new();
        // Four frames offered, two taken: the last two frames wait.
        carry_tail(&mut pending, &[1.0, 1.1, 2.0, 2.1, 3.0, 3.1, 4.0, 4.1], 2);
        assert_eq!(pending, vec![3.0, 3.1, 4.0, 4.1]);

        // A fully accepted block leaves nothing behind.
        let mut pending = Vec::new();
        carry_tail(&mut pending, &[0.1, 0.2, 0.3, 0.4], 2);
        assert!(pending.is_empty());
    }

    #[test]
    fn draining_the_carry_consumes_from_the_front() {
        let mut pending = vec![1.0, 1.1, 2.0, 2.1, 3.0, 3.1];
        assert!(retain_unwritten(&mut pending, 1));
        assert_eq!(pending, vec![2.0, 2.1, 3.0, 3.1]);
        assert!(!retain_unwritten(&mut pending, 2));
        assert!(pending.is_empty());

        // An endpoint that took nothing leaves the carry intact.
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
        // The two frames a skip would be visible on -- a crossing's prefetched
        // first frame and the one a seek asked for -- arrive with `droppable`
        // false (through `must_show_next`, since t260914-8d69), the same guard
        // `should_drop_frame` uses.
        assert!(
            !should_skip_frame(false, Some(Duration::ZERO), STEP_120, Some(PERIOD)),
            "a must_show_next frame is shown however early"
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

        // A 60fps recording on a 60Hz stage loses nothing -- including when the
        // clock anchor puts a frame slightly under the period.
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
mod edge_seek_tests {
    use super::*;

    /// The scrub pair (2026-09-15): coarse to a spot, then precise to the same
    /// spot. The precise seek decodes forward from where the coarse one left
    /// the reader instead of asking the source to seek again -- unless the
    /// frame it has to show is one the reader already handed out.
    #[test]
    fn a_precise_seek_after_a_coarse_one_resumes_from_the_frame_it_left() {
        let _runtime = crate::encoder::mf_test_runtime();
        let directory =
            std::env::temp_dir().join(format!("livia-scrub-pair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let record = crate::export::tests::fixtures::write_video_only_segment(&directory);
        let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
        let delta_100ns = record.end_100ns / 2;
        let (mut worker, shared) = worker_over_record(&bytes, &record, record.end_100ns);
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
        crate::com::init_mta();
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

        let (mut worker, shared) = worker_over_record(&bytes, &record, end_100ns);
        let edge = worker.live_edge_100ns - 1;
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
            assert_eq!(
                worker.audio_pump.audio_align_100ns,
                Some(worker.position_100ns)
            );
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
        let (mut worker, shared) = worker_over_record(&bytes, &record, record.end_100ns);
        let edge = worker.live_edge_100ns - 1;
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
        assert_eq!(
            worker.audio_pump.audio_align_100ns,
            Some(worker.position_100ns)
        );
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
    /// in `do_seek` takes `live_edge_100ns` from. MF converts the position
    /// to the track's own ticks, where `end - 1` rounds onto the track duration
    /// and `SetCurrentPosition` refuses it -- so without the shift the whole
    /// edge-seek landing of t260913-b4a6 never runs on a real recording.
    ///
    /// Both reader states are measured because they behave differently on a
    /// refusal (b4a6): a fresh reader stays at 0 and a used one carries on from
    /// where it stood. Refused is refused either way.
    #[test]
    fn a_production_segment_refuses_a_seek_to_its_own_end() {
        let _runtime = crate::encoder::mf_test_runtime();
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
                crate::playback::segment_reader::open_segment(&bytes, 0).expect("the segment opens")
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

    /// One entry over `record`, ending at `end_100ns`, with the live edge there.
    pub(super) fn worker_over_record(
        bytes: &[u8],
        record: &crate::ring_buffer::SegmentRecord,
        end_100ns: i64,
    ) -> (Worker, Arc<PlaybackShared>) {
        let shared = Arc::new(PlaybackShared::default());
        let worker = worker_over(
            Arc::new(BytesSource::new(bytes.to_vec())),
            "t260913-b4a6",
            vec![segment(record.index, record.start_100ns, end_100ns)],
            end_100ns,
            shared.clone(),
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
mod t260914_1627_tests {
    use super::edge_seek_tests::worker_over_record;

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
        crate::com::init_mta();
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
        let (mut worker, shared) = worker_over_record(&bytes, &record, record.end_100ns);
        // On the CPU pair on purpose: with no display target a decoder texture
        // is now blitted 1:1 on the GPU (t260917-d792), whose first pipelined
        // tick is `Warming` and publishes nothing. These tests count skips.
        worker.gpu_stage.gpu = super::GpuSlot::Absent;
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
            worker.counters.frames_shown, 1,
            "the first tick has to publish, or the scaffold never got going"
        );
        assert!(shared.frame.lock().unwrap().take().is_some());
        assert_eq!(worker.position_100ns, record.start_100ns);
        assert_eq!(
            worker.counters.frames_skipped, 0,
            "the first frame is never skipped"
        );
        let dropped_before = worker.counters.frames_dropped;

        worker.tick();
        assert_eq!(
            worker.position_100ns,
            record.start_100ns + DELTA_100NS,
            "a skipped frame still moves the clock, or x2 stops being x2"
        );
        assert_eq!(
            worker.counters.frames_dropped, dropped_before,
            "a skip is not a late drop: folding it in makes a healthy run look like a failing one"
        );
        assert_eq!(worker.counters.frames_skipped, 1);
        assert_eq!(
            worker.counters.frames_shown, 1,
            "the skipped frame was never built"
        );

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The control for the one above: the same material and the same two ticks
    /// with no refresh rate declared. `display_period()` is `None`, so there is
    /// nothing to skip against and neither frame is skipped.
    #[test]
    fn without_a_known_refresh_rate_the_same_two_ticks_show_both_frames() {
        let _runtime = crate::encoder::MfRuntime::start().expect("MF starts");
        let (directory, record, bytes) = fixture("noskip");
        let (mut worker, shared) = worker_over_record(&bytes, &record, record.end_100ns);
        // On the CPU pair on purpose: with no display target a decoder texture
        // is now blitted 1:1 on the GPU (t260917-d792), whose first pipelined
        // tick is `Warming` and publishes nothing. These tests count skips.
        worker.gpu_stage.gpu = super::GpuSlot::Absent;
        assert!(shared.display_period().is_none());

        worker.tick();
        worker.tick();
        assert_eq!(
            worker.counters.frames_skipped, 0,
            "no display period, nothing to skip against"
        );
        // Whether the second frame is shown or dropped as late depends on wall
        // clock time (the deadline is anchored to `Instant::now()`), so only
        // the sum is pinned: both frames went through a non-skip path.
        assert_eq!(
            worker.counters.frames_shown + worker.counters.frames_dropped,
            2,
            "both frames were handled by a non-skip path (shown or dropped as late)"
        );
        assert_eq!(worker.position_100ns, record.start_100ns + DELTA_100NS);

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }
}

mod t260914_8d69_tests {
    use super::*;

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
        let _runtime = crate::encoder::mf_test_runtime();
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
            worker_over(
                Arc::new(BytesSource::new(bytes.clone())),
                "t260914-8d69",
                vec![
                    segment(record.index, record.start_100ns, record.start_100ns + span),
                    segment(record.index + 1, second_start, second_start + span),
                ],
                second_start + span,
                Arc::new(PlaybackShared::default()),
            )
        };

        // Control first, and on its own worker because `must_show_next` only
        // ever goes up here: a crossing whose prefetch carried no frame arms
        // nothing and queues nothing, exactly as before. Without it the
        // assertions below would also pass if the flag were simply set on
        // every crossing.
        let mut barren = worker_over_two();
        barren.prefetch.next = Some(Prefetched {
            playlist_index: 1,
            reader: crate::playback::segment_reader::open_segment(&bytes, 1)
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
        // t260924-327f control: at the shipped bar a fast crossing writes nothing.
        assert!(barren.stall_lines.is_empty(), "{:?}", barren.stall_lines);
        drop(barren);

        let mut worker = worker_over_two();
        let mut ahead =
            crate::playback::segment_reader::open_segment(&bytes, 1).expect("the segment opens");
        let (local, sample) = ahead
            .next_video_sample()
            .expect("the incoming segment's first frame decodes");
        let expected = second_start + local;
        worker.prefetch.next = Some(Prefetched {
            playlist_index: 1,
            reader: ahead,
            first: Some((expected, sample)),
        });
        // t260924-327f: the bar at zero, so this crossing proves both lines are emitted.
        worker.stall_threshold = Duration::ZERO;

        assert!(worker.cross_to_next(0), "the crossing has a frame to show");
        assert_eq!(worker.stall_lines, vec!["refresh_live", "cross"]);
        assert_eq!(
            worker.counters.crossings_prefetched, 1,
            "the assertions below say nothing unless this crossing took the prefetch"
        );
        let Some(Pending {
            absolute: queued, ..
        }) = worker.pending_frame.as_ref()
        else {
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
        let frame = previous.into_frame(rgba, None);
        assert_eq!((frame.width, frame.height), (8, 4));
        let (width, height, full) = frame.full.expect("the GPU path always carries the planes");
        assert_eq!((width, height), (16, 8));
        assert!(
            full.rgba(width, height).len() == 16 * 8 * 4,
            "the planes converted at their own size, not the arriving frame's"
        );
        // The control: the arriving frame's own pairing differs in every field,
        // so the assertions above cannot pass by coincidence.
        let other = arriving.into_frame(vec![0x22; 16 * 8 * 4], None);
        assert_ne!((other.width, other.height), (frame.width, frame.height));

        // t261016-97b7: a playing decoder-texture frame has no planes, and
        // still carries the recording size -- `full: None` would size the
        // stage off the stage (task990's loop).
        let unread = GpuPending {
            nv12: Vec::new(),
            source: (16, 8),
            scaled: (8, 4),
        }
        .into_frame(vec![0x33; 8 * 4 * 4], None);
        let (width, height, full) = unread.full.expect("the size goes out every frame");
        assert_eq!((width, height), (16, 8));
        assert!(matches!(full, FullFrame::Unread));
    }
}

mod t260915_7088_tests {
    use super::*;

    /// A worker with nothing to play: `pause()` touches no segment, so the
    /// playlist can be empty and no Media Foundation fixture is needed.
    pub(super) fn paused_worker(shared: Arc<PlaybackShared>) -> Worker {
        worker_over(
            Arc::new(BytesSource::new(Vec::new())),
            "t260915-7088",
            Vec::new(),
            0,
            shared,
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
            gpu: None,
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
        assert!(idle.gpu_stage.gpu_pending.is_none());
        idle.pause("test");
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
        idle.gpu_stage.gpu_pending = Some(GpuPending {
            nv12: nv12_flat(source, 0x40),
            source,
            scaled,
        });
        idle.gpu_stage.gpu = GpuSlot::Absent;
        assert!(
            idle.gpu_stage.gpu.get().is_none(),
            "this half runs without a device"
        );
        idle.pause("test");
        assert!(
            idle.gpu_stage.gpu_pending.is_none(),
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
            gpu: None,
            full: None,
        });
        let mut worker = paused_worker(shared.clone());
        worker.gpu_stage.gpu = GpuSlot::Ready(scaler);
        worker.gpu_stage.gpu_pending = Some(GpuPending {
            nv12: light.clone(),
            source,
            scaled,
        });

        worker.pause("test");

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
        assert!(
            worker.gpu_stage.gpu_pending.is_none(),
            "nothing is left in flight"
        );
    }
}

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

mod t260917_d792_tests {
    use super::*;

    /// No stage yet (the process's first poster): a decoder texture is blitted
    /// 1:1 instead of going back through `Lock2D`, while a system-memory frame
    /// still takes the CPU pair. A target that shrinks is used either way.
    #[test]
    fn a_decoder_texture_without_a_target_is_blitted_one_to_one() {
        assert_eq!(blit_dims((1920, 1080), None, true), Some((1920, 1080)));
        assert_eq!(blit_dims((1920, 1080), None, false), None);
        assert_eq!(
            blit_dims((1920, 1080), Some((960, 540)), true),
            Some((960, 540))
        );
        assert_eq!(
            blit_dims((1920, 1080), Some((960, 540)), false),
            Some((960, 540))
        );
        // t260918-8ffa: the near-1:1 band (4510's 1280x720 in a 1244-wide
        // stage) -- a texture is blitted 1:1 since t260915-e6c8, which is why
        // the window size no longer selects the CPU pair; a system-memory
        // frame still takes it.
        assert_eq!(
            blit_dims((1280, 720), Some((1244, 700)), true),
            Some((1280, 720))
        );
        assert_eq!(blit_dims((1280, 720), Some((1244, 700)), false), None);
    }

    /// The `cpu_readback` edge in order: one warn per entry into the slow
    /// path, re-armed by any GPU result but `Failed`, and a driver that fails
    /// every frame warns once.
    #[test]
    fn the_cpu_readback_warns_once_per_entry_and_failed_does_not_rearm_it() {
        let mut latched = false;
        assert!(readback_edge(&mut latched, true), "first texture warns");
        assert!(
            !readback_edge(&mut latched, true),
            "staying in it is silent"
        );

        clear_readback_edge(&mut latched, &gpu::Converted::This);
        assert!(!latched, "a working GPU pair re-arms the edge");
        assert!(
            readback_edge(&mut latched, true),
            "the next entry warns again"
        );

        clear_readback_edge(&mut latched, &gpu::Converted::Failed);
        assert!(latched, "Failed leaves it latched");
        assert!(
            !readback_edge(&mut latched, true),
            "a driver refusing every frame warns once"
        );

        assert!(
            !readback_edge(&mut latched, false),
            "a CPU frame never warns"
        );
        assert!(!latched, "and leaves the slow path");
    }
}

mod t261016_153c_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `BytesSource` behind a slow disk: every read takes `delay` and is
    /// counted, so the test can hold the prefetch in flight across the crossing
    /// and see how many times the segment was read.
    struct SlowRead {
        delay: Duration,
        reads: Arc<AtomicUsize>,
    }

    impl ReadBytes for SlowRead {
        fn byte_stream(
            &self,
            bytes: &[u8],
            playlist_index: usize,
        ) -> Result<IMFByteStream, String> {
            std::thread::sleep(self.delay);
            self.reads.fetch_add(1, Ordering::SeqCst);
            memory_byte_stream(bytes, playlist_index)
        }
    }

    /// Two two-second entries over the same fixture bytes, behind a 400ms
    /// disk, with the transport at the start of the first. The caller keeps
    /// `directory` for cleanup and `reads` to count what was read.
    fn slow_two_segment_worker(
        tag: &str,
    ) -> (
        crate::encoder::MfRuntime,
        Worker,
        Arc<AtomicUsize>,
        crate::ring_buffer::SegmentRecord,
        std::path::PathBuf,
    ) {
        let runtime = crate::encoder::mf_test_runtime();
        let directory = std::env::temp_dir().join(format!("livia-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let record = crate::export::tests::fixtures::write_video_only_segment(&directory);
        let bytes = std::fs::read(&record.path).expect("fixture segment bytes");
        // Two-second entries, as the shipped encoder writes them, so the read-ahead
        // window is checked too: the fixture's media is shorter, which
        // nothing here reads past its first frame.
        let span = 2 * HNS_PER_SECOND;
        let second_start = record.start_100ns + span;
        let reads = Arc::new(AtomicUsize::new(0));
        let mut worker = worker_over(
            Arc::new(BytesSource {
                bytes,
                read: SlowRead {
                    delay: Duration::from_millis(400),
                    reads: reads.clone(),
                },
            }),
            "t261016-153c",
            vec![
                segment(record.index, record.start_100ns, record.start_100ns + span),
                segment(record.index + 1, second_start, second_start + span),
            ],
            second_start + span,
            Arc::new(PlaybackShared::default()),
        );
        // The transport has just entered segment 0: two seconds left, which
        // task203's one-second window did not read ahead on.
        worker.position_100ns = record.start_100ns;
        (runtime, worker, reads, record, directory)
    }

    /// A crossing that arrives while the read-ahead of the incoming segment is
    /// still being read waits for it instead of opening the segment a second
    /// time on the playback thread (10-09..15 on an HDD buffer: 268 crossings
    /// read their segment twice).
    #[test]
    fn a_crossing_waits_for_the_prefetch_still_in_flight() {
        let (_runtime, mut worker, reads, _record, directory) =
            slow_two_segment_worker("t261016-153c");

        worker.prefetch_next(0);
        assert_eq!(
            worker.prefetch.in_flight.map(|(_, index)| index),
            Some(1),
            "the read-ahead starts on entering the segment and is in flight at the crossing"
        );
        assert!(worker.prefetch.next.is_none(), "and has not come back yet");

        worker.cross_to_next(0);

        assert_eq!(
            worker.counters.crossings_prefetched, 1,
            "the crossing took the read-ahead instead of opening the segment itself"
        );
        assert_eq!(
            worker
                .current
                .as_ref()
                .map(|current| current.playlist_index),
            Some(1)
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the incoming segment was read once, by the fetch thread"
        );

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// A seek into the segment whose read-ahead is still in flight waits for
    /// that read instead of discarding it and reading the segment again on the
    /// playback thread (t261017-cd44: 6 of 23 seeks on 10-16).
    #[test]
    fn a_seek_into_the_segment_in_flight_waits_for_its_prefetch() {
        let (_runtime, mut worker, reads, _record, directory) =
            slow_two_segment_worker("t261017-cd44-same");
        let second_start = worker.playlist[1].start_100ns;
        worker.prefetch_next(0);
        assert_eq!(
            worker.prefetch.in_flight.map(|(_, index)| index),
            Some(1),
            "the read-ahead of segment 1 is in flight at the seek"
        );

        worker.do_seek(second_start + 1, false);

        assert_eq!(
            worker
                .current
                .as_ref()
                .map(|current| current.playlist_index),
            Some(1)
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "segment 1 was read once, by the fetch thread"
        );
        assert_eq!(worker.counters.prefetches_abandoned, 0);
        assert_eq!(worker.counters.prefetches_stale, 0);

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// The boundary of the above: a seek into another segment still throws the
    /// read-ahead away, and its reader is never made current. It comes back
    /// stale and is counted as such.
    #[test]
    fn a_seek_elsewhere_discards_the_prefetch_in_flight() {
        let (_runtime, mut worker, reads, record, directory) =
            slow_two_segment_worker("t261017-cd44-other");
        worker.prefetch_next(0);
        assert_eq!(worker.prefetch.in_flight.map(|(_, index)| index), Some(1));

        worker.do_seek(record.start_100ns + 1, false);

        assert_eq!(
            worker
                .current
                .as_ref()
                .map(|current| current.playlist_index),
            Some(0),
            "the seek target is current, not the read-ahead"
        );
        assert!(worker.prefetch.next.is_none());
        assert_eq!(
            worker.prefetch.in_flight, None,
            "the read-ahead was disowned"
        );
        // The disowned read still finishes and comes back to nobody.
        let response = worker
            .prefetch
            .fetcher
            .recv()
            .expect("the fetch thread answers");
        worker.take_prefetch_response(response);
        assert!(worker.prefetch.next.is_none(), "and is not kept");
        assert_eq!(worker.counters.prefetches_stale, 1);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "segment 0 by the seek, segment 1 by the read-ahead"
        );

        drop(worker);
        let _ = std::fs::remove_dir_all(&directory);
    }
}

mod t261017_3f94_tests {
    use super::*;
    use crate::ring_buffer::container::fragment_table;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// The review source's cut over in-memory bytes: `byte_stream_from` serves
    /// the head plus the fragments from the target's GOP, by the production
    /// table and rule, as `read_located_from` does out of the container.
    /// `cuts: false` is the same source without the cut -- the control. Every
    /// open is counted by the kind of read it asked for.
    struct CuttingRead {
        cuts: bool,
        whole_reads: Arc<AtomicUsize>,
        from_reads: Arc<AtomicUsize>,
    }

    impl ReadBytes for CuttingRead {
        fn byte_stream(
            &self,
            bytes: &[u8],
            playlist_index: usize,
        ) -> Result<IMFByteStream, String> {
            self.whole_reads.fetch_add(1, Ordering::SeqCst);
            memory_byte_stream(bytes, playlist_index)
        }

        fn byte_stream_from(
            &self,
            all: &[u8],
            playlist_index: usize,
            from_100ns: i64,
        ) -> Result<(IMFByteStream, i64), String> {
            self.from_reads.fetch_add(1, Ordering::SeqCst);
            let table = fragment_table(all).expect("a production segment has a table");
            let bytes = match table.cut(from_100ns).filter(|_| self.cuts) {
                Some((cut, gop)) => {
                    let head = &all[..table.head_len as usize];
                    let rest = &all[table.rows[cut].offset as usize..];
                    return Ok((
                        memory_byte_stream(&[head, rest].concat(), playlist_index)?,
                        gop,
                    ));
                }
                None => all,
            };
            Ok((memory_byte_stream(bytes, playlist_index)?, 0))
        }
    }

    const SPAN: i64 = 4 * HNS_PER_SECOND;

    /// Three 4s entries over one 4s, 120fps, 8-GOP production segment.
    fn worker(bytes: &[u8], cuts: bool) -> (Worker, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let whole_reads = Arc::new(AtomicUsize::new(0));
        let from_reads = Arc::new(AtomicUsize::new(0));
        let entry = |index: u64| segment(index, index as i64 * SPAN, (index as i64 + 1) * SPAN);
        let worker = worker_over(
            Arc::new(BytesSource {
                bytes: bytes.to_vec(),
                read: CuttingRead {
                    cuts,
                    whole_reads: whole_reads.clone(),
                    from_reads: from_reads.clone(),
                },
            }),
            "t261017-3f94",
            vec![entry(0), entry(1), entry(2)],
            3 * SPAN,
            Arc::new(PlaybackShared::default()),
        );
        (worker, whole_reads, from_reads)
    }

    fn encode() -> (crate::encoder::MfRuntime, Vec<u8>) {
        let runtime = crate::encoder::mf_test_runtime();
        let bytes = crate::export::tests::fixtures::encode_production_segment(
            crate::encoder::VideoCodec::H264_HIGH,
            120,
            480,
        );
        (runtime, bytes)
    }

    fn readable_from(worker: &Worker) -> Option<i64> {
        worker.current.as_ref().map(|current| current.readable_from)
    }

    /// A seek into a later GOP opens the segment from that GOP; a seek further
    /// on stays on that reader; a seek back before its start opens the
    /// segment again, whole, and shows the frame it asked for. Every shown
    /// frame matches the same seeks on a source that never cuts.
    ///
    /// cargo test --lib a_seek_back_past_a_cut_reader -- --ignored --nocapture
    #[test]
    #[ignore = "t261017-3f94 cut seek: costs an encode session, run alone"]
    fn a_seek_back_past_a_cut_reader_reopens_the_segment_whole() {
        let (_runtime, bytes) = encode();
        let gop = fragment_table(&bytes)
            .and_then(|table| table.cut(22_500_000))
            .map(|(_, gop)| gop)
            .expect("2.25s lies in a later GOP");
        let (mut cutting, whole_reads, from_reads) = worker(&bytes, true);
        let (mut control, _, _) = worker(&bytes, false);
        let mut shown = Vec::new();
        for target in [22_500_000, 32_500_000, 2_500_000] {
            cutting.do_seek(target, true);
            control.do_seek(target, true);
            assert!(cutting.last_seek.1.is_some(), "a frame at {target}");
            assert_eq!(
                cutting.last_seek.1, control.last_seek.1,
                "the frame at {target}"
            );
            shown.push((
                readable_from(&cutting),
                cutting.cut_reads,
                whole_reads.load(Ordering::SeqCst),
                from_reads.load(Ordering::SeqCst),
            ));
        }
        assert_eq!(
            shown,
            [
                // Cut to the GOP at 2.0s.
                (Some(gop), 1, 0, 1),
                // Ahead of the reader's start: no read at all.
                (Some(gop), 1, 0, 1),
                // Behind it: opened again, whole.
                (Some(0), 1, 1, 1),
            ]
        );
        assert_eq!(
            (readable_from(&control), control.cut_reads),
            (Some(0), 0),
            "the control never cut"
        );
    }

    /// Only a seek passes its time: the crossing into the next segment and
    /// the read-ahead of the one after read whole, with a cut reader current.
    ///
    /// cargo test --lib crossings_and_read_aheads_read_whole -- --ignored --nocapture
    #[test]
    #[ignore = "t261017-3f94 cut seek: costs an encode session, run alone"]
    fn crossings_and_read_aheads_read_whole() {
        let (_runtime, bytes) = encode();
        let (mut worker, whole_reads, from_reads) = worker(&bytes, true);
        worker.do_seek(22_500_000, false);
        assert_eq!(
            (worker.cut_reads, whole_reads.load(Ordering::SeqCst)),
            (1, 0)
        );

        worker.cross_to_next(0);
        assert_eq!(
            worker
                .current
                .as_ref()
                .map(|current| (current.playlist_index, current.readable_from)),
            Some((1, 0))
        );
        assert_eq!(
            whole_reads.load(Ordering::SeqCst),
            1,
            "the crossing read whole"
        );

        worker.prefetch_next(1);
        let response = worker
            .prefetch
            .fetcher
            .recv()
            .expect("the fetch thread answers");
        worker.take_prefetch_response(response);
        assert!(worker.prefetch.next.is_some(), "the read-ahead came back");
        assert_eq!(whole_reads.load(Ordering::SeqCst), 2, "and read whole");
        assert_eq!(
            (from_reads.load(Ordering::SeqCst), worker.cut_reads),
            (1, 1),
            "the seek was the only read given a time"
        );
    }
}
