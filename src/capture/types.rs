use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureSize {
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureColorSpace {
    Bgra8Bt709,
    ScRgb,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureConfig {
    pub window_handle: String,
    pub process_id: u32,
    /// Window or whole screen (task165). Defaulted so a config written before
    /// monitor capture existed still deserializes as what it was.
    #[serde(default)]
    pub kind: super::targets::CaptureTargetKind,
    pub frame_rate: u8,
    pub include_cursor: bool,
    pub output_size: CaptureSize,
    // Defaulted rather than required so an older settings.json (or any caller
    // written before retention became configurable) still starts a capture,
    // with the same 15 minutes it used to get. RingBuffer::create clamps it to
    // MIN/MAX_RETENTION_MINUTES, except for NO_RETENTION_LIMIT (0), which the
    // ring buffer switch sends to mean "never drop the head" (task1410).
    #[serde(default = "default_retention_minutes")]
    pub retention_minutes: u16,
    #[serde(default = "default_encoder_output_dir")]
    pub encoder_output_dir: PathBuf,
    /// The `.lvb` this recording writes into (task450), or `None` for the old
    /// directory-per-session shape.
    ///
    /// Defaulted so a config serialized before containers existed still starts
    /// a capture. `CaptureController::start` fills it in from
    /// `encoder_output_dir`, which keeps the session id (the stem) identical to
    /// what a directory session would have had.
    #[serde(default)]
    pub container_path: Option<PathBuf>,
    /// Record every application's sound, not just the target's tree
    /// (task1430). Still exactly one track: the mix excludes only Liveback
    /// itself, so a review played back while recording is not recorded.
    ///
    /// Read from `AppSettings::capture_all_audio` when the recording starts,
    /// so a flip only reaches the next one. Defaulted for the same reason the
    /// fields above are: a config written before the toggle existed still
    /// starts a capture, with the target-only sound it always had.
    #[serde(default)]
    pub capture_all_audio: bool,
    /// Which video codec this recording encodes with (task1760). Read from
    /// `AppSettings::codec` when the recording starts, the same way and for
    /// the same reason as `capture_all_audio` and `frame_rate` above.
    ///
    /// Defaulted so a config written before AV1 existed still starts a
    /// capture, with the H.264 it always had.
    #[serde(default)]
    pub codec: crate::settings::RecordingCodec,
}

fn default_retention_minutes() -> u16 {
    crate::ring_buffer::DEFAULT_RETENTION_MINUTES
}

/// Also the value callers outside the IPC layer have to supply by hand: the
/// slint bin builds `CaptureConfig` in Rust, where serde's `default` never runs
/// (task124).
/// Where the next recording's segments go.
///
/// Rooted at `CaptureController::buffer_root()`, which is the *configured*
/// buffer folder (task164). This used to rebuild the default path from
/// `LOCALAPPDATA` by hand, so once a custom folder was set the two halves of
/// the app disagreed: recordings were written to the default location while
/// listing, loading, discard and retention all read the configured one. Every
/// recording made that way was invisible to the history screen and never
/// pruned -- the sessions were simply somewhere nothing looked.
pub fn default_encoder_output_dir() -> PathBuf {
    let session = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    crate::capture::CaptureController::buffer_root().join(format!("capture-{session:032x}"))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureDiagnostics {
    pub input_size: CaptureSize,
    pub output_size: CaptureSize,
    pub measured_fps: f32,
    pub hdr_to_sdr: bool,
    pub cursor_included: bool,
    pub dropped_frames: u64,
    pub encoder_errors: Vec<String>,
    pub last_audio_pts_100ns: Option<i64>,
    pub last_video_pts_100ns: Option<i64>,
    pub max_abs_audio_video_drift_100ns: i64,
    pub audio_reinitializations: u32,
    pub audio_unavailable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureStopReason {
    Requested,
    SourceClosed,
    /// `stage` is the worker's `startup_stage` at the moment it gave up -- the
    /// same fixed name the `capture_startup_failed` log records. It rides along
    /// so the UI can say *which* part failed without reading the log (task193).
    /// It is not only a startup value: a capture that dies mid-recording is
    /// reported here too, under `MID_RECORDING_STAGE` (t260911-77c3) or
    /// `AUDIO_SESSION_STAGE` (task2150) rather than under the stage the worker
    /// happened to be holding. `start_capture` still arrives from a status an
    /// older build wrote, and from the narrow window where the loop really did
    /// fail to start; it is no longer what a running capture's death says.
    InitializationFailed {
        stage: &'static str,
    },
    /// The buffer disk fell under `ring_buffer::MIN_FREE_BYTES` while recording
    /// (task1420). A safe stop, not a failure: with task1410's toggle off a
    /// session never drops its head, so nothing else bounds how far it grows.
    DiskFull,
}

/// The one `startup_stage` name `ui_state` branches on, shared so the two ends
/// cannot drift apart again: task1760 renamed it from `h264_encoder` when AV1
/// joined H.264, `ui_state::lifecycle` kept matching the old spelling, and every
/// encoder failure fell into the generic GPU bucket for two months while the
/// tests -- which pinned the dead name as a literal -- stayed green (task2100).
/// The other stages are literals still; only this one has a reader.
pub const VIDEO_ENCODER_STAGE: &str = "video_encoder";

/// The audio session died while the capture was running, taking the recording
/// with it (task2150). Its own stage because the mid-recording death said
/// `start_capture` back then, which makes the toast say 「対象を選び直して」 --
/// advice that is simply wrong here: the window is fine, the audio endpoint is
/// what went away. It still needs its own stage now that the generic case has
/// `MID_RECORDING_STAGE`: that one says nothing about the cause, and here the
/// cause is the one thing the user can act on. This stage wins over
/// `MID_RECORDING_STAGE` when both apply.
pub const AUDIO_SESSION_STAGE: &str = "audio_session";

/// The capture was already running and died (t260911-77c3). Its own stage for
/// the same reason the audio one has: until now a mid-recording death carried
/// `start_capture` -- the last stage set before the loop -- and the toast read
/// 「対象を選び直して再開してください」, which repairs nothing, because the
/// target never broke. On 2026-09-11 the cause was the fMP4 sink refusing
/// video (`root/t260911-e7f3`) with the target window healthy to the end.
/// task2150 carved out the audio-device death alone; this is the rest of them.
/// The worker decides by `startup_stage == "start_capture"` at the moment
/// `record` returned `Err` and **does not rewrite `startup_stage` itself** --
/// the `capture_startup_failed` log would then claim a startup failure.
pub const MID_RECORDING_STAGE: &str = "mid_recording";

/// One number per stop **event**, handed out by `stopped()` (task2090).
static STOP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureLifecycleStatus {
    pub state: String,
    pub diagnostic_code: Option<String>,
    pub message: Option<String>,
    /// Set only on a failed stop. `ui_state::lifecycle` turns it into the
    /// toast's second line; the wording never leaves this crate's UI layer.
    pub failure_stage: Option<String>,
    /// A number unique to **one stop event**, so the toast can tell a status
    /// the 1s poll is re-reading from a second capture that has just died
    /// under the same `diagnostic_code` (task2090). Deliberately *not* a
    /// session id -- `CaptureStopReason` does not say which capture it came
    /// from. `0` on `idle()`/`recording()`: those are not stop events.
    ///
    /// Note this makes two stops with identical reasons compare `!=`. Compare
    /// the fields that matter, not the whole struct.
    pub seq: u64,
}

impl CaptureLifecycleStatus {
    pub(super) fn idle() -> Self {
        Self {
            state: "idle".into(),
            diagnostic_code: None,
            message: None,
            failure_stage: None,
            seq: 0,
        }
    }

    pub(super) fn recording() -> Self {
        Self {
            state: "recording".into(),
            diagnostic_code: None,
            message: None,
            failure_stage: None,
            seq: 0,
        }
    }

    pub(super) fn stopped(reason: CaptureStopReason) -> Self {
        let (diagnostic_code, message) = match reason {
            CaptureStopReason::SourceClosed => ("CAP-TGT-001", "対象ウィンドウが終了したため録画を安全に停止しました。対象を選択して再開してください。"),
            CaptureStopReason::InitializationFailed { .. } => ("CAP-DEV-001", "GPUまたはキャプチャ処理のエラーにより録画を安全に停止しました。対象を選択して再開してください。"),
            CaptureStopReason::Requested => ("CAP-EXIT-001", "録画を安全に停止しました。"),
            CaptureStopReason::DiskFull => ("CAP-DSK-001", "保存先の空き容量が少なくなったため録画を安全に停止しました。容量を空けてから対象を選び直してください。"),
        };
        Self {
            state: "stopped".into(),
            diagnostic_code: Some(diagnostic_code.into()),
            message: Some(message.into()),
            failure_stage: match reason {
                CaptureStopReason::InitializationFailed { stage } => Some(stage.into()),
                _ => None,
            },
            seq: STOP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// Native texture is intentionally not serialized or persisted. Downstream encoding owns it.
pub struct CapturedFrame {
    pub texture: ID3D11Texture2D,
    pub sequence: u64,
    pub timestamp_100ns: i64,
    pub input_size: CaptureSize,
    pub output_size: CaptureSize,
    pub color_space: CaptureColorSpace,
    pub cursor_included: bool,
}

pub(super) struct RawCaptureFrame {
    pub(super) texture: ID3D11Texture2D,
    pub(super) timestamp_100ns: i64,
    pub(super) input_size: CaptureSize,
}
