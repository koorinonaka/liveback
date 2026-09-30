//! Windows 11 Application Loopback capture.  Microphone endpoints are never used by
//! Liveback, and nothing here ever falls back from one scope to another on its own --
//! see `AudioSource`, whose three variants are each chosen explicitly by the caller.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::Sender;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u16 = 2;
pub const AUDIO_BITRATE: u32 = 192_000;
/// Device-position discontinuities larger than this many frames (1 second at
/// `AUDIO_SAMPLE_RATE`) are reported via telemetry instead of being filled with synthetic
/// silence. A hole this large more likely signals a capture/device glitch than a quiet
/// moment in the game, and silently manufacturing a full second of audio would mask it.
pub const MAX_SYNTHETIC_SILENCE_GAP_FRAMES: u64 = AUDIO_SAMPLE_RATE as u64;
/// How long the endpoint has to stay quiet before `run_event_driven_capture`
/// starts manufacturing silence for it (task410).
///
/// Comfortably longer than the 200ms `wait_for_period_event` timeout, so one
/// period event running late is never mistaken for an idle endpoint.
pub const IDLE_SILENCE_START_100NS: i64 = 3_000_000;
/// Task088: `poll()` used to be driven only by WGC video frame arrival (called once per
/// accepted frame in `capture.rs`'s main loop), not a fixed timer. Video frames on a
/// static/sparse scene can arrive hundreds of ms apart (real measurement against a static
/// window: consistently ~250ms). Requesting a larger `hnsBufferDuration` in `Initialize`
/// does not help: measured with `GetBufferSize()` after `Initialize(hnsBufferDuration =
/// 10_000_000 /* 1s */)`, this process-loopback virtual endpoint still reports only 480
/// frames (10ms) allocated -- it ignores the requested size. And even independent of that,
/// frames returned per `poll()` call plateaued at ~1400-1500 (about 30ms) regardless of how
/// long the previous call was, with no `AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY` ever set --
/// audio simply vanishes once more than ~30ms accumulates unread. The fix is event-driven
/// capture (`AUDCLNT_STREAMFLAGS_EVENTCALLBACK`) on a dedicated thread independent of video
/// frame arrival, so `poll()` is called roughly once per WASAPI engine period regardless of
/// what the video side is doing. See `run_event_driven` and its use in `capture.rs`.

#[derive(Default, Debug)]
pub struct AudioRecoveryState {
    reinitializations: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioPollFailureAction {
    ReinitializeOnce,
    StopSafely,
}

impl AudioRecoveryState {
    pub fn on_poll_failure(&mut self) -> AudioPollFailureAction {
        if self.reinitializations == 0 {
            self.reinitializations = 1;
            AudioPollFailureAction::ReinitializeOnce
        } else {
            AudioPollFailureAction::StopSafely
        }
    }

    pub fn reinitializations(&self) -> u32 {
        self.reinitializations
    }
}

mod aac;
pub use aac::AacEncoder;

mod loopback;
pub use loopback::{AudioPacket, ProcessLoopbackAudioCapture};

/// What one recording's single audio track listens to (task1430).
///
/// One variant per way the app can be pointed at sound, decided once at start
/// and used for both the throwaway trial and the capture thread, so the two can
/// never disagree about what is being recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// The capture target and its child processes -- the default, and what
    /// every recording did before the "record other applications too" toggle.
    SelectedProcess(u32),
    /// Everything except Liveback itself: the toggle turned on. One mixed
    /// track, because the API can exclude exactly one process tree and that
    /// one has to be ours -- otherwise the review playback running during a
    /// recording would be recorded back into it.
    AllAppsExceptSelf,
    /// The default render endpoint's whole mix (task165). A monitor has no
    /// process to point at, so this is what screen recording always uses,
    /// toggle or not.
    SystemMix,
}

impl AudioSource {
    /// The one place a `CaptureConfig` becomes an audio source (task2020).
    ///
    /// The capture worker decides what to record; `CaptureController::start`
    /// decides whether a second recording may have it at all. Two copies of
    /// this if/else would mean the thing judged and the thing recorded could
    /// drift apart, so both call this.
    ///
    /// A screen has no owning process, so its sound is the system's whatever
    /// the toggle says (task165); the toggle only chooses between the target's
    /// tree and everything-but-Liveback (task1430).
    pub fn for_config(config: &super::types::CaptureConfig) -> Self {
        if config.kind == super::targets::CaptureTargetKind::Monitor {
            Self::SystemMix
        } else if config.capture_all_audio {
            Self::AllAppsExceptSelf
        } else {
            Self::SelectedProcess(config.process_id)
        }
    }

    pub fn start(self) -> windows::core::Result<ProcessLoopbackAudioCapture> {
        match self {
            Self::SelectedProcess(process_id) => ProcessLoopbackAudioCapture::start(process_id),
            Self::AllAppsExceptSelf => ProcessLoopbackAudioCapture::start_excluding_self(),
            Self::SystemMix => ProcessLoopbackAudioCapture::start_system(),
        }
    }
}

/// One capture tick's worth of already-normalized PCM, ready for the AAC encoder.
/// Normalizing on the capture thread (not the main capture loop) keeps
/// `ProcessLoopbackAudioCapture`'s COM objects, which that thread owns exclusively, off the
/// main thread entirely.
pub struct NormalizedAudioPacket {
    pub timestamp_100ns: i64,
    pub pcm: Vec<f32>,
}

/// Lifecycle events from `run_event_driven_capture`, mirroring what the old inline
/// `audio_capture.poll()` call site in `capture.rs` observed directly via `Result` and
/// `AudioRecoveryState`.
pub enum AudioWorkerEvent {
    Packets(Vec<NormalizedAudioPacket>),
    Reinitialized { reinitializations: u32 },
    Stopped(windows::core::Error),
}

/// Task088 Step4: drives one `ProcessLoopbackAudioCapture` on the calling thread via its own
/// WASAPI period event, independent of video frame arrival -- see the module-level doc
/// comment on `MAX_SYNTHETIC_SILENCE_GAP_FRAMES` for why that decoupling is required. Meant
/// to run for the lifetime of one capture session on a dedicated `std::thread` spawned by
/// `capture.rs`; returns when `stop` fires or the audio session fails unrecoverably (after
/// one reinitialize attempt, matching the previous inline recovery policy in
/// `AudioRecoveryState`).
/// `source` is what this track listens to, decided by the worker before the
/// thread is spawned (see `AudioSource`); the same value drives the reinitialize
/// path below, so recovery never quietly changes scope.
/// `track` labels every event this thread sends, so the worker loop can tell
/// them apart in the channel it shares with the sink's other streams. Track 0
/// is the capture target -- the only track a recording gets since task1430.
pub fn run_event_driven_capture(
    source: AudioSource,
    track: usize,
    events: &Sender<(usize, AudioWorkerEvent)>,
    stop: &Arc<AtomicBool>,
) {
    let start = || source.start();
    // This is a bare `std::thread::spawn` thread (see `capture.rs`'s spawn site) with no COM
    // apartment of its own yet. Every other thread in this codebase that touches COM
    // (`run_capture`'s worker thread; every audio test in this file) calls this first --
    // skipping it here left `ProcessLoopbackAudioCapture::start`'s `ActivateAudioInterfaceAsync`/
    // `CoTaskMemAlloc`/`PROPVARIANT` calls running on an uninitialized apartment, which surfaced
    // as an intermittent `STATUS_ACCESS_VIOLATION` under load rather than a clean, immediate
    // failure.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    // Created here, not by the caller: `IAudioClient`/`IAudioCaptureClient` are COM
    // interfaces and not `Send`, so the capture must be built on the same thread that uses
    // it. `capture.rs` only does a throwaway trial `start()` on its own thread beforehand to
    // decide whether an audio track exists at all.
    let mut idle = IdleSilence::new();
    let mut capture = match start() {
        Ok(capture) => capture,
        Err(error) => {
            let _ = events.send((track, AudioWorkerEvent::Stopped(error)));
            return;
        }
    };
    let mut recovery = AudioRecoveryState::default();
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // 200ms timeout, not INFINITE: without it a stream that stops signaling (device
        // removed, session ended) would block this thread past `stop` firing.
        //
        // A timeout no longer `continue`s straight past the rest of the loop (task410):
        // "no period event" and "an event with no packets" are the two ways a silent
        // endpoint looks, and both have to reach the idle fill below. Skipping it is what
        // left the audio track empty while video piled up, until the fMP4 sink refused
        // the next video sample and the capture died ~1.6s in.
        let polled = if capture.wait_for_period_event(200) {
            capture.poll()
        } else {
            Ok(Vec::new())
        };
        match polled {
            Ok(packets) if packets.is_empty() => {
                if let Some(silence) = idle.fill() {
                    if events
                        .send((track, AudioWorkerEvent::Packets(vec![silence])))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Ok(packets) => {
                let normalized: Vec<NormalizedAudioPacket> = packets
                    .iter()
                    .map(|packet| NormalizedAudioPacket {
                        timestamp_100ns: packet.timestamp_100ns,
                        pcm: capture.normalize_to_f32(packet),
                    })
                    .collect();
                idle.observe(&normalized);
                if events
                    .send((track, AudioWorkerEvent::Packets(normalized)))
                    .is_err()
                {
                    break;
                }
            }
            Err(error) => match recovery.on_poll_failure() {
                AudioPollFailureAction::ReinitializeOnce => {
                    match start() {
                        Ok(reinitialized) => {
                            capture.stop();
                            capture = reinitialized;
                            let _ = events.send((
                                track,
                                AudioWorkerEvent::Reinitialized {
                                    reinitializations: recovery.reinitializations(),
                                },
                            ));
                        }
                        Err(error) => {
                            // Stop the original capture before bailing: it is
                            // still running (no Drop impl), and leaving it
                            // leaks the audio client and its period event.
                            capture.stop();
                            let _ = events.send((track, AudioWorkerEvent::Stopped(error)));
                            return;
                        }
                    }
                }
                AudioPollFailureAction::StopSafely => {
                    let _ = events.send((track, AudioWorkerEvent::Stopped(error)));
                    break;
                }
            },
        }
    }
    capture.stop();
}

/// Converts a frame count at `sample_rate` to duration in 100ns units (QPC's unit).
fn frames_to_100ns(frames: u64, sample_rate: u32) -> i64 {
    (frames as i64 * 10_000_000) / sample_rate.max(1) as i64
}

/// `block_align` bytes of PCM per frame, all zero: silence for `AUDCLNT_BUFFERFLAGS_SILENT`
/// packets and for interpolated device-position gaps alike.
fn zero_pcm(frames: u32, block_align: usize) -> Vec<u8> {
    vec![0u8; frames as usize * block_align]
}

/// Gap in frames between the end of the previously seen packet and `device_position_frames`,
/// or `None` when there is no prior packet to compare against (first packet after
/// `start`/reinit) or the position did not advance past the expected continuation point.
fn device_position_gap_frames(
    last_end_frames: Option<u64>,
    device_position_frames: u64,
) -> Option<u64> {
    let last_end = last_end_frames?;
    (device_position_frames > last_end).then(|| device_position_frames - last_end)
}

/// Whether a device-position gap of `gap_frames` should be filled with synthetic silence,
/// versus left unfilled and reported (see `MAX_SYNTHETIC_SILENCE_GAP_FRAMES`).
fn should_interpolate_gap(gap_frames: u64) -> bool {
    gap_frames <= MAX_SYNTHETIC_SILENCE_GAP_FRAMES
}

/// How many frames of silence an endpoint that has delivered nothing for
/// `silent_for_100ns` owes the encoder, or `None` while it is too early to say
/// (task410).
///
/// **`MAX_SYNTHETIC_SILENCE_GAP_FRAMES` deliberately does not apply here.**
/// That cap exists so a *running* stream's device-position discontinuity is
/// reported rather than painted over -- a second of missing audio between two
/// real packets is a glitch worth seeing. An endpoint nobody is playing
/// through is not glitching; it is legitimately silent, and it can stay that
/// way for the whole recording. Capping this would put the failure back:
/// after one second the audio track would stop advancing again, and the fMP4
/// sink would stop accepting video.
///
/// The failure it exists to prevent: the sink interleaves the two tracks, so
/// a video-only stream of samples backs up until `ProcessSample` returns
/// `MF_E_NOTACCEPTING` and the capture dies about 1.6 seconds in. Nothing in
/// the recording is wrong at that point -- the endpoint is just quiet.
pub fn idle_silence_frames(silent_for_100ns: i64) -> Option<u32> {
    if silent_for_100ns < IDLE_SILENCE_START_100NS {
        return None;
    }
    let frames = (silent_for_100ns as i128 * AUDIO_SAMPLE_RATE as i128) / 10_000_000;
    (frames > 0).then(|| frames.min(i128::from(u32::MAX)) as u32)
}

/// Tracks where the audio timeline has got to, so an idle endpoint can be
/// handed contiguous silence rather than a hole.
///
/// The cursor is a timestamp on the same QPC-based clock WASAPI stamps packets
/// with and WGC stamps frames with (`MFGetSystemTime`), so silence lands where
/// the missing audio would have.
struct IdleSilence {
    /// Just past the end of the last audio delivered, or `None` before any.
    cursor_100ns: Option<i64>,
    /// When that cursor was set, for measuring how long the quiet has lasted.
    cursor_at: Instant,
    total_frames: u64,
    next_log_frames: u64,
}

impl IdleSilence {
    fn new() -> Self {
        Self {
            cursor_100ns: None,
            cursor_at: Instant::now(),
            total_frames: 0,
            next_log_frames: u64::from(AUDIO_SAMPLE_RATE),
        }
    }

    /// Real audio arrived: the timeline is theirs again.
    fn observe(&mut self, packets: &[NormalizedAudioPacket]) {
        let Some(last) = packets.last() else {
            return;
        };
        // `normalize_to_f32` always returns interleaved stereo at
        // `AUDIO_SAMPLE_RATE`, whatever the device's own format is.
        let frames = (last.pcm.len() / usize::from(AUDIO_CHANNELS)) as u64;
        self.cursor_100ns = Some(last.timestamp_100ns + frames_to_100ns(frames, AUDIO_SAMPLE_RATE));
        self.cursor_at = Instant::now();
    }

    /// A silent packet covering the quiet so far, or `None` if it is too soon.
    fn fill(&mut self) -> Option<NormalizedAudioPacket> {
        let silent_for_100ns = (self.cursor_at.elapsed().as_nanos() / 100) as i64;
        let frames = idle_silence_frames(silent_for_100ns)?;
        let start_100ns = match self.cursor_100ns {
            Some(cursor) => cursor,
            // Nothing has ever arrived -- the case that used to kill the
            // capture. Anchor to now minus what is about to be emitted, so the
            // first silence ends at the present rather than starting there.
            None => unsafe {
                windows::Win32::Media::MediaFoundation::MFGetSystemTime()
                    - frames_to_100ns(u64::from(frames), AUDIO_SAMPLE_RATE)
            },
        };
        let duration = frames_to_100ns(u64::from(frames), AUDIO_SAMPLE_RATE);
        self.cursor_100ns = Some(start_100ns + duration);
        self.cursor_at = Instant::now();
        self.total_frames += u64::from(frames);
        if self.total_frames >= self.next_log_frames {
            tracing::info!(
                event = "audio_idle_synthetic_silence",
                cumulative_idle_silence_frames = self.total_frames,
                "the audio endpoint is quiet; filling the track with synthetic silence"
            );
            self.next_log_frames = self.total_frames + u64::from(AUDIO_SAMPLE_RATE);
        }
        Some(NormalizedAudioPacket {
            timestamp_100ns: start_100ns,
            pcm: vec![0.0; frames as usize * usize::from(AUDIO_CHANNELS)],
        })
    }
}

pub fn drift_100ns(audio_timestamp_100ns: i64, video_timestamp_100ns: i64) -> i64 {
    audio_timestamp_100ns - video_timestamp_100ns
}

#[cfg(test)]
mod tests {
    use super::*;
    /// COM on this thread and a loopback capture pointed at our own process:
    /// what every test below needs before it can poke at one.
    fn started_loopback() -> ProcessLoopbackAudioCapture {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
            .expect("COM");
        }
        ProcessLoopbackAudioCapture::start(std::process::id())
            .expect("Application Loopback activation and PCM initialization")
    }
    /// Task198: a monitor has no process to point at, so its sound is the
    /// default render endpoint's whole mix. Shared mode accepts only that
    /// device's own format, which is why this path asks `GetMixFormat` -- and
    /// why it has to hand `Initialize` the original allocation rather than a
    /// `WAVEFORMATEX`-sized copy of it. Failing here is invisible in the app:
    /// the trial in `worker.rs` just sets `audio_available = false` and the
    /// recording comes out with no audio track at all.
    #[test]
    fn system_loopback_starts_on_the_default_render_endpoint() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
            .expect("COM");
        }
        let capture = ProcessLoopbackAudioCapture::start_system()
            .expect("system loopback activation on the default render endpoint");
        capture.stop();
    }

    /// Task1430: the "record other applications too" toggle's whole mechanism.
    /// `EXCLUDE_TARGET_PROCESS_TREE` pointed at this process is the only way to
    /// get every other application's sound on one track without the review
    /// playback Liveback itself makes during a recording being recorded back
    /// into it. If this activation is not available, the toggle silently
    /// records nothing at all -- the trial in `worker.rs` just reports
    /// `audio_unavailable` and the recording comes out mute.
    #[test]
    fn excluding_self_activates_the_same_process_loopback_device() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
            .expect("COM");
        }
        let capture = AudioSource::AllAppsExceptSelf
            .start()
            .expect("EXCLUDE_TARGET_PROCESS_TREE activation and PCM initialization");
        capture.stop();
    }

    /// Task410. A default render endpoint nobody is playing through never
    /// produces a packet, so the audio track never advances -- and because the
    /// fMP4 sink interleaves, video stopped being accepted about 1.6 seconds
    /// in and the whole capture died. The fix is to notice the quiet and fill
    /// it; this is the noticing, with WASAPI left out of it.
    #[test]
    fn an_endpoint_that_stays_quiet_earns_synthetic_silence() {
        // Too soon to tell an idle endpoint from a late period event: the wait
        // itself is 200ms, so nothing under that can mean anything.
        assert_eq!(idle_silence_frames(0), None);
        assert_eq!(idle_silence_frames(2_000_000), None);
        assert_eq!(idle_silence_frames(IDLE_SILENCE_START_100NS - 1), None);

        // At the threshold and beyond, the frame count is the quiet converted
        // at the encoder's rate.
        assert_eq!(
            idle_silence_frames(IDLE_SILENCE_START_100NS),
            Some(AUDIO_SAMPLE_RATE * 3 / 10)
        );
        assert_eq!(
            idle_silence_frames(10_000_000),
            Some(AUDIO_SAMPLE_RATE),
            "one second of quiet is one second of frames"
        );

        // And it keeps going: the one-second interpolation cap is for gaps in
        // a *running* stream, and applying it here would let the audio track
        // stall again -- which is the whole bug.
        let ten_minutes = 600 * 10_000_000;
        assert_eq!(
            idle_silence_frames(ten_minutes),
            Some(AUDIO_SAMPLE_RATE * 600)
        );
        assert!(
            u64::from(idle_silence_frames(ten_minutes).unwrap()) > MAX_SYNTHETIC_SILENCE_GAP_FRAMES
        );
    }
    /// Task088 regression: capture used to be driven only by `poll()` calls the video
    /// capture loop made on accepted-frame arrival (`capture.rs`), so on a source with sparse
    /// video (WGC frame starvation on a static/quiet scene) audio silently accumulated past
    /// what the ~30ms effective endpoint buffer could hold between polls and was dropped.
    /// Nothing in this test ever calls `poll()` -- the only thing driving capture is
    /// `run_event_driven_capture`'s own WASAPI period event, on its own dedicated thread.
    /// If that decoupling regresses (event registration removed, thread not driven by the
    /// event), this test times out instead of observing any `Packets` event.
    #[test]
    fn run_event_driven_capture_delivers_packets_with_no_external_poll_caller() {
        unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
            .expect("COM");
        }
        let (tx, rx) = crossbeam_channel::bounded(64);
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();
        let source = AudioSource::SelectedProcess(std::process::id());
        let handle = std::thread::spawn(move || {
            run_event_driven_capture(source, 0, &tx, &stop_for_thread);
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut received_packets = false;
        while Instant::now() < deadline {
            if let Ok((0, AudioWorkerEvent::Packets(_))) =
                rx.recv_timeout(Duration::from_millis(500))
            {
                received_packets = true;
                break;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let _ = handle.join();
        assert!(
            received_packets,
            "event-driven capture must deliver packets on its own schedule, independent of \
             any external poll() caller"
        );
    }
    /// Task088 Step2 offline check: does the system AAC MFT carry leftover PCM
    /// across separate `ProcessInput` calls when fed WASAPI-period-sized
    /// (~10ms/~480-frame) chunks, the way `capture.rs`'s real capture loop
    /// does (it drains WASAPI once per incoming video frame, not once per AAC
    /// frame)? If it does, output sample count should track
    /// `total_frames_fed / 1024` closely and this app's own chunking has
    /// nothing to do with the sample-density defect (Task087/088). If it
    /// doesn't, most content silently vanishes instead of accumulating, and
    /// that is the defect site -- no real recording or rebuild needed to
    /// tell the two apart.
    #[test]
    fn aac_mft_output_frame_count_when_fed_small_wasapi_period_chunks() {
        let _runtime = crate::encoder::MfRuntime::start().expect("Media Foundation runtime");
        let encoder = AacEncoder::create().expect("Windows system AAC MFT");
        const FRAMES_PER_CALL: usize = 480; // ~10ms at 48kHz, matching a real WASAPI period
        const CALLS: usize = 40;
        let pcm_per_call = vec![0.0_f32; FRAMES_PER_CALL * AUDIO_CHANNELS as usize];
        let mut total_output_samples = 0usize;
        let mut timestamp_100ns = 0i64;
        let frame_duration_100ns =
            FRAMES_PER_CALL as i64 * 10_000_000 / i64::from(AUDIO_SAMPLE_RATE);
        for _ in 0..CALLS {
            let output = encoder
                .encode_f32(&pcm_per_call, timestamp_100ns)
                .expect("encode_f32 with a small WASAPI-period chunk must not error");
            total_output_samples += output.len();
            timestamp_100ns += frame_duration_100ns;
        }
        let total_frames_fed = FRAMES_PER_CALL * CALLS;
        let expected_output_samples = total_frames_fed / 1024;
        // The MFT holds back up to two frames of lookahead (16 of 18 on the
        // system encoder, measured 2026-09-26); a chunking defect loses most
        // of the content instead, so anything below that is the defect.
        assert!(
            (expected_output_samples - 2..=expected_output_samples).contains(&total_output_samples),
            "Task088 AAC MFT small-chunk carryover: calls={CALLS}, frames_per_call={FRAMES_PER_CALL}, \
             total_frames_fed={total_frames_fed}, expected_output_samples~={expected_output_samples}, \
             actual_output_samples={total_output_samples}"
        );
    }
    #[test]
    fn test_failure_injection_allows_only_one_reinitialization() {
        let mut recovery = AudioRecoveryState::default();
        assert_eq!(
            recovery.on_poll_failure(),
            AudioPollFailureAction::ReinitializeOnce
        );
        assert_eq!(recovery.reinitializations(), 1);
        assert_eq!(
            recovery.on_poll_failure(),
            AudioPollFailureAction::StopSafely
        );
    }
    #[test]
    fn silent_packet_gets_full_frame_count_of_zero_pcm_instead_of_empty_vec() {
        // AUDCLNT_BUFFERFLAGS_SILENT packets used to be dropped as an empty Vec, leaving
        // that span with zero audio fragments (see `poll`). They must instead carry
        // `frames` worth of zeroed PCM so the segment still gets an audio fragment.
        let block_align = 4usize;
        let pcm = zero_pcm(240, block_align);
        assert_eq!(pcm.len(), 240 * block_align);
        assert!(pcm.iter().all(|&byte| byte == 0));
    }
    #[test]
    fn device_position_discontinuity_is_detected_as_a_gap() {
        assert_eq!(device_position_gap_frames(None, 1_000), None);
        assert_eq!(device_position_gap_frames(Some(1_000), 1_000), None);
        assert_eq!(device_position_gap_frames(Some(1_000), 900), None);
        assert_eq!(device_position_gap_frames(Some(1_000), 1_240), Some(240));
    }
    #[test]
    fn gap_within_cap_is_interpolated_gap_beyond_cap_is_not() {
        assert!(should_interpolate_gap(MAX_SYNTHETIC_SILENCE_GAP_FRAMES));
        assert!(!should_interpolate_gap(
            MAX_SYNTHETIC_SILENCE_GAP_FRAMES + 1
        ));
    }
    #[test]
    fn normalize_to_f32_turns_zeroed_pcm_into_silent_samples() {
        let capture = started_loopback();
        let packet = AudioPacket {
            timestamp_100ns: 0,
            frames: 10,
            pcm: zero_pcm(10, 4),
        };
        let samples = capture.normalize_to_f32(&packet);
        assert_eq!(samples.len(), 10 * AUDIO_CHANNELS as usize);
        assert!(samples.iter().all(|&sample| sample == 0.0));
        capture.stop();
    }
}
