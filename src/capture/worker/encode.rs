//! Everything one frame passes through between the capture pool and the
//! container, as one value.
//!
//! Split out of `run_capture` because the pieces are never used apart: a frame
//! is transformed on `device`, converted to NV12, submitted to `encoder`,
//! pulled into `muxer`, and whatever segment that opens is what `thumbnails`
//! has been holding a picture for. Passing the six around separately is what
//! made the capture loop's steps un-extractable -- every one of them wanted
//! all six.

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::IMFSample;

use crate::capture::gpu::{GpuNv12Converter, TransformPipeline};
use crate::encoder;

use super::setup::ThumbnailRelay;
use super::{worker_error, BoundaryProbe, Counters, ThinSecond, ThreadWork};

pub(super) struct EncodePath {
    pub(super) pipeline: TransformPipeline,
    pub(super) converter: GpuNv12Converter,
    pub(super) encoder: encoder::HardwareVideoEncoder,
    pub(super) muxer: encoder::SegmentMuxer,
    pub(super) thumbnails: ThumbnailRelay,
    /// Set when the encoder was rebuilt at a new size: the segment that size
    /// opens has no thumbnail candidate yet, so the next real frame takes one.
    pub(super) thumbnail_wanted: bool,
    /// t261108-0681: the second under observation for a thin-second line.
    pub(super) thin: ThinSecond,
}

impl EncodePath {
    /// One turn of the encoder crank: pull whatever it has ready into the
    /// muxer, route the events that produced, and hand over a segment
    /// thumbnail if the drain just opened the segment it belongs to. The three
    /// always go together -- draining is what frees encoder credit *and* what
    /// opens the segment.
    pub(super) fn pump(
        &mut self,
        emit: &impl Fn(encoder::EncoderEvent),
    ) -> windows::core::Result<()> {
        // Before the poll, so a conversion is never seen finishing after the
        // output it fed (t261021-375d).
        let now = Instant::now();
        let encoder = &mut self.encoder;
        self.converter
            .collect_converted(|pts_100ns| encoder.credit_watch().on_convert_done(pts_100ns, now));
        let open_before = self.muxer.open_index();
        let poll_started = Instant::now();
        let produced_events = self
            .encoder
            .poll_to_muxer(&mut self.muxer)
            .map_err(worker_error)?;
        if self.muxer.open_index() != open_before {
            self.thin
                .record(ThreadWork::SegmentRotate, poll_started.elapsed());
        }
        for event in produced_events {
            emit(event);
        }
        let pipeline = &self.pipeline;
        let thin = &mut self.thin;
        self.thumbnails
            .drain(self.muxer.open_index(), false, |wait| {
                let started = Instant::now();
                let collected = pipeline.collect_thumbnail(wait);
                thin.record(ThreadWork::ThumbnailCollect, started.elapsed());
                collected
            });
        Ok(())
    }

    /// Puts `texture` into an encoder sample in the form the encoder takes --
    /// NV12 by conversion, ARGB32 by copy (t261021-8388) -- and hands it over
    /// at `pts_100ns`. The caller has already checked there is input credit.
    pub(super) fn submit(
        &mut self,
        texture: &ID3D11Texture2D,
        pts_100ns: i64,
    ) -> windows::core::Result<()> {
        let sample = self.encoder.allocate_input_sample().map_err(worker_error)?;
        let target =
            encoder::HardwareVideoEncoder::input_sample_texture(&sample).map_err(worker_error)?;
        match self.encoder.input() {
            encoder::EncoderInput::Nv12 => self.converter.convert_into(texture, &target)?,
            encoder::EncoderInput::Argb32 => self.converter.copy_into(texture, &target)?,
        }
        // After the last GPU job either way, so `convert_wait` keeps measuring
        // the same thing on both paths.
        self.converter.mark_converted(pts_100ns);
        self.encoder
            .process_input_sample(&sample, pts_100ns)
            .map_err(worker_error)
    }

    /// Whether this frame's timestamp is where the muxer wants to start a new
    /// segment.
    pub(super) fn should_rotate(&mut self, pts_100ns: i64) -> bool {
        self.muxer.should_force_keyframe(pts_100ns)
    }

    /// Asks the encoder for a keyframe, which is what actually opens the next
    /// segment a few frames later.
    pub(super) fn request_keyframe(&mut self) -> windows::core::Result<()> {
        self.encoder.request_next_keyframe().map_err(worker_error)
    }

    /// Keeps this frame as the picture for whichever segment opens next.
    pub(super) fn capture_thumbnail(&mut self, texture: &ID3D11Texture2D) {
        self.thumbnails.capture(&self.pipeline, texture);
    }
}

/// Where the capture loop is: the two clocks, the frame-hold watchdog, the
/// counters task480's stall probe reads, the audio the sink has refused so far
/// and the counters `CaptureSession` reads.
///
/// One value for the same reason `EncodePath` is one: the stall probe is only
/// meaningful when it can read all of them together, and every arm of the loop
/// moves several at once. Passing them apart is what grew the loop functions to
/// eleven arguments.
pub(super) struct LoopState {
    pub(super) last_video_pts_100ns: i64,
    pub(super) last_audio_pts_100ns: Option<i64>,
    pub(super) max_abs_audio_video_drift_100ns: i64,
    /// Wall time, so the synthesized hold timestamps advance with the clock
    /// rather than with frame arrivals (task163).
    pub(super) last_frame_at: Instant,
    pub(super) have_frame: bool,
    /// Task480: the three candidate causes of the still-target death --
    /// watchdog never due, due but starved of credit, fed but not reaching the
    /// muxer -- are only distinguishable by counting them apart.
    pub(super) holds_fed: u64,
    pub(super) holds_without_credit: u64,
    pub(super) real_frames: u64,
    pub(super) aac_samples_pushed: u64,
    last_stall_report: Instant,
    last_audio_late_report: Instant,
    /// Task610: AAC samples the sink has refused so far, per track, oldest
    /// first, and when the oldest of them was first offered. Nothing is
    /// dropped -- this is a wait, not a discard -- but it cannot grow forever
    /// either, hence `PENDING_AAC_LIMIT`.
    pub(super) pending_aac: Vec<VecDeque<IMFSample>>,
    pub(super) pending_aac_since: Option<Instant>,
    /// The target audio session's reinitialize count, for the telemetry.
    pub(super) reinitializations: u32,
    /// Set when the audio session is what ended the capture (task410).
    pub(super) audio_death: bool,
    /// Task202 instrumentation: open from a boundary until the window after
    /// it has been observed. One `task202_boundary` record per rotation.
    pub(super) boundary: Option<BoundaryProbe>,
    pub(super) counters: Counters,
}

impl LoopState {
    pub(super) fn new(audio_tracks: usize, counters: Counters) -> Self {
        let now = Instant::now();
        Self {
            last_video_pts_100ns: 0,
            last_audio_pts_100ns: None,
            max_abs_audio_video_drift_100ns: 0,
            last_frame_at: now,
            have_frame: false,
            holds_fed: 0,
            holds_without_credit: 0,
            real_frames: 0,
            aac_samples_pushed: 0,
            last_stall_report: now,
            last_audio_late_report: now,
            pending_aac: (0..audio_tracks.max(1)).map(|_| VecDeque::new()).collect(),
            pending_aac_since: None,
            reinitializations: 0,
            audio_death: false,
            boundary: None,
            counters,
        }
    }

    /// How far audio has run ahead of video, which is the discriminator the
    /// stall probe and its log line are both written around.
    fn audio_ahead_ms(&self) -> i64 {
        (self
            .last_audio_pts_100ns
            .unwrap_or(self.last_video_pts_100ns)
            - self.last_video_pts_100ns)
            / 10_000
    }

    /// One line a second, and only when something is actually off (task480).
    ///
    /// The audio/video gap sits under ~40ms on a healthy still target (measured
    /// over 4.5 minutes at ~1.3fps), and the sink refuses audio when video falls
    /// behind. Hence 250ms: about six times the healthy gap, far enough out that
    /// normal jitter cannot reach it, close enough to fire well before the sink
    /// starts refusing. 500ms since the last frame is the same idea on the video
    /// side -- the task163 watchdog re-feeds every two frame intervals (16ms at
    /// 120fps), so half a second without one means the hold itself has stopped
    /// running, not that the screen is merely still. Both are floors for
    /// *reporting only*; nothing about the capture's behaviour changes when they
    /// trip.
    pub(super) fn report_stall(&mut self, open_index: Option<u64>) {
        let audio_ahead_ms = self.audio_ahead_ms();
        let anomalous = audio_ahead_ms.abs() >= 250
            || self.last_frame_at.elapsed() >= Duration::from_millis(500);
        if !anomalous || self.last_stall_report.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_stall_report = Instant::now();
        tracing::warn!(
            target: "task480_stall",
            event = "capture_stall",
            holds_fed = self.holds_fed,
            holds_without_credit = self.holds_without_credit,
            real_frames = self.real_frames,
            aac_samples_pushed = self.aac_samples_pushed,
            last_video_pts_100ns = self.last_video_pts_100ns,
            last_audio_pts_100ns = self.last_audio_pts_100ns.unwrap_or(0),
            audio_ahead_ms,
            since_last_frame_ms = self.last_frame_at.elapsed().as_millis() as u64,
            open_index = ?open_index,
            "still-target stall probe"
        );
    }

    /// The worker's half of t261108-4f53, from the target track's `Packets`
    /// arm: the newest audio in this batch ended 250ms or more before now.
    /// `queued` is what was still waiting in the audio channel behind it --
    /// a deep queue means this loop was away, a shallow one with a big batch
    /// means the audio thread was. `video_late_ms` is the same lateness for
    /// the last video this loop took, so a stall of both reads as one.
    /// One line a second at most, reporting only, like `report_stall`.
    pub(super) fn report_audio_late(
        &mut self,
        newest_end_100ns: i64,
        packets: usize,
        queued: usize,
    ) {
        let now_100ns = unsafe { windows::Win32::Media::MediaFoundation::MFGetSystemTime() };
        let late_ms = (now_100ns - newest_end_100ns) / 10_000;
        if late_ms < 250 || self.last_audio_late_report.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_audio_late_report = Instant::now();
        tracing::warn!(
            target: "t261108_4f53_audio_late",
            late_ms,
            packets,
            queued,
            video_late_ms = (now_100ns - self.last_video_pts_100ns) / 10_000,
            "the newest audio reached the capture loop late"
        );
    }
}

impl EncodePath {
    /// The frame-hold watchdog's feed (task163): a still or minimized target
    /// delivers no WGC frames, so the transport hands the encoder the last
    /// picture (or black, while minimized) on the clock instead.
    ///
    /// Credit only comes back from draining, and the drain used to sit inside
    /// the fed branch. So an encoder that was out of credit at the moment real
    /// frames stopped -- a minimize, or a still target at 60fps -- never fed and
    /// never drained: video stalled for good, and about a second later the
    /// muxer's audio queue filled and `push_aac_sample` failed with
    /// `MF_E_NOTACCEPTING`, taking the whole capture down (found verifying
    /// task163). `last_frame_at` deliberately stays put on that path: the hold
    /// is still due, so the next pass feeds it once this drain frees a slot.
    pub(super) fn feed_hold(
        &mut self,
        state: &mut LoopState,
        minimized: bool,
        emit: &impl Fn(encoder::EncoderEvent),
    ) -> windows::core::Result<()> {
        if !self.encoder.has_input_credit() {
            state.holds_without_credit += 1;
            return self.pump(emit);
        }
        // A frozen frame reads as "still going"; black reads as "you are not
        // looking at it", which is the truth while minimized.
        let held = if minimized {
            self.pipeline.blank()
        } else {
            self.pipeline.last_output()
        };
        // Advanced only when the frame is actually fed, so the video clock can
        // never run ahead of the samples the encoder saw.
        let elapsed_100ns = state.last_frame_at.elapsed().as_nanos() as i64 / 100;
        state.last_frame_at = Instant::now();
        state.last_video_pts_100ns = state.last_video_pts_100ns.saturating_add(elapsed_100ns);
        // The whole reason this atomic exists (task1680): a still window
        // delivers no WGC frames, so only the hold advances the recording
        // position -- and the marker hotkey used to read the preview channel,
        // which the hold never feeds.
        state
            .counters
            .position
            .store(state.last_video_pts_100ns, Ordering::Relaxed);
        // Rotation still has to happen on held frames: without it a minimized
        // recording would never close a segment, and nothing would reach the
        // manifest until the window came back.
        if self.should_rotate(state.last_video_pts_100ns) {
            self.capture_thumbnail(&held);
            self.request_keyframe()?;
        }
        self.submit(&held, state.last_video_pts_100ns)?;
        state
            .counters
            .encoded_frames
            .fetch_add(1, Ordering::Relaxed);
        state.holds_fed += 1;
        self.pump(emit)
    }
}
