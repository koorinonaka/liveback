use std::{
    sync::{
        atomic::{AtomicI64, AtomicU64, Ordering},
        Arc,
    },
    thread,
    time::Instant,
};

use crate::encoder;
use crossbeam_channel::{bounded, Receiver, SendError, Sender};

use super::worker::{measured_fps_from_counts, run_capture, Job, WorkerChannels};
use super::{
    CaptureColorSpace, CaptureConfig, CaptureDiagnostics, CaptureStopReason, CapturedFrame,
    FRAME_QUEUE_CAPACITY,
};

pub struct CaptureSession {
    pub(super) stop: Sender<()>,
    pub(super) worker: Option<thread::JoinHandle<()>>,
    /// Where a held worker takes its next recording (t260929-ea5e).
    pub(super) jobs: Sender<Job>,
    /// The worker's answer once this recording is over: `true` when it now
    /// holds its surface waiting on `jobs`. Disconnected means it ended.
    pub(super) recording_done: Receiver<bool>,
    pub(super) frames: Receiver<CapturedFrame>,
    pub(super) encoder_events: Receiver<encoder::EncoderEvent>,
    pub(super) diagnostics: CaptureDiagnostics,
    pub(super) last_sequence: u64,
    // Last video PTS the worker actually fed the encoder, which is where the
    // recording currently is (task1680). This used to be the latest *preview*
    // frame's timestamp, but preview frames only come from real WGC frames --
    // a still window delivers none, so the marker hotkey read a position frozen
    // at the recording's start while task163's held frames kept recorded time
    // flowing. Written by the worker, so it needs no lock and never leads the
    // pts the encoder has seen.
    pub(super) recording_position: Arc<AtomicI64>,
    // `measured_fps` source of truth (Task095): counts frames at the point they're
    // actually handed to the encoder (`run_capture`'s `h264.process_input_sample`
    // success), not preview-frame drain -- the preview channel is
    // `bounded(FRAME_QUEUE_CAPACITY=1)` latest-wins, so drain interval degenerates
    // into the frontend's polling period rather than the real frame interval.
    pub(super) encoded_frame_count: Arc<AtomicU64>,
    pub(super) last_measured_frame_count: u64,
    pub(super) last_measured_at: Instant,
}

/// A capture thread that finished a recording and kept its WGC session open
/// for the same target's next one (t260929-ea5e). Dropping it lets the thread
/// close the surface and end once every `jobs` sender is gone.
pub(super) struct HeldWorker {
    worker: thread::JoinHandle<()>,
    stop: Sender<()>,
    jobs: Sender<Job>,
}

type Spawned = (thread::JoinHandle<()>, Sender<()>, Sender<Job>);

fn spawn_worker(job: Job) -> Result<Spawned, String> {
    let (stop, stop_receiver) = bounded(1);
    let (jobs, job_receiver) = bounded(1);
    let worker_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("livia-wgc".into())
        .spawn(move || run_capture(job, stop_receiver, worker_stop, job_receiver))
        .map_err(|_| "could not start the capture thread".to_owned())?;
    Ok((worker, stop, jobs))
}

impl CaptureSession {
    pub fn start(
        config: CaptureConfig,
        stopped: Sender<CaptureStopReason>,
        finalized_segments: Sender<super::indexer::IndexEvent>,
    ) -> Result<Self, String> {
        Self::start_on(config, stopped, finalized_segments, false, None)
    }

    /// `start`, on `held`'s open surface when there is one, keeping the
    /// surface open after the stop when `hold` is set (t260929-ea5e). A held
    /// worker that has already ended (its target closed) falls back to a
    /// fresh thread.
    pub(super) fn start_on(
        config: CaptureConfig,
        stopped: Sender<CaptureStopReason>,
        finalized_segments: Sender<super::indexer::IndexEvent>,
        hold: bool,
        held: Option<HeldWorker>,
    ) -> Result<Self, String> {
        let (frame_sender, frames) = bounded(FRAME_QUEUE_CAPACITY);
        let (encoder_event_sender, encoder_events) = bounded(32);
        let (done, recording_done) = bounded(1);
        let encoded_frame_count = Arc::new(AtomicU64::new(0));
        let recording_position = Arc::new(AtomicI64::new(0));
        let output_size = config.output_size;
        let include_cursor = config.include_cursor;
        let job = Job {
            config,
            hold,
            channels: WorkerChannels {
                frames: frame_sender,
                stopped,
                encoder_events: encoder_event_sender,
                finalized_segments,
            },
            counters: (encoded_frame_count.clone(), recording_position.clone()),
            done,
        };
        let (worker, stop, jobs) = match held {
            Some(held) => match held.jobs.send(job) {
                Ok(()) => (held.worker, held.stop, held.jobs),
                Err(SendError(job)) => spawn_worker(job)?,
            },
            None => spawn_worker(job)?,
        };
        Ok(Self {
            stop,
            worker: Some(worker),
            jobs,
            recording_done,
            frames,
            encoder_events,
            diagnostics: CaptureDiagnostics {
                input_size: output_size,
                output_size,
                measured_fps: 0.0,
                hdr_to_sdr: false,
                cursor_included: include_cursor,
                dropped_frames: 0,
                encoder_errors: Vec::new(),
                last_audio_pts_100ns: None,
                last_video_pts_100ns: None,
                max_abs_audio_video_drift_100ns: 0,
                audio_reinitializations: 0,
                audio_unavailable: false,
            },
            last_sequence: 0,
            recording_position,
            encoded_frame_count,
            last_measured_frame_count: 0,
            last_measured_at: Instant::now(),
        })
    }

    /// Stops the recording and waits until it is over. Returns the worker when
    /// it held its surface for the next recording (t260929-ea5e); otherwise
    /// the thread is joined here, as it always was.
    pub(super) fn stop_holding(&mut self) -> Option<HeldWorker> {
        let worker = self.worker.take()?;
        let _ = self.stop.try_send(());
        if self.recording_done.recv().unwrap_or(false) {
            return Some(HeldWorker {
                worker,
                stop: self.stop.clone(),
                jobs: self.jobs.clone(),
            });
        }
        let _ = worker.join();
        None
    }

    pub fn stop(&mut self) {
        drop(self.stop_holding());
    }

    /// Drains the worker channels and refreshes `measured_fps`, without producing
    /// a snapshot. Kept apart from `diagnostics()` for callers that only want
    /// the drain.
    pub(super) fn pump(&mut self) {
        while let Ok(event) = self.encoder_events.try_recv() {
            match event {
                // Never arrives here since task430: the worker routes finalized
                // segments straight to the index writer, so the one channel the
                // UI drains carries telemetry only -- which is what stopped
                // telemetry from being able to starve them out of it.
                encoder::EncoderEvent::Finalized(_) => {}
                encoder::EncoderEvent::AudioTelemetry {
                    last_audio_pts_100ns,
                    last_video_pts_100ns,
                    max_abs_drift_100ns,
                    reinitializations,
                } => {
                    self.diagnostics.last_audio_pts_100ns = Some(last_audio_pts_100ns);
                    self.diagnostics.last_video_pts_100ns = Some(last_video_pts_100ns);
                    self.diagnostics.max_abs_audio_video_drift_100ns = max_abs_drift_100ns;
                    self.diagnostics.audio_reinitializations = reinitializations;
                }
                encoder::EncoderEvent::AudioUnavailable => {
                    self.diagnostics.audio_unavailable = true
                }
            }
        }
        while let Ok(frame) = self.frames.try_recv() {
            if self.last_sequence != 0 && frame.sequence <= self.last_sequence {
                self.diagnostics.dropped_frames += 1;
                continue;
            }
            self.last_sequence = frame.sequence;
            self.diagnostics.input_size = frame.input_size;
            self.diagnostics.output_size = frame.output_size;
            self.diagnostics.hdr_to_sdr = frame.color_space == CaptureColorSpace::ScRgb;
            self.diagnostics.cursor_included = frame.cursor_included;
            drop(frame.texture);
        }
        let count_now = self.encoded_frame_count.load(Ordering::Relaxed);
        let now = Instant::now();
        self.diagnostics.measured_fps = measured_fps_from_counts(
            count_now.saturating_sub(self.last_measured_frame_count),
            now.saturating_duration_since(self.last_measured_at),
        );
        self.last_measured_frame_count = count_now;
        self.last_measured_at = now;
    }

    /// A snapshot of what the worker has reported. Purely a read since task430:
    /// finalized segments used to be handed over through here exactly once --
    /// the drain mattered because `sync_ring` re-added every entry it did not
    /// find in the ring, so anything pruned past the retention window came back
    /// on the next poll (task226's 1526 re-added records). They now go straight
    /// from the worker to the index writer and never enter this struct.
    pub(super) fn diagnostics(&mut self) -> CaptureDiagnostics {
        self.pump();
        self.diagnostics.clone()
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.stop();
    }
}
