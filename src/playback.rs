//! Native playback engine for the slint bin (task128). Replaces the WebView
//! build's hand-written MSE streaming engine (`src/playback/playbackEngine.ts`,
//! 1,347 lines): segments are decoded with a Media Foundation Source Reader
//! per segment (they are self-contained fragmented MP4s, task120), video comes
//! out as BGRA via the reader's auto-inserted Video Processor, audio as float
//! PCM into a WASAPI shared-mode render stream. No HTTP shim, no ports.
//!
//! The engine runs on its own thread. The UI sends `PlaybackCommand`s and
//! reads frames from a latest-wins mailbox (`PlaybackShared::frame`): a slow
//! UI drops frames instead of queueing 8MB buffers. `notify` is called after
//! every publish so the UI thread can pull.
//!
//! Master clock is the wall clock scaled by the playback rate, re-anchored on
//! every seek and segment switch (which is also what carries playback across
//! gaps: the next segment simply re-anchors at its own start). Audio follows
//! the clock and is dropped/refilled when it drifts more than 50ms.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crossbeam_channel::{unbounded, Sender};

use crate::capture::CaptureController;
use crate::encoder::MfRuntime;
use crate::timeline::{TimelineSegment, TimelineSnapshot};
use source::{ClipSource, ControllerSource, SegmentSource};

/// Segments the lease window pins ahead of the current one.
const LEASE_WINDOW: usize = 4;
/// How much decoded audio is kept queued in the WASAPI buffer.
const AUDIO_BUFFER: Duration = Duration::from_millis(200);
/// Audio behind the master clock by more than this is dropped to resync.
const DRIFT_LIMIT_100NS: i64 = 50 * 10_000;

pub struct PlaybackFrame {
    pub width: u32,
    pub height: u32,
    /// Empty when the picture is in `gpu` instead.
    pub rgba: Vec<u8>,
    /// The picture left on the GPU for slint to draw (t260917-66c2), present
    /// only while the UI has said it can open one
    /// ([`PlaybackShared::set_gpu_stage`]). `rgba` is empty then: no readback,
    /// no full-frame copy on the UI thread.
    pub gpu: Option<SharedPicture>,
    /// The same picture before it was resampled down to the stage (task990),
    /// present only when it was. Screenshots save this, and the stage aspect is
    /// measured from it -- the aspect is what the stage sizes itself by, and
    /// deriving it from dimensions that were themselves derived from the stage
    /// closes a loop that never settles.
    pub full: Option<(u32, u32, FullFrame)>,
}

/// Bytes in a tightly packed RGBA picture of `width` x `height`.
fn rgba_bytes(width: u32, height: u32) -> usize {
    width as usize * height as usize * 4
}

/// The recording-sized picture behind a stage-sized frame.
///
/// It used to always be RGBA, because the CPU path produced recording-sized
/// RGBA on its way to the stage and keeping it cost nothing. The GPU path
/// (t260913-2527) fuses the conversion and the downscale and reads back only
/// the stage, so there is no recording-sized RGBA to keep -- what it keeps
/// instead is the decoder's own NV12, three eighths the bytes, converted on the
/// one occasion anything asks for the pixels: a screenshot, or the paused-stage
/// redraw. Neither is per frame.
pub enum FullFrame {
    Rgba(Vec<u8>),
    /// Tightly packed NV12 (`width` bytes a row, luma then interleaved chroma),
    /// studio-range BT.709 like everything else this app decodes, plus the
    /// conversion once anything has asked for it.
    ///
    /// The cache is why the paused stage can be redrawn on the UI thread at
    /// all: a resize is a drag, so `paused_stage_image` -- the common body
    /// behind both the review stage and the clip screen (t260914-39c4) --
    /// re-reads *the same* frame many times a second, and the conversion is
    /// ~3ms at 1922x1112 -- exactly the work task990 moved off this thread.
    /// Living inside the frame is what makes it safe: there is no
    /// invalidation rule to get wrong, because replacing `last_frame` drops
    /// the cache with the pixels it was made from.
    Nv12(Vec<u8>, std::sync::OnceLock<Vec<u8>>),
    /// No pixels: a playing frame from the hardware decoder, whose NV12 was
    /// left in the GPU's staging slot instead of read back (t261016-97b7).
    /// The size beside it is still the recording's, which is what the stage
    /// aspect needs every frame. A screenshot of it asks the engine with
    /// [`PlaybackCommand::Screenshot`].
    Unread,
}

impl FullFrame {
    /// The decoder's NV12, with an empty conversion cache.
    pub fn nv12(planes: Vec<u8>) -> Self {
        Self::Nv12(planes, std::sync::OnceLock::new())
    }

    /// The recording-sized RGBA rows, converting the NV12 case the first time
    /// and handing back the same bytes after that.
    ///
    /// `width` / `height` describe this frame and never change for it -- they
    /// are the pair `last_frame` keeps beside it -- so they take part in the
    /// first call only.
    /// Empty for [`Self::Unread`], which has no pixels: callers that draw or
    /// save check for it first.
    pub fn rgba(&self, width: u32, height: u32) -> &[u8] {
        match self {
            Self::Rgba(rgba) => rgba,
            Self::Nv12(nv12, rgba) => {
                rgba.get_or_init(|| livia_pixels::nv12_to_rgba(nv12, width, height, width))
            }
            Self::Unread => &[],
        }
    }
}

/// What the engine says about itself, written by the worker after each step.
///
/// Only what the UI reads. The engine's own counters are `Counters`, which
/// stay on the worker thread (t261025-6699).
#[derive(Clone, Copy, Debug, Default)]
pub struct PlaybackStatus {
    pub playing: bool,
    pub position_100ns: i64,
    pub at_live_edge: bool,
    /// The target of the last seek the *UI* asked for, echoed back once the
    /// worker has run it (task2910). Until it matches what the UI last sent,
    /// `position_100ns` still describes where the transport was before that
    /// seek, and the UI must keep its own position rather than take this one
    /// (`ui_state::playback::owns_position`). Internal seeks -- the poster
    /// frame, play-from-head -- deliberately leave it alone: they answer no
    /// request the UI is waiting on.
    pub acked_seek_100ns: Option<i64>,
    /// A live cold start is holding its still while the lead fills
    /// (edge-start-stutter, 2026-10-26). The position is parked m short of an
    /// edge that keeps growing, so a knob drawn there would slide left across
    /// the track for ~D; the UI draws neither knob nor played fill until it
    /// clears.
    pub cold_start_still: bool,
}

pub enum PlaybackCommand {
    Play,
    /// `reason` names the UI route that sent it (t260925-8bc3); it is the
    /// `reason` of the `task327f_stall` `stopped` line.
    Pause {
        reason: &'static str,
    },
    Seek {
        target_100ns: i64,
        precise: bool,
    },
    SetRate(f64),
    SetVolume {
        percent: i64,
        muted: bool,
    },
    /// One audio track's own level, on top of the master `SetVolume`
    /// (task1270). Track 0 is the capture target; the rest are the extra
    /// applications, in the order `SessionManifest::audio_tracks` lists them.
    ///
    /// Session-lived on purpose: nothing persists it, so a recording sounds
    /// the way it was recorded the next time it is opened.
    SetTrackVolume {
        track: usize,
        percent: i64,
        muted: bool,
    },
    /// Appends segments finalized after the engine started, and moves the live
    /// edge with them (task161). Push-only: the UI's 100ms drain sends the
    /// difference, so the worker thread stays the only owner of the playlist
    /// and never has to reach back into the controller for a timeline.
    ExtendPlaylist {
        segments: Vec<TimelineSegment>,
        live_edge_100ns: i64,
    },
    /// The NV12 of the frame last published, read back for a screenshot
    /// (t261016-97b7): `(width, height, planes)`, or `None` when the engine no
    /// longer holds it. For a `last_frame` that is [`FullFrame::Unread`].
    Screenshot(std::sync::mpsc::Sender<Option<(u32, u32, Vec<u8>)>>),
    Shutdown,
    /// Fade the audio out over `audio_out::FADE`, then shut down as `Shutdown`
    /// does (t260926-2144). Sent only by `PlaybackEngine::fade_out_and_detach`.
    FadeOutAndShutdown,
}

/// The engine's own counts (t261025-6699): the worker's, read by
/// `log_playback_window`'s once-a-second line and by the measurements in
/// `playback/tests.rs`, never by the UI -- so they live on the worker and are
/// not behind `PlaybackShared::status`.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    frames_shown: u64,
    /// Frames published later than one frame interval past their deadline.
    frames_late: u64,
    actual_fps: u32,
    last_seek_ms: u64,
    drift_corrections: u64,
    audio_enabled: bool,
    /// How much audio the WASAPI endpoint still has queued. Non-zero while
    /// playing means the device is actually consuming what was written.
    audio_queued_ms: u32,
    /// Interleaved f32 frames handed to the endpoint since the last flush.
    audio_frames_written: u64,
    /// How many audio tracks the segment being played carries (task1270): 1
    /// for every recording made before multi-track audio, 0 for one with no
    /// audio at all. Their names live in `SessionManifest::audio_tracks` --
    /// the engine only ever knows how many there are.
    audio_tracks: usize,
    /// Segment crossings and how many of them landed on a prefetch. Reported
    /// with the 1Hz playback line rather than one `debug!` per crossing, so the
    /// hit rate task209 was decided on is readable at any rate from the shipped
    /// log (task630 needed it at x1.5 and x2).
    crossings: u64,
    crossings_prefetched: u64,
    /// The same two numbers restricted to crossings made *at the live edge* --
    /// the incoming segment was the newest one the playlist had (task1530).
    /// Task1510's 15/22 was the two kinds added together, which hid that the
    /// misses were all of one kind.
    crossings_live: u64,
    crossings_live_prefetched: u64,
    /// Read-aheads retired unused (t261017-cd44): `stale` came back after a
    /// seek disowned them, `abandoned` were ready when one did. Reported with
    /// the 1Hz playback line, like the crossings above.
    prefetches_stale: u64,
    prefetches_abandoned: u64,
    /// How many times playback ran out of playlist and parked (task1530). The
    /// number that says whether live follow is riding the edge itself or
    /// trailing it: a transport with any lead at all never parks.
    parks: u64,
    frames_dropped: u64,
    /// Frames skipped for landing inside a refresh the display is already
    /// showing something in (t260913-3842). Deliberately *not* folded into
    /// `frames_dropped`: that number is the reactive late-frame drop, and a
    /// run whose two are added together reads as if the machine were failing
    /// to keep up.
    frames_skipped: u64,
}

#[derive(Default)]
pub struct PlaybackShared {
    pub frame: Mutex<Option<PlaybackFrame>>,
    pub status: Mutex<PlaybackStatus>,
    /// The worker's `Counters` as of its last `write_status`, for the tests
    /// that hold a `PlaybackEngine` rather than a `Worker` (the worker is on
    /// its own thread). Never compiled into the app.
    #[cfg(test)]
    counters: Mutex<Counters>,
    /// Physical pixels the stage is drawing the picture into, packed
    /// `width << 32 | height` (task990). Zero means "publish at full
    /// resolution".
    ///
    /// Written first by `PlaybackEngine::start_with`, before the worker thread
    /// exists (t260915-c1ca), so the poster frame `Worker::new` decodes already
    /// sees it; after that by the UI whenever the stage is resized. Still zero
    /// for the first session of a process: no stage has been laid out yet when
    /// that engine is created.
    ///
    /// An atomic rather than another mutex: the worker reads it once per frame
    /// and the UI writes it while dragging a window edge, and neither has
    /// anything to gain from blocking the other over eight bytes.
    display_target: AtomicU64,
    /// How long one display refresh lasts, in nanoseconds (t260913-3842).
    /// Zero -- the value a `Default` engine starts at -- means "no answer",
    /// and the worker then shows every frame exactly as it did before.
    ///
    /// Same reasoning as `display_target` for the atomic: written once per
    /// engine by the UI thread, read once per frame by the worker.
    display_period_ns: AtomicU64,
    /// The last recording-sized buffer the UI finished with, on its way back to
    /// the engine to be written again (task1640). A fresh `Vec` costs the engine
    /// thread page faults it does not pay writing one it has already touched:
    /// 1171us against 428us, measured in-app on a 7.8MB frame (8.3MB at 1080p).
    ///
    /// Ownership, which is the whole safety argument: the UI puts a buffer here
    /// only at the moment it *drops* its own last reference to it, so anything
    /// the engine takes out is a buffer nobody else can read -- not slint (
    /// `image_from_rgba` copies into its own `SharedPixelBuffer`) and not the
    /// screenshot path (it reads `Review::last_frame`, which owns its `Vec`
    /// until the frame after it replaces it).
    ///
    /// RGBA only, which is why there are three slots and not two: the full
    /// frame comes in two variants of different sizes (`FullFrame`), and
    /// `SpareSlot::take` drops what does not fit, so one slot serving both hands
    /// the next taker a misfit whenever the path switches (t260914-862b).
    spare: SpareSlot,
    /// The same channel for the NV12 planes the GPU path publishes
    /// (t260914-862b), kept apart from `spare` for the reason written there.
    /// The switch is rare -- a failed blit, or the stage crossing the size
    /// where scaling is worth it -- but each one cost both buffers before.
    nv12_spare: SpareSlot,
    /// The same channel for the *stage-sized* buffer the frame was resampled
    /// into (task1690). A separate slot rather than a second tenant of `spare`:
    /// the two sizes differ, and `SpareSlot::take` drops what does not fit, so one
    /// slot serving both would throw away whichever buffer arrived last. Only
    /// one variant reaches this one -- the stage is always RGBA.
    ///
    /// Same ownership argument as `spare`, one step earlier: the UI puts the
    /// resampled buffer here only after `image_from_rgba` has copied it into
    /// slint's own `SharedPixelBuffer`, at the moment it drops its only
    /// reference. Screenshots never read this one at all -- they read
    /// `Review::last_frame`, which is the full-resolution picture.
    scaled_spare: SpareSlot,
    /// Whether the UI can draw a [`SharedPicture`] (t260917-66c2). Off by
    /// default -- a clip player, the software renderer, a failed open -- and
    /// then every frame is read back into `rgba` exactly as before.
    gpu_stage: AtomicBool,
}

impl PlaybackShared {
    /// Says whether the UI can take the stage picture as a [`SharedPicture`].
    pub fn set_gpu_stage(&self, on: bool) {
        self.gpu_stage.store(on, Ordering::Relaxed);
    }

    pub fn gpu_stage(&self) -> bool {
        self.gpu_stage.load(Ordering::Relaxed)
    }

    /// Tells the engine how big the picture is actually being drawn, in
    /// physical pixels. `None` puts it back to full resolution.
    pub fn set_display_target(&self, target: Option<(u32, u32)>) {
        let packed = target.map_or(0, |(width, height)| {
            (u64::from(width) << 32) | u64::from(height)
        });
        self.display_target.store(packed, Ordering::Relaxed);
    }

    pub fn display_target(&self) -> Option<(u32, u32)> {
        match self.display_target.load(Ordering::Relaxed) {
            0 => None,
            packed => Some(((packed >> 32) as u32, packed as u32)),
        }
    }

    /// Tells the engine how often the display refreshes, so it can stop
    /// building frames that will never reach a refresh (t260913-3842). An
    /// implausible `hz` -- `display_refresh_hz` answers 0 when Windows will
    /// not say -- leaves the engine converting every frame, because a period
    /// that is wrong the other way freezes the picture.
    ///
    /// The period is `refresh_period` itself, which gates
    /// `StageGate` for the same reason -- one definition, not two.
    pub fn set_display_refresh_hz(&self, hz: u32) {
        let period_ns = refresh_period(hz).map_or(0, |period| period.as_nanos() as u64);
        self.display_period_ns.store(period_ns, Ordering::Relaxed);
    }

    pub(super) fn display_period(&self) -> Option<Duration> {
        match self.display_period_ns.load(Ordering::Relaxed) {
            0 => None,
            ns => Some(Duration::from_nanos(ns)),
        }
    }

    /// Hands a recording-sized *RGBA* buffer the UI is done with back to the
    /// engine (task1640). Latest-wins like the frame slot, and never blocks: a
    /// UI that cannot take the lock just frees the buffer as it did before, and
    /// the engine allocates the next one. NV12 planes go to
    /// [`recycle_nv12`](Self::recycle_nv12) instead, and a `FullFrame` whose
    /// variant is not known to the caller to
    /// [`recycle_full`](Self::recycle_full).
    pub fn recycle(&self, buffer: Vec<u8>) {
        self.spare.put(buffer);
    }

    /// A returned buffer of exactly `bytes`, or `None` to allocate one. Never
    /// waits: a frame is due, and a spare is an optimization.
    pub(super) fn take_spare(&self, bytes: usize) -> Option<Vec<u8>> {
        self.spare.take(bytes)
    }

    /// Hands a finished full frame back to the slot its variant belongs in
    /// (t260914-862b). The one door for the UI and for frames the UI never
    /// took: the variant decides, so neither caller has to know that the two
    /// sizes are not interchangeable -- a slot returns a buffer of exactly the
    /// size asked for, so a buffer put in the wrong one is dropped on the next
    /// take rather than reused.
    pub fn recycle_full(&self, full: FullFrame) {
        match full {
            FullFrame::Rgba(buffer) => self.recycle(buffer),
            FullFrame::Nv12(buffer, _) => self.recycle_nv12(buffer),
            FullFrame::Unread => {}
        }
    }

    /// [`recycle`](Self::recycle) for the NV12 planes (t260914-862b). Taken
    /// directly by the GPU path, which holds the planes as a bare `Vec` before
    /// a frame is ever built around them.
    pub(super) fn recycle_nv12(&self, buffer: Vec<u8>) {
        self.nv12_spare.put(buffer);
    }

    /// [`take_spare`](Self::take_spare) for the NV12 planes (t260914-862b).
    pub(super) fn take_nv12_spare(&self, bytes: usize) -> Option<Vec<u8>> {
        self.nv12_spare.take(bytes)
    }

    /// [`recycle`](Self::recycle) for the stage-sized buffer (task1690).
    pub fn recycle_scaled(&self, buffer: Vec<u8>) {
        self.scaled_spare.put(buffer);
    }

    /// [`take_spare`](Self::take_spare) for the stage-sized buffer (task1690).
    pub(super) fn take_scaled_spare(&self, bytes: usize) -> Option<Vec<u8>> {
        self.scaled_spare.take(bytes)
    }

    /// Empties all three spare slots and reports how many bytes each one gave
    /// back, in the order `spare`, `nv12_spare`, `scaled_spare`
    /// (t260917-efbe).
    ///
    /// `SpareSlot::take` already drops a misfit -- but only when something
    /// *takes*, and nothing takes while the engine publishes nothing: a paused
    /// 4K session, or one parked at the live edge, sat on ~23-56MB of
    /// recording- and stage-sized buffers for as long as it stayed loaded. The
    /// worker calls this once it has been quiet for `worker::SPARE_IDLE`;
    /// the three slots stay three (t260914-862b, task1690), this only clears
    /// them. Never blocks, like every other door here: a slot whose lock is
    /// busy is being refilled by a publish, which is not idle.
    pub(super) fn release_spares(&self) -> [usize; 3] {
        [
            self.spare.release(),
            self.nv12_spare.release(),
            self.scaled_spare.release(),
        ]
    }
}

/// `spare` if it is exactly `bytes` long, else a new zeroed buffer of
/// `bytes`: a frame is never written into a buffer of another size.
fn exact_or_new(spare: Option<Vec<u8>>, bytes: usize) -> Vec<u8> {
    match spare {
        Some(buffer) if buffer.len() == bytes => buffer,
        _ => vec![0u8; bytes],
    }
}

/// One returned buffer the engine may write a later frame into (task1640).
/// Latest-wins, and no door ever waits: a busy lock frees the buffer, or
/// allocates a new one, exactly as if there were no slot.
#[derive(Default)]
struct SpareSlot(Mutex<Option<Vec<u8>>>);

impl SpareSlot {
    fn put(&self, buffer: Vec<u8>) {
        if let Ok(mut slot) = self.0.try_lock() {
            *slot = Some(buffer);
        }
    }

    /// The spare if it is exactly `bytes`, else `None` to allocate one.
    ///
    /// A misfit is dropped rather than left in the slot -- the frame size changes
    /// when the recording does, so a spare that no longer fits never will, and
    /// keeping it would block every later frame from being recycled.
    fn take(&self, bytes: usize) -> Option<Vec<u8>> {
        self.0
            .try_lock()
            .ok()?
            .take()
            .filter(|spare| spare.len() == bytes)
    }

    /// Empties the slot and says how many bytes it gave back.
    fn release(&self) -> usize {
        self.0
            .try_lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .map_or(0, |buffer| buffer.len())
    }
}

/// How many `PlaybackEngine`s are alive right now (task4280).
///
/// Starting a recording loads the review from every door (`picker.rs`'s
/// `spawn_start_capture` sends `Cmd::LoadStarted`), so one record->stop round
/// stands up one engine and its decoder MFT whether or not anyone opened the
/// review. `load_review`'s `review.engine = Some(..)` is supposed to drop the
/// previous one on the UI thread; this counter is how that gets asserted rather
/// than assumed:
///
/// - **stays at 1 across rounds** -> the replace works; the fixed ~20.5 MB/round
///   4210 measured is not a pile of live engines, so look at the encoder.
/// - **grows with the rounds** -> the old engine is still held and the decoder
///   side owns the leak.
///
/// The decrement is after the worker join, so a log line here means the thread
/// is gone too -- not merely that the handle was dropped.
static ENGINES_ALIVE: AtomicUsize = AtomicUsize::new(0);

/// [`ENGINES_ALIVE`]'s current value, for a log line that has to say whether
/// the engine it describes is the only one (t260917-efbe).
fn live_engines() -> usize {
    ENGINES_ALIVE.load(Ordering::Relaxed)
}

/// How an engine starts, whichever source it plays (t261025-6699): where the
/// playhead is, how loud, how big the stage draws the picture, and who to ring.
/// The timeline is not here: `start_file` builds its own from the clip.
pub struct EngineParams {
    pub initial_position_100ns: i64,
    pub volume_percent: i64,
    pub muted: bool,
    /// The stage's size in physical pixels (task990), or `None` before one
    /// has been laid out; on the engine before its worker decodes the poster
    /// (t260915-c1ca).
    pub display_target: Option<(u32, u32)>,
    pub notify: Box<dyn Fn() + Send>,
}

pub struct PlaybackEngine {
    tx: Sender<PlaybackCommand>,
    /// Nothing is ever sent on it; only its *disconnect* carries meaning
    /// (task1620). `Drop` releases it before the join, which is what wakes the
    /// worker out of a frame-deadline sleep -- see `worker::nap`.
    stop_tx: Option<Sender<()>>,
    shared: Arc<PlaybackShared>,
    // Only the tests read it, through `is_alive`.
    #[cfg_attr(not(test), allow(dead_code))]
    alive: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl PlaybackEngine {
    pub fn start(
        controller: CaptureController,
        snapshot: TimelineSnapshot,
        params: EngineParams,
    ) -> Self {
        Self::start_with(
            Arc::new(ControllerSource::new(controller)),
            snapshot,
            params,
        )
    }

    /// One exported clip mp4, played by the same engine as the review screen
    /// (task3500).
    ///
    /// The clip is a one-segment timeline: nothing prunes it, nothing extends
    /// it, and there is no live edge past its end. `duration_100ns` is what the
    /// shell already knows about the file (`clips::details`); a zero or missing
    /// duration would leave the transport with nowhere to seek, so the caller
    /// passes the real one.
    ///
    /// MF reads the file directly -- the bytes are never copied into the
    /// process, which is the whole reason the seam is an `IMFByteStream`
    /// (task3480 (d): a 3-minute clip of a 100Mbps recording is ~2.2GB).
    pub fn start_file(path: std::path::PathBuf, duration_100ns: i64, params: EngineParams) -> Self {
        let snapshot = TimelineSnapshot {
            session_id: format!("clip:{}", path.display()),
            segments: vec![TimelineSegment {
                index: 0,
                start_100ns: 0,
                end_100ns: duration_100ns,
                audio_offsets_100ns: Vec::new(),
            }],
            gaps: Vec::new(),
            live_edge_100ns: duration_100ns,
            target_title: None,
            target_executable: None,
            target_executable_path: None,
            audio_tracks: Vec::new(),
        };
        Self::start_with(Arc::new(ClipSource::new(path)), snapshot, params)
    }

    fn start_with(
        source: Arc<dyn SegmentSource>,
        snapshot: TimelineSnapshot,
        params: EngineParams,
    ) -> Self {
        let (tx, rx) = unbounded();
        let (stop_tx, stop_rx) = unbounded();
        let shared = Arc::new(PlaybackShared::default());
        // Before the spawn, not after it (t260915-c1ca). The worker's first
        // act is the poster frame (`Worker::new`'s `do_seek`), and a store the
        // caller made once `spawn` had returned only usually beat it: the
        // atomic is `Relaxed` and there is no handshake. Stored here, the
        // value is simply there before the thread that reads it exists.
        shared.set_display_target(params.display_target);
        let alive = Arc::new(AtomicBool::new(true));
        let worker_shared = shared.clone();
        let worker_alive = alive.clone();
        let thread = thread::Builder::new()
            .name("playback-engine".into())
            .spawn(move || {
                // Cleared on *every* exit -- normal return, MF startup
                // failure, or a panic anywhere in the worker. Only the tests
                // read it through `is_alive` (the UI has no dead-engine check),
                // and an early `return` must not leave it stuck true for them.
                struct AliveGuard(Arc<AtomicBool>);
                impl Drop for AliveGuard {
                    fn drop(&mut self) {
                        self.0.store(false, Ordering::Release);
                    }
                }
                let _alive_guard = AliveGuard(worker_alive);
                // MF and WASAPI both live on this thread for the engine's
                // lifetime; MTA keeps their proxies happy off the UI thread.
                crate::com::init_mta();
                let _runtime = match MfRuntime::start() {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        tracing::error!(?error, "playback: MF startup failed");
                        return;
                    }
                };
                let mut worker = Worker::new(source, snapshot, params, worker_shared, stop_rx);
                worker.run(rx);
            })
            .expect("playback engine thread");
        let engines_alive = ENGINES_ALIVE.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(
            event = "playback_engine_started",
            alive = engines_alive,
            "playback engine started"
        );
        Self {
            tx,
            stop_tx: Some(stop_tx),
            shared,
            alive,
            thread: Some(thread),
        }
    }

    pub fn send(&self, command: PlaybackCommand) {
        let _ = self.tx.send(command);
    }

    pub fn shared(&self) -> Arc<PlaybackShared> {
        self.shared.clone()
    }

    #[cfg(test)]
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    /// Lets the engine fade its audio out and finish on its own, and returns
    /// at once (t260926-2144): a clip switch crossfades instead of waiting for
    /// the old engine. The join moves to a thread of its own, and the
    /// `ENGINES_ALIVE` decrement and `playback_engine_dropped` line still come
    /// after it. Nothing here releases the file by the time it returns --
    /// callers that need that keep dropping the engine.
    pub fn fade_out_and_detach(mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // Both travel with the join: the stop sender's disconnect would cut
        // every frame-deadline nap short for the whole fade, and a
        // disconnected command channel ends the worker before the fade does.
        let stop_tx = self.stop_tx.take();
        let tx = self.tx.clone();
        let _ = tx.send(PlaybackCommand::FadeOutAndShutdown);
        thread::Builder::new()
            .name("playback-fade-join".into())
            .spawn(move || {
                let _ = thread.join();
                drop((tx, stop_tx));
                engine_gone();
            })
            .expect("playback fade join thread");
    }
}

/// After the worker join on purpose (task4280): the line means the worker
/// thread is gone, not just that the handle was dropped.
fn engine_gone() {
    let engines_alive = ENGINES_ALIVE.fetch_sub(1, Ordering::Relaxed) - 1;
    tracing::info!(
        event = "playback_engine_dropped",
        alive = engines_alive,
        "playback engine dropped"
    );
}

impl Drop for PlaybackEngine {
    fn drop(&mut self) {
        // Already handed to `fade_out_and_detach`, whose thread does the rest.
        let Some(thread) = self.thread.take() else {
            return;
        };
        // The join stays synchronous on purpose: the worker releases the review
        // lease on its way out, and three callers depend on that having
        // happened by the time they return -- `unload_review`,
        // `sessions_confirm_accepted`, and through them
        // `CaptureController::discard_session`'s `has_leases` refusal
        // (task1520).
        //
        // What task1620 added is the line above it. `load_review` drops the old
        // engine on the *UI thread*, so anything the worker can wait on
        // unboundedly hangs the whole window -- and `tick` waits on the next
        // frame's deadline, which a stale master-clock anchor can put hours
        // out (2026-08-25: a fresh LiveReview load anchored at media 0 while
        // its segments carried ~9.6h absolute timestamps, and opening the same
        // session from 履歴 froze the app until it was killed). Releasing the
        // sender disconnects the channel `nap` waits on, so every deadline
        // sleep returns at once and the join costs one tick's remaining work.
        self.stop_tx = None;
        let _ = self.tx.send(PlaybackCommand::Shutdown);
        let _ = thread.join();
        engine_gone();
    }
}

mod audio_out;
mod fetch_gate;
mod gpu;
pub use gpu::{SharedPicture, StageDevice};

pub mod scale;

mod fetcher;
mod mixer;
mod retirer;
mod segment_reader;
pub(crate) use segment_reader::audio_streams_in_track_order;
mod source;

/// One ring-buffer segment's bytes as something Media Foundation can read,
/// with the same 64KiB repack the review applies (task1900) -- for the
/// history's hover preview, which decodes segments outside this engine.
pub fn segment_byte_stream(
    bytes: &[u8],
) -> Result<windows::Win32::Media::MediaFoundation::IMFByteStream, String> {
    source::memory_byte_stream(bytes, 0)
}

mod stretcher;

mod worker;
use worker::Worker;
pub use worker::{live_follow_landing_100ns, LIVE_FOLLOW_MARGIN_100NS};

#[cfg(test)]
mod tests;

/// Refresh rates outside this range are not believable answers from the
/// display, and are treated as "no answer" rather than trusted.
pub const REFRESH_HZ_RANGE: std::ops::RangeInclusive<u32> = 20..=1000;

/// One refresh of a display at `hz`, or `None` for an implausible `hz`
/// (outside [`REFRESH_HZ_RANGE`]). The stage gate and the playback engine's
/// frame skipping both read the display through this.
pub fn refresh_period(hz: u32) -> Option<Duration> {
    REFRESH_HZ_RANGE
        .contains(&hz)
        .then(|| Duration::from_nanos(1_000_000_000 / u64::from(hz)))
}
