//! Where a playback [`Worker`](super::worker::Worker) gets its container bytes.
//!
//! The review screen reads segments out of the ring buffer through a
//! [`CaptureController`] lease; a clip is one finished mp4 on disk. Everything
//! above this seam -- seeking, the audio mixer, drift correction, the scaler,
//! frame pacing -- is identical for both, so the only thing that varies is how
//! a segment turns into something Media Foundation can read.
//!
//! **The seam is an `IMFByteStream`, not a `Vec<u8>`** (task3480 (d)). Handing
//! bytes across would cost one extra copy of the whole file resident, plus a
//! second during the open, and a 3-minute clip of a 100Mbps recording is about
//! 2.2GB -- the byte route does not scale. The clip implementation hands MF the
//! file itself and never reads it into the process.

use std::path::PathBuf;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFByteStream, MFCreateFile, MFCreateMFByteStreamOnStream, MF_ACCESSMODE_READ,
    MF_FILEFLAGS_NONE, MF_OPENMODE_FAIL_IF_NOT_EXIST,
};
use windows::Win32::UI::Shell::SHCreateMemStream;

use crate::capture::CaptureController;

/// The `Worker`'s whole dependency on where a recording lives.
///
/// `Send + Sync` because the read-ahead thread (task1510) holds a clone of it:
/// `FetchRequest` carries an `Arc<dyn SegmentSource>` exactly where it used to
/// carry a cloned `CaptureController`.
pub(super) trait SegmentSource: Send + Sync {
    /// Holds the given segment indices against pruning for as long as the
    /// returned lease is alive. A source with nothing to prune returns an
    /// empty token.
    fn acquire_lease(&self, session_id: &str, window: Vec<u64>) -> Result<String, String>;

    fn release_lease(&self, lease: &str);

    /// One segment as a byte stream MF can resolve. Called again, for the same
    /// segment, when the NV12 open is refused and the RGB32 fallback needs a
    /// stream that has not been read from.
    fn byte_stream(
        &self,
        lease: &str,
        segment_index: u64,
        playlist_index: usize,
    ) -> Result<IMFByteStream, String>;

    /// Whether the session under the transport is the one still recording
    /// (task1530), which is what arms the one-segment live reserve.
    fn is_live(&self, session_id: &str) -> bool;
}

/// The review screen's source: leased reads out of the ring buffer.
pub(super) struct ControllerSource {
    controller: CaptureController,
}

impl ControllerSource {
    pub(super) fn new(controller: CaptureController) -> Self {
        Self { controller }
    }
}

impl SegmentSource for ControllerSource {
    fn acquire_lease(&self, session_id: &str, window: Vec<u64>) -> Result<String, String> {
        self.controller
            .acquire_review_lease(Some(session_id.to_string()), window)
    }

    fn release_lease(&self, lease: &str) {
        self.controller.release_review_lease(lease);
    }

    fn byte_stream(
        &self,
        lease: &str,
        segment_index: u64,
        playlist_index: usize,
    ) -> Result<IMFByteStream, String> {
        crate::insight_scope!("playback_open_bytes");
        let bytes = self.controller.read_review_segment(lease, segment_index)?;
        memory_byte_stream(&bytes, playlist_index)
    }

    fn is_live(&self, session_id: &str) -> bool {
        self.controller.is_last_started(session_id)
    }
}

/// A single finished mp4 -- an exported clip (task3500).
///
/// No lease (nothing prunes a file), never live, and the bytes stay on disk.
pub(super) struct ClipSource {
    path: PathBuf,
}

impl ClipSource {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path }
    }
}

impl SegmentSource for ClipSource {
    fn acquire_lease(&self, _session_id: &str, _window: Vec<u64>) -> Result<String, String> {
        Ok(String::new())
    }

    fn release_lease(&self, _lease: &str) {}

    fn byte_stream(
        &self,
        _lease: &str,
        _segment_index: u64,
        _playlist_index: usize,
    ) -> Result<IMFByteStream, String> {
        crate::insight_scope!("playback_open_bytes");
        unsafe {
            MFCreateFile(
                MF_ACCESSMODE_READ,
                MF_OPENMODE_FAIL_IF_NOT_EXIST,
                MF_FILEFLAGS_NONE,
                &HSTRING::from(self.path.as_os_str()),
            )
            .map_err(|error| format!("MFCreateFile({}): {error}", self.path.display()))
        }
    }

    fn is_live(&self, _session_id: &str) -> bool {
        false
    }
}

/// The ring buffer's segments, in memory, exactly as before the seam existed.
///
/// The 64KiB-boundary repack (task1900/1920) belongs here and nowhere else: it
/// rewrites `moof` offsets, and a clip has no `moof` at all (task3480 (e)).
pub(super) fn memory_byte_stream(
    bytes: &[u8],
    playlist_index: usize,
) -> Result<IMFByteStream, String> {
    // Before the stream, not after: what MF reads is what it is handed, and the
    // repack is the whole fix (task1900).
    let repacked = crate::encoder::mp4_boxes::unstraddle_fragments(bytes);
    if let Some(padded) = repacked.as_deref() {
        tracing::info!(
            target: "task1900_repack",
            playlist_index,
            was = bytes.len(),
            now = padded.len(),
            "padded a fragment off the MP4 source's 64KiB read boundary"
        );
    }
    let bytes = repacked.as_deref().unwrap_or(bytes);
    unsafe {
        let stream = SHCreateMemStream(Some(bytes)).ok_or("SHCreateMemStream failed")?;
        MFCreateMFByteStreamOnStream(&stream).map_err(|error| format!("byte stream: {error}"))
    }
}
