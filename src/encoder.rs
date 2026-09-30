use std::path::PathBuf;

use serde::Serialize;

use crate::capture::CaptureSize;
use windows::Win32::Media::MediaFoundation::{IMFSample, MFShutdown, MFStartup, MFSTARTUP_FULL};

pub const SEGMENT_DURATION_100NS: i64 = 20_000_000;

/// How long a video sample may sit unaccepted by the fMP4 sink before the
/// capture is failed (t260911-e7f3).
///
/// The video arm's twin of `capture::worker::PENDING_AAC_LIMIT`, and
/// deliberately its own constant rather than a shared one: the two arms fail
/// for the same reason and five seconds is the same judgement, but neither
/// value is the other's to move. Below it a refusal is back-pressure -- the
/// sink is waiting for the audio the capture loop is about to offer it, and the
/// sample is held and offered again. Above it the sink has stopped accepting,
/// which is a real failure and has to be reported rather than absorbed
/// (task410's disease).
pub const HELD_VIDEO_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderConfig {
    pub output_dir: PathBuf,
    pub output_size: CaptureSize,
    pub frame_rate: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VideoSegmentMetadata {
    pub index: u64,
    pub start_timestamp_100ns: i64,
    pub end_timestamp_100ns: i64,
    /// Where the finalized mp4 landed. Empty for a container recording, which
    /// never gives a segment a file of its own -- read `bytes` instead.
    pub path: PathBuf,
    /// The finalized mp4 itself, for a session whose muxer writes into memory
    /// (task450). `None` means the file arm wrote `path` and nobody is holding
    /// the segment in RAM.
    ///
    /// `#[serde(skip)]` because this struct is only ever serialized for
    /// diagnostics: a few megabytes of mp4 has no business in a log line, and
    /// the manifest stores `SegmentRecord`, not this.
    #[serde(skip)]
    pub bytes: Option<Vec<u8>>,
    pub resolution: CaptureSize,
    pub fps: u8,
    pub bitrate: u32,
    pub keyframe_first: bool,
    /// See `OpenSegment::audio_offsets_100ns` (task204/task1260), one entry per
    /// audio track in track order. Carried to the manifest
    /// so readers can undo the shift and drop the duplicated access unit.
    pub audio_offsets_100ns: Vec<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncoderStartErrorKind {
    NoHardwareEncoder,
    UnsupportedFormat,
    Device,
    Io,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncoderStartError {
    pub kind: EncoderStartErrorKind,
    pub diagnostics: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EncoderEvent {
    Finalized(VideoSegmentMetadata),
    AudioTelemetry {
        last_audio_pts_100ns: i64,
        last_video_pts_100ns: i64,
        max_abs_drift_100ns: i64,
        reinitializations: u32,
    },
    AudioUnavailable,
}

/// What came of offering one AAC sample to the muxer (task610).
///
/// `accepted == false` is **not** an error: the fMP4 sink is refusing input
/// until the capture thread drains the H.264 encoder into it, so the caller has
/// to hold this sample, go round its loop once, and offer the *same* sample
/// again. `events` is whatever was produced before the refusal -- a segment
/// finalize can already be in there -- and must be emitted either way.
#[derive(Debug)]
pub struct AacPush {
    pub events: Vec<EncoderEvent>,
    pub accepted: bool,
}

impl AacPush {
    fn accepted(events: Vec<EncoderEvent>) -> Self {
        Self {
            events,
            accepted: true,
        }
    }

    fn refused(events: Vec<EncoderEvent>) -> Self {
        Self {
            events,
            accepted: false,
        }
    }
}

/// The capture worker's queue-and-retry loop, at test scale (task1000).
///
/// A refusal is protocol, not failure: the sink is asking the caller to drain
/// the video encoder into it and offer the same sample again, which is what
/// `capture::worker::flush_pending_aac` does. A test that generates audio on a
/// fixed timeline while video arrives at the encoder's own pace will meet one
/// eventually -- raising the H.264 profile deepens the encoder's pipeline and
/// makes it sooner -- and `expect_accepted` there reports working backpressure
/// as a defect. The 2ms sleeps dotted through these tests were the older, and
/// weaker, answer to the same thing.
///
/// Nothing may go missing on the way: [`Self::assert_drained`] checks that
/// every sample offered was eventually taken, so a genuine audio-dropping bug
/// still fails the tests it used to fail.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct PendingAac {
    queue: std::collections::VecDeque<IMFSample>,
    offered: usize,
    accepted: usize,
}

#[cfg(test)]
impl PendingAac {
    /// Queues `sample` behind anything already waiting, then offers the queue
    /// oldest-first -- the order the worker preserves too.
    pub(crate) fn offer(
        &mut self,
        muxer: &mut SegmentMuxer,
        sample: &IMFSample,
    ) -> Vec<EncoderEvent> {
        self.queue.push_back(sample.clone());
        self.offered += 1;
        self.flush(muxer)
    }

    /// Re-offers what the sink refused earlier, stopping at the first refusal.
    /// Call it as video drains, or the queue has no reason to move.
    pub(crate) fn flush(&mut self, muxer: &mut SegmentMuxer) -> Vec<EncoderEvent> {
        let mut events = Vec::new();
        while let Some(sample) = self.queue.front() {
            let push = muxer.push_aac_sample(sample).expect("AAC push");
            events.extend(push.events);
            if !push.accepted {
                break;
            }
            self.queue.pop_front();
            self.accepted += 1;
        }
        events
    }

    /// Every sample offered reached the muxer, and none is still waiting.
    pub(crate) fn assert_drained(&self) {
        assert!(
            self.queue.is_empty(),
            "the fMP4 sink never took {} queued AAC samples",
            self.queue.len()
        );
        assert_eq!(
            self.accepted, self.offered,
            "every AAC sample offered must reach the muxer"
        );
    }
}

mod export_writer;
pub use export_writer::Mp4ExportWriter;

mod hardware;
pub(crate) use hardware::release_mft_activates;
pub use hardware::{
    attach_d3d11_manager, av1_recording_supported, count_av1_decoders, ensure_recording_codec,
    HardwareEncoderCandidate, HardwareVideoEncoder, VideoCodec,
};

mod segment_writer;
pub use segment_writer::Mp4SegmentWriter;

// The memory byte stream behind `Mp4SegmentWriter::create_in_memory` (task420,
// on task402's spike).
mod memory_sink;

pub struct MfRuntime;

impl MfRuntime {
    pub fn start() -> Result<Self, EncoderStartError> {
        unsafe { MFStartup(0x0002_0070, MFSTARTUP_FULL) }.map_err(|error| EncoderStartError {
            kind: EncoderStartErrorKind::Device,
            diagnostics: format!("MFStartup failed: {error}"),
        })?;
        Ok(Self)
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

// `pub(crate)` because the read paths need it too: `playback::segment_reader`
// and `export::probe` both open a segment through
// `mp4_boxes::unstraddle_fragments` (task1920).
pub(crate) mod mp4_boxes;
#[cfg(test)]
#[cfg(test)]
use mp4_boxes::{parse_tfhd, parse_trun, read_boxes, read_u32, Mp4BoxEntry};

struct OpenSegment {
    writer: Mp4SegmentWriter,
    index: u64,
    start_timestamp_100ns: i64,
    end_timestamp_100ns: i64,
    /// How far each audio track was shifted so its priming access unit could
    /// sit at local time 0 (task204), one entry per track in track order
    /// (task1260). Zero means no priming, which is only ever the first segment
    /// of a recording -- or a track whose priming unit was stale.
    audio_offsets_100ns: Vec<i64>,
    /// While this segment sits in `closing`: which tracks have already sent a
    /// sample belonging to the *next* segment. A closing segment is finalized
    /// only once every track has moved on, or its slowest track would find its
    /// segment already gone (task1260).
    audio_moved_on: Vec<bool>,
}

/// How old the AAC priming unit may be and still be worth repeating at the head
/// of a new segment (task610). One AAC-LC block is 1024 samples -- 21.3ms at
/// 48kHz -- so 100ms is several blocks of slack for ordinary jitter and far
/// below the multi-hundred-ms gaps a stalled capture thread leaves behind.
const MAX_PRIMING_AGE_100NS: i64 = 1_000_000;

/// Whether the last AAC access unit is recent enough to be this segment's
/// priming unit (task610).
///
/// It is repeated at local time 0 and the whole audio track is shifted by its
/// age, so a stale one puts every later sample that far ahead of the video --
/// which the fMP4 sink refuses outright, and the refusal is what used to kill
/// the recording. A unit from seconds ago is not the predecessor of anything in
/// this segment anyway.
pub(crate) fn priming_is_fresh(segment_start_100ns: i64, priming_100ns: i64) -> bool {
    segment_start_100ns.saturating_sub(priming_100ns) <= MAX_PRIMING_AGE_100NS
}

/// Owns only compressed muxing and segment boundaries, for whichever codec the
/// encoder produced -- it never parses the bitstream, it hands the encoder's
/// media type to the fMP4 sink and the sink writes the sample entry (task1750).
/// Caller keeps original capture timestamps; MP4 samples are rebased to zero for
/// every independent file.
pub struct SegmentMuxer {
    config: EncoderConfig,
    output_type: windows::Win32::Media::MediaFoundation::IMFMediaType,
    aac_output_type: Option<windows::Win32::Media::MediaFoundation::IMFMediaType>,
    /// How many audio tracks each segment carries: 0 with no audio at all,
    /// 1 for the capture target alone, up to 4 with the extra applications
    /// (task1260).
    audio_tracks: usize,
    open: Option<OpenSegment>,
    /// The segment that was `open` immediately before the current one, kept
    /// alive (not yet finalized) so AAC access units that arrive late relative
    /// to the video clean point that rotated the segment still have somewhere
    /// to land instead of being discarded (Task062). At most one segment is
    /// ever held here alongside `open` — never a third.
    closing: Option<OpenSegment>,
    next_index: u64,
    boundary_requested: bool,
    /// Video samples the fMP4 sink has refused, oldest first (t260911-e7f3).
    ///
    /// The audio arm's queue lives in the capture loop because that is where
    /// AAC arrives; the video arm's lives here because video arrives *through*
    /// this struct, and a refusal has to stop the rest of a `poll_to_muxer`
    /// backlog from being poured in behind it. They hold retained copies, not
    /// the MFT's own output samples, which it reclaims on its next event.
    pending_video: std::collections::VecDeque<IMFSample>,
    /// When the sink first refused what is still queued, or `None` when it is
    /// keeping up. `HELD_VIDEO_LIMIT` is measured from here.
    pending_video_since: Option<std::time::Instant>,
    /// AAC samples dropped because no segment had opened yet (before the first
    /// video clean point). Counted, never silent.
    dropped_audio_before_open: u32,
    /// AAC samples dropped because they belonged to a segment that had already
    /// finalized by the time they arrived (no `closing` slot left to catch
    /// them). Counted, never silent.
    dropped_audio_after_close: u32,
    /// The most recent AAC access unit written to any segment, with its
    /// capture timestamp (task204).
    ///
    /// AAC-LC reconstructs each block by overlap-adding it with its
    /// predecessor, so the first access unit of a segment decoded by a fresh
    /// decoder comes out as a ~21ms fade-in from near silence -- measured at
    /// -3.0dB over the block and rising monotonically to level, at every seam.
    /// Repeating the previous unit at the head of each new segment gives that
    /// decoder the predecessor it needs; readers throw the duplicate away.
    /// One slot per audio track, in track order (task1260): the streams are
    /// independent, and one being stale says nothing about the others.
    last_aac: Vec<Option<(IMFSample, i64)>>,
    /// Whether each segment is muxed into memory instead of its own file
    /// (task450). A container recording has nowhere to put a per-segment mp4,
    /// so the bytes come back from `finalize_into_bytes` and travel to the
    /// index writer, which is the only thing that touches the `.lvb`.
    in_memory: bool,
}

mod inspect;
pub use inspect::*;
#[cfg(test)]
pub(crate) use mp4_boxes::dump_video_sample_durations;

impl SegmentMuxer {
    pub fn new(
        config: EncoderConfig,
        output_type: windows::Win32::Media::MediaFoundation::IMFMediaType,
    ) -> Self {
        Self {
            config,
            output_type,
            aac_output_type: None,
            audio_tracks: 0,
            open: None,
            closing: None,
            next_index: 0,
            boundary_requested: false,
            pending_video: std::collections::VecDeque::new(),
            pending_video_since: None,
            dropped_audio_before_open: 0,
            dropped_audio_after_close: 0,
            last_aac: Vec::new(),
            in_memory: false,
        }
    }

    /// Mux every segment into memory rather than into its own file (task450).
    ///
    /// A builder rather than another `EncoderConfig` field: the destination is
    /// a property of *this recording's* muxer, and `EncoderConfig` is built at
    /// 24 call sites that have no opinion about it.
    pub fn writing_into_memory(mut self) -> Self {
        self.in_memory = true;
        self
    }

    /// AAC samples dropped before any segment had ever opened. Exposed for
    /// diagnostics/tests; production code only needs to know this never grows
    /// unboundedly silent (every increment is paired with a `tracing` log line).
    #[cfg(test)]
    pub(crate) fn dropped_audio_before_open(&self) -> u32 {
        self.dropped_audio_before_open
    }

    /// AAC samples dropped because no `closing` segment existed to hold them
    /// (see [`Self::push_aac_sample`] branch (c) -- in practice always because
    /// `open` is still segment 0, not because a segment had finalized). See
    /// [`Self::dropped_audio_before_open`].
    #[cfg(test)]
    pub(crate) fn dropped_audio_after_close(&self) -> u32 {
        self.dropped_audio_after_close
    }

    /// One audio track: the capture target, which is every recording that
    /// predates multi-track audio.
    pub fn with_aac(
        config: EncoderConfig,
        output_type: windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac_output_type: windows::Win32::Media::MediaFoundation::IMFMediaType,
    ) -> Self {
        Self::with_aac_tracks(config, output_type, aac_output_type, 1)
    }

    /// `audio_tracks` counts the target as track 0 (task1260), so 1 is the
    /// single-track shape and 4 is the ceiling the settings clamp allows.
    pub fn with_aac_tracks(
        config: EncoderConfig,
        output_type: windows::Win32::Media::MediaFoundation::IMFMediaType,
        aac_output_type: windows::Win32::Media::MediaFoundation::IMFMediaType,
        audio_tracks: usize,
    ) -> Self {
        let audio_tracks = audio_tracks.max(1);
        let mut muxer = Self::new(config, output_type);
        muxer.aac_output_type = Some(aac_output_type);
        muxer.audio_tracks = audio_tracks;
        muxer.last_aac = vec![None; audio_tracks];
        muxer
    }

    /// H.264 clean point remains segment authority. AAC access units use their start
    /// time and are never split across MP4 files.
    ///
    /// WASAPI/AAC encode latency means audio for a segment keeps arriving for a
    /// short while (up to ~21ms observed) after the video clean point that
    /// rotates `open` to a new segment. Every case is handled explicitly — none
    /// of them silently drop a sample without counting it (Task062):
    /// (a) no segment has ever opened yet: dropped, counted.
    /// (b) sample belongs to the segment that just closed (`timestamp` is
    ///     before the new `open`'s start) and that segment is still held in
    ///     `closing`: written there.
    /// (c) same as (b), but there is no `closing` to write into: it already
    ///     finalized, a third rotation happened without this sample ever
    ///     arriving, or -- the only case measured in practice -- nothing has
    ///     ever closed because `open` is still segment 0. Dropped, counted.
    /// (d) sample belongs to the current `open` segment: written there. Any
    ///     still-pending `closing` has now received all the audio it will ever
    ///     get (AAC timestamps are monotonic), so it is finalized here.
    ///
    /// Branch (c) is not a rotation defect, measured in task2060: all 123
    /// occurrences in 8 days of production logs (122 sessions, 24,756
    /// rotations) and all 3 of 3 fresh single-capture runs landed in segment
    /// 0, ~1 per session and never more than 2 — not one on a real rotation.
    /// The AU is audio captured before the recording's first video frame, so
    /// no earlier segment exists to hold it; only its 6.8–9.5ms tail overlaps
    /// the recording, which is why the audio starts that much after the
    /// video. The material is intact: 13,461 AUs across 142 segment seams
    /// were all exactly 21.3333ms apart, with no hole anywhere.
    pub fn push_aac_sample(&mut self, sample: &IMFSample) -> Result<AacPush, EncoderStartError> {
        self.push_aac_sample_on(0, sample)
    }

    /// The same, for one specific audio track (task1260). Track 0 is the
    /// capture target; the rest are the extra applications, in settings order.
    pub fn push_aac_sample_on(
        &mut self,
        track: usize,
        sample: &IMFSample,
    ) -> Result<AacPush, EncoderStartError> {
        let mut events = Vec::new();
        let Some(open_start) = self.open.as_ref().map(|open| open.start_timestamp_100ns) else {
            self.dropped_audio_before_open += 1;
            tracing::debug!(
                total_dropped = self.dropped_audio_before_open,
                "dropping AAC sample: no segment has opened yet"
            );
            return Ok(AacPush::accepted(events));
        };
        let timestamp = unsafe {
            sample
                .GetSampleTime()
                .map_err(|error| device_error("AAC output timestamp", error))?
        };
        if timestamp < open_start {
            match self.closing.as_ref() {
                Some(closing) => {
                    if !closing.writer.write_aac_sample_on(
                        track,
                        sample,
                        timestamp - closing.start_timestamp_100ns
                            + closing.audio_offsets_100ns.get(track).copied().unwrap_or(0),
                    )? {
                        return Ok(AacPush::refused(events));
                    }
                    self.remember_priming(track, sample, timestamp);
                }
                None => {
                    self.dropped_audio_after_close += 1;
                    tracing::warn!(
                        total_dropped = self.dropped_audio_after_close,
                        timestamp,
                        open_start,
                        "dropping AAC sample: it predates the open segment and there \
                         is no closing segment to hold it -- normally audio captured \
                         before the recording's first video frame (task2060)"
                    );
                }
            }
            return Ok(AacPush::accepted(events));
        }
        // This track has moved on to the new segment. The closing one is
        // finalized only when *every* track has (task1260) -- finalizing on the
        // first would take the segment out from under the slower tracks, and
        // their remaining samples would be counted as
        // `dropped_audio_after_close`.
        if let Some(closing) = self.closing.as_mut() {
            if let Some(moved) = closing.audio_moved_on.get_mut(track) {
                *moved = true;
            }
            if closing.audio_moved_on.iter().all(|moved| *moved) {
                let closing = self.closing.take().expect("checked just above");
                events.push(self.finalize_segment(closing)?);
            }
        }
        let open = self.open.as_ref().expect("checked open_start above");
        let relative =
            timestamp - open_start + open.audio_offsets_100ns.get(track).copied().unwrap_or(0);
        if !open.writer.write_aac_sample_on(track, sample, relative)? {
            // The finalize above (if there was one) has already happened and is
            // in `events`; `self.closing` is taken, so the retry will not repeat
            // it. Priming deliberately not remembered until the sample lands.
            return Ok(AacPush::refused(events));
        }
        self.remember_priming(track, sample, timestamp);
        Ok(AacPush::accepted(events))
    }

    /// Keeps the newest access unit so the *next* segment can open with it
    /// (task204). Only the latest is ever needed: AAC overlap-adds with exactly
    /// one predecessor.
    fn remember_priming(&mut self, track: usize, sample: &IMFSample, timestamp_100ns: i64) {
        let Some(slot) = self.last_aac.get_mut(track) else {
            return;
        };
        if slot
            .as_ref()
            .is_none_or(|(_, previous)| timestamp_100ns >= *previous)
        {
            *slot = Some((sample.clone(), timestamp_100ns));
        }
    }

    /// Where the open segment is putting audio: index, its start timestamp, and
    /// the offset the whole audio track is shifted by (task610). The three
    /// numbers that decide the local time an AAC sample lands at, which is what
    /// a refusing sink is a symptom of.
    pub fn open_audio_placement(&self) -> Option<(u64, i64, i64)> {
        self.open.as_ref().map(|open| {
            (
                open.index,
                open.start_timestamp_100ns,
                open.audio_offsets_100ns.first().copied().unwrap_or(0),
            )
        })
    }

    /// The currently-open segment's index, if any (`None` before the first
    /// video clean point ever arrives). Task066 uses this to notice exactly
    /// when a new segment has opened, so a thumbnail captured earlier (at the
    /// video clean point that requested this rotation, held in caller-side
    /// CPU memory) can be written out under the right index.
    pub fn open_index(&self) -> Option<u64> {
        self.open.as_ref().map(|open| open.index)
    }

    /// Returns true exactly once per segment, before caller submits boundary input.
    pub fn should_force_keyframe(&mut self, input_timestamp_100ns: i64) -> bool {
        let Some(open) = &self.open else {
            return false;
        };
        if !self.boundary_requested
            && input_timestamp_100ns - open.start_timestamp_100ns >= SEGMENT_DURATION_100NS
        {
            self.boundary_requested = true;
            true
        } else {
            false
        }
    }

    /// Hands `sample` to the open segment, behind anything the sink has already
    /// refused.
    ///
    /// Nothing is ever dropped and nothing is ever fatal here: a refusal leaves
    /// the sample queued, and `HELD_VIDEO_LIMIT` -- enforced by the capture loop
    /// that can see the whole picture -- is what turns a refusal that never
    /// clears into a reported failure. The audio arm has worked this way since
    /// task610; before t260911-e7f3 the video arm died on the spot instead, and
    /// a segment boundary reached during an encoder stall is exactly when it
    /// did.
    pub fn push_video_sample(
        &mut self,
        sample: &IMFSample,
    ) -> Result<Vec<EncoderEvent>, EncoderStartError> {
        let mut events = self.drain_pending_video()?;
        if self.pending_video.is_empty() {
            let (produced, accepted) = self.write_one_video_sample(sample)?;
            events.extend(produced);
            if accepted {
                return Ok(events);
            }
        }
        // Behind what is already waiting, in arrival order: the muxer's own
        // `write_one_video_sample` and the fMP4 sink both need video to rise.
        self.pending_video
            .push_back(segment_writer::retain_video_sample(sample)?);
        self.note_pending_video();
        Ok(events)
    }

    /// Re-offers whatever the sink refused earlier, oldest first, stopping at
    /// the first refusal. `poll_to_muxer` calls it every turn of the capture
    /// loop -- the turn that also gets the queued AAC to the sink, which is what
    /// actually clears the refusal.
    pub fn drain_pending_video(&mut self) -> Result<Vec<EncoderEvent>, EncoderStartError> {
        let mut events = Vec::new();
        while let Some(sample) = self.pending_video.front().cloned() {
            let (produced, accepted) = self.write_one_video_sample(&sample)?;
            events.extend(produced);
            if !accepted {
                break;
            }
            self.pending_video.pop_front();
        }
        self.note_pending_video();
        Ok(events)
    }

    fn note_pending_video(&mut self) {
        self.pending_video_since = if self.pending_video.is_empty() {
            None
        } else {
            self.pending_video_since
                .or_else(|| Some(std::time::Instant::now()))
        };
    }

    /// How many video samples the sink has not taken yet.
    pub fn pending_video_len(&self) -> usize {
        self.pending_video.len()
    }

    /// How long the oldest unaccepted video sample has been waiting, or `None`
    /// when the sink is keeping up. The capture loop's `HELD_VIDEO_LIMIT` reads
    /// this the way it reads `PENDING_AAC_LIMIT` on the audio side.
    pub fn pending_video_waiting(&self) -> Option<std::time::Duration> {
        self.pending_video_since.map(|since| since.elapsed())
    }

    /// `false` means the sink would not take `sample`; it has not been consumed
    /// and the same sample has to be offered again.
    fn write_one_video_sample(
        &mut self,
        sample: &IMFSample,
    ) -> Result<(Vec<EncoderEvent>, bool), EncoderStartError> {
        unsafe {
            let timestamp = sample
                .GetSampleTime()
                .map_err(|error| device_error("video output timestamp", error))?;
            let clean_point = sample
                .GetUINT32(&windows::Win32::Media::MediaFoundation::MFSampleExtension_CleanPoint)
                .unwrap_or_default()
                != 0;
            let mut events = Vec::new();

            if self.open.is_none() {
                if !clean_point {
                    return Ok((events, true));
                }
                self.open_segment(timestamp)?;
            } else if self.boundary_requested && clean_point {
                // Rotate -- and the rotation is not undone if the write below is
                // refused (t260911-e7f3): `closing` has been taken and
                // `boundary_requested` cleared, so the retry of this same
                // sample finds `open` already current and rotates nothing. The
                // `Finalized` events it produced go back to the caller on this
                // call, once.
                //
                // Finalize any already-pending `closing` first (this
                // guarantees at most two writers open at once, and that
                // `Finalized` events are emitted in index order — `closing`
                // always holds a strictly lower index than `open`), then move
                // the current `open` into `closing` without finalizing it yet.
                // Its audio may still be in flight (see `push_aac_sample`).
                if let Some(closing) = self.closing.take() {
                    events.push(self.finalize_segment(closing)?);
                }
                let previous = self.open.take().expect("segment opens before rotation");
                self.closing = Some(previous);
                self.open_segment(timestamp)?;
                self.boundary_requested = false;
            }

            let open = self
                .open
                .as_mut()
                .expect("segment opens before sample write");
            if !open
                .writer
                .write_video_sample_at(sample, timestamp - open.start_timestamp_100ns)?
            {
                // Not this segment's end yet: the sample never reached the
                // writer, so claiming it did would put a timestamp in the
                // catalog that the file does not carry.
                return Ok((events, false));
            }
            open.end_timestamp_100ns = timestamp;
            Ok((events, true))
        }
    }

    /// Finalizes every still-open writer (at most `closing` then `open`), in
    /// index order.
    pub fn finalize(mut self) -> Result<Vec<EncoderEvent>, EncoderStartError> {
        let mut events = Vec::new();
        // Anything the sink was still refusing belongs in the segment it was
        // written for; finalizing over it would swap a recording that used to
        // die outright for one that quietly loses its tail (t260911-e7f3).
        // Bounded by the same limit the capture loop fails on, so a sink that
        // is genuinely stuck cannot hang the stop instead.
        let started = std::time::Instant::now();
        while !self.pending_video.is_empty() {
            events.extend(self.drain_pending_video()?);
            if self.pending_video.is_empty() || started.elapsed() >= HELD_VIDEO_LIMIT {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        if !self.pending_video.is_empty() {
            tracing::error!(
                target: "task610_backpressure",
                pending_video = self.pending_video.len(),
                waited_ms = started.elapsed().as_millis() as u64,
                "the video sink never took the last samples; finalizing without them"
            );
        }
        if let Some(closing) = self.closing.take() {
            events.push(self.finalize_segment(closing)?);
        }
        if self.open.is_some() {
            events.push(self.finalize_open_segment()?);
        }
        Ok(events)
    }

    fn open_segment(&mut self, timestamp_100ns: i64) -> Result<(), EncoderStartError> {
        let index = self.next_index;
        self.next_index += 1;
        let writer = if self.in_memory {
            Mp4SegmentWriter::create_in_memory(
                &self.config,
                &self.output_type,
                self.aac_output_type.as_ref(),
                self.audio_tracks,
            )?
        } else {
            Mp4SegmentWriter::create_with_audio(
                &self.config,
                &self.output_type,
                self.aac_output_type.as_ref(),
                self.audio_tracks,
                index,
            )?
        };
        // Repeat the previous access unit so this segment's decoder has the
        // predecessor every AAC block is overlap-added with (task204). It
        // belongs *before* the segment starts, so the whole audio track shifts
        // by however far back that is and the priming unit lands at local 0.
        // Readers subtract the same offset and discard the first unit.
        //
        // Only if it really is this segment's predecessor, though (task610).
        // The shift is `now - that unit's timestamp`, so a *stale* one shifts
        // the whole track by however long audio has been missing: measured at
        // 1.39s after a 2s capture-thread stall, which put the segment's first
        // real access unit at local 2.73s while its video was still at 0. The
        // fMP4 sink will not take a stream that far ahead of the other one, so
        // it refused the audio; the audio then never advanced, so before long
        // it refused the video too, and the recording died on whichever arm
        // gave up first. A unit that old is not a predecessor of anything here
        // -- dropping it costs one ~21ms fade-in at the seam, which is what
        // this segment would have had anyway.
        // Each track gets its own priming unit and therefore its own shift:
        // they are independent AAC streams and one being stale says nothing
        // about the others (task1260).
        let mut audio_offsets_100ns = Vec::with_capacity(self.audio_tracks);
        for (track, last) in self.last_aac.iter().enumerate() {
            let offset = match last {
                Some((priming, priming_timestamp))
                    if priming_is_fresh(timestamp_100ns, *priming_timestamp) =>
                {
                    // A sink this new has never refused anything; treat a refusal
                    // as "no priming" rather than failing the segment over it.
                    if writer.write_aac_sample_on(track, priming, 0)? {
                        (timestamp_100ns - priming_timestamp).max(1)
                    } else {
                        0
                    }
                }
                Some((_, priming_timestamp)) => {
                    tracing::warn!(
                        target: "task610_backpressure",
                        index,
                        track,
                        priming_age_100ns = timestamp_100ns - priming_timestamp,
                        "skipping a stale AAC priming unit: audio stopped arriving for too long"
                    );
                    0
                }
                None => 0,
            };
            audio_offsets_100ns.push(offset);
        }
        self.open = Some(OpenSegment {
            writer,
            index,
            start_timestamp_100ns: timestamp_100ns,
            end_timestamp_100ns: timestamp_100ns,
            audio_moved_on: vec![false; audio_offsets_100ns.len()],
            audio_offsets_100ns,
        });
        Ok(())
    }

    fn finalize_open_segment(&mut self) -> Result<EncoderEvent, EncoderStartError> {
        let open = self.open.take().expect("open segment required");
        self.finalize_segment(open)
    }

    /// Finalizes a specific segment (either the current `open` or a pending
    /// `closing`) and builds its `Finalized` event. Takes `&self`, not `&mut
    /// self`: the segment being finalized is passed by value, already removed
    /// from whichever `Option` field held it.
    fn finalize_segment(&self, segment: OpenSegment) -> Result<EncoderEvent, EncoderStartError> {
        let started = std::time::Instant::now();
        // The container arm keeps the mp4 in RAM until the index writer appends
        // it; there is no file to name, so `path` stays empty and every reader
        // of this event has to go through `bytes`.
        let (path, bytes) = if self.in_memory {
            (PathBuf::new(), Some(segment.writer.finalize_into_bytes()?))
        } else {
            (segment.writer.finalize()?, None)
        };
        // Task202 follow-up: this runs on the capture thread, so however long
        // it takes is time no frame reaches the encoder.
        tracing::info!(
            target: "task202_finalize",
            index = segment.index,
            took_ms = started.elapsed().as_millis() as u64,
            "segment finalized"
        );
        Ok(EncoderEvent::Finalized(VideoSegmentMetadata {
            index: segment.index,
            start_timestamp_100ns: segment.start_timestamp_100ns,
            end_timestamp_100ns: segment.end_timestamp_100ns,
            path,
            bytes,
            resolution: self.config.output_size,
            fps: self.config.frame_rate,
            bitrate: bitrate_for(self.config.output_size),
            keyframe_first: true,
            audio_offsets_100ns: segment.audio_offsets_100ns,
        }))
    }
}

fn media_type_error(error: windows::core::Error) -> EncoderStartError {
    EncoderStartError {
        kind: EncoderStartErrorKind::UnsupportedFormat,
        diagnostics: format!("media type setup failed: {error}"),
    }
}

fn device_error(operation: &str, error: windows::core::Error) -> EncoderStartError {
    EncoderStartError {
        kind: EncoderStartErrorKind::Device,
        diagnostics: format!("{operation} failed: {error}"),
    }
}

/// The bitrate the encoder is handed for a given output size.
///
/// What this means depends on the rate control that took (see
/// `configure_rate_control`): under constant quality it is advisory at most,
/// and under the VBR fallback it is the average being steered to. Both readings
/// want the same number, so there is one.
///
/// Roughly 40% below the tiers this shipped with (task1000). Those were sized
/// for Baseline, which spends bits on a picture CABAC and the 8x8 transform now
/// carry for less; the old numbers under High would mostly buy headroom nothing
/// asks for, on a recording that has to live in a ring buffer.
pub fn bitrate_for(size: CaptureSize) -> u32 {
    let pixels = i64::from(size.width.max(0)) * i64::from(size.height.max(0));
    if pixels >= 3840 * 2160 {
        35_000_000
    } else if pixels >= 2560 * 1440 {
        20_000_000
    } else {
        12_000_000
    }
}

pub fn validate_config(config: &EncoderConfig) -> Result<(), EncoderStartError> {
    if config.output_size.width <= 0
        || config.output_size.height <= 0
        || config.output_size.width % 2 != 0
        || config.output_size.height % 2 != 0
    {
        return Err(EncoderStartError {
            kind: EncoderStartErrorKind::UnsupportedFormat,
            diagnostics: "NV12 requires a positive even resolution".into(),
        });
    }
    // Kept in step with `gpu.rs`'s throttle, which has allowed 120 since
    // task164. This validator did not, so picking the 120 chip in settings
    // failed every capture at `stage="h264_encoder"` instead of recording.
    // That is the stage's name as it was then; task1760 renamed it to
    // `VIDEO_ENCODER_STAGE` -- history, not a live string.
    if !matches!(config.frame_rate, 30 | 60 | 120) {
        return Err(EncoderStartError {
            kind: EncoderStartErrorKind::UnsupportedFormat,
            diagnostics: "only 30fps, 60fps or 120fps is supported".into(),
        });
    }
    Ok(())
}

// The segment and thumbnail path builders used to live here. They are
// `ring_buffer::store`'s now (task400): what a session directory contains is
// one module's business, and the encoder is a caller of it like everyone else.

#[cfg(test)]
mod tests;
