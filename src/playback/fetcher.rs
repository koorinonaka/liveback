//! Reading a segment ahead on its own thread (task1510), split out of
//! `segment_reader.rs` (t261025-d24d).

use std::sync::Arc;

use windows::Win32::Media::MediaFoundation::{IMFDXGIDeviceManager, IMFSample};

use super::segment_reader::{open_segment_with, SegmentReader};
use super::source::SegmentSource;

/// One segment read-ahead, as the worker asks for it.
///
/// `generation` and `playlist_index` are carried through untouched so the
/// worker can recognise its own request coming back: a seek moves the
/// generation on, and a response from before it belongs to a position the
/// transport is no longer heading for.
///
/// The lease is a string the worker acquired *before* sending this -- the
/// lease window and the fetch gate stay on the worker thread, because both are
/// its own state and neither is expensive. All that crosses is the part that
/// costs hundreds of milliseconds on a 35MB segment: the read, the Source
/// Reader, and the first decode.
pub(super) struct FetchRequest {
    pub(super) generation: u64,
    pub(super) playlist_index: usize,
    pub(super) segment_index: u64,
    pub(super) start_100ns: i64,
    pub(super) lease: String,
    pub(super) source: Arc<dyn SegmentSource>,
    /// The device the decoder should decode onto (t260915-e6c8), or `None` on a
    /// machine with no GPU scaler to own one.
    pub(super) manager: Option<IMFDXGIDeviceManager>,
}

/// The same apartment argument [`FetchResponse`] below makes, in the other
/// direction (t260915-e6c8): windows-rs declines to mark COM interfaces `Send`,
/// the fetch thread is MTA (it calls `CoInitializeEx(COINIT_MULTITHREADED)`
/// before it touches anything) and `IMFDXGIDeviceManager` is documented
/// thread-safe -- which is also why the device under it is created with
/// `ID3D11Multithread::SetMultithreadProtected` (`gpu::create_device`).
unsafe impl Send for FetchRequest {}

/// A first frame as the worker wants it: (absolute 100ns, decoded sample).
///
/// Decoded, not converted (t260914-8d69). The swizzle used to happen here so
/// the crossing tick had nothing left to pay, but that made the seam frame the
/// one frame per crossing that skipped the worker's GPU convert-and-scale and
/// went through the CPU resample instead. The decode -- the part task209 sized
/// at hundreds of milliseconds on a 140Mbps keyframe -- still happens ahead.
pub(super) type FirstFrame = (i64, IMFSample);

pub(super) struct FetchResponse {
    pub(super) generation: u64,
    pub(super) playlist_index: usize,
    pub(super) segment_index: u64,
    pub(super) result: Result<(SegmentReader, Option<FirstFrame>), String>,
}

/// Same reasoning as [`Retiring`] above: `IMFSourceReader` is not `Send` only
/// because windows-rs is conservative about apartments. The fetch thread calls
/// `CoInitializeEx(COINIT_MULTITHREADED)` before it touches anything, so the
/// reader it builds is an MTA object and the (also MTA) worker thread may use
/// it. The retirer already carries readers the other way.
///
/// The same covers the [`IMFSample`] in [`FirstFrame`] (t260914-8d69): it comes
/// out of `ReadSample` on that same reader, on that same thread, and is read by
/// the worker only after the reader it came from is installed as `current`.
unsafe impl Send for FetchResponse {}

/// Reads the next segment on its own thread, so the playback thread's tick is
/// never blocked by it (task1510).
///
/// The costs this moves were sized for 2-4MB segments -- "a prefetch costs
/// ~30ms" (task203). A 140Mbps recording writes 28-35MB every two seconds, and
/// there the same work runs into the hundreds of milliseconds; paid on the
/// playback thread it stops frame presentation outright, which is the stutter
/// at every segment boundary this exists to remove.
///
/// Deliberately ungated, unlike [`ReaderRetirer`]: the retirer may hold work
/// back because a teardown can always happen later, but a prefetch has a
/// deadline. Frames that run late against the concurrent decode are absorbed by
/// task630's drop-before-swizzle.
pub(super) struct SegmentFetcher {
    tx: crossbeam_channel::Sender<FetchRequest>,
    rx: crossbeam_channel::Receiver<FetchResponse>,
}

impl SegmentFetcher {
    pub(super) fn spawn() -> Self {
        // Unbounded both ways. The worker keeps at most one request in flight,
        // so the real depth is one or two; bounding them would only introduce a
        // way for the fetch thread to block on `send` while the worker is busy.
        let (request_tx, request_rx) = crossbeam_channel::unbounded::<FetchRequest>();
        let (response_tx, response_rx) = crossbeam_channel::unbounded::<FetchResponse>();
        // Not joined at shutdown, for the same reason the retire thread is not:
        // the thread dies with its channel and the OS reclaims the rest.
        let _ = std::thread::Builder::new()
            .name("livia-segment-fetch".into())
            .spawn(move || {
                crate::com::init_mta();
                while let Ok(request) = request_rx.recv() {
                    let response = FetchResponse {
                        generation: request.generation,
                        playlist_index: request.playlist_index,
                        segment_index: request.segment_index,
                        result: fetch_segment(&request),
                    };
                    if response_tx.send(response).is_err() {
                        return;
                    }
                }
            });
        Self {
            tx: request_tx,
            rx: response_rx,
        }
    }

    /// Queues a read-ahead. `false` means the fetch thread is gone, in which
    /// case the worker simply never marks anything in flight and the crossing
    /// falls back to the synchronous `ensure_segment` it always had.
    pub(super) fn request(&self, request: FetchRequest) -> bool {
        self.tx.send(request).is_ok()
    }

    pub(super) fn try_recv(&self) -> Option<FetchResponse> {
        self.rx.try_recv().ok()
    }

    /// Blocks until the fetch thread answers (t261016-153c): a crossing whose
    /// read-ahead is still in flight waits for it instead of reading the same
    /// segment a second time. `None` means the fetch thread is gone -- it holds
    /// the only sender, so that never hangs.
    pub(super) fn recv(&self) -> Option<FetchResponse> {
        self.rx.recv().ok()
    }
}

/// The expensive half, on the fetch thread: bytes, reader, first frame.
///
/// The scopes are the ones that used to sit in `worker.rs::prefetch_next`; an
/// `--insight` run showing them here rather than under `playback_tick` is what
/// says the work actually moved.
fn fetch_segment(request: &FetchRequest) -> Result<(SegmentReader, Option<FirstFrame>), String> {
    crate::insight_scope!("playback_prefetch");
    let mut reader = open_segment_with(request.playlist_index, request.manager.as_ref(), || {
        request.source.byte_stream(
            &request.lease,
            request.segment_index,
            request.playlist_index,
        )
    })?;
    // Task209's question -- pay the first decode ahead or at the crossing --
    // answered ahead, now that "ahead" no longer means "on the playback
    // thread". At 140Mbps a keyframe costs far more than the 21-27ms that
    // made it a close call. The *conversion* stays at the crossing
    // (t260914-8d69), so the seam frame takes the worker's one scaling path.
    let first = {
        crate::insight_scope!("playback_prefetch_first");
        reader
            .next_video_sample()
            .map(|(local, sample)| (request.start_100ns + local, sample))
    };
    Ok((reader, first))
}
