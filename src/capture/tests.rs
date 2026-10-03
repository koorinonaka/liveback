use super::worker::exclusive_segment_end;
use super::*;
use std::sync::atomic::AtomicI64;
use std::sync::{LazyLock, Mutex};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SUBRESOURCE_DATA,
    D3D11_USAGE_STAGING,
};

const CAPTURE_WORKER_CASE: &str = "LIVIA_CAPTURE_WORKER_CASE";
static GPU_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
/// `CaptureController::apply_buffer_root` writes a process-wide static, so two
/// tests that point it somewhere of their own must not overlap -- one would
/// reset it out from under the other and both would read the wrong folder.
/// `pub(crate)` because `clips::tests` needs the same lock (task3030's default
/// clip folder now follows this same static) -- a second, unshared lock there
/// would not stop the two suites from racing each other.
pub(crate) static BUFFER_ROOT_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

mod automation;
mod catalog;
mod controller;
mod encoding;
mod review;
mod transform;

/// Test output must never land in the user's real buffer
/// (`default_encoder_output_dir()`), or `cargo test` fills the history pane with
/// hundreds of zero-length sessions and they compete with real recordings for
/// the retention budget.
pub(super) fn test_encoder_output_dir() -> std::path::PathBuf {
    // The sequence number, not the clock, is what makes this unique
    // (task3230/task3240): two parallel test threads in one process can read
    // the same 100ns tick and end up sharing this directory. This one stays a
    // bare `PathBuf` rather than the `TmpDir` guard below -- automation.rs's
    // callers hand it to `CaptureConfig` and clean it up themselves once the
    // capture session (and its file handles) have stopped, which the guard's
    // eager `Drop` cannot wait for.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir()
        .join("livia-tests")
        .join(format!("capture-{seq}-{nanos}"))
}

/// A temp directory that deletes itself when its binding goes out of scope
/// (task3230's rule, applied here by task3240). Explicit cleanup at the end of
/// a test is not enough: an assertion failure unwinds past it, so exactly the
/// runs worth keeping small -- the failing ones -- are the ones that leave
/// fixtures behind. `Drop` runs on the unwind too.
///
/// A third deliberate copy of the guard in `ring_buffer/tests.rs` and
/// `ring_buffer/container/tests.rs` rather than a shared helper: both of those
/// are private to their own test modules, and `src/ring_buffer/` is not
/// something this module may reach into for one.
///
/// Deliberately not `Clone`: two guards over one directory would each try to
/// remove it.
pub(super) struct TmpDir {
    path: std::path::PathBuf,
}

impl std::ops::Deref for TmpDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::path::Path> for TmpDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // Never panic here. A `.lvb` a writer still holds open (no
        // `FILE_SHARE_DELETE`, see `container.rs`) makes the removal a sharing
        // violation, which is not a reason to turn a green test red.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// A guarded temp directory named after the test that asked for it.
pub(super) fn tmp_dir(name: &str) -> TmpDir {
    // The sequence number, not the clock, is what makes the name unique
    // (task3230): two parallel test threads in one process can read the same
    // 100ns tick and end up sharing -- and then removing -- one directory.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!(
        "livia-capture-{name}-{}-{seq}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).expect("temp dir");
    TmpDir { path }
}

/// A `CaptureSession` with no worker and no live channels, for the tests that
/// only need "something is recording" to be true.
fn idle_session() -> CaptureSession {
    session_with_encoder_events().0
}

/// The same session with its encoder channel held open, for the tests that need
/// the worker to report something into it (task2140).
fn session_with_encoder_events() -> (
    CaptureSession,
    crossbeam_channel::Sender<encoder::EncoderEvent>,
) {
    let (stop, _stop_rx) = bounded::<()>(1);
    let (_frames_tx, frames) = bounded::<CapturedFrame>(1);
    let (encoder_tx, encoder_events) = bounded::<encoder::EncoderEvent>(4);
    let session = CaptureSession {
        stop,
        worker: None,
        // No real worker: the done channel's sender is already gone, which
        // reads as a worker that ended (t260929-ea5e).
        jobs: bounded(1).0,
        extra_audio: bounded(1).0,
        recording_done: bounded(1).1,
        frames,
        encoder_events,
        diagnostics: blank_diagnostics(),
        last_sequence: 0,
        recording_position: Arc::new(AtomicI64::new(0)),
        encoded_frame_count: Arc::new(AtomicU64::new(0)),
        last_measured_frame_count: 0,
        last_measured_at: Instant::now(),
    };
    (session, encoder_tx)
}

/// Installs a running capture under `session_id`, the way `start` would
/// (task1990). Returns the stop channel's sender, which a caller that ignores it
/// drops straight away: a session with no real worker has nobody to report a
/// reason, which reads as "still running" exactly like an empty channel does.
/// Hold on to it to make that capture report it ended on its own, which is what
/// `stop_if_worker_ended` reacts to.
///
/// The start sequence comes from the controller's own counter (task2000), so
/// installing twice orders those two captures exactly as two `start` calls
/// would -- which is what makes "the last-started one" testable here.
fn install_active(
    controller: &CaptureController,
    session_id: &str,
    session: CaptureSession,
) -> crossbeam_channel::Sender<CaptureStopReason> {
    let (stopped, stop_events) = bounded(4);
    controller.active.lock().unwrap().insert(
        session_id.to_owned(),
        ActiveCapture {
            sequence: controller.next_start_seq.fetch_add(1, Ordering::Relaxed),
            session,
            target: ("0x0".to_owned(), targets::CaptureTargetKind::Window),
            // The fake's on-screen name is its id, so a report about one of
            // several installed captures says which one it is about (task2140).
            target_title: Some(session_id.to_owned()),
            pending_markers: Arc::new(Mutex::new(Vec::new())),
            pending_meta: Arc::new(Mutex::new(Vec::new())),
            indexer: None,
            stop_events,
            audio: SessionAudio {
                key: None,
                capture_all_audio: false,
                monitor: false,
                extras: Vec::new(),
                tracks: Vec::new(),
                target_name: String::new(),
            },
        },
    );
    stopped
}

fn blank_diagnostics() -> CaptureDiagnostics {
    CaptureDiagnostics {
        input_size: CaptureSize {
            width: 0,
            height: 0,
        },
        output_size: CaptureSize {
            width: 0,
            height: 0,
        },
        measured_fps: 0.0,
        hdr_to_sdr: false,
        cursor_included: false,
        dropped_frames: 0,
        encoder_errors: Vec::new(),
        last_audio_pts_100ns: None,
        last_video_pts_100ns: None,
        max_abs_audio_video_drift_100ns: 0,
        audio_reinitializations: 0,
        audio_unavailable: false,
    }
}
