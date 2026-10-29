use std::{
    collections::VecDeque,
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

use crate::encoder;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use windows::{
    core::{factory, AgileReference, IInspectable, Interface},
    Foundation::{TimeSpan, TypedEventHandler},
    Graphics::{
        Capture::{Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession},
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Foundation::HWND,
        Graphics::{
            Direct3D11::{ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D},
            Gdi::HMONITOR,
        },
        System::{
            Com::{CoInitializeEx, COINIT_MULTITHREADED},
            WinRT::{
                Direct3D11::IDirect3DDxgiInterfaceAccess,
                Graphics::Capture::IGraphicsCaptureItemInterop,
            },
        },
        UI::WindowsAndMessaging::{IsIconic, IsWindow},
    },
};

use super::targets::CaptureTargetKind;
use super::{
    audio, gpu, CaptureColorSpace, CaptureConfig, CaptureSize, CaptureStopReason, CapturedFrame,
    FrameDebug, FrameThrottle, GpuNv12Converter, RawCaptureFrame, TransformPipeline,
    FRAME_QUEUE_CAPACITY,
};

mod encode;
mod setup;
#[cfg(test)]
pub(in crate::capture) use setup::overhang;
use setup::{
    maximized_overhang, monitor_hdr_state, monitor_sdr_white_point, monitor_uses_hdr, parse_hwnd,
    window_monitor,
};

/// How often a running recording re-reads its monitor's HDR state (t260917-7faa).
const HDR_RECHECK_INTERVAL: Duration = Duration::from_secs(1);

/// The capture monitor's HDR state as the tone mapping sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct HdrState {
    pub(super) hdr: bool,
    pub(super) white_point: f32,
}

/// What a recording has to switch to when a re-read [`HdrState`] differs from
/// the one it is recording with (t260917-7faa).
///
/// Only the HDR flag counts. A white level that moves while HDR stays on is
/// *not* followed: task1820 measured that the composition scale WGC hands over
/// is frozen at session start and does not move with the SDR brightness
/// slider, so re-dividing by the new level would break the identity. A flip of
/// the flag changes the frame format itself; turning HDR on takes the white
/// point re-read with it.
///
/// Turning HDR off keeps the white point the recording had. The SDR shader
/// never reads it, and the float frames still in flight when the flag flips
/// are HDR-composited: measured 2026-09-17 on a 1080p desk,
/// dividing those by the
/// SDR floor instead blew out 8 frames (~133ms) to `60 -> 110`, `120 -> 209`.
fn hdr_follow(recording: HdrState, current: HdrState) -> Option<HdrState> {
    (recording.hdr != current.hdr).then_some(if current.hdr {
        current
    } else {
        HdrState {
            hdr: false,
            white_point: recording.white_point,
        }
    })
}

/// One segment boundary under observation (task202). Holds what the rotation
/// cost and what happened to the frames right after it, so the recorded hole
/// can be attributed rather than guessed at.
struct BoundaryProbe {
    started: std::time::Instant,
    readback_us: u64,
    pts_100ns: i64,
    throttled_before: u64,
    no_credit_before: u64,
    queue_full_before: u64,
    /// How long the previous boundary's credit took to climb back, in ms:
    /// -1 never, -2 no previous boundary (t261019-9ce9).
    prev_credit_recovery_ms: i64,
    submitted: Vec<i64>,
    /// `SegmentMuxer::video_keyframes` when the boundary was asked for, before
    /// its own keyframe is written (t261019-8f9b).
    keyframes: u64,
    /// The boundary thumbnail's queue-to-collect time and the drains that
    /// found it not ready (t261017-613d), once `drain` has read it back.
    thumbnail_collect: Option<(Duration, u32)>,
}

impl BoundaryProbe {
    /// Opens a probe over the rotation that starts at `pts_100ns`.
    ///
    /// Task202: every segment starts with a 250-290ms hole in its recorded
    /// frames. The thumbnail readback is a synchronous GPU->CPU map on the one
    /// thread that drains `raw_frames` (capacity 1), so `readback_started` is
    /// taken before it and its cost is measured against the frames dropped and
    /// the encoder credit missing over the same boundary. Since t261017-613d
    /// the boundary only queues the copy, so `readback_us` is the queueing
    /// cost and the wait moved to `thumbnail_collect`.
    fn open(
        readback_started: std::time::Instant,
        pts_100ns: i64,
        frame_debug: &FrameDebug,
        keyframes: u64,
        prev_credit_recovery: Option<Option<Duration>>,
    ) -> Self {
        Self {
            started: readback_started,
            readback_us: readback_started.elapsed().as_micros() as u64,
            pts_100ns,
            throttled_before: frame_debug.throttled.load(Ordering::Relaxed),
            no_credit_before: frame_debug.no_credit.load(Ordering::Relaxed),
            queue_full_before: frame_debug.queue_full.load(Ordering::Relaxed),
            prev_credit_recovery_ms: match prev_credit_recovery {
                None => -2,
                Some(None) => -1,
                Some(Some(took)) => took.as_millis() as i64,
            },
            submitted: Vec::new(),
            keyframes,
            thumbnail_collect: None,
        }
    }

    /// Whether enough of the window after the rotation has gone by to show
    /// whether frames kept flowing.
    fn is_due(&self) -> bool {
        self.submitted.len() >= 12 || self.started.elapsed() >= Duration::from_millis(500)
    }
}

/// Whether the buffer disk has run below `MIN_FREE_BYTES` (task1420), logging
/// the reason it says so. Failing to read the free space is not a reason to end
/// a recording, so that only logs and answers false.
fn buffer_disk_is_full(disk_root: &Path) -> bool {
    match crate::ring_buffer::free_bytes(disk_root) {
        Ok(free) if free < crate::ring_buffer::MIN_FREE_BYTES => {
            tracing::warn!(
                event = "disk_space_low",
                free_bytes = free,
                min_free_bytes = crate::ring_buffer::MIN_FREE_BYTES,
                "stopping the recording: the buffer disk is nearly full"
            );
            true
        }
        Ok(_) => false,
        Err(error) => {
            tracing::warn!(
                event = "disk_space_unknown",
                %error,
                "could not read the buffer disk's free space; the recording continues"
            );
            false
        }
    }
}

/// Microseconds, or -1 for not measured.
fn us_or_unknown(value: Option<Duration>) -> i64 {
    value.map_or(-1, |value| value.as_micros() as i64)
}

/// The task202 boundary probe, once enough of the window after a rotation has
/// gone by to show whether frames kept flowing. `credit` is the encoder's
/// credit over the same window and its recovery time, if known yet
/// (t261019-9ce9).
fn report_segment_boundary(
    probe: &BoundaryProbe,
    frame_debug: &FrameDebug,
    credit: Option<(encoder::CreditWindow, Option<Duration>)>,
) {
    let gaps: Vec<i64> = probe
        .submitted
        .windows(2)
        .map(|pair| (pair[1] - pair[0]) / 10_000)
        .collect();
    // -1: not read back yet when the probe closed (12 frames / 500ms).
    let (collect_us, not_ready) = probe
        .thumbnail_collect
        .map_or((-1, 0), |(elapsed, not_ready)| {
            (elapsed.as_micros() as i64, not_ready)
        });
    let (window, recovery) = match &credit {
        Some((window, recovery)) => (Some(window), *recovery),
        None => (None, None),
    };
    tracing::info!(
        target: "task202_boundary",
        readback_us = probe.readback_us,
        thumbnail_collect_us = collect_us,
        thumbnail_not_ready = not_ready,
        pts_100ns = probe.pts_100ns,
        submitted = probe.submitted.len(),
        throttled = frame_debug.throttled.load(Ordering::Relaxed) - probe.throttled_before,
        no_credit = frame_debug.no_credit.load(Ordering::Relaxed) - probe.no_credit_before,
        gaps_ms = ?gaps,
        // t261019-9ce9. -1 = not seen in the window.
        credit_at_open = window.map_or(-1, |w| i64::from(w.credit_at_open)),
        credit_min = window.map_or(-1, |w| i64::from(w.credit_min)),
        need_input_max = window.map_or(-1, |w| i64::from(w.need_input_max)),
        poll_gap_max_ms = window.map_or(-1, |w| w.poll_gap_max.as_millis() as i64),
        idr_latency_us = us_or_unknown(window.and_then(|w| w.first_latency)),
        p_latency_p50_us = us_or_unknown(window.and_then(|w| w.other_latency_p50())),
        p_latency_n = window.map_or(0, |w| w.other_latencies.len()),
        credit_recovery_ms = recovery.map_or(-1, |took| took.as_millis() as i64),
        prev_credit_recovery_ms = probe.prev_credit_recovery_ms,
        queue_full = frame_debug.queue_full.load(Ordering::Relaxed) - probe.queue_full_before,
        // t261021-375d: the IDR's submit-to-output, split at the moment its
        // conversion was seen finishing on the GPU. -1 = not seen.
        idr_convert_wait_us = us_or_unknown(window.and_then(|w| w.first_split).map(|s| s.0)),
        idr_nvenc_us = us_or_unknown(window.and_then(|w| w.first_split).map(|s| s.1)),
        // Last: the evidence scripts match `submitted=… throttled=` and
        // `no_credit=… gaps_ms=[…]` as adjacent pairs.
        keyframes = probe.keyframes,
        "segment boundary"
    );
}

/// Whether the target window has gone, asked at most four times a second.
///
/// The check used to live only in the `default` arm, which crossbeam only
/// reaches when *no* other arm is ready -- an audio stream delivering packets
/// every ~10ms starved it forever, so a closed target was never detected while
/// it played audio. Every arm asks now, and this is the throttle that keeps
/// that cheap.
fn target_closed_since(
    last_window_check: &mut std::time::Instant,
    monitor_target: bool,
    hwnd: HWND,
) -> bool {
    if last_window_check.elapsed() < Duration::from_millis(250) {
        return false;
    }
    *last_window_check = std::time::Instant::now();
    target_window_gone(monitor_target, hwnd)
}

/// Whether the capture target's window has gone. A monitor target carries an
/// HMONITOR in the same field (task165) and has no window to lose, so it never
/// has.
fn target_window_gone(monitor_target: bool, hwnd: HWND) -> bool {
    !monitor_target && unsafe { !IsWindow(Some(hwnd)).as_bool() }
}

/// Converts a Rust-side failure into the `windows::core::Error` the capture
/// worker's signatures deal in -- **after writing down what it actually was**
/// (task410).
///
/// Every one of these used to be `map_err(|_| Error::from_win32())`. That
/// discards the message and then asks `GetLastError()`, which a Rust-side
/// failure never set, so the log read `hresult=0` and said nothing else. The
/// bug this function was written for -- the fMP4 sink refusing video because
/// the audio track had never advanced -- cost about forty minutes to find, and
/// the whole of that was putting this line back temporarily to see
/// `MF_E_NOTACCEPTING` once.
///
/// `from_win32()` is still what gets returned: the callers' types are
/// unchanged, and the useful half now lives in the log instead of being
/// thrown away. Every call site is a `?` that ends the capture, so this
/// cannot log per frame -- the first one to fire is also the last.
fn worker_error<E: std::fmt::Debug>(error: E) -> windows::core::Error {
    tracing::error!(
        event = "capture_worker_step_failed",
        ?error,
        "a capture worker step failed; the hresult reported alongside is from GetLastError and is \
         usually 0 for a failure that started on the Rust side"
    );
    windows::core::Error::from_win32()
}

/// How long AAC samples may sit queued before the capture is failed (task610).
///
/// The queue exists because the fMP4 sink refuses audio until this thread
/// drains the H.264 encoder into it, and this thread cannot do that while it is
/// sleeping on the refusal. Going round the loop clears it in one pass, so a
/// queue that is still full seconds later is not back-pressure any more -- it
/// is a sink that has stopped accepting, which is a real failure and has to be
/// reported rather than absorbed (task410's disease).
const PENDING_AAC_LIMIT: Duration = Duration::from_secs(5);

/// Which track's front sample goes to the sink next: the oldest one that is
/// not already blocked, or `None` when every track is empty or blocked
/// (task1260).
///
/// Its own function because it is the whole liveness argument for multi-track
/// audio and the only part of it that can be checked without a real sink.
pub(crate) fn next_pending_track(fronts: &[Option<i64>], blocked: &[bool]) -> Option<usize> {
    let mut best: Option<(usize, i64)> = None;
    for (track, front) in fronts.iter().enumerate() {
        if blocked.get(track).copied().unwrap_or(false) {
            continue;
        }
        let Some(timestamp) = front else {
            continue;
        };
        if best.is_none_or(|(_, chosen)| *timestamp < chosen) {
            best = Some((track, *timestamp));
        }
    }
    best.map(|(track, _)| track)
}

/// Offers queued AAC samples to the muxer, oldest first, until one is refused
/// or the queue empties. Returns how many landed.
///
/// A refusal is not an error here: it means "come back after you have run your
/// loop once". Nothing is ever dropped -- `PENDING_AAC_LIMIT` is what turns a
/// refusal that never clears into a reported failure.
fn flush_pending_aac(
    pending: &mut [VecDeque<windows::Win32::Media::MediaFoundation::IMFSample>],
    muxer: &mut encoder::SegmentMuxer,
    waiting_since: &mut Option<std::time::Instant>,
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<u64> {
    let mut pushed = 0;
    // Oldest sample first *across* tracks, not one track drained at a time
    // (task1260). The fMP4 sink interleaves every stream and refuses one that
    // runs ahead of the others, so draining track 0 to exhaustion before
    // touching track 1 recreates exactly the stall task610 was written for.
    // Per-track FIFO order still holds: only the fronts ever compete.
    let mut blocked = vec![false; pending.len()];
    loop {
        let fronts: Vec<Option<i64>> = pending
            .iter()
            .map(|queue| {
                queue
                    .front()
                    .map(|sample| unsafe { sample.GetSampleTime() }.unwrap_or(i64::MAX))
            })
            .collect();
        let Some(track) = next_pending_track(&fronts, &blocked) else {
            break;
        };
        let sample = pending[track].front().expect("checked above").clone();
        let push = muxer
            .push_aac_sample_on(track, &sample)
            .map_err(worker_error)?;
        for event in push.events {
            emit(event);
        }
        if !push.accepted {
            blocked[track] = true;
            continue;
        }
        pending[track].pop_front();
        pushed += 1;
    }
    let pending_total: usize = pending.iter().map(VecDeque::len).sum();
    *waiting_since = if pending_total == 0 {
        if let Some(since) = waiting_since.take() {
            tracing::warn!(
                target: "task610_backpressure",
                queued_ms = since.elapsed().as_millis() as u64,
                pushed,
                "the audio sink started taking samples again"
            );
        }
        None
    } else {
        Some(match *waiting_since {
            Some(since) => since,
            None => {
                tracing::warn!(
                    target: "task610_backpressure",
                    queued = pending_total,
                    pushed,
                    // Index, segment start and the audio track's shift: where a
                    // refused sample was being placed is the thing worth
                    // knowing, and a stale priming unit shows up here as an
                    // offset in the hundreds of milliseconds.
                    placement = ?muxer.open_audio_placement(),
                    // Per track: the head's capture timestamp, which says
                    // whether the refused sample belongs to the open segment
                    // or the closing one (t261007-a63e).
                    first_pending_aac_100ns = ?pending
                        .iter()
                        .map(|queue| queue
                            .front()
                            .and_then(|sample| unsafe { sample.GetSampleTime() }.ok()))
                        .collect::<Vec<_>>(),
                    "the audio sink is refusing; queueing instead of sleeping on it"
                );
                std::time::Instant::now()
            }
        })
    };
    Ok(pushed)
}

impl encode::EncodePath {
    /// Drains the encoder into the muxer: pump, offer the queued audio, run
    /// `on_turn`, until the encoder is done *and* the muxer holds no refused
    /// video, or `encoder::HELD_VIDEO_LIMIT` has passed. `true` when it
    /// finished. The resize and the stop both go through here.
    ///
    /// Not `drain_complete()` alone (t260911-e7f3): the MFT can be finished
    /// while the sink is still refusing the samples it produced, and leaving
    /// the loop there would hand `muxer.finalize()` a queue to log away. The
    /// wall clock is what keeps a genuinely stuck sink from holding the stop
    /// open -- `finalize` gives it the same five seconds once more.
    fn drain_until_complete(
        &mut self,
        state: &mut encode::LoopState,
        emit: &impl Fn(encoder::EncoderEvent),
        mut on_turn: impl FnMut(&mut Self),
    ) -> windows::core::Result<bool> {
        self.encoder.begin_drain().map_err(worker_error)?;
        let started = std::time::Instant::now();
        loop {
            self.pump(emit)?;
            state.aac_samples_pushed += flush_pending_aac(
                &mut state.pending_aac,
                &mut self.muxer,
                &mut state.pending_aac_since,
                emit,
            )?;
            on_turn(self);
            if self.encoder.drain_complete() && self.muxer.pending_video_len() == 0 {
                return Ok(true);
            }
            if started.elapsed() >= encoder::HELD_VIDEO_LIMIT {
                return Ok(false);
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
}

/// How long the watchdog waits before re-supplying the last frame (task163):
/// two frame intervals. WGC only delivers a frame when the content *changes*,
/// so a still target -- a paused game, a minimized window -- stops the encoder
/// dead and `derive_gaps` later reads the silence as a hole in the recording.
fn hold_interval(frame_rate: u8) -> Duration {
    Duration::from_millis(2 * 1000 / u64::from(frame_rate.max(1)))
}

/// Whether the frame-hold watchdog is due (task163).
pub(super) fn should_hold_frame(since_last_frame: Duration, frame_rate: u8) -> bool {
    since_last_frame >= hold_interval(frame_rate)
}

/// How long the capture loop may block before it must run again (task202).
///
/// The watchdog above is checked once per loop iteration, so it can only be as
/// punctual as the loop's slowest wake-up. That used to be a flat 250ms
/// `default` arm, and a still target with no audio to wake it therefore left a
/// 250-290ms hole at the head of every segment -- the watchdog wanted to fire
/// at 33ms and was not asked until 250ms had passed. Capping the block at
/// whatever is left of the hold interval makes the watchdog's own deadline the
/// thing that decides, whatever else is or isn't arriving.
///
/// `WINDOW_CHECK` is the floor-and-ceiling for everything else the arm does
/// (noticing a closed target); with no frame yet to hold there is nothing to be
/// punctual for, so it stands alone.
pub(super) fn loop_block_timeout(
    since_last_frame: Duration,
    have_frame: bool,
    frame_rate: u8,
) -> Duration {
    const WINDOW_CHECK: Duration = Duration::from_millis(250);
    if !have_frame {
        return WINDOW_CHECK;
    }
    hold_interval(frame_rate)
        .saturating_sub(since_last_frame)
        .clamp(Duration::from_millis(1), WINDOW_CHECK)
}

/// The frame's own D3D texture, or the error that stopped it -- each step
/// counted apart so a dead capture says which call failed (task029 probe).
fn frame_texture(
    frame: &windows::Graphics::Capture::Direct3D11CaptureFrame,
    frame_debug: &FrameDebug,
    n: u64,
) -> windows::core::Result<ID3D11Texture2D> {
    let surface = match frame.Surface() {
        Ok(surface) => surface,
        Err(error) => {
            frame_debug.err_surface.fetch_add(1, Ordering::Relaxed);
            frame_debug.log(&format!(
                "n={n} surface_err hresult={:#010x}",
                error.code().0
            ));
            return Err(error);
        }
    };
    let access: IDirect3DDxgiInterfaceAccess = match surface.cast() {
        Ok(access) => access,
        Err(error) => {
            frame_debug.err_cast.fetch_add(1, Ordering::Relaxed);
            frame_debug.log(&format!("n={n} cast_err hresult={:#010x}", error.code().0));
            return Err(error);
        }
    };
    let texture: ID3D11Texture2D = match unsafe { access.GetInterface() } {
        Ok(texture) => texture,
        Err(error) => {
            frame_debug
                .err_get_interface
                .fetch_add(1, Ordering::Relaxed);
            frame_debug.log(&format!(
                "n={n} get_interface_err hresult={:#010x}",
                error.code().0
            ));
            return Err(error);
        }
    };
    Ok(texture)
}

/// Rebuilds the frame pool when the target's content size changes under it,
/// or when the recording has switched between HDR and SDR input.
///
/// The pool's surfaces are fixed size, so a window that resizes mid-recording
/// would otherwise keep handing over frames at the old dimensions. The format
/// follows `hdr_input`, which the capture loop flips when the monitor's HDR
/// state changes (t260917-7faa).
/// The frame pool's pixel format: half floats for an HDR source, so nothing
/// above SDR white is clipped before the tone map, 8-bit BGRA otherwise.
fn pool_format(hdr: bool) -> DirectXPixelFormat {
    if hdr {
        DirectXPixelFormat::R16G16B16A16Float
    } else {
        DirectXPixelFormat::B8G8R8A8UIntNormalized
    }
}

/// The monitor a capture's colour comes from: the target itself for a
/// monitor, which carries its HMONITOR in the hwnd field (task165), or the
/// one a window is on.
fn capture_monitor(monitor_target: bool, hwnd: HWND) -> HMONITOR {
    if monitor_target {
        HMONITOR(hwnd.0)
    } else {
        window_monitor(hwnd)
    }
}

/// The black overhang to crop off a frame of `size` (task2770). A monitor has
/// none; a maximized window's frame hangs past the work area.
fn crop_for(monitor_target: bool, hwnd: HWND, size: CaptureSize) -> gpu::Inset {
    if monitor_target {
        gpu::Inset::default()
    } else {
        maximized_overhang(hwnd, size)
    }
}

fn recreate_pool_if_resized(
    pool: &Direct3D11CaptureFramePool,
    direct3d: &AgileReference<IDirect3DDevice>,
    (hdr_input, pool_hdr): (bool, &mut bool),
    pool_size: &mut SizeInt32,
    input: SizeInt32,
    frame_debug: &FrameDebug,
    n: u64,
) -> windows::core::Result<()> {
    if input.Width == pool_size.Width && input.Height == pool_size.Height && hdr_input == *pool_hdr
    {
        return Ok(());
    }
    {
        let callback_device = direct3d.resolve()?;
        if let Err(error) = pool.Recreate(&callback_device, pool_format(hdr_input), 2, input) {
            frame_debug.err_recreate.fetch_add(1, Ordering::Relaxed);
            frame_debug.log(&format!(
                "n={n} recreate_err hresult={:#010x} new_size={}x{}",
                error.code().0,
                input.Width,
                input.Height
            ));
            return Err(error);
        }
        *pool_size = input;
        *pool_hdr = hdr_input;
        frame_debug.log(&format!(
            "n={n} pool_recreated new_size={}x{} hdr={hdr_input}",
            input.Width, input.Height
        ));
    }
    Ok(())
}

/// Lifts WGC's own cap on how often it hands this session a frame.
///
/// `MinUpdateInterval` is the smallest gap DWM leaves between two frames it puts
/// in the pool, and nothing here ever set it, so every recording ran at the
/// system default of 16.000ms -- **not** at the rate the settings asked for.
/// DWM rounds the interval up to a whole number of refreshes, so the rate that
/// comes out is `refresh / ceil(interval / refresh_period)`: on a 279.86Hz
/// display 16ms is five refreshes, which is the 55.97/s that task1710 measured
/// and mistook for an upstream ceiling (see its 2026-09-13 postscript).
///
/// Half the target period, not the period itself: asking for exactly 1/120s
/// rounds *up* to three refreshes and lands on 93/s. Half rounds to two and
/// lands on 140/s, and `FrameThrottle` -- which every arrival already passes
/// through -- trims that to the 120 the settings asked for. The cost of the
/// overshoot is a cheap early return per skipped arrival; the cost of
/// undershooting is a recording that silently ignores its own setting.
///
/// Not fatal, for `SetIsBorderRequired`'s reason: `IGraphicsCaptureSession5` is
/// Win11-only, and a recording at the system default beats no recording.
fn configure_min_update_interval(session: &GraphicsCaptureSession, frame_rate: u8) {
    let wanted = TimeSpan {
        Duration: 10_000_000 / (2 * i64::from(frame_rate)),
    };
    let default_100ns = session
        .MinUpdateInterval()
        .map(|interval| interval.Duration);
    match session.SetMinUpdateInterval(wanted) {
        Ok(()) => tracing::info!(
            event = "wgc_min_update_interval",
            default_100ns = ?default_100ns,
            wanted_100ns = wanted.Duration,
            frame_rate,
            "WGC minimum update interval set"
        ),
        Err(error) => tracing::warn!(
            event = "wgc_min_update_interval",
            %error,
            default_100ns = ?default_100ns,
            "WGC minimum update interval could not be set; keeping the system default"
        ),
    }
}

/// The WGC callback: throttle, unwrap the frame's texture and hand it to the
/// capture loop over `raw_frames`. Runs on WGC's own thread with the capture
/// target waiting behind the pool, so everything it does lands on the user's
/// game (task205) -- see the tearing invariant inside before adding work.
fn frame_arrived_handler(
    direct3d: AgileReference<IDirect3DDevice>,
    sender: Sender<RawCaptureFrame>,
    frame_debug: Arc<FrameDebug>,
    mut throttle: FrameThrottle,
    mut pool_size: SizeInt32,
    hdr_input: Arc<AtomicBool>,
    content: Arc<AtomicU64>,
) -> TypedEventHandler<Direct3D11CaptureFramePool, IInspectable> {
    let mut pool_hdr = hdr_input.load(Ordering::Relaxed);
    TypedEventHandler::new(
        move |pool: windows::core::Ref<'_, Direct3D11CaptureFramePool>, _| {
            // WGC calls this on its own thread and the capture target waits
            // on the pool behind it, so this callback is the scope whose
            // cost lands on the user's game (task205).
            crate::insight_scope!("wgc_frame_arrived");
            let n = frame_debug.arrived.fetch_add(1, Ordering::Relaxed) + 1;
            let pool = pool.ok()?;
            let frame = match pool.TryGetNextFrame() {
                Ok(frame) => frame,
                Err(error) => {
                    frame_debug
                        .err_try_get_frame
                        .fetch_add(1, Ordering::Relaxed);
                    frame_debug.log(&format!(
                        "n={n} try_get_next_frame_err hresult={:#010x}",
                        error.code().0
                    ));
                    return Err(error);
                }
            };
            let timestamp = match frame.SystemRelativeTime() {
                Ok(t) => t.Duration,
                Err(error) => {
                    frame_debug
                        .err_system_relative_time
                        .fetch_add(1, Ordering::Relaxed);
                    frame_debug.log(&format!(
                        "n={n} system_relative_time_err hresult={:#010x}",
                        error.code().0
                    ));
                    return Err(error);
                }
            };
            if n <= 10 {
                frame_debug.log(&format!("n={n} arrived ts_100ns={timestamp}"));
            }
            if !throttle.accepts(timestamp) {
                frame_debug.throttled.fetch_add(1, Ordering::Relaxed);
                if n <= 10 {
                    frame_debug.log(&format!("n={n} throttled ts_100ns={timestamp}"));
                }
                return Ok(());
            }
            frame_debug.accepted.fetch_add(1, Ordering::Relaxed);
            let texture = frame_texture(&frame, &frame_debug, n)?;
            let input = frame.ContentSize()?;
            content.store(pack_size(input), Ordering::Relaxed);
            recreate_pool_if_resized(
                pool,
                &direct3d,
                (hdr_input.load(Ordering::Relaxed), &mut pool_hdr),
                &mut pool_size,
                input,
                &frame_debug,
                n,
            )?;
            // Tearing invariant (task1810, measured by task1710 on 2026-08-27).
            //
            // `texture` is the pool's own surface, not a copy. Holding the COM
            // reference keeps the object alive but does NOT stop WGC from writing
            // the next frame into it, and `frame` drops at the end of this closure,
            // which returns the surface to the pool immediately. So the downstream
            // reader races WGC by design, and only the timing margin makes it safe:
            //
            //   pool depth                 2 surfaces (CreateFreeThreaded/Recreate)
            //   arrival rate               set by `configure_min_update_interval`
            //                              to at least the recording's frame rate
            //                              and **below twice it** -- that bound is
            //                              the invariant, not any one reading.
            //                              At the 120 setting, 2026-09-13:
            //                              139/s against a windowed 1280x720
            //                              source, 214/s against a 1920x1080 one
            //                              clipped by the screen edge
            //   same surface rewritten     2 x period: 14.3ms and 9.3ms above,
            //                              8.3ms at the 240/s the bound allows
            //   downstream round trip      mean 0.33ms, worst observed 1.42ms
            //                              (`capture_frame` scope, debug + insight,
            //                              re-measured 2026-09-13 by t260913-578a)
            //
            // Margin against the 1.42ms worst-case read: 10x at 139/s, 6.6x at the
            // measured 214/s, **5.9x at the 240/s the interval allows**. Judge by
            // that last one -- it is the only number a different display cannot
            // beat. All three are far from where task1710's stage 2 (copy into an
            // app-owned texture inside this handler, rather than more pool buffers)
            // would be needed.
            //
            // This used to read 55.9-56.0/s and ~36ms, which was not a property of
            // WGC at all: `MinUpdateInterval` sat at its 16ms default because
            // nothing set it. Re-check this block before raising the frame rate
            // ceiling or adding work to `capture_frame`.
            //
            // The worst case used to be 4.70ms, and the segment-boundary
            // thumbnail was most of it: it read the whole frame back to the CPU
            // and took 13-27ms doing it, which `capture_frame` wore once every
            // two seconds. task2820 shrank that readback to 480x270 on the GPU.
            //
            // Numbers: `.agents/tasks/evidence/1710-capture-ceiling/`,
            // `.agents/tasks/evidence/1660-120fps-budget/insight-run1-preprobe.log`
            // and `.agents/tasks/evidence/2820-thumbnail-readback/`.
            let raw = RawCaptureFrame {
                texture,
                timestamp_100ns: timestamp,
                input_size: CaptureSize {
                    width: input.Width,
                    height: input.Height,
                },
            };
            match sender.try_send(raw) {
                Ok(()) => Ok(()),
                Err(TrySendError::Full(_)) => {
                    // Still not an error -- dropping the newest frame is the
                    // right call when the consumer is behind. Counting it is
                    // what task1710 needs: a zero here says arrival is not
                    // being refused, so a low arrival rate has to come from
                    // upstream (the pool having no free surface) rather than
                    // from this queue.
                    frame_debug.queue_full.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Err(TrySendError::Disconnected(_)) => Ok(()),
            }
        },
    )
}

/// Whether this recording gets an audio track, and the AAC encoder for it
/// when it does (task088/task1430).
///
/// The trial `start()`/`stop()` is what answers "is there an audio session
/// at all" synchronously: `IAudioClient` is not `Send`, so the long-lived
/// capture has to be created on the thread that polls it, and this one only
/// decides.
fn start_audio_track(
    config: &CaptureConfig,
    monitor_target: bool,
    audio_source: audio::AudioSource,
    encoder_events: &Sender<encoder::EncoderEvent>,
) -> (Option<encoder::aac::AacEncoder>, bool) {
    // A window whose pid could not be read has nothing to point loopback at
    // -- unless the toggle is on, in which case the source does not depend
    // on the pid at all and the recording still gets its sound (task1430).
    if !monitor_target && config.process_id == 0 && !config.capture_all_audio {
        (None, false)
    } else {
        match (encoder::aac::AacEncoder::create(), audio_source.start()) {
            (Ok(aac), Ok(trial_capture)) => {
                trial_capture.stop();
                (Some(aac), true)
            }
            (aac, trial) => {
                // The only account anyone gets of a recording that came out
                // silent: nothing downstream distinguishes "no audio track"
                // from "no sound was playing", and task198 spent a session
                // proving that by hand (task165's monitor path failed here
                // with E_INVALIDARG and said nothing).
                tracing::warn!(
                    event = "audio_unavailable",
                    monitor = monitor_target,
                    process_id = config.process_id,
                    source = ?audio_source,
                    aac_error = ?aac.as_ref().err(),
                    loopback_error = ?trial.as_ref().err(),
                    "recording without an audio track"
                );
                // The trial may have started even though the AAC encoder did
                // not: stop it, or its IAudioClient keeps running (no Drop
                // impl) for the rest of the process.
                if let Ok(trial_capture) = trial {
                    trial_capture.stop();
                }
                // Never fall back from the scope that was asked for to a wider one. The
                // "record other applications too" toggle (task1430) is the only way a
                // recording reaches past the target's process tree, and it is an
                // explicit choice made in the settings screen -- an audio session that
                // fails to start is reported as no audio, never quietly replaced with
                // the desktop mix or another process's. An unavailable audio session
                // still permits isolated video capture.
                let _ = encoder_events.try_send(encoder::EncoderEvent::AudioUnavailable);
                (None, false)
            }
        }
    }
}

/// Submits one real frame to the encoder, rotating the segment first when this
/// timestamp is where the muxer wants the next one to start.
///
/// The frame present when the *next* segment's boundary is requested becomes
/// that segment's thumbnail once it actually opens a few frames later (encoder
/// latency), not the frame at the segment's own first video sample. At 2s
/// granularity that offset is visually inconsequential, but it is not
/// "exactly the first frame of the segment".
///
/// No credit means the encoder is full; the frame is counted and dropped, and
/// the pump that follows is what frees a slot for the next one.
fn encode_real_frame(
    path: &mut encode::EncodePath,
    texture: &ID3D11Texture2D,
    pts_100ns: i64,
    boundary: &mut Option<BoundaryProbe>,
    frame_debug: &FrameDebug,
    encoded_frame_count: &Arc<AtomicU64>,
) -> windows::core::Result<()> {
    if !path.encoder.has_input_credit() {
        frame_debug.no_credit.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    if path.should_rotate(pts_100ns) {
        let readback_started = std::time::Instant::now();
        path.capture_thumbnail(texture);
        let credit = path.encoder.input_credit();
        let prev_credit_recovery = path
            .encoder
            .credit_watch()
            .open_boundary(std::time::Instant::now(), credit);
        *boundary = Some(BoundaryProbe::open(
            readback_started,
            pts_100ns,
            frame_debug,
            path.muxer.video_keyframes(),
            prev_credit_recovery,
        ));
        path.request_keyframe()?;
    }
    path.submit(texture, pts_100ns)?;
    encoded_frame_count.fetch_add(1, Ordering::Relaxed);
    if let Some(probe) = boundary.as_mut() {
        probe.submitted.push(pts_100ns);
    }
    Ok(())
}

/// The Windows Graphics Capture side of a recording: the item being captured,
/// its frame pool and session, the two event registrations that have to be
/// removed again, and the D3D device everything above runs on.
///
/// One type because opening them is one sequence with one failure story
/// (`startup_stage`), and closing them is one sequence that has to happen in
/// the reverse order.
struct CaptureSurface {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// The WinRT wrapper the frame pool was created on. Kept so task4280's
    /// probe can read its refcount and `close` can try closing it.
    direct3d: IDirect3DDevice,
    hwnd: HWND,
    monitor_target: bool,
    /// Shared with the WGC callback, which rebuilds the pool in the matching
    /// format when this flips.
    hdr_input: Arc<AtomicBool>,
    state: SurfaceState,
    item: GraphicsCaptureItem,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    frame_arrived: i64,
    closed: i64,
    source_closed: Arc<AtomicBool>,
    frame_debug: Arc<FrameDebug>,
    raw_frames: Receiver<RawCaptureFrame>,
    /// What `open` read from the config, and the content size the output size
    /// came from: a held surface is reused only for a recording that wants the
    /// same (t260929-ea5e).
    opened_for: SurfaceKey,
    /// The last frame's content size, packed by `pack_size`. Written by the
    /// WGC callback, which keeps running while a held surface waits.
    content: Arc<AtomicU64>,
}

/// What moves on a surface while it records. Only the capture thread touches
/// it, through the `&mut` the worker holds on its surface.
struct SurfaceState {
    /// The monitor's HDR state this recording is converting for. Starts as
    /// read at open and is re-read every [`HDR_RECHECK_INTERVAL`] by
    /// `follow_hdr_toggle` (t260917-7faa).
    hdr: HdrState,
    last_hdr_check: std::time::Instant,
    /// What the encoder is sized to. Moves when the window is resized and
    /// stays resized long enough for `follow_resize` to rebuild the encoder.
    output_size: CaptureSize,
    /// Whether `output_size` has ever moved off the size `open` derived.
    output_moved: bool,
    /// The black overhang to crop off a frame of the size it was computed for
    /// (task2770). Re-read from the window when the frame size changes -- a
    /// maximize or restore mid-recording -- and only then: it is Win32 calls.
    maximized_crop: (CaptureSize, gpu::Inset),
    /// `StartCapture` runs once per surface, not once per recording.
    started: bool,
}

/// What a held surface has to match for the next recording to reuse it
/// instead of opening a new one (t260929-ea5e): everything `open` reads from
/// the config once, plus the content size `output_size` was derived from --
/// a window resized between two recordings gets a new resolution, as it did
/// when every recording opened its own surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SurfaceKey {
    include_cursor: bool,
    frame_rate: u8,
    requested_output: CaptureSize,
    content: u64,
}

impl SurfaceKey {
    fn wanted(config: &CaptureConfig, content: u64) -> Self {
        Self {
            include_cursor: config.include_cursor,
            frame_rate: config.frame_rate,
            requested_output: config.output_size,
            content,
        }
    }
}

fn pack_size(size: SizeInt32) -> u64 {
    (u64::from(size.Width as u32) << 32) | u64::from(size.Height as u32)
}

/// The recording's own D3D device, named for task4280's live-object report.
fn open_capture_device(
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    // task4280: anything listed here belongs to an earlier recording (or
    // the review/UI side), because this recording has created nothing yet.
    super::leak_probe::report_live_objects("before_capture_device");
    let (device, context, direct3d) = crate::gpu::create_d3d_device()?;
    if super::leak_probe::enabled() {
        let name = b"livia-capture-surface-device";
        unsafe {
            let _ = device.SetPrivateData(
                &windows::Win32::Graphics::Direct3D::WKPDID_D3DDebugObjectName,
                name.len() as u32,
                Some(name.as_ptr().cast()),
            );
        }
    }
    Ok((device, context, direct3d))
}

/// The WGC item for the target: a monitor carries its HMONITOR in the same
/// hex field a window carries its HWND (task165).
fn create_capture_item(
    monitor_target: bool,
    hwnd: HWND,
) -> windows::core::Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, _>()?;
    pin_graphics_capture_dll();
    unsafe {
        if monitor_target {
            interop.CreateForMonitor(HMONITOR(hwnd.0))
        } else {
            interop.CreateForWindow(hwnd)
        }
    }
}

/// Keeps `GraphicsCapture.dll` mapped for the rest of the process
/// (t261029-c4c3). COM may unload it as soon as its factory refcount drops --
/// on the capture thread's exit when it was the last MTA thread -- while a
/// thread the DLL started for a closed item is still pending. That thread's
/// first instruction then fetches unmapped code: an access violation at an
/// address in the DLL's unloaded range (`rw=8`), seen in 5/8 `close` runs
/// without an MTA kept alive, 0/8 with one. A pinned module is never unloaded.
fn pin_graphics_capture_dll() {
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_PIN,
    };
    let mut module = Default::default();
    // SAFETY: a lookup by name with an out-param we own; a pin is never released,
    // so there is nothing to free. The factory call above has loaded the DLL.
    let _ = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_PIN,
            windows::core::w!("GraphicsCapture.dll"),
            &mut module,
        )
    };
}

/// The item's content size. A window that was just created or is mid-restore
/// can briefly report a 0x0 content size; wait for it to settle instead of
/// failing the frame pool immediately on that transient race.
fn settled_item_size(item: &GraphicsCaptureItem) -> windows::core::Result<SizeInt32> {
    let mut size = item.Size()?;
    for _ in 0..20 {
        if size.Width > 0 && size.Height > 0 {
            break;
        }
        thread::sleep(Duration::from_millis(25));
        size = item.Size()?;
    }
    Ok(size)
}

/// Raises `source_closed` and the thread's stop when the target goes away.
fn watch_closed(
    item: &GraphicsCaptureItem,
    stop_signal: &Sender<()>,
) -> windows::core::Result<(Arc<AtomicBool>, i64)> {
    let source_closed = Arc::new(AtomicBool::new(false));
    let closed_flag = source_closed.clone();
    let closed_stop = stop_signal.clone();
    let token = item.Closed(&windows::Foundation::TypedEventHandler::new(move |_, _| {
        closed_flag.store(true, Ordering::Release);
        let _ = closed_stop.try_send(());
        Ok(())
    }))?;
    Ok((source_closed, token))
}

/// The capture session on `pool`, with the cursor, border and update-rate
/// settings `config` asks for.
fn open_session(
    pool: &Direct3D11CaptureFramePool,
    item: &GraphicsCaptureItem,
    config: &CaptureConfig,
) -> windows::core::Result<GraphicsCaptureSession> {
    let session = pool.CreateCaptureSession(item)?;
    session.SetIsCursorCaptureEnabled(config.include_cursor)?;
    // WGC は既定で対象の縁に黄色い枠を描く。IGraphicsCaptureSession3 は
    // Win11 / Win10 20348 以降にしかないので、無い環境では枠が出るだけ —
    // 録画開始を落とす理由にはならない（task2500）。
    if let Err(error) = session.SetIsBorderRequired(false) {
        tracing::warn!(%error, "capture border could not be turned off");
    }
    configure_min_update_interval(&session, config.frame_rate);
    Ok(session)
}

impl CaptureSurface {
    /// Whether the next recording, asking for `config`, can run on this
    /// surface. A closed target never can: its item is dead.
    fn reusable_for(&self, config: &CaptureConfig) -> bool {
        // A surface whose output size `follow_resize` moved is not reused even
        // when the window is back at its opening size: `output_size` no longer
        // says what the next recording would derive from it.
        !self.source_closed.load(Ordering::Acquire)
            && !self.state.output_moved
            && self.opened_for == SurfaceKey::wanted(config, self.content.load(Ordering::Relaxed))
    }

    /// Opens everything WGC needs for `config`, moving `startup_stage` on at
    /// each step so a failure names the step it failed at (task410).
    fn open(
        config: &CaptureConfig,
        stop_signal: &Sender<()>,
        startup_stage: &mut &'static str,
    ) -> windows::core::Result<Self> {
        *startup_stage = "d3d_device";
        let (device, context, direct3d) = open_capture_device()?;
        *startup_stage = "window_handle";
        // Not the parse error itself: it quotes the handle, and the startup
        // log must not carry it (`startup_stage_log_excludes_pid_hwnd_title_and_path`).
        let hwnd = parse_hwnd(&config.window_handle)
            .map_err(|_| worker_error("the capture target handle did not parse"))?;
        *startup_stage = "wgc_item";
        // A monitor target carries its HMONITOR in the same hex field a window
        // carries its HWND (task165), so the parse above is shared and only the
        // Win32 calls that actually interpret it branch.
        let monitor_target = config.kind == CaptureTargetKind::Monitor;
        let monitor = capture_monitor(monitor_target, hwnd);
        let hdr_input = monitor_uses_hdr(monitor, true);
        // Only meaningful for an HDR source, and only asked for then: an SDR
        // session would otherwise log a white level nothing reads (task1790).
        let hdr_white_point = if hdr_input {
            monitor_sdr_white_point(monitor)
        } else {
            gpu::HDR_WHITE_POINT_FLOOR
        };
        let item = create_capture_item(monitor_target, hwnd)?;
        *startup_stage = "wgc_frame_pool";
        let initial_input = settled_item_size(&item)?;
        let initial_size = CaptureSize {
            width: initial_input.Width,
            height: initial_input.Height,
        };
        // A maximized window's frame hangs past the work area and WGC records
        // that part black (task2770); the output is sized to what is left.
        let maximized_crop = crop_for(monitor_target, hwnd, initial_size);
        let output_size = if config.output_size.width > 0 && config.output_size.height > 0 {
            CaptureSize {
                width: config.output_size.width & !1,
                height: config.output_size.height & !1,
            }
        } else {
            output_for(initial_size, maximized_crop)
        };
        let (source_closed, closed_token) = watch_closed(&item, stop_signal)?;
        let size = initial_input;
        let frame_debug = Arc::new(FrameDebug::new(hdr_input, (size.Width, size.Height)));
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &direct3d,
            pool_format(hdr_input),
            2,
            size,
        )?;
        *startup_stage = "wgc_session";
        let session = open_session(&pool, &item, config)?;
        let (raw_sender, raw_frames) = bounded(FRAME_QUEUE_CAPACITY);
        let sender_for_callback = raw_sender.clone();
        let direct3d_for_callback = AgileReference::new(&direct3d)?;
        let pool_size = initial_input;
        let hdr_input_shared = Arc::new(AtomicBool::new(hdr_input));
        let throttle = FrameThrottle::new(config.frame_rate).map_err(worker_error)?;
        let content = Arc::new(AtomicU64::new(pack_size(initial_input)));
        let token = pool.FrameArrived(&frame_arrived_handler(
            direct3d_for_callback,
            sender_for_callback,
            frame_debug.clone(),
            throttle,
            pool_size,
            hdr_input_shared.clone(),
            content.clone(),
        ))?;
        let alive = SURFACES_ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            event = "capture_surface_opened",
            alive,
            width = output_size.width,
            height = output_size.height,
            "capture surface opened"
        );
        // Positive control for the report above: this recording's device has to
        // appear here, or an empty "before" proves nothing.
        super::leak_probe::report_live_objects("after_capture_surface_open");
        Ok(CaptureSurface {
            device,
            context,
            direct3d,
            hwnd,
            monitor_target,
            hdr_input: hdr_input_shared,
            state: SurfaceState {
                hdr: HdrState {
                    hdr: hdr_input,
                    white_point: hdr_white_point,
                },
                last_hdr_check: std::time::Instant::now(),
                output_size,
                output_moved: false,
                maximized_crop: (initial_size, maximized_crop),
                started: false,
            },
            item,
            pool,
            session,
            frame_arrived: token,
            closed: closed_token,
            source_closed,
            frame_debug,
            raw_frames,
            opened_for: SurfaceKey::wanted(config, pack_size(initial_input)),
            content,
        })
    }

    /// Unwinds what `open` set up, in reverse.
    fn close(self) -> windows::core::Result<()> {
        self.pool.RemoveFrameArrived(self.frame_arrived)?;
        self.session.Close()?;
        self.pool.Close()?;
        self.item.RemoveClosed(self.closed)?;
        Ok(())
    }
}

/// Holds the probe's references until the end of `close_surface`, which runs
/// once the recording is gone -- after every recording that did not hold its
/// surface, and never for one that did (t260929-ea5e) -- then reads them.
struct SurfaceRefsGuard(Option<SurfaceRefs>);

impl Drop for SurfaceRefsGuard {
    fn drop(&mut self) {
        if let Some(refs) = self.0.take() {
            refs.finish();
            super::leak_probe::report_live_objects("after_record_teardown");
        }
    }
}

/// Extra references to a `CaptureSurface`'s objects, taken just before it
/// closes so their refcounts can be read once the surface *and* the recording
/// built on it are gone (task4280). Reading inside `Drop for CaptureSurface`
/// would count the still-live `Recording` (pipeline, converter, encoder) as an
/// outside holder.
struct SurfaceRefs {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    direct3d: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    item: GraphicsCaptureItem,
}

impl SurfaceRefs {
    fn wanted() -> bool {
        super::leak_probe::enabled()
    }

    fn of(surface: &CaptureSurface) -> Self {
        Self {
            device: surface.device.clone(),
            context: surface.context.clone(),
            direct3d: surface.direct3d.clone(),
            pool: surface.pool.clone(),
            session: surface.session.clone(),
            item: surface.item.clone(),
        }
    }

    /// Expectation when nothing outside holds on: 1 for each, except `device`,
    /// which the live `direct3d` wrapper also references (2). A higher number
    /// names a holder outside the surface and the recording.
    fn finish(self) {
        use super::leak_probe::com_refcount;
        unsafe {
            let device = com_refcount(&self.device);
            // Control: whether a child resource moves the device's public
            // count at all. If it does not, `device` is blind to children and
            // only the live-object report can speak for them.
            let desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC {
                Width: 1,
                Height: 1,
                MipLevels: 1,
                ArraySize: 1,
                Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: windows::Win32::Graphics::Direct3D11::D3D11_USAGE_DEFAULT,
                ..Default::default()
            };
            let mut child = None;
            let device_with_child = match self.device.CreateTexture2D(&desc, None, Some(&mut child))
            {
                Ok(()) => com_refcount(&self.device),
                Err(_) => 0,
            };
            drop(child);
            tracing::info!(
                event = "capture_surface_refs",
                device,
                device_with_child,
                context = com_refcount(&self.context),
                direct3d = com_refcount(&self.direct3d),
                pool = com_refcount(&self.pool),
                session = com_refcount(&self.session),
                item = com_refcount(&self.item),
                "refcounts after the surface and the recording were dropped"
            );
        }
    }
}

/// Counts the per-recording GPU side of capture: the `ID3D11Device`, the WGC
/// item/session/frame pool, the interop `IDirect3DDevice` (task4280).
///
/// One recording opens exactly one. The pair with `ENCODERS_ALIVE`
/// (`crate::encoder::hardware`) and `ENGINES_ALIVE` (`crate::playback`) is what
/// makes "which reference kept the round's GPU objects alive" readable from one
/// log file: every counter back to its floor after a stop, with dedicated GPU
/// memory still stepping, means no Rust owner is holding anything and the
/// COM/driver side is.
///
/// **That is exactly what 2026-09-11 read**, for all three counters at once, and
/// the encoder MFT's full shutdown (now in `Drop for HardwareVideoEncoder`) did
/// not recover a byte. 2026-09-13 answered it with `SurfaceRefs` and
/// `leak_probe`: after a 6647-frame recording the device carried 6651
/// references, held by one leaked `ID3D11VideoProcessorInputView` per frame
/// (`GpuNv12Converter::convert_into`, `ManuallyDrop` stream field), so no
/// recording's device was ever destroyed. Fixed there; +21.3 MB/round became
/// +0.16 MB at 258x146. About 4.3 B/pixel per round still remains (6.2 MB at
/// 1600x900) and is *not* on this device (the next round's
/// `before_capture_device` report is empty): WGC alone reproduces it in a bare
/// test process (`task4280_probe_wgc_rounds`) -- read task4280's evidence
/// before probing here again.
static SURFACES_ALIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for CaptureSurface {
    fn drop(&mut self) {
        let alive = SURFACES_ALIVE.fetch_sub(1, Ordering::Relaxed) - 1;
        tracing::info!(
            event = "capture_surface_dropped",
            alive,
            "capture surface dropped"
        );
    }
}

/// One message from the audio thread. `false` means the audio session stopped
/// and the capture stops with it.
///
/// `break`, not `return Err`, is what the caller does with that: bailing out of
/// the worker closure would skip `muxer.finalize()` and throw away the tail
/// segment. The other autonomous stops in this loop (DiskFull, SourceClosed)
/// already go that way.
fn handle_audio_event(
    track: usize,
    event: audio::AudioWorkerEvent,
    path: &mut encode::EncodePath,
    state: &mut encode::LoopState,
    encoder_events: &Sender<encoder::EncoderEvent>,
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<bool> {
    match event {
        audio::AudioWorkerEvent::Packets(packets) => {
            // Task088 follow-up: telemetry for this state is sent from the
            // `recv(raw_frames)` arm below, paced by video frame arrival (as
            // it always was) rather than here. This arm fires roughly once
            // per WASAPI period (~10ms) now, and `encoder_events` is a
            // `bounded(32)` channel shared with `Finalized(segment)` -- at
            // that cadence, sending a telemetry event per `Packets` event
            // measurably starved `Finalized` out of the channel (observed:
            // real segments on disk, empty manifest) whenever nothing was
            // draining the channel between video frames.
            // Track 0 is the target; the rest are the extra sounds
            // (t261002-577c). Each track's thread has already encoded its
            // packets (t261025-cec6); only the sample rebuild happens here.
            // Task480 isolation: the killer is the 50ms retry budget in
            // `write_aac_sample_at`, so what matters at the failing push is
            // how long the sink refused and how much audio this one arm
            // entry was carrying -- a backlog drained in a single pass looks
            // very different from a steady ~10ms packet. Counters reset per
            // arm entry; nothing is logged unless the push actually fails.
            for packet in packets {
                // Drift is the target's alone: an extra sound is free to be
                // silent, and its silence must not bury the one number that
                // says the recording is in trouble (task1260's rule).
                if track == 0 {
                    let drift =
                        audio::drift_100ns(packet.timestamp_100ns, state.last_video_pts_100ns);
                    state.max_abs_audio_video_drift_100ns =
                        state.max_abs_audio_video_drift_100ns.max(drift.abs());
                    state.last_audio_pts_100ns = Some(packet.timestamp_100ns);
                }
                for unit in packet.units {
                    let sample = {
                        crate::insight_scope!("aac_to_sample");
                        unit.to_sample()?
                    };
                    // Order is the whole point of the queue: once
                    // one sample is waiting, every later one waits
                    // behind it rather than jumping the sink.
                    state.pending_aac[track].push_back(sample);
                }
            }
            let pushed = flush_pending_aac(
                &mut state.pending_aac,
                &mut path.muxer,
                &mut state.pending_aac_since,
                &emit,
            )?;
            state.aac_samples_pushed += pushed;
        }
        // Any track: an encode failure failed the recording when the encode
        // ran here, and still does.
        audio::AudioWorkerEvent::EncodeFailed(error) => return Err(error),
        // `run_extra_track` sends neither of the next two; only the target's
        // session can reinitialize the telemetry or stop the recording.
        audio::AudioWorkerEvent::Reinitialized { .. } | audio::AudioWorkerEvent::Stopped(_)
            if track != 0 => {}
        audio::AudioWorkerEvent::Reinitialized { reinitializations } => {
            state.reinitializations = reinitializations;
            let _ = encoder_events.try_send(encoder::EncoderEvent::AudioTelemetry {
                last_audio_pts_100ns: state
                    .last_audio_pts_100ns
                    .unwrap_or(state.last_video_pts_100ns),
                last_video_pts_100ns: state.last_video_pts_100ns,
                max_abs_drift_100ns: state.max_abs_audio_video_drift_100ns,
                reinitializations,
            });
        }
        audio::AudioWorkerEvent::Stopped(error) => {
            // The only place the cause exists. It used to be
            // packed into `EncoderEvent::AudioStopped` and parked
            // in a diagnostics field nothing read, so a recording
            // killed by a vanishing headset left no trace of why
            // (task2150). The log is the right home for an HRESULT
            // string; the toast gets the stage below.
            tracing::warn!(
                event = "capture_audio_session_stopped",
                hresult = error.code().0,
                error = %error,
                "the audio session stopped while recording; the capture stops with it"
            );
            return Ok(false);
        }
    }
    Ok(true)
}

/// What a recording needs beyond the WGC surface: the encode path itself and
/// the audio thread feeding it, which only exists when there is a track.
struct Recording {
    path: encode::EncodePath,
    audio_events: Receiver<(usize, audio::AudioWorkerEvent)>,
    audio_threads: Vec<thread::JoinHandle<()>>,
    audio_worker_stop: Arc<AtomicBool>,
    /// The target's track plus `extra_active.len()` (t261002-577c), or 0 with
    /// no audio at all.
    audio_tracks: usize,
    /// Whether each extra track (track 1 onwards) records its source or
    /// silence. Each track's encoder lives on its thread (t261025-cec6).
    extra_active: Vec<Arc<AtomicBool>>,
    /// What each extra track listens to; a microphone's can move to another
    /// device mid-recording (`audio::merge_extras`).
    extra_sources: Vec<Arc<Mutex<super::ExtraTrackSource>>>,
    /// Where a track added mid-recording sends its audio; `None` without audio.
    audio_sender: Option<Sender<(usize, audio::AudioWorkerEvent)>>,
    /// Mid-recording changes to the extra tracks, from
    /// `CaptureController::apply_target_audio`.
    extra_updates: Receiver<ExtraAudioUpdate>,
    /// A new output size the window has been holding, and since when
    /// (`settled_resize`).
    resize_candidate: Option<(CaptureSize, std::time::Instant)>,
    /// A size the encoder could not be rebuilt at: not retried until the
    /// window moves on to another one.
    resize_refused: Option<CaptureSize>,
}

/// The whole extra-track list a recording should now have, and which of them
/// record (t261002-577c). Only ever longer than the last one: the controller
/// merges (`audio::merge_extras`), so a track index here is the same index the
/// container's `AudioTracksSet` names. A microphone's entry may name another
/// device than before: the track's thread reopens on it.
pub(crate) struct ExtraAudioUpdate {
    pub(crate) tracks: Vec<super::ExtraTrackSource>,
    pub(crate) active: Vec<bool>,
}

impl Recording {
    /// Starts the thread for extra track `1 + extra_active.len()`. `false` when
    /// it could not get an encoder -- then no later track may be added in the
    /// same pass, or the muxer's track numbers would drift from the
    /// container's; the next update retries from this one.
    fn add_extra_track(&mut self, source: super::ExtraTrackSource, active: bool) -> bool {
        let Some(sender) = self.audio_sender.clone() else {
            return false;
        };
        let encoder = match encoder::aac::AacEncoder::create() {
            Ok(encoder) => encoder,
            Err(error) => {
                tracing::warn!(
                    event = "extra_audio_track_unavailable",
                    source = ?source,
                    %error,
                    "no AAC encoder for an extra audio track; it is not added"
                );
                return false;
            }
        };
        let track = 1 + self.extra_active.len();
        let flag = Arc::new(AtomicBool::new(active));
        let stop = self.audio_worker_stop.clone();
        let thread_flag = flag.clone();
        let shared = Arc::new(Mutex::new(source));
        let thread_source = shared.clone();
        self.audio_threads.push(thread::spawn(move || {
            // The same Media Foundation hold as the target's thread (`open`).
            let runtime = encoder::MfRuntime::start();
            audio::run_extra_track(
                &thread_source,
                track,
                &encoder,
                &thread_flag,
                &sender,
                &stop,
            );
            drop(encoder);
            drop(runtime);
        }));
        self.extra_active.push(flag);
        self.extra_sources.push(shared);
        self.audio_tracks = track + 1;
        self.path.muxer.grow_audio_tracks(self.audio_tracks);
        true
    }

    /// Applies whatever the controller has sent since the last turn.
    fn apply_extra_updates(
        &mut self,
        pending_aac: &mut Vec<VecDeque<windows::Win32::Media::MediaFoundation::IMFSample>>,
    ) {
        while let Ok(update) = self.extra_updates.try_recv() {
            for (index, source) in update.tracks.into_iter().enumerate() {
                let active = update.active.get(index).copied().unwrap_or(false);
                match self.extra_active.get(index) {
                    Some(flag) => {
                        flag.store(active, Ordering::Relaxed);
                        if let Ok(mut current) = self.extra_sources[index].lock() {
                            *current = source;
                        }
                    }
                    None => {
                        if !self.add_extra_track(source, active) {
                            break;
                        }
                    }
                }
            }
            pending_aac.resize_with(self.audio_tracks.max(1), VecDeque::new);
        }
    }
}

/// The three things that are rebuilt together when the output size moves: the
/// encoder, the transform that sizes frames for it and the NV12 converter
/// that feeds it -- plus the encoder config they were built for.
struct EncodeStage {
    config: encoder::EncoderConfig,
    encoder: encoder::HardwareVideoEncoder,
    pipeline: TransformPipeline,
    converter: GpuNv12Converter,
}

enum StageError {
    Encoder(encoder::EncoderStartError),
    Gpu(windows::core::Error),
}

impl std::fmt::Display for StageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Encoder(error) => write!(f, "{:?}: {}", error.kind, error.diagnostics),
            Self::Gpu(error) => write!(f, "{error}"),
        }
    }
}

/// Builds `EncodeStage` at `size` on the surface's device, moving `stage` on
/// so a failure names the step it failed at.
fn build_encode_stage(
    config: &CaptureConfig,
    surface: &CaptureSurface,
    size: CaptureSize,
    stage: &mut &'static str,
) -> Result<EncodeStage, StageError> {
    let encoder_config = encoder::EncoderConfig {
        output_dir: config.encoder_output_dir.clone(),
        output_size: size,
        frame_rate: config.frame_rate,
    };
    *stage = super::VIDEO_ENCODER_STAGE;
    // The setting's codec, decided once when the recording started
    // (task1760). `CaptureController::start` has already refused a codec
    // this machine cannot both record and play back.
    let encoder = encoder::HardwareVideoEncoder::create_for_recording(
        &encoder_config,
        &surface.device,
        config.codec.into(),
    )
    .map_err(StageError::Encoder)?;
    *stage = "transform_pipeline";
    let pipeline = TransformPipeline::new(
        surface.device.clone(),
        surface.context.clone(),
        size,
        surface.state.hdr.white_point,
    )
    .map_err(StageError::Gpu)?;
    *stage = "nv12_converter";
    let converter =
        GpuNv12Converter::new(&surface.device, &surface.context, size).map_err(StageError::Gpu)?;
    Ok(EncodeStage {
        config: encoder_config,
        encoder,
        pipeline,
        converter,
    })
}

/// The audio channel every track shares, plus the target track's thread
/// when there is a target track. Tagged with the track id; sized for a
/// handful of tracks at 64 events each, as task1260 had it.
type AudioWiring = (
    Receiver<(usize, audio::AudioWorkerEvent)>,
    Vec<thread::JoinHandle<()>>,
    Option<Sender<(usize, audio::AudioWorkerEvent)>>,
);

/// Task088: audio is polled on its own dedicated thread, driven by WASAPI's own period
/// event, instead of inline in the capture loop on every accepted video frame -- video frame arrival
/// can go hundreds of ms between frames on a static/sparse scene, and the
/// process-loopback endpoint silently drops audio once ~30ms of it goes unread (see
/// `audio::run_event_driven_capture`'s doc comment). `IAudioClient` is a COM interface
/// and not `Send`, so the long-lived capture used by that thread must be created on
/// it, not handed off from the worker; `start_audio_track` does a throwaway trial
/// `start()`/`stop()` on the worker thread purely to decide, synchronously, whether an audio track exists at all
/// (same decision the original single `start()` call made, just not reusing its
/// result).
fn spawn_target_audio(
    config: &CaptureConfig,
    aac: Option<encoder::aac::AacEncoder>,
    audio_source: audio::AudioSource,
    audio_worker_stop: &Arc<AtomicBool>,
) -> AudioWiring {
    let Some(aac) = aac else {
        if !config.extra_audio.is_empty() {
            // Known limit: without the target's track the muxer has no
            // audio type, so the extra sounds have nothing to ride on.
            tracing::warn!(
                event = "extra_audio_tracks_skipped",
                extras = config.extra_audio.len(),
                "the target has no audio track, so the added sounds are not recorded"
            );
        }
        return (crossbeam_channel::never(), Vec::new(), None);
    };
    let (tx, rx) = bounded(256);
    let stop_flag = audio_worker_stop.clone();
    let target_tx = tx.clone();
    let handles = vec![thread::spawn(move || {
        // The encoder lives on this thread now (t261025-cec6), and an
        // early `?` out of the worker drops `Recording` without the
        // join in `drain_and_stop_audio`: this thread then outlives
        // the video encoder's `MfRuntime`. Holding one here keeps
        // Media Foundation up until the encoder is gone.
        let runtime = encoder::MfRuntime::start();
        audio::run_event_driven_capture(audio_source, 0, &aac, &target_tx, &stop_flag);
        drop(aac);
        drop(runtime);
    })];
    (rx, handles, Some(tx))
}

impl Recording {
    /// Creates the encoder, the muxer, the GPU pipeline and the audio thread,
    /// moving `startup_stage` on so a failure names the step it failed at.
    fn open(
        config: &CaptureConfig,
        surface: &CaptureSurface,
        (encoder_events, finalized_segments): (
            &Sender<encoder::EncoderEvent>,
            &Sender<super::indexer::IndexEvent>,
        ),
        startup_stage: &mut &'static str,
        extra_updates: Receiver<ExtraAudioUpdate>,
    ) -> windows::core::Result<Self> {
        let monitor_target = surface.monitor_target;
        // A recording has no sidecar to write, so the JPEG goes to the index
        // writer -- the one thread that owns the `.lvb`.
        let thumbnail_writer = setup::spawn_thumbnail_encoder(finalized_segments.clone());
        let EncodeStage {
            config: encoder_config,
            encoder: video_encoder,
            pipeline,
            converter,
        } = match build_encode_stage(config, surface, surface.state.output_size, startup_stage) {
            Ok(stage) => stage,
            Err(StageError::Encoder(error)) => {
                tracing::warn!(
                    event = "capture_startup_failed",
                    stage = startup_stage,
                    codec = ?config.codec,
                    encoder_error_kind = ?error.kind,
                    encoder_error_diagnostics = %error.diagnostics,
                    "video encoder create failed"
                );
                return Err(windows::core::Error::from_win32());
            }
            Err(StageError::Gpu(error)) => return Err(error),
        };
        // Decided once, here, and used for both the trial and the capture
        // thread below, so the two can never disagree about what is recorded
        // (task1430). Nothing outside this worker judges it any more -- task2110
        // dropped the combination rule `CaptureController::start` used to apply
        // to the same config.
        let audio_source = audio::AudioSource::for_config(config);
        let (aac, audio_available) =
            start_audio_track(config, monitor_target, audio_source, encoder_events);
        // The target's track first; the extra sounds (t261002-577c) are added
        // below, once the muxer exists, through the same path a sound switched
        // on mid-recording takes. "Record other applications too" stays a
        // wider scope on track 0, not more tracks (task1430).
        let audio_tracks = usize::from(audio_available);
        let audio_worker_stop = Arc::new(AtomicBool::new(false));
        let aac_type = aac.as_ref().map(|aac| aac.output_media_type().clone());
        let (audio_events, audio_threads, audio_sender) =
            spawn_target_audio(config, aac, audio_source, &audio_worker_stop);
        let muxer = if let Some(aac_type) = aac_type {
            encoder::SegmentMuxer::with_aac_tracks(
                encoder_config,
                video_encoder.output_media_type().map_err(worker_error)?,
                aac_type,
                audio_tracks,
            )
        } else {
            encoder::SegmentMuxer::new(
                encoder_config,
                video_encoder.output_media_type().map_err(worker_error)?,
            )
        };
        let muxer = muxer.writing_into_memory();
        // From here the six are one value (`EncodePath`): every step of the
        // loop below wants all of them at once.
        let thumbnails = setup::ThumbnailRelay::new(thumbnail_writer);
        let path = encode::EncodePath {
            pipeline,
            converter,
            encoder: video_encoder,
            muxer,
            thumbnails,
            thumbnail_wanted: false,
        };
        let mut recording = Self {
            path,
            audio_events,
            audio_threads,
            audio_worker_stop,
            audio_tracks,
            extra_active: Vec::new(),
            extra_sources: Vec::new(),
            audio_sender,
            extra_updates,
            resize_candidate: None,
            resize_refused: None,
        };
        for source in config.extra_audio.iter().cloned() {
            if !recording.add_extra_track(source, true) {
                break;
            }
        }
        if !recording.extra_active.is_empty() {
            tracing::info!(
                event = "audio_tracks_selected",
                tracks = recording.audio_tracks,
                extras = ?config.extra_audio,
                "recording extra audio tracks"
            );
        }
        Ok(recording)
    }
}

/// Everything the loop does before it waits on the channels: the stall probe,
/// the disk guard, the frame-hold feed and the audio the sink has refused so
/// far.
///
/// Ahead of the select rather than inside an arm: with audio running the
/// `default` arm never fires (the ~10ms packet cadence keeps a higher-priority
/// arm ready), and that is exactly the case these have to cover. Every arm
/// returns to the top of the loop, so this is the one place all of them pass
/// through.
fn service_encoder(
    config: &CaptureConfig,
    surface: &mut CaptureSurface,
    recording: &mut Recording,
    state: &mut encode::LoopState,
    (disk_root, last_disk_check): (&Path, &mut std::time::Instant),
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<Option<CaptureStopReason>> {
    let hwnd = surface.hwnd;
    let monitor_target = surface.monitor_target;

    state.report_stall(recording.path.muxer.open_index());
    follow_hdr_toggle(surface, recording)?;
    follow_resize(config, surface, recording, state, emit)?;
    // Task1420: stop before the disk does. Here, ahead of the select,
    // for the same reason the frame hold below is -- it is the one place
    // every arm passes through. The `recv(raw_frames)` arm alone would
    // not do: WGC hands over nothing at all while the target is
    // minimized (see `state.last_frame_at` above), and a minimized recording
    // still fills the disk from the hold path and the audio tracks.
    // Failing to read the free space is not a reason to end a recording,
    // so it only logs; 10s is plenty, disks do not empty in seconds.
    // Still plenty with two captures (task2010): even at the unverified
    // heavy estimate, 30MB/s x 10s = 600MB against a 10GB floor. Each
    // worker polls for itself, so both stop -- correct, if twice-toasted.
    if last_disk_check.elapsed() >= Duration::from_secs(10) {
        *last_disk_check = std::time::Instant::now();
        if buffer_disk_is_full(disk_root) {
            return Ok(Some(CaptureStopReason::DiskFull));
        }
    }
    // Ahead of the select rather than inside an arm: with audio running
    // the `default` arm never fires (the ~10ms packet cadence keeps a
    // higher-priority arm ready), and that is exactly the case this has
    // to cover. Every arm returns to the top of the loop, so this is the
    // one place all of them pass through.
    if state.have_frame && should_hold_frame(state.last_frame_at.elapsed(), config.frame_rate) {
        // The watchdog's synthesized frame costs the same pipeline a
        // real one does; counted apart so a still target's cost is not
        // read as the target's own (task205).
        crate::insight_scope!("capture_hold_frame");
        // Never asked of a monitor: the handle is an HMONITOR, and a
        // screen cannot be minimized anyway (task165).
        let minimized = !monitor_target && unsafe { IsIconic(hwnd).as_bool() };
        recording.path.feed_hold(state, minimized, emit)?;
    }
    // After the hold feed and its drain, never before (task610): what
    // clears the sink's refusal is this thread pulling encoded video out
    // of the H.264 encoder and into the muxer, which is what the block
    // above just did. Offering the queue first would only find the same
    // refusal again.
    //
    // The reverse dependency is real too, and is what t260911-e7f3 fixed: a
    // video sample the sink refuses now waits in the muxer instead of killing
    // the recording, and this flush is the thing that unblocks it, on the next
    // turn's drain.
    state.aac_samples_pushed += flush_pending_aac(
        &mut state.pending_aac,
        &mut recording.path.muxer,
        &mut state.pending_aac_since,
        emit,
    )?;
    if let Some(since) = state.pending_aac_since {
        if since.elapsed() >= PENDING_AAC_LIMIT {
            tracing::error!(
                target: "task480_stall",
                pending_aac = state.pending_aac.iter().map(VecDeque::len).sum::<usize>(),
                waited_ms = since.elapsed().as_millis() as u64,
                limit_ms = PENDING_AAC_LIMIT.as_millis() as u64,
                aac_samples_pushed = state.aac_samples_pushed,
                real_frames = state.real_frames,
                holds_fed = state.holds_fed,
                holds_without_credit = state.holds_without_credit,
                last_video_pts_100ns = state.last_video_pts_100ns,
                open_index = ?recording.path.muxer.open_index(),
                "the muxer's audio sink has refused input for too long"
            );
            return Err(windows::core::Error::from_win32());
        }
    }
    // The video arm's twin of the block above (t260911-e7f3). A refused video
    // sample is no longer fatal where it is refused -- it waits in the muxer
    // while the loop keeps offering the audio the sink is actually waiting for
    // -- so something has to decide when a refusal has stopped being
    // back-pressure. Here, because this is the only place that can see all of
    // it at once: what the recording died of on 2026-09-11 was legible as
    // `video_accepted=18 first_audio=[None]` and *not* as how much audio was
    // waiting to be offered, which is the number that says whether the queue or
    // the order of offering was the problem.
    if let Some(waited) = recording.path.muxer.pending_video_waiting() {
        if waited >= encoder::HELD_VIDEO_LIMIT {
            let queued_aac: usize = state.pending_aac.iter().map(VecDeque::len).sum();
            let first_pending_aac_100ns = state
                .pending_aac
                .iter()
                .filter_map(|queue| queue.front())
                .filter_map(|sample| unsafe { sample.GetSampleTime() }.ok())
                .min();
            tracing::error!(
                target: "task610_backpressure",
                pending_video = recording.path.muxer.pending_video_len(),
                waited_ms = waited.as_millis() as u64,
                limit_ms = encoder::HELD_VIDEO_LIMIT.as_millis() as u64,
                queued_aac,
                first_pending_aac_100ns = ?first_pending_aac_100ns,
                open_index = ?recording.path.muxer.open_index(),
                has_input_credit = recording.path.encoder.has_input_credit(),
                holds_without_credit = state.holds_without_credit,
                aac_samples_pushed = state.aac_samples_pushed,
                real_frames = state.real_frames,
                last_video_pts_100ns = state.last_video_pts_100ns,
                "the muxer's video sink has refused input for too long"
            );
            return Err(windows::core::Error::from_win32());
        }
    }
    Ok(None)
}

/// Re-reads the capture monitor's HDR state once a second and, when HDR has
/// been turned on or off under the recording, switches the tone curve's white
/// point and tells the WGC callback to rebuild the pool in the new format
/// (t260917-7faa). Frames already in flight keep their old format; the
/// transform picks its path from each texture's own format, so they are not
/// misread.
///
/// A window target re-resolves its monitor each time, so a window dragged
/// between an HDR and an SDR monitor is followed the same way.
fn follow_hdr_toggle(
    surface: &mut CaptureSurface,
    recording: &mut Recording,
) -> windows::core::Result<()> {
    if surface.state.last_hdr_check.elapsed() < HDR_RECHECK_INTERVAL {
        return Ok(());
    }
    surface.state.last_hdr_check = std::time::Instant::now();
    // t261025-cec6: `monitor_hdr_state` builds a DXGI factory every call.
    crate::insight_scope!("hdr_poll");
    let monitor = capture_monitor(surface.monitor_target, surface.hwnd);
    let recording_state = surface.state.hdr;
    let Some(next) = hdr_follow(recording_state, monitor_hdr_state(monitor)) else {
        return Ok(());
    };
    recording
        .path
        .pipeline
        .set_hdr_white_point(next.white_point)?;
    surface.state.hdr = next;
    surface.hdr_input.store(next.hdr, Ordering::Relaxed);
    tracing::info!(
        event = "capture_hdr_changed",
        old_hdr_input = recording_state.hdr,
        new_hdr_input = next.hdr,
        old_white_point = recording_state.white_point,
        new_white_point = next.white_point,
        "the capture monitor's HDR state changed mid-recording"
    );
    Ok(())
}

/// The encoder size for a frame of `input` with `crop` off it, rounded down
/// to even: H.264 NV12 rejects odd dimensions (encoder::validate_config) and a
/// window's client area has no such constraint, so rounding here keeps
/// video_encoder startup from failing with CAP-DEV-001.
fn output_for(input: CaptureSize, crop: gpu::Inset) -> CaptureSize {
    CaptureSize {
        width: (input.width - crop.left - crop.right).max(0) & !1,
        height: (input.height - crop.top - crop.bottom).max(0) & !1,
    }
}

/// How long a resized window has to hold its new size before the encoder is
/// rebuilt for it. A drag-resize hands over a new size on nearly every frame;
/// until it settles the frame keeps being fitted into the old size, as it
/// always was.
const RESIZE_SETTLE: Duration = Duration::from_millis(500);

/// The size to rebuild the encoder at, once `wanted` has differed from
/// `current` -- the same size the whole time -- for `RESIZE_SETTLE`.
fn settled_resize(
    candidate: &mut Option<(CaptureSize, std::time::Instant)>,
    current: CaptureSize,
    wanted: CaptureSize,
    now: std::time::Instant,
) -> Option<CaptureSize> {
    if wanted == current || wanted.width <= 0 || wanted.height <= 0 {
        *candidate = None;
        return None;
    }
    match *candidate {
        Some((size, since)) if size == wanted => {
            if now.duration_since(since) < RESIZE_SETTLE {
                return None;
            }
            *candidate = None;
            Some(wanted)
        }
        _ => {
            *candidate = Some((wanted, now));
            None
        }
    }
}

/// Rebuilds the encoder at the window's new size once it has settled there
/// (the window was resized mid-recording -- a game going from its launch window
/// to borderless is the case that asked for it). Until now the first frame's
/// size held for the whole recording and every later size was letterboxed
/// into it.
///
/// The new encoder is built *before* the old one is drained, so a size it
/// cannot be built at costs nothing: the old encoder carries on and the frame
/// stays fitted, as before. The two are alive together only for the drain.
/// The muxer is kept (`switch_video`): index, AAC priming and the closing
/// segment all carry on, and the new encoder's first keyframe opens the
/// segment the new size starts in.
fn follow_resize(
    config: &CaptureConfig,
    surface: &mut CaptureSurface,
    recording: &mut Recording,
    state: &mut encode::LoopState,
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<()> {
    if config.output_size.width > 0 && config.output_size.height > 0 {
        return Ok(());
    }
    let (input, crop) = surface.state.maximized_crop;
    let wanted = output_for(input, crop);
    if recording.resize_refused == Some(wanted) {
        return Ok(());
    }
    let current = surface.state.output_size;
    let Some(size) = settled_resize(
        &mut recording.resize_candidate,
        current,
        wanted,
        std::time::Instant::now(),
    ) else {
        return Ok(());
    };
    let started = std::time::Instant::now();
    let built = build_encode_stage(config, surface, size, &mut "resize")
        .map_err(|error| error.to_string())
        .and_then(|stage| {
            let output_type = stage
                .encoder
                .output_media_type()
                .map_err(|error| error.diagnostics)?;
            Ok((stage, output_type))
        });
    let (stage, output_type) = match built {
        Ok(built) => built,
        Err(error) => {
            recording.resize_refused = Some(size);
            tracing::warn!(
                event = "capture_resize_refused",
                old_width = current.width,
                old_height = current.height,
                width = size.width,
                height = size.height,
                %error,
                "the encoder could not be rebuilt at the window's new size; fitting into the old one"
            );
            return Ok(());
        }
    };
    let path = &mut recording.path;
    // The old encoder's last frames belong to the old size's segment: drain
    // them into the muxer first, as the stop sequence does -- minus stopping
    // the audio, which carries on.
    if !path.drain_until_complete(state, emit, |_| {})? {
        tracing::warn!(
            event = "capture_resize_drain_incomplete",
            pending_video = path.muxer.pending_video_len(),
            "the old encoder did not finish draining; switching without its last frames"
        );
    }
    let last_picture = path.pipeline.last_output();
    path.encoder = stage.encoder;
    path.pipeline = stage.pipeline;
    path.converter = stage.converter;
    path.muxer.switch_video(size, output_type);
    // The hold feed can reach the new pipeline before WGC's next frame does
    // (the rebuild took 65ms against a 33ms hold at 60fps), and a fresh output
    // is black -- the new segment opened on a black frame. The last picture,
    // fitted, is what the hold would have shown anyway.
    path.pipeline
        .transform(&last_picture, current, gpu::Inset::default(), false)?;
    path.thumbnail_wanted = true;
    surface.state.output_size = size;
    surface.state.output_moved = true;
    recording.resize_refused = None;
    tracing::info!(
        event = "capture_output_resized",
        old_width = current.width,
        old_height = current.height,
        width = size.width,
        height = size.height,
        took_ms = started.elapsed().as_millis() as u64,
        "the encoder was rebuilt at the window's new size"
    );
    Ok(())
}

/// The capture loop: hold a frame when WGC has none, keep the audio moving,
/// and answer every arm of the select until something stops the recording.
///
/// A stop of its own (the target closing, the disk filling, the audio session
/// dying) comes back as `Some(reason)`; the caller still runs the drain, the
/// finalize and the audio join, because an early return here used to lose the
/// open segment and leave the audio thread spinning.
fn capture_loop(
    config: &CaptureConfig,
    surface: &mut CaptureSurface,
    recording: &mut Recording,
    state: &mut encode::LoopState,
    stop: &Receiver<()>,
    channels: &WorkerChannels,
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<Option<CaptureStopReason>> {
    let hwnd = surface.hwnd;
    let monitor_target = surface.monitor_target;
    // A handle of its own, so the arm below can take the surface `&mut`.
    let raw_frames = surface.raw_frames.clone();
    // Autonomous stops (the target window closing) record their reason and
    // `break`, so the encoder drain / muxer finalize / audio-thread join
    // below still run -- an early `return` here silently lost the open
    // segment (it stayed a `.partial.mp4`) and left the audio thread
    // spinning.
    let mut autonomous_stop: Option<CaptureStopReason> = None;
    // The window checks used to live only in the `default` arm, which
    // crossbeam only reaches when *no* other arm is ready -- an audio
    // stream delivering packets every ~10ms starved it forever, so a
    // closed target was never detected while it played audio.
    let mut last_window_check = std::time::Instant::now();
    // Task1420. The volume the startup guard measured, not the session's own
    // path: `encoder_output_dir` is never created as a directory any more
    // (the session is the sibling `.lvb` file), and `free_bytes` calls
    // `GetDiskFreeSpaceExW` on the path, which fails on one that does not
    // exist. The parent is the buffer root, which `ensure_buffer_root_has_space`
    // has already created by the time the worker runs.
    let disk_root = config
        .encoder_output_dir
        .parent()
        .unwrap_or(&config.encoder_output_dir)
        .to_path_buf();
    let mut last_disk_check = std::time::Instant::now();
    loop {
        recording.apply_extra_updates(&mut state.pending_aac);
        if let Some(reason) = service_encoder(
            config,
            surface,
            recording,
            state,
            (&disk_root, &mut last_disk_check),
            emit,
        )? {
            autonomous_stop = Some(reason);
            break;
        }
        crossbeam_channel::select! {
            recv(stop) -> _ => break,
            // Task088: audio arrives on its own schedule (WASAPI's period event, via the
            // dedicated thread started above), not tied to video frame arrival -- see
            // that spawn site's comment for why. `never()` when there's no audio track
            // means this arm simply never fires.
            recv(recording.audio_events) -> event => {
                if target_closed_since(&mut last_window_check, monitor_target, hwnd) {
                    autonomous_stop = Some(CaptureStopReason::SourceClosed);
                    break;
                }
                let Ok((track, event)) = event else { continue; };
                if !handle_audio_event(
                    track,
                    event,
                    &mut recording.path,
                    state,
                    &channels.encoder_events,
                    emit,
                )? {
                    state.audio_death = true;
                    autonomous_stop = Some(CaptureStopReason::InitializationFailed {
                        stage: super::AUDIO_SESSION_STAGE,
                    });
                    break;
                }
            }
            default(loop_block_timeout(state.last_frame_at.elapsed(), state.have_frame, config.frame_rate)) => {
                // This arm now fires on the frame-hold deadline (~33ms at
                // 60fps) rather than a flat 250ms, so the window check is
                // rate-limited the same way the audio arm's is instead of
                // running eight times as often as it needs to.
                if target_closed_since(&mut last_window_check, monitor_target, hwnd) {
                    autonomous_stop = Some(CaptureStopReason::SourceClosed);
                    break;
                }
            }
            recv(raw_frames) -> raw => {
                last_window_check = std::time::Instant::now();
                if target_window_gone(monitor_target, hwnd) {
                    autonomous_stop = Some(CaptureStopReason::SourceClosed);
                    break;
                }
                let Ok(raw) = raw else { break; };
                handle_raw_frame(raw, &mut recording.path, state, surface, config, channels, emit)?;
            }
        }
    }
    Ok(autonomous_stop)
}

/// The recording itself: open WGC and the encoder, run the capture loop, and
/// stop cleanly. Split from `run_capture`, which keeps the `emit` fork and
/// turns whatever comes back into a `CaptureStopReason`.
///
/// `progress.stage` moves on at each step so a failure names where it failed;
/// `progress.audio_death` is set when the audio session is what ended the
/// capture, because the shutdown that follows can fail on its own and would
/// otherwise report the wrong stage (task410).
fn record(
    config: &CaptureConfig,
    surface_slot: &mut Option<CaptureSurface>,
    channels: &WorkerChannels,
    counters: &Counters,
    (stop, stop_signal): (&Receiver<()>, &Sender<()>),
    emit: &impl Fn(encoder::EncoderEvent),
    progress: &mut Progress,
) -> windows::core::Result<CaptureStopReason> {
    unsafe {
        CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let surface = match surface_slot {
        Some(held) => {
            // A held surface kept delivering while nobody recorded
            // (t260929-ea5e): whatever waits in the queue predates this
            // recording, and the HDR state was not followed meanwhile, so
            // the first turn of the loop re-reads it before any frame.
            while held.raw_frames.try_recv().is_ok() {}
            if let Some(past) = std::time::Instant::now().checked_sub(HDR_RECHECK_INTERVAL) {
                held.state.last_hdr_check = past;
            }
            held
        }
        None => surface_slot.insert(CaptureSurface::open(
            config,
            stop_signal,
            &mut progress.stage,
        )?),
    };
    let source_closed_flag = surface.source_closed.clone();
    let mut recording = Recording::open(
        config,
        surface,
        (&channels.encoder_events, &channels.finalized_segments),
        &mut progress.stage,
        channels.extra_audio.clone(),
    )?;
    let mut state = encode::LoopState::new(recording.audio_tracks, counters.clone());
    progress.stage = "start_capture";
    if !std::mem::replace(&mut surface.state.started, true) {
        surface.session.StartCapture()?;
    }
    let autonomous_stop = capture_loop(
        config,
        surface,
        &mut recording,
        &mut state,
        stop,
        channels,
        emit,
    )?;
    // Audio death only ever ends the loop through `Ok`, so this is where it
    // leaves the loop state for `run_one` to read.
    progress.audio_death = state.audio_death;
    drain_and_stop_audio(&mut recording, &mut state, &emit)?;
    let final_events = recording.path.muxer.finalize().map_err(worker_error)?;
    for event in final_events {
        emit(event);
    }
    Ok(if let Some(reason) = autonomous_stop {
        reason
    } else if source_closed_flag.load(Ordering::Acquire) {
        CaptureStopReason::SourceClosed
    } else {
        CaptureStopReason::Requested
    })
}

/// One real WGC frame: transform it, feed the encoder, publish it to the
/// preview channel and report the segment boundary it may have crossed.
fn handle_raw_frame(
    raw: RawCaptureFrame,
    path: &mut encode::EncodePath,
    state: &mut encode::LoopState,
    surface: &mut CaptureSurface,
    config: &CaptureConfig,
    channels: &WorkerChannels,
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<()> {
    let encoder_events = &channels.encoder_events;
    // From the texture, not `surface.hdr`: frames already queued when HDR was
    // toggled still carry the old pool's format (t260917-7faa).
    let hdr_input = {
        let mut desc = windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC::default();
        unsafe { raw.texture.GetDesc(&mut desc) };
        desc.Format == windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R16G16B16A16_FLOAT
    };
    let output_size = surface.state.output_size;
    let frame_debug = &surface.frame_debug;
    // Everything one real frame costs the capture thread:
    // transform, encode, mux and the thumbnail hand-off
    // (task205). The scopes inside it break down the total.
    crate::insight_scope!("capture_frame");
    if frame_debug.accepted.load(Ordering::Relaxed) <= 10 {
        frame_debug.log(&format!(
            "n={} consumer_received ts_100ns={}",
            frame_debug.accepted.load(Ordering::Relaxed),
            raw.timestamp_100ns
        ));
    }
    let (cropped_size, mut crop) = surface.state.maximized_crop;
    if cropped_size != raw.input_size {
        crop = crop_for(surface.monitor_target, surface.hwnd, raw.input_size);
        surface.state.maximized_crop = (raw.input_size, crop);
    }
    let texture = path
        .pipeline
        .transform(&raw.texture, raw.input_size, crop, hdr_input)?;
    if path.muxer.open_index().is_none() || std::mem::take(&mut path.thumbnail_wanted) {
        // No segment has opened yet (segment 0 never goes through
        // should_force_keyframe below): keep refreshing the
        // candidate so whichever frame is current when segment 0
        // finally opens becomes its thumbnail.
        path.capture_thumbnail(&texture);
    }
    path.pump(&emit)?;
    // A held frame (task163) may have already claimed the
    // instant this real frame carries -- WGC stamps a frame
    // when it is composed and hands it over a little later, so
    // the two clocks can cross by well under a frame. The
    // encoder and the muxer both need strictly rising input, so
    // the hold's timestamp becomes the floor here rather than
    // being allowed to fight with it.
    let pts_100ns = raw
        .timestamp_100ns
        .max(state.last_video_pts_100ns.saturating_add(1));
    state.last_video_pts_100ns = pts_100ns;
    state
        .counters
        .position
        .store(state.last_video_pts_100ns, Ordering::Relaxed);
    // Real frames reset the watchdog: the hold only fills the
    // silence WGC leaves behind (task163).
    state.last_frame_at = std::time::Instant::now();
    state.have_frame = true;
    state.real_frames += 1;
    // Telemetry is skipped when the events channel is filling
    // up: it shares the bounded(32) channel with `Finalized`,
    // and a `Finalized` dropped at capacity is a segment
    // permanently missing from the manifest, while telemetry
    // is refreshed on the next frame anyway.
    if state.last_audio_pts_100ns.is_some() && encoder_events.len() >= 16 {
        // keep the channel's headroom for Finalized events
    } else if let Some(last_audio_pts_100ns) = state.last_audio_pts_100ns {
        let _ = encoder_events.try_send(encoder::EncoderEvent::AudioTelemetry {
            last_audio_pts_100ns,
            last_video_pts_100ns: state.last_video_pts_100ns,
            max_abs_drift_100ns: state.max_abs_audio_video_drift_100ns,
            reinitializations: state.reinitializations,
        });
    }
    encode_real_frame(
        path,
        &texture,
        pts_100ns,
        &mut state.boundary,
        frame_debug,
        &state.counters.encoded_frames,
    )?;
    let boundary = &mut state.boundary;
    if let Some(probe) = boundary
        .as_mut()
        .filter(|probe| probe.thumbnail_collect.is_none())
    {
        probe.thumbnail_collect = path.thumbnails.take_collected();
    }
    if boundary.as_ref().is_some_and(BoundaryProbe::is_due) {
        let probe = boundary.take().expect("checked");
        let credit = path.encoder.credit_watch().close_boundary();
        report_segment_boundary(&probe, frame_debug, credit);
    }
    path.pump(&emit)?;
    let captured = CapturedFrame {
        texture,
        sequence: raw.timestamp_100ns as u64,
        timestamp_100ns: raw.timestamp_100ns,
        input_size: raw.input_size,
        output_size,
        color_space: if hdr_input {
            CaptureColorSpace::ScRgb
        } else {
            CaptureColorSpace::Bgra8Bt709
        },
        cursor_included: config.include_cursor,
    };
    let _ = channels.frames.try_send(captured);
    Ok(())
}

/// The stop sequence: drain the encoder into the muxer, get the audio still
/// queued into the sink, then let the audio threads finish and join them.
///
/// Nothing reads `audio_events` once the select loop is over, and the audio
/// thread's `send` *blocks* on a bounded(64) channel -- 64 WASAPI periods is
/// about 640ms. Any shutdown slower than that left the audio thread parked
/// inside `send`, where it never sees `audio_worker_stop`, and the join waited
/// for it forever (observed as a capture that stopped logging and sat idle for
/// 24 minutes). So every wait in here keeps draining that channel.
fn drain_and_stop_audio(
    recording: &mut Recording,
    state: &mut encode::LoopState,
    emit: &impl Fn(encoder::EncoderEvent),
) -> windows::core::Result<()> {
    let path = &mut recording.path;
    let audio_events = &recording.audio_events;
    let audio_threads = std::mem::take(&mut recording.audio_threads);
    let audio_worker_stop = &recording.audio_worker_stop;
    // Task610: audio still queued at stop has to reach the sink before
    // finalize, or the fix that stopped dropping the recording would start
    // dropping its last fraction of a second of sound instead. The drain runs
    // the very thing the sink is waiting for, so a queue held up by
    // back-pressure empties there.
    path.drain_until_complete(state, emit, |path| {
        // Nothing reads `audio_events` once the select loop is over, and the
        // audio thread's `send` *blocks* on a bounded(64) channel -- 64
        // WASAPI periods is about 640ms. Any shutdown slower than that left
        // the audio thread parked inside `send`, where it never sees
        // `audio_worker_stop`, and `audio_thread.join()` below waited for it
        // forever. Observed as a capture that stopped logging and sat idle
        // for 24 minutes. These packets are past the stop request and were
        // already being ignored; draining them is only what keeps the
        // sender alive to be told to stop.
        while audio_events.try_recv().is_ok() {}
        // A rotation requested just before stop can still be buffered in
        // the encoder and only surface here during drain.
        // Waits for the read-back (t261017-613d): nothing pumps after this, so a
        // copy still in flight would cost the last segment its thumbnail.
        let pipeline = &path.pipeline;
        path.thumbnails
            .drain(path.muxer.open_index(), true, |wait| {
                pipeline.collect_thumbnail(wait)
            });
    })?;
    if !path.encoder.drain_complete() {
        return Err(worker_error(
            "the encoder did not finish draining before the stop",
        ));
    }
    audio_worker_stop.store(true, Ordering::Relaxed);
    if !audio_threads.is_empty() {
        // Same reason as the drain above: the stop flag is only looked at
        // *between* sends, so the channel has to keep having room right up
        // to the moment the threads notice. They share one channel, so the
        // drain has to run until *all* of them are finished -- draining per
        // thread in turn would let the ones still running fill the channel
        // back up behind the one being waited on (task1260). Bounded so a
        // thread stuck for some other reason still reaches the join.
        let waiting_since = std::time::Instant::now();
        while audio_threads.iter().any(|handle| !handle.is_finished())
            && waiting_since.elapsed() < Duration::from_millis(2000)
        {
            while audio_events.try_recv().is_ok() {}
            thread::sleep(Duration::from_millis(2));
        }
        for handle in audio_threads {
            let _ = handle.join();
        }
    }
    Ok(())
}

/// Every channel one recording talks over.
///
/// One value because they are handed over together and never apart: the
/// preview frames, the reason the capture ended, the telemetry the UI polls,
/// and the finalized segments the index writer owns. All of them drop when the
/// recording ends, even when the worker thread lives on holding its surface
/// (t260929-ea5e) -- `finish_capture` joins the index writer on the last
/// `finalized_segments` sender going away. The stop handshake is the thread's,
/// not the recording's: see `run_capture`.
pub(super) struct WorkerChannels {
    pub(super) frames: Sender<CapturedFrame>,
    pub(super) stopped: Sender<CaptureStopReason>,
    pub(super) encoder_events: Sender<encoder::EncoderEvent>,
    pub(super) finalized_segments: Sender<super::indexer::IndexEvent>,
    /// Mid-recording changes to the extra audio tracks (t261002-577c).
    pub(super) extra_audio: Receiver<ExtraAudioUpdate>,
}

/// One recording handed to a capture worker: the first one it is spawned
/// with, or a later one a held worker picks up (t260929-ea5e).
pub(super) struct Job {
    pub(super) config: CaptureConfig,
    /// Keep the WGC surface open once this recording stops, because its
    /// target is armed (the user's 2026-09-29 ruling C on t260929-ea5e).
    pub(super) hold: bool,
    pub(super) channels: WorkerChannels,
    pub(super) counters: Counters,
    /// Sent once the recording's channels are gone: `true` when the thread
    /// now waits on `jobs` holding its surface, `false` when it is ending.
    pub(super) done: Sender<bool>,
}

/// Written by this worker and read by `CaptureSession`: the frames actually
/// handed to the encoder (task095) and where recorded time has reached
/// (task1680).
#[derive(Clone)]
pub(super) struct Counters {
    pub(super) encoded_frames: Arc<AtomicU64>,
    pub(super) position: Arc<AtomicI64>,
}

/// How far a recording got, kept outside `record` so its `Err` can still be
/// named: the startup stage it had reached, and whether the audio session is
/// what ended it.
struct Progress {
    stage: &'static str,
    audio_death: bool,
}

/// A capture thread: runs `first`, then -- while each recording asks to hold
/// and stopped because it was asked to -- waits with the WGC session still
/// open for the next job on `jobs` (t260929-ea5e). Same-target recordings then
/// share one `GraphicsCaptureSession`, and the ≈4.3 B/pixel WGC leaves behind
/// per closed session is paid once instead of per recording.
///
/// `stop` lives as long as the thread: `item.Closed` signals it whether or not
/// a recording is running, and a closed target ends the wait.
pub(super) fn run_capture(
    first: Job,
    stop: Receiver<()>,
    stop_signal: Sender<()>,
    jobs: Receiver<Job>,
) {
    let mut surface: Option<CaptureSurface> = None;
    let mut job = first;
    loop {
        let Job {
            config,
            hold,
            channels,
            counters,
            done,
        } = job;
        if surface
            .as_ref()
            .is_some_and(|held| !held.reusable_for(&config))
        {
            close_surface(surface.take());
        }
        let usable = run_one(
            &config,
            &mut surface,
            channels,
            counters,
            (&stop, &stop_signal),
        );
        let held = hold && usable && surface.is_some();
        if !held {
            close_surface(surface.take());
        }
        let _ = done.send(held);
        if !held {
            return;
        }
        let next = crossbeam_channel::select! {
            recv(jobs) -> next => next.ok(),
            recv(stop) -> _ => {
                if surface.as_ref().is_some_and(|held| held.source_closed.load(Ordering::Acquire)) {
                    None
                } else {
                    // Not `item.Closed`, so the stop of a recording whose job
                    // is already queued (a session sends its job before it
                    // can send a stop): take the job and put the stop back.
                    let queued = jobs.try_recv().ok();
                    let _ = stop_signal.try_send(());
                    queued
                }
            },
        };
        match next {
            Some(next) => job = next,
            None => {
                close_surface(surface.take());
                // ponytail: a job sent between this drain and the thread's
                // end is dropped unanswered; the window is the target closing
                // within microseconds of a start.
                for orphan in jobs.try_iter() {
                    let _ = orphan
                        .channels
                        .stopped
                        .try_send(CaptureStopReason::SourceClosed);
                }
                return;
            }
        }
    }
}

/// Closes a surface for good, reading task4280's refcounts after it.
fn close_surface(surface: Option<CaptureSurface>) {
    let Some(surface) = surface else {
        return;
    };
    let _refs = SurfaceRefsGuard(SurfaceRefs::wanted().then(|| SurfaceRefs::of(&surface)));
    if let Err(error) = surface.close() {
        tracing::warn!(%error, "capture surface close failed");
    }
}

/// One recording, start to stop. `true` when it ended because it was asked
/// to, which is the only way its surface may be held for the next one.
fn run_one(
    config: &CaptureConfig,
    surface: &mut Option<CaptureSurface>,
    channels: WorkerChannels,
    counters: Counters,
    stop: (&Receiver<()>, &Sender<()>),
) -> bool {
    // The one fork in the road for what the encoder produces (task430):
    // a finalized segment goes to the index writer, which is the only thing
    // that writes the manifest; everything else is telemetry for the UI poll.
    // `send` rather than `try_send` on the first: the channel is unbounded, so
    // it cannot block here, and a dropped segment is a file on disk the
    // manifest would never mention.
    let emit = |event: encoder::EncoderEvent| match event {
        encoder::EncoderEvent::Finalized(segment) => {
            let _ = channels
                .finalized_segments
                .send(super::indexer::IndexEvent::Segment(Box::new(segment)));
        }
        telemetry => {
            let _ = channels.encoder_events.try_send(telemetry);
        }
    };
    // Outlives `record` so the `Err` handler below can still name the cause.
    // The audio arm sets `audio_death` and `break`s, but the shutdown that
    // follows the break can itself fail -- task410's disease, the fMP4 sink
    // refusing video once the audio track stops advancing, would leave the
    // drain incomplete. Without it the error would be reported under the
    // generic `start_capture` stage, i.e. straight back to the wrong toast.
    let mut progress = Progress {
        stage: "com",
        audio_death: false,
    };
    let outcome = record(
        config,
        surface,
        &channels,
        &counters,
        stop,
        &emit,
        &mut progress,
    );
    let Progress {
        stage: startup_stage,
        audio_death,
    } = progress;
    let reason = match outcome {
        Ok(reason) => reason,
        Err(error) => {
            // `start_capture` is the last stage set before the loop, so seeing
            // it here means the capture was *running* and died -- not that
            // startup failed (task410). The event name says startup because
            // the UI and `InitializationFailed` are built on it; this field is
            // what tells a reader which of the two they are looking at, and
            // where the real cause is written down.
            let mid_recording = startup_stage == "start_capture";
            tracing::warn!(
                event = "capture_startup_failed",
                stage = startup_stage,
                hresult = error.code().0,
                mid_recording,
                "capture ended with an error; when mid_recording is true the capture was already \
                 running and the cause is the capture_worker_step_failed line above, not this one \
                 (hresult here is GetLastError and is usually 0)"
            );
            CaptureStopReason::InitializationFailed {
                // `startup_stage` itself is left alone: the `mid_recording`
                // check above reads it, and rewriting it would make the log
                // claim a startup failure. Only what the UI is told changes.
                //
                // The audio arm comes first: it is a mid-recording death too,
                // but it is the one whose cause the user can act on, so it must
                // not be swallowed by the generic mid-recording stage
                // (t260911-77c3).
                stage: if audio_death {
                    super::AUDIO_SESSION_STAGE
                } else if mid_recording {
                    super::MID_RECORDING_STAGE
                } else {
                    startup_stage
                },
            }
        }
    };
    let _ = channels.stopped.try_send(reason);
    reason == CaptureStopReason::Requested
}

// `measured_fps` (Task095): frames actually handed to the encoder, divided by real
// elapsed time since the previous `diagnostics()` poll -- not preview-frame drain
// interval, which degenerates into the frontend's ~500ms poll period (see
// `CaptureSession::diagnostics`).
pub(super) fn measured_fps_from_counts(count_delta: u64, elapsed: Duration) -> f32 {
    let elapsed_secs = elapsed.as_secs_f32();
    if elapsed_secs <= 0.0 {
        return 0.0;
    }
    count_delta as f32 / elapsed_secs
}

#[cfg(test)]
mod hdr_follow_tests {
    use super::*;

    const ON: HdrState = HdrState {
        hdr: true,
        white_point: 3.5,
    };
    const OFF: HdrState = HdrState {
        hdr: false,
        white_point: gpu::HDR_WHITE_POINT_FLOOR,
    };

    #[test]
    fn hdr_turned_off_switches_to_sdr_keeping_the_white_point_for_frames_in_flight() {
        assert_eq!(
            hdr_follow(ON, OFF),
            Some(HdrState {
                hdr: false,
                white_point: 3.5,
            })
        );
        // Turned on: HDR with the new white point.
        assert_eq!(hdr_follow(OFF, ON), Some(ON));
        // No change does nothing.
        assert_eq!(hdr_follow(ON, ON), None);
        assert_eq!(hdr_follow(OFF, OFF), None);
    }

    /// task1820: the composition scale is frozen at session start, so a
    /// slider move alone must not re-divide the frames.
    #[test]
    fn a_white_level_change_alone_is_not_followed() {
        let brighter = HdrState {
            white_point: 4.0,
            ..ON
        };
        assert_eq!(hdr_follow(ON, brighter), None);
    }
}

#[cfg(test)]
mod resize_tests {
    use super::*;

    fn size(width: i32, height: i32) -> CaptureSize {
        CaptureSize { width, height }
    }

    /// The frame minus the crop, rounded down to even, is the size the
    /// encoder is built at -- at the start and on every rebuild alike.
    #[test]
    fn the_output_is_the_frame_minus_the_crop_rounded_to_even() {
        assert_eq!(
            output_for(size(1922, 1112), gpu::Inset::default()),
            size(1922, 1112)
        );
        let crop = gpu::Inset {
            left: 8,
            top: 8,
            right: 8,
            bottom: 9,
        };
        assert_eq!(output_for(size(1937, 1097), crop), size(1920, 1080));
    }

    /// A drag-resize changes size every frame and must not rebuild the
    /// encoder each time; only a size held for `RESIZE_SETTLE` does.
    #[test]
    fn a_resize_is_followed_only_once_it_settles() {
        let start = std::time::Instant::now();
        let at = |ms| start + Duration::from_millis(ms);
        let current = size(1922, 1112);
        let mut candidate = None;
        assert_eq!(
            settled_resize(&mut candidate, current, current, at(0)),
            None
        );
        assert_eq!(
            settled_resize(&mut candidate, current, size(1800, 1000), at(0)),
            None
        );
        assert_eq!(
            settled_resize(&mut candidate, current, size(1900, 1060), at(300)),
            None
        );
        assert_eq!(
            settled_resize(&mut candidate, current, size(1920, 1080), at(600)),
            None
        );
        assert_eq!(
            settled_resize(&mut candidate, current, size(1920, 1080), at(1000)),
            None
        );
        assert_eq!(
            settled_resize(&mut candidate, current, size(1920, 1080), at(1100)),
            Some(size(1920, 1080))
        );
        assert_eq!(candidate, None);
        // Back to the recorded size before it settled: nothing to do.
        assert_eq!(
            settled_resize(&mut candidate, current, size(1280, 720), at(1200)),
            None
        );
        assert_eq!(
            settled_resize(&mut candidate, current, current, at(1300)),
            None
        );
        assert_eq!(candidate, None);
        assert_eq!(
            settled_resize(&mut candidate, current, size(0, 0), at(1400)),
            None
        );
    }
}

#[cfg(test)]
mod surface_key_tests {
    use super::*;

    /// t260929-ea5e: a held surface is reused only for the content size its
    /// output size came from, so the packing must keep width and height apart.
    #[test]
    fn a_resized_or_rotated_window_does_not_match_its_held_surface() {
        let key = |width, height| SurfaceKey {
            include_cursor: true,
            frame_rate: 60,
            requested_output: CaptureSize {
                width: 0,
                height: 0,
            },
            content: pack_size(SizeInt32 {
                Width: width,
                Height: height,
            }),
        };
        assert_eq!(key(1600, 900), key(1600, 900));
        assert_ne!(key(1600, 900), key(900, 1600));
        assert_ne!(key(1600, 900), key(1400, 800));
        assert_ne!(
            key(1600, 900),
            SurfaceKey {
                frame_rate: 120,
                ..key(1600, 900)
            }
        );
    }
}

#[cfg(test)]
mod hold_tests {
    use super::*;

    /// Two frame intervals, per fps (task163). 30 -> 66ms, 60 -> 33ms,
    /// 120 -> 16ms; the watchdog fires at the boundary, not past it.
    #[test]
    fn the_hold_is_due_after_two_frame_intervals() {
        for (fps, interval_ms) in [(30u8, 66u64), (60, 33), (120, 16)] {
            assert!(
                !should_hold_frame(Duration::from_millis(interval_ms - 1), fps),
                "{fps}fps must not hold before its interval"
            );
            assert!(
                should_hold_frame(Duration::from_millis(interval_ms), fps),
                "{fps}fps must hold at its interval"
            );
            assert!(should_hold_frame(
                Duration::from_millis(interval_ms + 1),
                fps
            ));
            // A frame arriving on schedule never triggers a hold: one interval
            // of silence is normal, two is the signal.
            let one_interval = Duration::from_millis(1000 / u64::from(fps));
            assert!(!should_hold_frame(one_interval, fps), "{fps}fps");
        }
    }

    /// Defensive: a zero frame rate would divide by zero rather than merely
    /// misbehave, and `CaptureConfig` is built from persisted settings.
    #[test]
    fn a_zero_frame_rate_does_not_divide_by_zero() {
        assert!(should_hold_frame(Duration::from_secs(3), 0));
        assert!(!should_hold_frame(Duration::from_millis(1), 0));
    }
}
