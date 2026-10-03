//! The review screen's state and its render/engine adapters (task127-129):
//! the `Review` state machine, the playback-engine bridge, the track/marker
//! renderers, and the export sink. The callback wiring lives next door in
//! `review_wiring` and `review_stage`.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use livia::capture::targets as capture_targets;
use livia::capture::CaptureController;
use livia::events::EventSink;
use livia::export::{ExportProgress, ExportStatus};
use livia::playback::{scale::FrameScaler, PlaybackCommand, PlaybackEngine, PlaybackStatus};
use livia::ring_buffer::{self, MarkerRecord};
use livia::settings::AppSettings;
use livia::ui_state::auto_capture;
use livia::ui_state::export as ex;
use livia::ui_state::lifecycle;
use livia::ui_state::playback as pb;
use livia::ui_state::timeline::{self as tl, TimelineSnapshot, Viewport};
use slint::{ComponentHandle, Image, Model, SharedString, VecModel};

use super::tr_locale;
use super::{
    image_from_rgba, AppWindow, GapSpan, MarkerRow, MarkerSpan, MenuEntry, ReviewVm, RulerTick,
    TrackSpan,
};

/// A stage gesture in flight (task108/119): zone fixed at pointerdown, hold
/// bookkeeping accumulated by the repeat timer.
pub(super) struct StageGesture {
    pub(super) zone: pb::StageZone,
    pub(super) held: bool,
    pub(super) hold_seconds: i64,
    pub(super) hold_steps: u32,
    /// The rate the centre hold borrowed x2 from (task650), owed back on
    /// release or cancel. `None` for every other gesture.
    pub(super) restore_rate: Option<f64>,
    /// The centre hold started on a paused video, so release owes a pause as
    /// well as the rate (task1010).
    pub(super) repause: bool,
}

/// UI-thread state for the review timeline (task127) and its playback
/// engine (task128). `position`/`playing` mirror the engine's status for
/// rendering; the engine owns the truth.
#[derive(Default)]
pub(super) struct Review {
    /// Whether the stage has ever been handed a frame for the session now
    /// loaded (task239). Cleared by `load_review`, set by the first frame.
    pub(super) stage_has_frame: bool,
    /// Paces frames to the display's refresh (task1580). Not per-session
    /// state: the display it is measured from outlives every load.
    pub(super) stage_gate: pb::StageGate,
    /// Collects the frame the gate turned away. Only load-bearing while the
    /// engine is quiet -- a seek while paused publishes exactly one frame, and
    /// no further pump would come for it.
    pub(super) stage_gate_timer: slint::Timer,
    /// The 1s capture poll's last answer (task237). Cached rather than asked
    /// per frame: `is_active()` takes the controller's `active` mutex, and
    /// `pump_playback` runs once per presented frame.
    pub(super) recording: bool,
    /// The captured window is minimized: the stage is showing the black the
    /// recorder is writing, and says so (round7 §2-7).
    pub(super) target_minimized: bool,
    /// The stop the user asked for on **this** session has not landed yet
    /// (task2030). Never the controller's process-wide `is_stopping()`: with two
    /// captures running that flag is raised while the *other* one is being
    /// joined, and the capsule would say 停止中… about a recording nobody
    /// touched.
    pub(super) stopping: bool,
    /// The stage session id the last capture poll saw recording
    /// (t260917-fe49): the "same id" half of the stop edge.
    pub(super) recording_session: Option<String>,
    /// A stop skip to the live edge still waiting for the tail to land, with
    /// the edge it last skipped to (`pb::live_skip`).
    pub(super) live_skip: Option<(String, i64)>,
    pub(super) engine: Option<PlaybackEngine>,
    /// Per-track level and mute, in track order (task1300). Seeded at load
    /// from the target's saved levels (t261003-83d3) and saved back by the
    /// 音量 tab; a track nothing has said anything about is full volume.
    pub(super) track_volumes: Vec<(i64, bool)>,
    /// The 音量 tab (t261003-83d3): see `review_volume`.
    pub(super) volume: super::review_volume::VolumeTab,
    pub(super) gesture: Option<StageGesture>,
    /// When the last centre tap toggled the transport (task1010), and where
    /// the playhead stood just before it did (task3270). A press that lands
    /// inside `STAGE_DOUBLE_PRESS_MS` of the instant is the second half of a
    /// double click, and full screen has to happen on that press rather than
    /// waiting for slint's release-time `double-clicked`.
    ///
    /// The position rides in the same `Option` rather than in a field of its
    /// own so the two cannot outlive each other: a stale position restored by
    /// some later, unrelated gesture is exactly the bug a second field with
    /// its own clear sites invites. `pb::restore_after_double_press` decides
    /// whether the undo half of the pair puts it back.
    pub(super) last_center_tap: Option<(std::time::Instant, i64)>,
    /// When a track pin last seeked, and to which marker time. Slint fires
    /// `clicked` on both releases of a double click (the one that opens the
    /// panel), and the second seek rewound the audio a second time.
    pub(super) last_pin_seek: Option<(std::time::Instant, i64)>,
    pub(super) stage_feedback: Option<pb::StageFeedback>,
    /// The mark's dwell, then its fade (task1020). Held on the state rather
    /// than in `review_stage::wire` because the keyboard's ±seek raises the
    /// same mark from `review_wiring` and used to leave it on screen for good.
    pub(super) feedback_timer: slint::Timer,
    pub(super) feedback_fade_timer: slint::Timer,
    /// The key currently held down, as its own press told us (task2540).
    /// slint's `KeyEvent.repeat` is dead weight on the desktop backends --
    /// `i-slint-backend-winit-1.17.1/event_loop.rs:346` builds the event from
    /// `KeyEvent::default()` and only ever fills `text`, so the flag is false
    /// even for a physical hold (only the wasm helper sets it). A press whose
    /// key matches this is therefore what "held" means here. Set by every
    /// press, cleared by every release.
    pub(super) key_down: Option<slint::SharedString>,
    /// Arrow-key scan (task2540): repeats counted while a transport key is
    /// held, gated to `pb::KEY_SCAN_INTERVAL_MS`. Cleared by a fresh tap and
    /// by release.
    pub(super) key_scan_steps: u32,
    pub(super) key_scan_last: Option<Instant>,
    /// One-shot settle armed on key release (task2540): the coarse frame the
    /// taps showed is confirmed with one precise decode once the flurry
    /// stops. Any precise seek from anywhere cancels it (`seek_clamped`).
    pub(super) key_settle_timer: slint::Timer,
    /// The engine's own last-seen playing state. Separate from `playing`,
    /// which the transport handlers set optimistically before the engine
    /// answers -- comparing against that would swallow the transition.
    pub(super) engine_playing: bool,
    /// A recording just started is played back muted: the screen it is
    /// recording is the one about to play its own audio back into the system
    /// loopback the capture is listening to. `Some((percent, muted))` is the
    /// volume to hand back the moment the user takes over -- a seek, the
    /// volume slider, or the mute button. Only ever set by the load that
    /// follows a start, so a session opened for review is untouched.
    pub(super) live_mute: Option<(i64, bool)>,
    /// Paused because the review pane went off screen, and therefore owed a
    /// resume when it comes back. Distinct from a pause the user asked for,
    /// which stays paused however many times they leave and return.
    pub(super) paused_offscreen: bool,
    /// Debounce for "another window is fully on top of us" (task2800). The
    /// observation is a per-tick Win32 hit test; this is what keeps a window
    /// merely crossing the stage from pausing anything.
    pub(super) cover: livia::ui_state::playback::CoverGate,
    pub(super) hint_counted: bool,
    /// Latest status the export pipeline published (task129). `None` until a
    /// job is started in this session.
    pub(super) export_status: Option<ExportStatus>,
    /// `(job id, comment)` of the clip job in flight. Tied to the id the
    /// controller handed back rather than kept as a loose "last comment": a
    /// draft the user cancelled must not be written into the 指定フォルダに書き出し
    /// they started next. Cleared the moment that job reaches a terminal
    /// state, whichever one.
    pub(super) clip_pending: Option<(String, String)>,
    /// The last decoded frame, kept in RGBA so a screenshot can be encoded on
    /// demand (task129). The WebView build re-read its `<video>` into a canvas;
    /// holding one frame is the native equivalent and costs 8MB at 1080p.
    /// The recording-sized picture behind the last frame shown, for the
    /// screenshot and the paused-stage redraw. Since t260913-2527 it is not
    /// necessarily RGBA -- a frame the engine scaled on the GPU keeps the
    /// decoder's NV12 instead, and `FullFrame::rgba` converts it the first time
    /// anything wants pixels and keeps the result for the rest of the frame's
    /// life.
    pub(super) last_frame: Option<(u32, u32, livia::playback::FullFrame)>,
    /// Physical pixels the stage draws the picture into, as last reported by
    /// the layout (task990). Held here as well as on the engine because each
    /// session starts a new engine, and the stage does not necessarily change
    /// size on the way -- so there would be nothing to re-report.
    pub(super) stage_target: Option<(u32, u32)>,
    pub(super) snapshot: Option<TimelineSnapshot>,
    /// What `autorec_image_path` last resolved, and what it resolved it for
    /// (task3670). See that function for why the answer is stashed at all.
    pub(super) autorec_path: Option<AutorecPath>,
    /// The session the title bar's app icon (`loaded-icon`) was resolved for
    /// (2026-09-20). Resolved once per session: it reads a file and may walk
    /// the process list.
    pub(super) title_icon: Option<String>,
    pub(super) markers: Vec<MarkerRecord>,
    /// Marker times of the rendered marker model, in row order -- the .slint
    /// side clicks by row index because 100ns ticks do not fit slint's int.
    pub(super) visible_markers: Vec<i64>,
    pub(super) range: Option<(i64, i64)>,
    // task1060's `range_editing` stood here, and task3860's 確定ステップ
    // (`clip_editing`) after it. Both are gone (t260927-7d08, user ruling
    // 2026-09-27): the track's handles are live whenever there is a range.
    /// 30 / 60 / 300, 0 = 全体, -1 = manual (chip echo only, like React's
    /// `selectedQuickRange`).
    pub(super) quick: i32,
    pub(super) zoom: Option<Viewport>,
    pub(super) dragging: bool,
    /// A range-band drag is in flight (task3160). Deliberately *not*
    /// `dragging`: that one carries the scrub telemetry and is read by the
    /// track's own move/release handlers, none of which a band drag goes
    /// through. What the two share is ownership of the position, so both are
    /// fed to `pb::owns_position`, and both pin the control bar.
    pub(super) range_dragging: bool,
    /// A Shift+press on the bare track (t260928-88a2): where it landed, the
    /// fixed end of the range the drag sweeps out. `None` for a plain scrub.
    pub(super) sweep_anchor: Option<i64>,
    /// The band drag started while playing, so its release owes a `Play`
    /// (task3160). Only 開始端 and 移動 pay it back -- a 終了端 drag is a
    /// request to stop at the new end, so it stays stopped.
    pub(super) resume_after_range_drag: bool,
    /// The end was set to the playhead while playing (task4230): the chip,
    /// `i`/`o` or the playhead button, which do not seek, so the playhead is
    /// on `end` and `pb::range_guard` would stop it on the next tick. Holds
    /// the range it was armed for. Sticky -- past the end the stop is still
    /// due on every tick -- and released by any of four things (task4230
    /// Context): (1) playback stopped, not a live-edge park -- see
    /// `poll_range_guard_with`; (2) any seek -- `seek_clamped`; (3) the range
    /// changed by anything else -- the held range no longer matches; (4) the
    /// playhead back inside, `position < end` -- then the next real arrival
    /// stops as usual. (A fifth, the 確定ステップ ending, went with the step
    /// in t260927-7d08.) A family press while
    /// playing -- or parked at the live edge while recording
    /// (t260926-c0b3, `Review::arms_end_hold`) -- re-arms it.
    pub(super) hold_at_end: Option<(i64, i64)>,
    /// The last range chip pressed: its drawn index and the range it made.
    /// Lit while `range` still equals it (`tl::range_preset_index`).
    pub(super) chip: Option<(i32, (i64, i64))>,
    /// Sync Mode (t260926-cc7e): following the recording's live edge, and the
    /// selection's stops held off while it does. See `pb::Follow` for what
    /// joins and leaves it; `follow_live` is the one landing.
    pub(super) follow: pb::Follow,
    /// When `follow_live_edge` last grew the snapshot -- a segment's arrival
    /// as this side saw it. The follow landing is phased on it
    /// (`live_follow_landing_100ns`), and on `loaded_at` until then
    /// (t260929-e171). `None` until the loaded session grows.
    pub(super) live_grew_at: Option<Instant>,
    /// When the session was loaded (t260928-faac): the drawn live edge's
    /// clock until the first growth (`tl::apparent_live_edge`), and the
    /// follow landing's phase until then (`follow_live`). A live load takes
    /// the recorder's stamp of its last fold instead (t260929-e171,
    /// `RingBuffer::last_fold`), so both start at the segment's phase: the
    /// first growth takes no step, and a join before it (the cover gate's
    /// resume right after a history open) lands where following would.
    /// Kept apart from `live_grew_at`, which means "grown since the load".
    pub(super) loaded_at: Option<Instant>,
    /// The end handle's last move left it on the live edge as it was then
    /// (t260928-faac, `tl::release_pinned_range`). Seeded at the band's press.
    pub(super) range_end_on_live: bool,
    /// The loaded session's `retention_minutes` (t260928-2f77). Fixed per
    /// session, so it is read once at load and never follows growth.
    pub(super) retention_minutes: u16,
    pub(super) position: i64,
    /// The target of the last seek this side sent (task2910). Compared with
    /// the engine's echo so `pump_playback` does not write back a position
    /// that predates it -- see `pb::owns_position`.
    pub(super) last_seek_sent: Option<i64>,
    pub(super) playing: bool,
    /// The play/pause this side last sent, and when (task3290). The engine's
    /// status is written by the worker thread, so between the command going
    /// out and the worker running it `status.playing` still carries the old
    /// answer -- and the pump writing that back flips the icon for a few
    /// frames. There is no `acked_playing` to compare against the way
    /// `acked_seek_100ns` answers for the position, so the latch expires on a
    /// clock instead: see `pb::owns_playing`.
    pub(super) play_intent: Option<(bool, Instant)>,
    /// The clock counts down instead of up (task149). Per session, like
    /// YouTube's: `load_review` puts it back to elapsed.
    pub(super) show_remaining: bool,
    /// Loop the selection instead of stopping at its end (task153).
    pub(super) repeat: bool,
    pub(super) rate: f64,
    pub(super) hover: Option<i64>,
    /// The last failure of a panel action (clip start, 「中止」, a marker).
    /// Shown as an error toast where it happens (t260928-bae5 F5); kept here
    /// only for the control port's `reason`, since the notice card is gone.
    pub(super) status: Option<String>,
    /// The track's own width in px, reported by the `.slint` side. Only the
    /// gap marks need it: whether a seam fits two rules or one is a question
    /// about pixels, and it is answered in `tl::gap_mark` where a test can see
    /// it (task171).
    pub(super) track_width_px: f32,
    pub(super) thumbnails: HashMap<u64, Option<Image>>,
    pub(super) thumbnails_requested: HashSet<u64>,
    /// Drag latency instrumentation: per-drag move/seek counters and the
    /// summed handler cost, timed from handler entry through `render_review`.
    ///
    /// Shared by the track's scrub (task127, `on_track_pressed` /
    /// `on_track_released`) and the range band's own drags (task3400,
    /// bracketed by `on_range_drag_changed`). One set of fields, because the
    /// two gestures are mutually exclusive -- and because comparing them is
    /// the whole point: they have to be counted the same way or the numbers
    /// do not line up.
    pub(super) drag_started: Option<Instant>,
    pub(super) drag_moves: u32,
    pub(super) drag_seeks: u32,
    pub(super) drag_handler_nanos: u128,
    /// Cached track models, so a 60Hz scrub only rewrites scalar properties
    /// and never rebuilds the segment/gap/marker element trees.
    ///
    /// **Each one says what its slint model currently holds, and nothing
    /// outside `render_track_spans`/`render_markers` may write them** -- not
    /// even to empty them on a session change. The push is conditional on the
    /// comparison, so a cache emptied while its model was left alone makes the
    /// next render decide it has nothing to do: that is how loading a session
    /// with no markers used to leave the previous one's dots on the bar
    /// (2026-09-22). `debug_assert_model_cache` holds the pair to it.
    pub(super) cached_segments: Vec<TrackSpan>,
    pub(super) cached_gaps: Vec<GapSpan>,
    pub(super) cached_markers: Vec<MarkerSpan>,
    pub(super) cached_marker_rows: Vec<MarkerRow>,
    /// Where the pointer is on the track as a ratio, while a scrub holds it
    /// (t260930-1904). Only the edge trace reads it: what time is under a
    /// pointer held still, through the viewport as it is now.
    pub(super) pointer_ratio: Option<f32>,
    /// The live edge as drawn (t260930-1904). Advanced by `render_review`,
    /// read through `apparent_live_edge` by everything else.
    pub(super) edge_clock: tl::LiveEdgeClock,
    /// The drain's `on_review` (pane, on screen, not covered), for the
    /// per-frame redraw (`start_edge_frames`).
    pub(super) stage_watched: bool,
}

/// The per-render `t260930_1904_edge` line is written only when
/// `LIVEBACK_EDGE_TRACE` is set: at 60 renders a second it would otherwise
/// fill the daily log for as long as a recording is watched.
fn edge_trace_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LIVEBACK_EDGE_TRACE").is_some())
}

/// A redraw per display frame (62.5 a second) while the stage shows a
/// recording's live edge moving (t260930-1904, user decision a): before it
/// the edge moved only on the drain's 100 ms renders while paused or
/// scrubbing. Runs while the drain calls the stage watched and the session
/// records, or the stop's settle is under way; otherwise it looks twice a
/// second and does nothing.
///
/// Not stopped by reduce-motion (user ruling 2026-09-30): the edge moving is
/// the recording's clock, not decoration.
///
/// 16 ms, not the drift's 17 (t260927-9865): slint's timer clock is whole
/// milliseconds, so this is 62.5/s -- just above 60 Hz, so every frame of a
/// 60 Hz monitor has a fresh edge. 17 ms (58.8/s) would repeat one a second.
/// Faster than the refresh is drawn once per refresh by the vendored winit
/// throttle; the doubling it fixed was for requests slower than the refresh.
///
/// ponytail: a fixed 62.5/s, which is every frame on a 60 Hz monitor but every
/// other or fourth on the 144 / 280 Hz desks; follow the monitor's refresh if
/// that shows.
pub(super) fn start_edge_frames(
    ui: &AppWindow,
    review: &Rc<std::cell::RefCell<Review>>,
    (seg, gap, mark, rows): super::ReviewModels<'_>,
    settings: &Arc<Mutex<AppSettings>>,
) {
    const TICK: std::time::Duration = std::time::Duration::from_millis(16);
    const IDLE: std::time::Duration = std::time::Duration::from_millis(500);
    thread_local! {
        static EDGE_TIMER: slint::Timer = slint::Timer::default();
    }
    let weak = ui.as_weak();
    let review = review.clone();
    let (seg, gap, mark, rows) = (seg.clone(), gap.clone(), mark.clone(), rows.clone());
    let settings = settings.clone();
    EDGE_TIMER.with(|timer| {
        timer.start(slint::TimerMode::Repeated, IDLE, move || {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(mut review) = review.try_borrow_mut() else {
                return;
            };
            let running = review.stage_watched
                && review.snapshot.as_ref().is_some_and(|snapshot| {
                    review.recording || review.edge_clock.moving(snapshot.live_edge_100ns)
                });
            let want = if running { TICK } else { IDLE };
            EDGE_TIMER.with(|timer| {
                if timer.interval() != want {
                    timer.set_interval(want);
                }
            });
            if running {
                let current = super::settings_page::settings_snapshot(&settings);
                render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
            }
        });
    });
}

impl Review {
    /// Whether a chip / `i` `o` / playhead-button press arms `hold_at_end`
    /// now (t260926-c0b3): see `pb::arms_end_hold`. The park is the engine's
    /// own state, so it is read off the shared status rather than
    /// `self.playing`, which the pump has already set false for it.
    pub(super) fn arms_end_hold(&self) -> bool {
        let parked = self
            .engine
            .as_ref()
            .and_then(|engine| engine.shared().status.lock().ok().map(|s| s.at_live_edge))
            .unwrap_or(false);
        pb::arms_end_hold(self.playing, parked, self.recording)
    }

    pub(super) fn viewport(&self) -> Option<Viewport> {
        let snapshot = self.snapshot.as_ref()?;
        Some(self.viewport_of(snapshot))
    }

    /// The drawn live edge (t260928-b301, t260930-1904): what the last render
    /// advanced `edge_clock` to, so every reader between two renders --
    /// the bar, the total clock, the lag, a pointer's ratio-to-time -- sees
    /// the same edge. The real edge when nothing has been drawn yet.
    pub(super) fn apparent_live_edge(&self, snapshot: &TimelineSnapshot) -> i64 {
        self.edge_clock.shown().unwrap_or(snapshot.live_edge_100ns)
    }

    /// Moves the drawn live edge on to now (t260930-1904). Once per render,
    /// before anything reads it. A scrub in flight holds it (user decision c).
    pub(super) fn advance_live_edge(&mut self) {
        let Some(snapshot) = self.snapshot.as_ref() else {
            return;
        };
        let real = snapshot.live_edge_100ns;
        let target = tl::live_edge_target(
            real,
            self.recording,
            self.live_grew_at.map(|at| at.elapsed()),
            self.loaded_at.map(|at| at.elapsed()),
        );
        self.edge_clock
            .advance(Instant::now(), real, target, self.recording, self.dragging);
    }

    /// What the track maps onto 0..100%. Unzoomed, its end is the *apparent*
    /// live edge, so a recording's bar grows every drain instead of rescaling
    /// in 2 s steps (t260928-b301). Every reader -- the render, the per-frame
    /// pump and the pointer's ratio-to-time -- goes through here, so what is
    /// drawn and where a click lands cannot disagree. Seeks still clamp to the
    /// real snapshot.
    pub(super) fn viewport_of(&self, snapshot: &TimelineSnapshot) -> Viewport {
        self.viewport_ending_at(snapshot, self.apparent_live_edge(snapshot))
    }

    /// `viewport_of` with the apparent edge already read. The render reads
    /// the clock once for both: two reads put the drawn edge a few µs past the
    /// track's end, and `contains` then called the live edge off the track --
    /// no edge line, no ruler LIVE, unzoomed (t260928-ac18).
    pub(super) fn viewport_ending_at(&self, snapshot: &TimelineSnapshot, edge: i64) -> Viewport {
        self.zoom.unwrap_or_else(|| {
            let start = snapshot.start_100ns();
            Viewport {
                start_100ns: start,
                duration_100ns: (edge - start).max(1),
            }
        })
    }

    /// `seekClamped`: classify, move the UI's own position immediately (so
    /// the playhead never lags the pointer), and hand the engine the seek.
    /// `precise` is false while scrubbing -- a coarse seek shows the target
    /// segment's nearest clean point in a few ms, and the exact frame is
    /// decoded once the drag settles.
    ///
    /// A seek that leaves the selection pauses there (task3380) -- unless
    /// repeat is on and playback is running, in which case it is turned round
    /// to the selection's start and playback carries on (task3570). Every route
    /// that moves the playhead comes through here -- the track, the stage's
    /// side taps, the keys, the markers, the range panel's jumps -- so the rule
    /// is written once, the same way task3150 put the transport's own flip in
    /// `toggle_play`.
    ///
    /// One exception, and it is about the gesture rather than the route: while
    /// the centre hold's x2 skim is in flight (`Review::skimming`) the stop is
    /// off. A key tapped during a hold arrives here like any other seek, and
    /// stopping on it would end the skim mid-gesture -- which is precisely the
    /// way out of the selection the user reached for (task3280 案(A),
    /// task3580). Repeat's wrap is not exempt from *that* and still applies.
    ///
    /// A second, narrower one since task3650, and it is the wrap rather than
    /// the stop: while the track or a range edge is being dragged
    /// (`dragging || range_dragging`) no wrap is attempted, and such a seek
    /// takes the stop instead. Without it every pointer move of a drag past the
    /// end wrapped, pinning the playhead to the selection's start for the whole
    /// gesture. It catches a plain track click too -- a press cannot be told
    /// apart from the start of a drag -- which the user accepted on 2026-09-09
    /// in exchange; the range panel, keys and markers set no flag and wrap as
    /// task3570 left them.
    pub(super) fn seek_clamped(&mut self, target_100ns: i64, precise: bool) -> pb::SeekOutcome {
        // t260926-cc7e: every seek the user makes comes through here, so this
        // is where following ends. The mode's own landing does not come
        // through here (`follow_live`), and the two automatic callers -- the
        // stop skip, and `poll_range_guard_with`, which does not run while the
        // mode holds the selection off -- put it back or never reach it.
        self.follow = self.follow.apply(pb::FollowEvent::UserSeek);
        // task4230 release (2): the user moved the playhead, so 「今ここを
        // 終端にした」 is over. Only the latch -- which plan the seek takes
        // (`Go` / `PauseFirst`, 55a69411's live end) is untouched.
        self.hold_at_end = None;
        // A precise seek from anywhere -- timeline click, frame step, the
        // settle itself -- supersedes a pending settle confirm (task2540):
        // two precise decodes for one landing would double the cost.
        if precise {
            self.key_settle_timer.stop();
        }
        // Read before the borrow below, though both are shared and would
        // coexist: the value belongs to the gesture, not to the snapshot.
        let skimming = self.skimming();
        // task3650: the same set `poll_range_guard_with` keeps out of
        // `owns_position`. While the track or a range edge is being dragged the
        // wrap is not attempted -- `on_track_moved` seeks on every pointer
        // move, so wrapping there pinned the playhead to the selection's start
        // for the whole gesture. Read beside `skimming` because both belong to
        // the gesture rather than to the snapshot; `seek_plan` lets the skim
        // win where they meet.
        let gesturing = self.dragging || self.range_dragging;
        // Scoped so the `snapshot` borrow is finished with before the sends
        // below take `&mut self`.
        let (outcome, pause_first) = {
            let Some(snapshot) = self.snapshot.as_ref() else {
                return pb::SeekOutcome::Buffering;
            };
            let outcome = pb::classify_seek(snapshot, target_100ns);
            let Some(position) = outcome.position() else {
                return outcome;
            };
            // Out of the selection while playing: settled here, before the
            // engine is told anything (task3380). The order is the whole point
            // -- the `Seek` below reaches a worker that decodes and sounds
            // wherever it lands, so a `Pause` sent afterwards (or a
            // `range_guard` verdict a frame later, which is what this replaces)
            // is already too late to keep it silent. task3570's wrap is folded
            // into the same decision for exactly that reason: replacing the
            // target before it goes out leaves the frame outside the selection
            // undecoded just as the stop did, so the wrap costs no flash.
            //
            // Asked about `position` rather than `target_100ns`: the clamp and
            // the gap snap can move the landing, and it is where the playhead
            // ends up that decides whether it left the selection.
            //
            // The wrap resolves its own landing by handing `classify_seek` to
            // `pb::seek_plan` as a closure, rather than by calling
            // `self.seek_clamped(start, precise)` again. That re-entry has no
            // bound: a gap covering the whole selection snaps `start` past the
            // end, which is `WrapToStart` all over again, and it would recurse
            // until the stack ran out. The closure is `FnOnce`, run at most
            // once, and `seek_plan` answers `PauseFirst` when the hop lands
            // outside too and it is not a skim (task3660: a skim gets `Go`
            // there instead) -- so the depth is one by construction either way.
            //
            // `skimming` is the real value since task3580, not the `false` the
            // 3570 shape pinned here: the hold sends no seek itself, but a key
            // pressed while it is held sends one through this very call, and
            // stopping it would end the skim mid-gesture.
            // `>>` / End while a recording plays: keep playing and let the
            // engine's live park follow it. Not during a drag -- the gesture
            // keeps task3650's stop -- and not once the recording is closed,
            // where the end is the end (and the stop skip's paused landing).
            let plan = if self.recording
                && !gesturing
                && pb::lands_on_live_end(
                    self.repeat,
                    self.range,
                    snapshot.live_edge_100ns,
                    position,
                ) {
                pb::SeekPlan::Go
            } else {
                pb::seek_plan(
                    self.playing,
                    skimming,
                    gesturing,
                    self.repeat,
                    self.range,
                    position,
                    |start| pb::classify_seek(snapshot, start).position(),
                )
            };
            match plan {
                pb::SeekPlan::Go => (outcome, false),
                pb::SeekPlan::PauseFirst => (outcome, true),
                // Reclassified at the point actually landed on, so the stage's
                // feedback (`fold_hold` / `fold_tap`) does not claim a place
                // the playhead never went -- a wrap over a gap is still a
                // `Gap`, and 録画のない区間を飛ばしました is still true of it.
                pb::SeekPlan::WrapTo(_) => match self.range {
                    Some((start, _)) => (pb::classify_seek(snapshot, start), false),
                    // Unreachable: a wrap verdict implies a selection.
                    None => (outcome, true),
                },
            }
        };
        let Some(position) = outcome.position() else {
            return outcome;
        };
        // Through `send`, never straight into the shared state, so the
        // play/pause latch is armed (task3290) and the pump does not write
        // `playing` back to true until the engine has taken the pause.
        if pause_first {
            self.playing = false;
            self.send(PlaybackCommand::Pause {
                reason: "seek_pause_first",
            });
        }
        self.position = position;
        self.last_seek_sent = Some(position);
        self.restore_live_mute();
        self.send(PlaybackCommand::Seek {
            target_100ns: position,
            precise,
        });
        outcome
    }

    /// The one place the transport flips (task3150). The bar's button,
    /// Space / k, the stage's centre tap and the double press that undoes it
    /// each used to write `playing = !playing` plus the command themselves, so
    /// the selection's rule below would have had to be written four times --
    /// and forgetting one would leave that route ignoring the selection.
    ///
    /// Starting playback from outside the selection starts it at the
    /// selection's start. Otherwise the button parked on the end by
    /// `RangeGuard::PauseAtEnd` would look dead: the next frame's guard would
    /// simply pause it again where it stands.
    pub(super) fn toggle_play(&mut self) {
        self.playing = !self.playing;
        if !self.playing {
            self.follow = self.follow.apply(pb::FollowEvent::UserPause);
            self.send(PlaybackCommand::Pause { reason: "toggle" });
            return;
        }
        // t260926-cc7e: a follow the user paused carries on from where it
        // stands -- the selection's jump is off until the next seek.
        if let Some((start, _)) = self.range.filter(|_| !self.follow.range_exempt) {
            // Any verdict but `None` means the playhead is outside the
            // selection, whichever side and whether or not repeat is on.
            // Never a skim: the centre hold does not come through here at all
            // (task650/1010 write the transport themselves), and the double
            // press that does clears `gesture` before calling this.
            if pb::range_guard(true, false, false, self.repeat, self.range, self.position)
                != pb::RangeGuard::None
            {
                self.seek_clamped(start, true);
            }
        }
        self.send(PlaybackCommand::Play);
    }

    /// Joins Sync Mode (t260926-cc7e): lands where a live park would resume
    /// from and plays on at x1. The chip, End while recording, the cover
    /// gate's resume and the automatic rejoin all come here.
    ///
    /// Not through `seek_clamped`: that is the user's seek, which leaves the
    /// mode, and its `seek_plan` would stop the landing outside a hand-dragged
    /// selection -- the one thing following promises not to do until the next
    /// seek (`Follow::range_exempt`). The start-of-recording mute is kept: the
    /// loopback it guards against is exactly what following plays back into.
    ///
    /// Sends an ordinary `Seek` to a target this side computed, so
    /// `last_seek_sent` matches the engine's echo and `owns_position` covers
    /// the flight -- which is what keeps the rejoin to one seek per lag.
    pub(super) fn follow_live(&mut self, reason: &'static str) -> bool {
        let Some(target) = self.snapshot.as_ref().and_then(|snapshot| {
            livia::playback::live_follow_landing_100ns(
                &snapshot.segments,
                snapshot.live_edge_100ns,
                self.live_grew_at
                    .or(self.loaded_at)
                    .map_or(std::time::Duration::ZERO, |at| at.elapsed()),
            )
        }) else {
            return false;
        };
        tracing::info!(
            target: FOLLOW_LOG,
            reason,
            from_100ns = self.position,
            target_100ns = target,
            "follow_join"
        );
        self.follow = self.follow.apply(pb::FollowEvent::Join);
        self.hold_at_end = None;
        self.key_settle_timer.stop();
        self.position = target;
        self.last_seek_sent = Some(target);
        self.send(PlaybackCommand::Seek {
            target_100ns: target,
            precise: true,
        });
        if self.rate != 1.0 {
            self.rate = 1.0;
            self.send(PlaybackCommand::SetRate(1.0));
        }
        self.playing = true;
        self.send(PlaybackCommand::Play);
        true
    }

    /// Whether the stage gesture in flight is the centre hold's x2 skim
    /// (task3280), which the selection's rules are exempt from.
    ///
    /// One copy of the expression, because task3580 gave it a second reader.
    /// The exemption started life inside `poll_range_guard_with` -- the guard's
    /// own polling -- and now `seek_clamped` has to ask the same question about
    /// a seek's landing, since a key pressed *during* a hold does come through
    /// there. Two inline copies of `gesture -> stage_skimming` could drift into
    /// disagreeing about what a skim is, which is exactly the split task3280's
    /// 案(A) ("the exemption is about the gesture") rules out.
    pub(super) fn skimming(&self) -> bool {
        self.gesture
            .as_ref()
            .is_some_and(|g| pb::stage_skimming(g.zone, g.held, g.repause))
    }

    /// Arms the one-shot settle confirm (task2540/2580): after a flurry of
    /// coarse seeks -- key taps, stage side-taps, a hold's scan -- one precise
    /// decode lands the exact frame once `KEY_SETTLE_MS` passes with no new
    /// arm. Paused only: playing, the engine is already walking toward the
    /// target dropping late frames, and pays the precise cost itself
    /// (task2540 Steps 5). Any precise seek from anywhere cancels the pending
    /// confirm (`seek_clamped`).
    pub(super) fn arm_settle(&mut self, rc: &Rc<std::cell::RefCell<Review>>) {
        let rc = rc.clone();
        self.key_settle_timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(pb::KEY_SETTLE_MS),
            move || {
                let mut review = rc.borrow_mut();
                if review.playing {
                    return;
                }
                let position = review.position;
                review.seek_clamped(position, true);
            },
        );
    }

    /// Hands the volume back after the start-of-recording mute. The moment the
    /// user moves the playhead they are watching something they chose rather
    /// than the live edge that is being recorded, so the reason to be silent is
    /// gone. A no-op unless a live load armed it.
    pub(super) fn restore_live_mute(&mut self) {
        let Some((percent, muted)) = self.live_mute.take() else {
            return;
        };
        self.send(PlaybackCommand::SetVolume { percent, muted });
    }

    /// The one gate every transport command goes through -- which is why the
    /// play/pause latch is armed here rather than at the dozen call sites that
    /// flip `playing` (task3290). `toggle_play`, the band drag's down/up, the
    /// centre hold's skim, the range guard's `PauseAtEnd` / `WrapToStart`, the
    /// offscreen pause and the frame step all send through this, and one of
    /// them forgetting to arm the latch would flicker again on that route
    /// alone.
    pub(super) fn send(&mut self, command: PlaybackCommand) {
        match &command {
            PlaybackCommand::Play => self.play_intent = Some((true, Instant::now())),
            PlaybackCommand::Pause { .. } => self.play_intent = Some((false, Instant::now())),
            _ => {}
        }
        if let Some(engine) = &self.engine {
            engine.send(command);
        }
    }
}

/// The log target of Sync Mode's own lines (t260926-cc7e): a join, an arrival,
/// a rejoin. Named apart from `task327f_stall` so the two read separately.
pub(super) const FOLLOW_LOG: &str = "tcc7e_follow";

/// Resets the review screen onto a freshly loaded session's manifest and
/// starts a fresh playback engine for it. Dropping the old engine joins its
/// thread, which releases its lease -- so prune is never left blocked by a
/// session the user moved on from.
///
/// `live` is a session that is still being recorded (task162): it opens
/// following the live edge (t260926-cc7e), playing and muted, watching what is
/// happening now rather than autoplaying from a head the user has already
/// seen. The park task162 opened into is gone: it sat still and never
/// followed.
#[allow(clippy::too_many_arguments)]
pub(super) fn load_review(
    review: &mut Review,
    manifest: &ring_buffer::SessionManifest,
    folded_at: Option<Instant>,
    controller: &CaptureController,
    settings: &AppSettings,
    ui: &AppWindow,
    live: bool,
    fresh: bool,
) {
    let snapshot = TimelineSnapshot::from_manifest(manifest);
    // A recording the user started seconds ago opens at the head and plays --
    // for a session this young that is the live picture, and a seek is what
    // left the stage black. Every other live load lands where following lands
    // (t260926-cc7e), and plays from there. Muted either way: see `live_mute`.
    // t260929-e171: phased on the recorder's stamp of its last fold, so a
    // history LIVE row lands where following would at once -- the one
    // re-landing on the first growth (t260927-6960) is gone. No stamp (not
    // recording, nothing folded yet in this process) is phase 0.
    let folded_at = folded_at.filter(|_| live);
    let start = if live && !fresh {
        livia::playback::live_follow_landing_100ns(
            &snapshot.segments,
            snapshot.live_edge_100ns,
            folded_at.map_or(std::time::Duration::ZERO, |at| at.elapsed()),
        )
        .unwrap_or_else(|| snapshot.start_100ns())
    } else {
        snapshot.start_100ns()
    };
    review.markers = manifest.markers.clone();
    // The whole live range, with the 全体に戻す chip echoing it (task240).
    // React pre-echoed 直近1分 here while actually selecting everything, so the
    // panel and the chips disagreed from the first frame.
    review.range = Some(snapshot.next_range(None, true, tl::min_frame_100ns()));
    review.quick = 0;
    review.zoom = None;
    review.position = start;
    // Autoplay (task149): from the head for a closed session, from the
    // follow landing for a live one (t260926-cc7e).
    review.playing = true;
    review.live_grew_at = None;
    review.loaded_at = Some(folded_at.unwrap_or_else(Instant::now));
    review.edge_clock = tl::LiveEdgeClock::default();
    // t260928-faac: the 1 s capture poll is the only other writer, so without
    // this a fresh live load drew the real edge until the poll caught up and
    // then jumped by up to a second. `live` is the same answer, known now.
    review.recording = live;
    review.retention_minutes = manifest.retention_minutes;
    review.follow = if live {
        pb::Follow::default().apply(pb::FollowEvent::Join)
    } else {
        pb::Follow::default()
    };
    review.show_remaining = false;
    review.repeat = false;
    // Speed is deliberately not persisted (task101): a new session starts at
    // 1x, so a forgotten 0.25x can't silently follow the user around.
    review.rate = 1.0;
    // Refilled from the target's saved levels once the snapshot is in place
    // (t261003-83d3), never the way the last session was left.
    review.track_volumes.clear();
    review.hover = None;
    review.status = None;
    review.dragging = false;
    // A session change mid-gesture (task3160): the band drag's state belongs
    // to the range it was editing, latch included.
    review.range_dragging = false;
    review.resume_after_range_drag = false;
    review.hold_at_end = None;
    // The previous session's engine is about to be dropped, so a command it
    // never ran must not outrank the new one's status (task3290). Cleared
    // *here*, ahead of the autoplay `Play` at the end of this function, which
    // arms the latch legitimately.
    review.play_intent = None;
    // The next session's first frame has not arrived, whatever the last one
    // left on screen -- this is what puts the loading ring up (task239). The
    // VM flag goes with it: a ring over the *previous* session's last frame
    // would read as that picture being the one still loading.
    review.stage_has_frame = false;
    ui.global::<ReviewVm>().set_stage_has_frame(false);
    review.thumbnails.clear();
    livia::insight_images!(Review, 0);
    review.thumbnails_requested.clear();
    // The `cached_*` track models are deliberately **not** reset here. They are
    // not state belonging to the session; they are a record of what was last
    // pushed into the slint models, and `render_track_spans`/`render_markers`
    // push only when what they computed differs from that record. Emptying the
    // record without emptying the model it describes makes the two disagree,
    // and the next render then skips the push it most needs to make: loading a
    // session with no markers computed `[]`, compared it against a cache this
    // line had just emptied to `[]`, found them equal, and left the *previous*
    // session's marker dots on the bar. Same for a session with no gaps, and
    // for one still recording its first segment.
    //
    // Leaving the record alone is what makes the comparison honest -- `[]`
    // against the previous session's spans differs, so the model is emptied.
    // `cached_marker_rows` was never cleared here, which is why the marker
    // *panel* emptied correctly while the bar did not (2026-09-22).
    review.gesture = None;
    // The double-click pairing belongs to the session that was on screen, and
    // so does the position it would restore (task3270).
    review.last_center_tap = None;
    // Nothing to draw until the first segment closes, and a black stage with no
    // word on it is indistinguishable from a broken one -- which is exactly how
    // it read. The buffering line already exists for seeks that outrun the
    // buffer; `follow_live_edge` takes it back down.
    review.stage_feedback =
        (fresh && snapshot.segments.is_empty()).then_some(pb::StageFeedback::Buffering);
    // Every live load since t260926-cc7e, not only the one after a start: a
    // history LIVE row now opens following too, into the same loopback.
    review.live_mute = live.then_some((settings.volume_percent, settings.muted));
    review.hint_counted = false;
    review.engine_playing = false;
    let weak = ui.as_weak();
    // One outstanding wake-up at a time (task200 follow-up). Every published
    // frame used to post its own `invoke_from_event_loop`, and each of those
    // costs the UI thread a full frame copy plus a re-render. That was
    // affordable while the engine only managed ~15fps; once it returned every
    // recorded frame the event loop had no time left for input, and the app
    // went unresponsive with the picture standing still.
    //
    // The mailbox is latest-wins, so a skipped wake-up loses nothing -- the
    // next one still shows the newest frame. This makes the UI's own speed the
    // rate limiter: it is rung again only once it has drained the last ring.
    let frame_ring_pending = Arc::new(AtomicBool::new(false));
    review.last_seek_sent = None;
    // The old engine goes first (t260930-8bce): its worker parks the GPU scaler
    // as it exits, and the new one's poster seek is what takes it. The other way
    // round the new engine found nothing parked and made a scaler -- and a
    // shared ring -- of its own, every recording.
    review.engine = None;
    review.engine = Some(PlaybackEngine::start(
        controller.clone(),
        snapshot.clone(),
        start,
        settings.volume_percent,
        settings.muted || live,
        // How big the stage draws the picture (task990), on the engine before
        // its worker decodes the poster rather than after (t260915-c1ca). The
        // stage is not always laid out yet: for the first session of a process
        // this is `None`, because `ReviewPane` -- whose `init` rings
        // `stage-size-changed` -- only exists once `session-loaded` is set, and
        // the caller sets that after this returns. That one poster goes out at
        // full resolution and the stage's own ring, a frame later, brings the
        // frames after it down to size. From the second session on,
        // `stage_target` has outlived `unload_review` and is right from birth.
        review.stage_target,
        Box::new(move || {
            if frame_ring_pending.swap(true, Ordering::AcqRel) {
                return;
            }
            let weak = weak.clone();
            let pending = frame_ring_pending.clone();
            let _ = slint::invoke_from_event_loop(move || {
                pending.store(false, Ordering::Release);
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ReviewVm>().invoke_frame_ready();
                }
            });
        }),
    ));
    if let Some(engine) = review.engine.as_ref() {
        // t260913-3842: how often the stage actually refreshes, so the
        // engine stops converting the frames between two refreshes. Read here
        // rather than in the resize handler because it does not change with
        // the stage's size -- `StageGate` reads the same number once, in
        // `review_stage::wire_stage`.
        engine
            .shared()
            .set_display_refresh_hz(super::desktop::display_refresh_hz());
    }
    review.snapshot = Some(snapshot);
    // The saved per-track levels, before `Play` so the first sound has them.
    super::review_volume::load(review, manifest, controller, settings, ui);
    // After the engine is in place: the command channel is unbounded, so this
    // cannot block, and the engine picks it up as soon as its worker is up.
    review.send(PlaybackCommand::Play);
}

/// The session the review screen has loaded, which a reclaim sweep must spare
/// (task3970). `unload_review` clears `snapshot`, so an unloaded pane names none.
pub(super) fn loaded_session_id(review: &Review) -> Option<String> {
    review
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.session_id.clone())
}

/// Puts the review screen back to "nothing loaded" and lets the engine go.
///
/// Dropping the engine joins its worker thread, and the worker's `Drop`
/// releases the review lease -- which is what a discard of the session on
/// screen refuses on (`CaptureController::discard_session`'s `has_leases`).
/// So this is called *before* the discard command is sent, on the same thread,
/// and the lease is gone by the time the worker reaches the file (task1520).
///
/// Not `*review = Review::default()`: `recording`, `stage_target`,
/// `export_status` and the feedback timers outlive any one session.
pub(super) fn unload_review(review: &mut Review, ui: &AppWindow) {
    review.engine = None;
    review.last_seek_sent = None;
    review.snapshot = None;
    review.markers.clear();
    review.visible_markers.clear();
    review.range = None;
    review.zoom = None;
    review.position = 0;
    review.playing = false;
    review.engine_playing = false;
    review.play_intent = None;
    review.follow = pb::Follow::default();
    review.live_grew_at = None;
    review.loaded_at = None;
    review.edge_clock = tl::LiveEdgeClock::default();
    review.range_end_on_live = false;
    review.dragging = false;
    review.range_dragging = false;
    review.resume_after_range_drag = false;
    review.hold_at_end = None;
    review.hover = None;
    review.status = None;
    review.last_frame = None;
    review.track_volumes.clear();
    super::review_volume::unload(review, ui);
    review.thumbnails.clear();
    livia::insight_images!(Review, 0);
    review.thumbnails_requested.clear();
    // Not reset, for the reason `load_review` spells out: these describe the
    // slint models, which nothing here empties. `render_review` returns early
    // without a snapshot, so the models keep what they hold until the next
    // session loads -- and that load's comparison is only correct if the record
    // still says what is actually on screen.
    review.stage_feedback = None;
    // Same as the load path (task3270): nothing loaded means no pending pair
    // and no position worth putting back.
    review.last_center_tap = None;
    review.stage_has_frame = false;
    let vm = ui.global::<ReviewVm>();
    vm.set_stage_has_frame(false);
    // Nothing loaded is also "no executable to register", and the row is only
    // rewritten by `render_review`, which returns early without a snapshot.
    // The resolved path goes with the name it was resolved for (task3670): a
    // stale one left here would answer for the next session loaded.
    review.autorec_path = None;
    review.title_icon = None;
    ui.set_loaded_icon(Image::default());
    vm.set_autorec_visible(false);
    vm.set_autorec_executable("".into());
    vm.set_autorec_image_path("".into());
    // Back to the empty stage: the pane kept drawing a session with no engine.
    ui.set_session_loaded(false);
}

/// Re-derives the toggle's on/off and its second line from freshly committed
/// lists (round13 §1). The row's own handler gets this for free from the render
/// that follows it; the settings screen's × does not, so it calls this -- and
/// since task3670 so do the three folder handlers beside it, because a folder
/// rule now reaches this row.
///
/// Reads the executable back off the VM rather than the review state: the
/// caller with the list in hand is on the settings screen and has no borrow of
/// the review, and the name was stashed by the render that drew the row. The
/// resolved image path is stashed next to it for the same reason.
/// The `targetAudio` key of the session on the review screen (t261002-577c):
/// its executable, or the key every screen recording shares when it has none.
/// The export and the clip read the saved levels through it (t261003-b713).
pub(super) fn reviewed_audio_key(review: &Review) -> Option<String> {
    let executable = review.snapshot.as_ref()?.target_executable.as_deref();
    livia::settings::target_audio_key(executable, executable.is_none())
}

/// Changes one target's added sounds and volumes (t261002-577c) -- the one
/// path the control command and the volume tab both take: save the edit, then
/// carry it to every recording of that target already running. Returns the
/// edit's own answer (`None` when the settings lock was poisoned and nothing
/// ran) and how many recordings it reached.
pub(super) fn change_target_audio<T>(
    settings: &Mutex<AppSettings>,
    controller: &CaptureController,
    key: &str,
    edit: impl FnOnce(&mut livia::settings::TargetAudio) -> T,
) -> (Option<T>, usize) {
    let mut answer = None;
    let next = super::settings_page::commit_settings(settings, |current| {
        answer = Some(edit(
            current.target_audio.entry(key.to_owned()).or_default(),
        ));
    });
    let reached = controller.apply_target_audio(key, next.target_audio.get(key));
    (answer, reached)
}

pub(super) fn render_autorec(
    ui: &AppWindow,
    registered: &[livia::settings::AutoCaptureApp],
    folders: &[livia::settings::AutoCaptureFolder],
) {
    let vm = ui.global::<ReviewVm>();
    let executable = vm.get_autorec_executable();
    if executable.is_empty() {
        // No row on screen: nothing loaded, or a session naming no executable.
        return;
    }
    let mut path = vm.get_autorec_image_path().to_string();
    // `render_review` resolves nothing while no folder rule is registered, so
    // the click that adds the *first* rule arrives here with an empty stash --
    // and would read as "not covered" no matter what the new rule says. One
    // Toolhelp32 walk per click on the settings screen is affordable where one
    // per render is not, and with no rule registered it still never happens.
    if path.is_empty() && !folders.is_empty() {
        path = resolve_image_path(&executable).unwrap_or_default();
        vm.set_autorec_image_path(path.as_str().into());
    }
    let toggle = auto_capture::review_toggle(
        tr_locale(),
        Some(&executable),
        registered,
        folders,
        auto_capture::FolderGuard::machine(),
        image_path_arg(&path),
    );
    vm.set_autorec_on(toggle.checked);
    vm.set_autorec_body(toggle.body.into());
}

/// The stashed path as `review_toggle` wants it: empty means "not resolved",
/// which is the same answer as "nothing by that name is running".
fn image_path_arg(path: &str) -> Option<&std::path::Path> {
    (!path.is_empty()).then(|| std::path::Path::new(path))
}

/// The full image path of a running process of `executable`, the resolution
/// `app_meta::resolve_entry` already uses for the settings row's metadata.
/// `None` when nothing by that name is running -- a session opened long after
/// the app exited then answers from the executable list alone, which is the
/// limitation task3670 accepted rather than writing the path into the
/// container.
fn resolve_image_path(executable: &str) -> Option<String> {
    capture_targets::first_process_id_for_executable(executable)
        .and_then(capture_targets::executable_path_for_process)
}

/// What `autorec_image_path` resolved, and the inputs it is the answer for.
pub(super) struct AutorecPath {
    session: String,
    executable: String,
    folders: Vec<livia::settings::AutoCaptureFolder>,
    /// `None` is a real answer -- nothing by that name is running -- and is
    /// cached like any other, so a session of an app that has exited does not
    /// walk the process list on every render.
    path: Option<String>,
}

/// The recorded app's icon for the title bar. The image path the container
/// recorded at capture start first (2026-09-20); for a recording made before
/// that, the registered auto-capture entry's stored path, then a running
/// process of the same name. `None` for a monitor recording, and for an old
/// recording of an app that is neither registered nor running.
fn session_app_icon(
    snapshot: &TimelineSnapshot,
    registered: &[livia::settings::AutoCaptureApp],
) -> Option<Image> {
    let executable = snapshot.target_executable.as_deref()?.trim();
    if executable.is_empty() {
        return None;
    }
    let path = snapshot
        .target_executable_path
        .clone()
        .filter(|path| std::path::Path::new(path).is_file())
        .or_else(|| {
            registered
                .iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(executable))
                .and_then(|entry| entry.path.clone())
        })
        .or_else(|| resolve_image_path(executable))?;
    let (width, height, rgba) = super::app_meta::file_icon_rgba(&path)?;
    super::image_from_rgba(width, height, &rgba)
}

/// The running image path behind the recorded executable, for the review
/// toggle's folder half (task3670).
///
/// Stashed rather than resolved per render: `first_process_id_for_executable`
/// takes a Toolhelp32 snapshot, and `render_review` runs on every drain tick
/// that touched the pane. Re-resolved only when one of the three things the
/// answer depends on changes -- the session (a different recording, so a
/// different executable), the executable itself, or the folder list (a rule
/// added or removed on the settings screen).
///
/// Not resolved at all while no folder rule is registered: nothing would read
/// the answer. Same gate task3600 put on the poll's `resolve_path`, and what
/// keeps this task off the cost of every install that never uses folders.
fn autorec_image_path(
    stash: &mut Option<AutorecPath>,
    session: &str,
    executable: Option<&str>,
    folders: &[livia::settings::AutoCaptureFolder],
) -> Option<String> {
    let executable = executable.unwrap_or_default().trim();
    if executable.is_empty() || folders.is_empty() {
        *stash = None;
        return None;
    }
    if let Some(stashed) = stash.as_ref() {
        if stashed.session == session
            && stashed.executable == executable
            && stashed.folders == folders
        {
            return stashed.path.clone();
        }
    }
    let path = resolve_image_path(executable);
    *stash = Some(AutorecPath {
        session: session.to_owned(),
        executable: executable.to_owned(),
        folders: folders.to_vec(),
        path: path.clone(),
    });
    path
}

/// Pushes what a still-recording session has finalized since the last tick into
/// the engine, and grows the loaded timeline with it (task162). Called from the
/// 100ms drain; reports whether anything moved, which is what re-renders the
/// track and the total-time clock.
///
/// Asked for **by session id** (task2030). It used to read `controller.
/// timeline()` -- the last-loaded-or-written manifest -- and drop the tick when
/// the id did not match. With two recordings running, each one's index writer
/// overwrites that slot with its own on every finalized segment, so the stage
/// was fed only on the ticks that happened to land on the right session. Naming
/// the session removes the race rather than filtering it, and the id gate it
/// used to need is the lookup itself: a different recording's segments can no
/// longer be handed to a session loaded for review.
/// Cheap gate first: a closed session that will never grow is the common case
/// ten times a second, so `fetch` -- the manifest's full clone -- runs only once
/// the `(count, last end)` extent says it moved (t260913-784a: the clone used to
/// come before this gate).
fn manifest_if_grown(
    current: &TimelineSnapshot,
    extent: Option<(usize, Option<i64>)>,
    fetch: impl FnOnce() -> Option<ring_buffer::SessionManifest>,
) -> Option<ring_buffer::SessionManifest> {
    let (count, last_end) = extent?;
    let grew =
        count > current.segments.len() || last_end.is_some_and(|end| end > current.live_edge_100ns);
    if grew {
        fetch()
    } else {
        None
    }
}

pub(super) fn follow_live_edge(review: &mut Review, controller: &CaptureController) -> bool {
    let Some(current) = review.snapshot.as_ref() else {
        return false;
    };
    let id = &current.session_id;
    let Some(manifest) = manifest_if_grown(current, controller.timeline_extent(id), || {
        controller.timeline_for(id)
    }) else {
        return false;
    };
    let latest = TimelineSnapshot::from_manifest(&manifest);
    let Some(extension) = tl::playlist_extension(current, &latest) else {
        return false;
    };
    review.send(PlaybackCommand::ExtendPlaylist {
        segments: extension.segments,
        live_edge_100ns: extension.live_edge_100ns,
    });
    review.live_grew_at = Some(Instant::now());
    // Markers land in the manifest on the same cadence (the review hotkey adds
    // one on its way past), and the track rebuilds from these two.
    review.markers = manifest.markers.clone();
    // Task390: this used to rebuild the range only when the load had found an
    // empty timeline -- the `(0, -1)` case, which nothing in the export panel
    // or the track band can say anything with. But a live session loaded off a
    // timeline that already had one segment then kept that first length
    // forever: 全体に戻す stayed lit while the "whole" it echoed went stale,
    // and since the selection is also what clamps seeking, the right of the
    // seekbar and `End` were pinned inside those first seconds.
    //
    // Which preset is showing is what decides, and `range_after_growth` says
    // so: 全体 and 直近N follow the live edge, hand-dragged edges do not. The
    // empty-timeline case is inside that -- a load leaves `quick` at 0.
    if let Some(range) =
        tl::range_after_growth(&latest, review.quick, review.range, tl::min_frame_100ns())
    {
        review.range = Some(range);
    }
    review.snapshot = Some(latest);
    // The first segment answers the 読み込み中 the load put up. Only that one:
    // a gesture's own feedback is not this function's to clear.
    if matches!(review.stage_feedback, Some(pb::StageFeedback::Buffering)) {
        review.stage_feedback = None;
    }
    true
}

/// The transport's two clock strings. `force_hours` keeps the position column
/// aligned with an over-an-hour total, the way YouTube prints `0:05:00 /
/// 1:20:00` rather than `5:00 / 1:20:00` (task149).
/// The third field is `force_hours`: the .slint side sizes the clock off the
/// widest string it can hold, and that width is allowed to change at the hour
/// crossing but not on every second (task660).
///
/// Timecodes with tenths since t260927-77b8 (DS Timecode); the remaining time
/// is a negative timecode, so it wears the same U+2212 as the lag.
pub(super) fn clock_texts(review: &Review, snapshot: &TimelineSnapshot) -> (String, String) {
    // The apparent edge (t260928-b301): the total ticks on with the bar
    // instead of jumping 2 s per finalized segment.
    let total = (review.apparent_live_edge(snapshot) - snapshot.start_100ns()).max(0);
    let elapsed = review.position - snapshot.start_100ns();
    // Hours only once there are some (DS Timecode `hours='auto'`,
    // t260928-ac18): the column's width is `position-measure`'s now.
    let position = if review.show_remaining {
        tl::format_timecode(-(total - elapsed).max(0), false)
    } else {
        tl::format_timecode(elapsed.max(0), false)
    };
    (position, tl::format_timecode(total, false))
}

/// Writes one timecode as its whole part and its faint `.t` tail, each only
/// when it changed (task1600: a slint write dirties unconditionally).
fn set_timecode(
    text: &str,
    (get_whole, set_whole): (impl Fn() -> SharedString, impl Fn(SharedString)),
    (get_frac, set_frac): (impl Fn() -> SharedString, impl Fn(SharedString)),
) {
    let (whole, frac) = tl::split_timecode(text);
    if get_whole().as_str() != whole {
        set_whole(whole.into());
    }
    if get_frac().as_str() != frac {
        set_frac(frac.into());
    }
}

/// The transport's clock, both halves of both timecodes.
fn set_clock(vm: &ReviewVm, position: &str, total: &str) {
    set_timecode(
        position,
        (|| vm.get_position_text(), |v| vm.set_position_text(v)),
        (|| vm.get_position_frac(), |v| vm.set_position_frac(v)),
    );
    set_timecode(
        total,
        (|| vm.get_total_text(), |v| vm.set_total_text(v)),
        (|| vm.get_total_frac(), |v| vm.set_total_frac(v)),
    );
}

/// Pulls the engine's latest frame + status onto the UI. Only scalars are
/// written here: rebuilding the track models per frame would cost more than
/// the decode. Reports whether playback just started, which is the moment the
/// first-run hint keys off (React watched the same transition).
pub(super) fn pump_playback(ui: &AppWindow, review: &mut Review) -> bool {
    // The UI thread's share of playback: whatever this costs is frames the
    // event loop is not drawing (task205).
    livia::insight_scope!("ui_pump_playback");
    let Some(engine) = review.engine.as_ref() else {
        return false;
    };
    let shared = engine.shared();
    // t260917-66c2: every pump, because the answer changes when opening a
    // picture fails and a new engine starts with it off.
    shared.set_gpu_stage(super::review_stage::gpu_stage_available());
    let vm = ui.global::<ReviewVm>();
    // Task1580: the sub-scopes below tile this function, so the outer aggregate
    // can be split into "which call is the fullscreen cost". Sum(sub) + span
    // overhead == outer; anything left over is the instrumentation itself.
    // Task1580: not faster than the display can show. The slot is latest-wins,
    // so a frame left in it is not lost -- it is replaced by a newer one, which
    // is the picture the display would have reached anyway.
    let now = std::time::Instant::now();
    let wait = review.stage_gate.take_turn(now);
    let taken = {
        livia::insight_scope!("ui_frame_take");
        match (wait, shared.frame.lock()) {
            (None, Ok(mut slot)) => slot.take(),
            // Task t260916-e610: the gate refused while a frame was actually
            // waiting. Nothing is lost here (the slot is latest-wins), but the
            // next pump only comes with the engine's next publish -- so this
            // count against `ui_pump_playback` / `ui_image_from_rgba` is what
            // H1 is judged on. Zero work: the span is entered for its count.
            (Some(_), Ok(slot)) if slot.is_some() => {
                livia::insight_scope!("ui_stage_gate_turned_away");
                None
            }
            _ => None,
        }
    };
    // Only while the engine is quiet. Playing, its next tick is along in at
    // most a frame's time and collects the frame itself; arming the timer then
    // just doubles the pump rate for nothing (measured: 5900 pumps against
    // 3536 ticks).
    if let Some(wait) = wait.filter(|_| !review.engine_playing) {
        // A seek while paused publishes exactly one frame, and nothing else
        // would ever come back for the one this turned away.
        let weak = ui.as_weak();
        review
            .stage_gate_timer
            .start(slint::TimerMode::SingleShot, wait, move || {
                if let Some(ui) = weak.upgrade() {
                    ui.global::<ReviewVm>().invoke_frame_ready();
                }
            });
    }
    if let Some(frame) = taken {
        // A picture on the GPU goes to slint as it is (t260917-66c2); one that
        // cannot be opened leaves the last picture on the stage.
        let image = match frame.gpu.as_ref() {
            Some(picture) => super::review_stage::gpu_stage_image(picture),
            None => image_from_rgba(frame.width, frame.height, &frame.rgba),
        };
        if let Some(image) = image {
            {
                // Hands slint an 8.3MB image at 1080p, and drops the one it was
                // holding.
                livia::insight_scope!("ui_set_stage_frame");
                vm.set_stage_frame(image);
            }
            // Task1580: closed until the display has had its refresh.
            review.stage_gate.published(now);
            vm.set_stage_has_frame(true);
            review.stage_has_frame = true;
            // Measured off the recording, never off a frame the engine resized
            // for the stage (task990): the stage sizes itself by this aspect
            // and the resize target is measured off the stage, so an aspect
            // derived from the resized frame closes a loop that never settles.
            let (width, height) = frame
                .full
                .as_ref()
                .map_or((frame.width, frame.height), |(width, height, _)| {
                    (*width, *height)
                });
            // Set with the frame, not on the VM sync's own schedule (task183):
            // the overlay bar hangs off the video box that this aspect
            // describes, so any tick where the two disagree draws the bar
            // across the middle of the picture. Loading a portrait session
            // while the previous session's 16:9 frame was still the last one
            // seen was exactly that window.
            vm.set_stage_aspect(pb::stage_aspect(width, height));
        }
        // Full resolution, so a screenshot saves the recording rather than
        // whatever the stage happened to be sized at (task990).
        {
            // Task1580: the assignment drops the previous frame's buffer, which
            // at 1080p is 8.3MB going back to the allocator every frame.
            // Task1640: so it goes back to the *engine* instead, which writes
            // the next frame into it rather than faulting in a new one. This is
            // the one moment the UI is provably done with that buffer -- it is
            // replacing its only reference to it right here.
            livia::insight_scope!("ui_keep_full_frame");
            // Task1690: when the engine resampled, `frame.rgba` is the
            // stage-sized copy and this is the last line that can see it --
            // `image_from_rgba` above already copied it into slint's own
            // buffer, and nothing keeps it. So it goes back to be resampled
            // into again, on exactly the argument the full-size one goes back.
            let next = match frame.full {
                Some(full) => {
                    shared.recycle_scaled(frame.rgba);
                    full
                }
                None => (
                    frame.width,
                    frame.height,
                    livia::playback::FullFrame::Rgba(frame.rgba),
                ),
            };
            if let Some((_, _, previous)) = review.last_frame.replace(next) {
                shared.recycle_full(previous);
            }
        }
    }
    let status = {
        livia::insight_scope!("ui_status_lock");
        match shared.status.lock() {
            Ok(status) => *status,
            Err(_) => return false,
        }
    };
    // Both of these are the engine's *own* state and stay that way: the first
    // frame hint fires on the engine actually starting, not on the UI asking
    // it to (task3290).
    let started_playing = status.playing && !review.engine_playing;
    review.engine_playing = status.playing;
    // t260926-cc7e: read before `settle_play_intent` folds a park's
    // `playing: false` into the UI's own account (`pb::follow_arrives`).
    let in_flight = pb::owns_position(false, review.last_seek_sent, status.acked_seek_100ns);
    if !review.follow.on
        && pb::follow_arrives(
            review.recording,
            review.playing,
            status.at_live_edge,
            in_flight,
        )
    {
        review.follow = review.follow.apply(pb::FollowEvent::Arrive);
        // Caught up at x2: following is x1. Not mid-skim -- the hold gives
        // its own rate back on release.
        if review.rate != 1.0 && !review.skimming() {
            review.rate = 1.0;
            review.send(PlaybackCommand::SetRate(1.0));
            vm.set_rate_text(tl::playback_rate_text(review.rate).into());
            vm.set_rate_index(tl::rate_index(review.rate) as i32);
        }
        tracing::info!(
            target: FOLLOW_LOG,
            position_100ns = status.position_100ns,
            "follow_arrive"
        );
    }
    settle_play_intent(review, status.playing);
    // While dragging -- either gesture (task3160), or while a seek this side
    // sent is still unanswered -- the UI owns the position; otherwise the
    // engine does (task2910).
    if !pb::owns_position(
        review.dragging || review.range_dragging,
        review.last_seek_sent,
        status.acked_seek_100ns,
    ) {
        review.position = status.position_100ns;
    }
    // t260926-cc7e: a follow that fell more than 8 s behind is put back on
    // its landing, once per lag -- `in_flight` holds the next until the
    // engine has answered this one.
    let lag_100ns = review
        .snapshot
        .as_ref()
        .map_or(0, |snapshot| snapshot.live_edge_100ns - review.position);
    if pb::follow_rejoin_due(
        review.follow,
        review.recording,
        review.playing,
        lag_100ns,
        in_flight,
    ) {
        tracing::info!(
            target: FOLLOW_LOG,
            behind_ms = lag_100ns / 10_000,
            reason = "lag_over_8s",
            "follow_rejoin"
        );
        review.follow_live("rejoin");
    }
    let Some(snapshot) = review.snapshot.as_ref() else {
        return started_playing;
    };
    pump_transport_chrome(&vm, review, snapshot);
    {
        // Task1600: reuses the status this pump already read, rather than
        // taking `shared.status` a second time in the same frame.
        livia::insight_scope!("ui_poll_repeat");
        poll_range_guard_with(review, status);
    }
    started_playing
}

/// The stage's LIVE capsule (task237, round8 §1). What is decided here is only
/// whether it exists and which word it wears (LIVE / 停止中…); the stop half's
/// hover ground is pure slint and never feeds back (t260913-a18d).
///
/// Each property is compared before it is written (task1600): this runs per
/// frame from `pump_playback`, but what it computes only moves on a lifecycle
/// event. Comparing against the property itself rather than a cached copy is
/// what keeps it honest -- `review_wiring` writes `live-chip-text` directly on
/// the stop, and a cache beside it would go stale.
pub(super) fn sync_live_chip(vm: &ReviewVm, review: &Review) {
    let chip = lifecycle::live_chip(review.recording, review.stopping);
    let minimized = review.recording && review.target_minimized;
    if vm.get_minimized_visible() != minimized {
        vm.set_minimized_visible(minimized);
    }
    let visible = chip != lifecycle::LiveChip::Hidden;
    if vm.get_live_chip_visible() != visible {
        vm.set_live_chip_visible(visible);
    }
    let label = lifecycle::live_chip_label(tr_locale(), chip);
    if vm.get_live_chip_text().as_str() != label {
        vm.set_live_chip_text(label.into());
    }
    let stopping = chip == lifecycle::LiveChip::Stopping;
    if vm.get_live_stopping() != stopping {
        vm.set_live_stopping(stopping);
    }
}

/// The selection's hold on the playhead (task153 repeat, task3150 the
/// boundaries themselves), polled from the UI rather than held in the engine:
/// the engine already arbitrates seeks, rate and the live edge, and a second
/// loop-state living beside those would have to agree with all three.
///
/// Called from `pump_playback` *and* from the 100ms drain, because at the live
/// edge the engine stops producing frames and `pump_playback` stops being
/// rung -- which is exactly when a whole-session repeat needs to wrap.
///
/// Half of 3150's story moved out of here in task3380: a seek that leaves the
/// selection now stops in `Review::seek_clamped`, before the engine is told to
/// go there -- or, since task3570, is turned round to the selection's start
/// there when repeat is on. What still arrives here is playback that *ran* out of the
/// selection, a skim released outside it, and repeat's wrap -- not a playhead
/// the user moved by hand, which used to be caught a frame or two late and
/// yanked back to the start with a flash of sound.
pub(super) fn poll_range_guard(review: &mut Review) -> bool {
    let Some(engine) = review.engine.as_ref() else {
        return false;
    };
    let status = match engine.shared().status.lock() {
        Ok(status) => *status,
        Err(_) => return false,
    };
    // The play/pause latch's deadline is resolved here as well as in the pump
    // (task3290), for the same reason this poll exists at all: parked at the
    // live edge the engine renders nothing and `pump_playback` stops being
    // rung -- and a `Play` the engine never takes is exactly the route the
    // deadline is there to catch. Resolved on this entry rather than inside
    // `poll_range_guard_with`, which the pump calls having already resolved it.
    let settled = settle_play_intent(review, status.playing);
    poll_range_guard_with(review, status) || settled
}

/// Decides what `playing` shows this tick and writes it, reporting whether it
/// moved (task3290).
///
/// `engine_playing` is the engine's own state, straight out of the shared
/// status. The UI's last command wins over it only while `pb::owns_playing`
/// says so; the moment the latch is spent -- the engine agreed, or the
/// deadline passed -- it is dropped and the engine has the display back.
fn settle_play_intent(review: &mut Review, engine_playing: bool) -> bool {
    let before = review.playing;
    let held = review.play_intent.and_then(|(intent, sent)| {
        pb::owns_playing(
            Some(intent),
            sent.elapsed().as_millis() as u64,
            engine_playing,
        )
        .then_some(intent)
    });
    review.playing = match held {
        Some(intent) => intent,
        None => {
            review.play_intent = None;
            engine_playing
        }
    };
    review.playing != before
}

/// [`poll_range_guard`] for a caller that has already read the status
/// (task1600). `pump_playback` is the one that has: it reads `shared.status`
/// for the transport and used to hand the lock straight back so this could
/// take it again, once per displayed frame.
pub(super) fn poll_range_guard_with(review: &mut Review, status: PlaybackStatus) -> bool {
    // A deliberate pause stays paused -- with repeat on, a live-edge park is
    // the one exception (see `pb::range_guard`).
    //
    // task4230 release (1), ahead of that return so it runs on every drain:
    // stopped by either account -- except a live-edge park, which the engine
    // resumes on its own as the recording grows (55a69411), and which
    // `range_guard` never stops while `status.playing` is false anyway.
    if !status.at_live_edge && !(status.playing && review.playing) {
        review.hold_at_end = None;
    }
    if !(status.playing || status.at_live_edge) {
        return false;
    }
    // While this side owns the position -- a drag, or a seek it sent that the
    // engine has not echoed yet (task2910) -- the status still carries the
    // position from *before* it. Acting on that stale position is how playing
    // from the end would eat its own Play: `toggle_play`'s seek to the start is
    // still in flight, `status.playing` already true, and the end position left
    // in the status would pause it again on the very next frame.
    // `range_dragging` is in here for a case of its own (task3160): a 終了端
    // drag parks the playhead on the new end with the transport paused, and
    // if that end sits at the live edge the `at_live_edge` branch above lets
    // this run anyway -- so with repeat on, `WrapToStart` would restart
    // playback in the middle of the gesture. The selection's rule is 3150's,
    // and it applies once the band is let go, not while it is moving.
    if pb::owns_position(
        review.dragging || review.range_dragging,
        review.last_seek_sent,
        status.acked_seek_100ns,
    ) {
        return false;
    }
    // The centre hold's x2 is exempt (task3280): it sends `Play` directly
    // rather than through `toggle_play`, so from here a skim is
    // indistinguishable from ordinary playback -- and 3150's boundaries would
    // yank it back to the selection mid-gesture, which is precisely the way
    // out of the selection the user reached for.
    let skimming = review.skimming();
    // task4230 releases (3) and (4), read only past `owns_position` so the
    // position is one the engine has caught up with: the range is not the
    // one the hold was armed for, or the playhead is back inside it.
    if review
        .hold_at_end
        .is_some_and(|held| review.range != Some(held) || status.position_100ns < held.1)
    {
        review.hold_at_end = None;
    }
    // Both accounts have to say playback is running. `status.playing` alone
    // leaves a window: the release of a skim that started paused sends `Pause`,
    // and until the worker takes it the status still says true -- so
    // `JumpToStart` would fire on the very next frame and undo the skim the
    // moment the button came up. `review.playing` alone is no good either,
    // because a `Play` the engine has not taken (parked at the live edge, which
    // the entry above lets through) would then fire the boundaries the doc on
    // `pb::range_guard` forbids from a parked engine. Both, and the only frames
    // that change are the ones just after a pause this side issued.
    // t260926-cc7e: following holds the selection's stops off until the next
    // seek the user makes (`Follow::range_exempt`).
    if review.follow.range_exempt {
        return false;
    }
    // t260928-b301: an end pinned to the live edge moves on with the
    // recording, so reaching it is reaching the live edge, not the end of the
    // selection -- no `PauseAtEnd`, the same as while following.
    let end_pinned = review.recording && review.quick == tl::QUICK_END_PINNED;
    let guard = pb::range_guard(
        status.playing && review.playing,
        skimming,
        review.hold_at_end.is_some() || end_pinned,
        review.repeat,
        review.range,
        status.position_100ns,
    );
    let Some((start, end)) = review.range else {
        return false;
    };
    match guard {
        pb::RangeGuard::None => false,
        pb::RangeGuard::JumpToStart => {
            review.seek_clamped(start, true);
            true
        }
        pb::RangeGuard::WrapToStart => {
            review.seek_clamped(start, true);
            // Parked at the live edge the engine is not playing; wrapping has
            // to start it again or the loop would run exactly once.
            if !status.playing {
                review.playing = true;
                review.send(PlaybackCommand::Play);
            }
            true
        }
        pb::RangeGuard::PauseAtEnd => {
            // Already on the end (the common case: the engine walked on to it)
            // needs no seek, only the stop. Sending one anyway would re-issue
            // it every frame until the pause lands.
            if status.position_100ns != end {
                review.seek_clamped(end, true);
            }
            // That seek is itself a seek out of the selection (`end` is the
            // first position outside it), so task3380's rule has already
            // stopped playback on the way. Entering this branch guaranteed
            // `review.playing` was true, so the only thing that can have
            // cleared it is that seek -- and re-sending `Pause` would arm the
            // latch a second time for a stop already issued.
            //
            // task3570's wrap does not reach this: `range_guard` answers
            // `WrapToStart` before it ever reaches `PauseAtEnd`, so this arm
            // is only entered with repeat *off* -- and with repeat off
            // `seek_clamped(end)` still comes back `SeekPlan::PauseFirst`,
            // exactly as it did before.
            //
            // Nor does task3580's skim exemption, for the same shape of
            // reason: `skimming` above is the very value `seek_clamped` will
            // read back out of `review.gesture`, and a true one would have
            // cleared `PauseAtEnd` to `None` before this arm could be picked.
            // So reaching here proves the seek below is not exempt.
            if review.playing {
                review.playing = false;
                review.send(PlaybackCommand::Pause {
                    reason: "range_end",
                });
            }
            true
        }
    }
}

/// The empty stage's two lines. One version again since task1410: picking a
/// target always starts a recording, so 対象を選ぶと is never a lie.
pub(super) fn render_empty_stage(ui: &AppWindow) {
    let vm = ui.global::<ReviewVm>();
    vm.set_empty_title(tl::empty_stage_title(tr_locale()).into());
    vm.set_empty_body(tl::empty_stage_body(tr_locale()).into());
    vm.set_empty_action(tl::empty_stage_action(tr_locale()).into());
}

pub(super) fn init_review_labels(ui: &AppWindow) {
    let vm = ui.global::<ReviewVm>();
    // t260927-7d08: the panel's words (DS ReviewScreen / MarkerList).
    vm.set_marker_range_hint(tl::marker_range_hint(tr_locale()).into());
    vm.set_marker_rename_hint(tl::marker_rename_hint(tr_locale()).into());
    vm.set_marker_delete_hint(tl::marker_delete_hint(tr_locale()).into());
    vm.set_marker_hold_hint(tl::marker_hold_hint(tr_locale()).into());
    vm.set_marker_placeholder(tl::marker_label_placeholder(tr_locale()).into());
    vm.set_marker_unnamed(tl::marker_unnamed(tr_locale()).into());
    vm.set_export_label(tl::export_selection(tr_locale()).into());
    vm.set_clip_label(tl::clip_save(tr_locale()).into());
    vm.set_clip_comment_label(tl::clip_comment_label(tr_locale()).into());
    vm.set_clip_comment_placeholder(tl::clip_comment_placeholder(tr_locale()).into());
    vm.set_clip_comment_hint(tl::clip_comment_hint(tr_locale()).into());
    // 全体 and the three chips (t260929-40a4): the callback hands back an
    // index into this same row, which `tl::range_preset_at` reads.
    vm.set_range_presets(
        Rc::new(VecModel::from(
            tl::range_preset_labels(tr_locale())
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    vm.set_range_overline(tl::range_overline(tr_locale()).into());
    vm.set_range_hint(tl::range_hint(tr_locale()).into());
    vm.set_markers_empty_note(tl::markers_empty_note(tr_locale()).into());
    // The live chip and its confirmation (task237, round8 §1). The chip's
    // resting word is rewritten every frame by `sync_live_chip`; the capsule's
    // stop half belongs here.
    //
    // t260927-77b8: the stop half is its own IconButton now, beside the
    // tally, so its words are the tooltip's (DS Tally: 「録画を止める」).
    vm.set_stop_button_label(pb::stage_stop_tip(tr_locale()).into());
    vm.set_minimized_title(tl::minimized_title(tr_locale()).into());
    vm.set_minimized_body(tl::minimized_body(tr_locale()).into());
    vm.set_fullscreen_exit_label(tl::fullscreen_exit(tr_locale()).into());
    // ---------- right panel + empty state (round5 §4-A / §4-D) ----------
    vm.set_panel_toggle_label(tl::panel_toggle(tr_locale()).into());
    vm.set_panel_tabs(
        Rc::new(VecModel::from(vec![
            SharedString::from(tl::panel_export_tab(tr_locale())),
            SharedString::from(tl::panel_marker_section(tr_locale())),
            SharedString::from(livia::ui_state::volume::volume_tab(tr_locale())),
        ]))
        .into(),
    );
    super::review_volume::apply_labels(&vm);
    vm.set_range_start_label(tl::range_start_label(tr_locale()).into());
    vm.set_range_start_playhead_hint(tl::range_playhead_start(tr_locale()).into());
    vm.set_range_end_playhead_hint(tl::range_playhead_end(tr_locale()).into());
    vm.set_range_end_label(tl::range_end_label(tr_locale()).into());
    vm.set_range_length_label(tl::range_length_label(tr_locale()).into());
    vm.set_markers_empty_text(tl::markers_empty(tr_locale()).into());
    // round13 §1. Only the fixed half: the body names the session's executable
    // and is written by `render_review`.
    vm.set_autorec_label(auto_capture::review_toggle_label(tr_locale()).into());
    render_empty_stage(ui);
    vm.set_export_cancel_label(tl::export_abort(tr_locale()).into());
    vm.set_screenshot_label(tl::screenshot(tr_locale()).into());
    vm.set_screenshot_unavailable(tl::screenshot_no_frame(tr_locale()).into());
    vm.set_rate_text(tl::playback_rate_text(1.0).into());
    // The shared Menu's rows (t260928-ac18); the current one is its
    // `checked-index`, `rate-index`.
    vm.set_rate_labels(
        Rc::new(VecModel::from(
            tl::rate_labels()
                .into_iter()
                .map(|label| MenuEntry {
                    label: label.into(),
                    ..Default::default()
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    vm.set_stage_zone_seconds(pb::stage_zone_seconds(tr_locale()).into());
    vm.set_stage_hint_text(pb::stage_hint(tr_locale()).into());
    vm.set_transport_tips(
        Rc::new(VecModel::from(
            pb::transport_tips(tr_locale())
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
}

/// The rate chip on its own. Split out for the centre hold (task650), which
/// changes the rate twice a gesture and has no reason to redraw the timeline.
/// Changes one track's mixer state and tells the engine (task1300).
///
/// The list grows on demand: how many tracks a recording has is not known
/// until its manifest is read, and a track nothing has touched is full volume.
pub(super) fn set_track_volume(
    review: &mut Review,
    track: usize,
    mutate: impl FnOnce(&mut (i64, bool)),
) {
    if review.track_volumes.len() <= track {
        review.track_volumes.resize(track + 1, (100, false));
    }
    mutate(&mut review.track_volumes[track]);
    let (percent, muted) = review.track_volumes[track];
    review.send(PlaybackCommand::SetTrackVolume {
        track,
        percent,
        muted,
    });
}

pub(super) fn render_rate(ui: &AppWindow, review: &Review) {
    let vm = ui.global::<ReviewVm>();
    vm.set_rate_text(tl::playback_rate_text(review.rate).into());
    vm.set_rate_index(tl::rate_index(review.rate) as i32);
}

/// Paints whichever feedback the last gesture produced. Purely presentational:
/// every seek already happened in the handler that produced the outcome.
pub(super) fn render_stage_feedback(ui: &AppWindow, review: &Review) {
    let vm = ui.global::<ReviewVm>();
    let (kind, backward, seconds, dots, line) = match review.stage_feedback {
        None => (0, false, String::new(), 0, ""),
        Some(pb::StageFeedback::Tap { direction, seconds }) => (
            1,
            direction < 0,
            pb::stage_seconds_text(tr_locale(), seconds),
            0,
            "",
        ),
        Some(pb::StageFeedback::Hold {
            direction,
            seconds,
            steps,
        }) => (
            2,
            direction < 0,
            pb::stage_seconds_text(tr_locale(), seconds),
            pb::hold_dots_on(steps) as i32,
            "",
        ),
        Some(feedback @ pb::StageFeedback::Clamped { .. }) => {
            (3, false, String::new(), 0, feedback.line(tr_locale()))
        }
        Some(pb::StageFeedback::Gap) => (
            4,
            false,
            String::new(),
            0,
            pb::stage_gap_skipped(tr_locale()),
        ),
        Some(pb::StageFeedback::Buffering) => {
            (5, false, String::new(), 0, pb::stage_buffering(tr_locale()))
        }
        // No dots: a hold's dots count seek repeats, and this one is a single
        // continuous state with nothing to count.
        Some(pb::StageFeedback::RateHold) => {
            (6, false, tl::playback_rate_text(pb::STAGE_HOLD_RATE), 0, "")
        }
    };
    vm.set_stage_feedback_kind(kind);
    // Opaque whenever there is something to say. The hide below drops this on
    // its own first and only clears the state once the fade has run, so the
    // mark goes out with its glyph rather than blinking off (task1020).
    vm.set_stage_feedback_shown(review.stage_feedback.is_some());
    vm.set_stage_loading(pb::stage_loading(
        review.stage_has_frame,
        review.stage_feedback,
    ));
    vm.set_stage_feedback_backward(backward);
    vm.set_stage_feedback_seconds(seconds.into());
    vm.set_stage_feedback_dots(dots);
    vm.set_stage_feedback_line(line.into());
}

/// Books the mark's exit: dwell, fade, then clear (task1020). One schedule for
/// every path that raises a mark -- a stage tap, a hold's outcome, or the
/// keyboard's ±seek, which used to raise one and never take it down.
///
/// Re-arming replaces the pending run, so a second tap inside the dwell gets a
/// full dwell of its own rather than inheriting what was left of the first.
pub(super) fn hide_feedback_after(ui: &AppWindow, review_rc: &Rc<std::cell::RefCell<Review>>) {
    let dwell = match review_rc.borrow().stage_feedback {
        // A tap's accumulator (±10 → ±20) is what this window is for: it has
        // to outlast the double-tap it is counting.
        Some(pb::StageFeedback::Tap { .. }) => pb::STAGE_TAP_RESET_MS,
        _ => pb::STAGE_FEEDBACK_HIDE_MS,
    };
    let weak = ui.as_weak();
    let rc = review_rc.clone();
    review_rc.borrow().feedback_timer.start(
        slint::TimerMode::SingleShot,
        std::time::Duration::from_millis(dwell),
        move || {
            let Some(ui) = weak.upgrade() else { return };
            ui.global::<ReviewVm>().set_stage_feedback_shown(false);
            let weak = ui.as_weak();
            let rc2 = rc.clone();
            rc.borrow().feedback_fade_timer.start(
                slint::TimerMode::SingleShot,
                std::time::Duration::from_millis(pb::STAGE_FEEDBACK_FADE_MS),
                move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let mut review = rc2.borrow_mut();
                    review.stage_feedback = None;
                    render_stage_feedback(&ui, &review);
                },
            );
        },
    );
}

/// Everything the review screen shows. The three `for`-loop models are only
/// rewritten when their content actually changed (`cached_*`); a scrub frame
/// costs scalar property writes only.
/// Every `cached_*` below is a record of what was last pushed into the slint
/// model beside it, and the pushes are conditional on that record -- so the two
/// have to move together or a render skips the update it owes.
///
/// Emptying a cache without emptying its model is what this catches, because
/// that is the shape the bug took (2026-09-22): `load_review` reset
/// `cached_markers` on every session load, so a session with no markers
/// computed `[]`, matched the `[]` the load had just written, and left the
/// previous session's dots on the bar. Cheap enough to run per frame in a dev
/// build, and gone entirely in the shipped one.
fn debug_assert_model_cache(cached: usize, model: usize, what: &str) {
    debug_assert_eq!(
        cached, model,
        "the {what} cache says {cached} rows and the model holds {model}: \
         something reset one without the other, and the next render will skip \
         a push it should make"
    );
}

/// The track's own boxes: the recorded segments and the seams between them.
///
/// Each model is only replaced when the shape actually changed -- setting a
/// `VecModel` rebuilds every element it holds.
fn render_track_spans(
    snapshot: &TimelineSnapshot,
    viewport: &Viewport,
    track_width_px: f32,
    (cached_segments, cached_gaps): (&mut Vec<TrackSpan>, &mut Vec<GapSpan>),
    (seg_model, gap_model): (&Rc<VecModel<TrackSpan>>, &Rc<VecModel<GapSpan>>),
) {
    debug_assert_model_cache(cached_segments.len(), seg_model.row_count(), "segments");
    debug_assert_model_cache(cached_gaps.len(), gap_model.row_count(), "gaps");
    let segments: Vec<TrackSpan> = tl::segment_boxes(snapshot, viewport)
        .into_iter()
        .map(|b| TrackSpan {
            left: b.left,
            width: b.width,
        })
        .collect();
    if segments != *cached_segments {
        *cached_segments = segments.clone();
        seg_model.set_vec(segments);
    }
    let gaps: Vec<GapSpan> = tl::gap_boxes(snapshot, viewport)
        .into_iter()
        .map(|b| GapSpan {
            left: b.left,
            width: b.width,
            // The seam's width in pixels is what decides whether it gets a
            // rule at each edge or a single one down the middle (task171) --
            // and a track that has not reported its width yet is its own case,
            // not a 0px track (task380).
            single: tl::gap_mark_on_track(b.width, track_width_px) == tl::GapMark::Single,
        })
        .collect();
    if gaps != *cached_gaps {
        *cached_gaps = gaps.clone();
        gap_model.set_vec(gaps);
    }
}

/// The markers, in both places they are drawn: the panel's list (every
/// recorded marker) and the bar (only the ones the viewport can reach).
fn render_markers(
    snapshot: &TimelineSnapshot,
    viewport: &Viewport,
    markers: &[MarkerRecord],
    (visible_markers, cached_markers, cached_marker_rows): (
        &mut Vec<i64>,
        &mut Vec<MarkerSpan>,
        &mut Vec<MarkerRow>,
    ),
    (marker_model, marker_row_model): (&Rc<VecModel<MarkerSpan>>, &Rc<VecModel<MarkerRow>>),
) {
    debug_assert_model_cache(
        cached_markers.len(),
        marker_model.row_count(),
        "marker spans",
    );
    debug_assert_model_cache(
        cached_marker_rows.len(),
        marker_row_model.row_count(),
        "marker rows",
    );
    // Only markers that land on recorded time survive. One colour since
    // t260927-7d08 (DS MarkerList: `marker` only), so no palette slot.
    let recorded = snapshot.recorded_markers(
        &markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
    );

    // The panel lists every recorded marker; the bar only draws the ones the
    // viewport can reach. The row index is the panel's list, which is what the
    // click callbacks carry.
    let marker_rows: Vec<MarkerRow> = recorded
        .iter()
        .map(|time| {
            let label = markers
                .iter()
                .find(|marker| marker.time_100ns == *time)
                .map(|marker| marker.label.clone())
                .unwrap_or_default();
            // Split for the panel's Timecode: the `.t` tail is drawn faint
            // (t260928-bae5, review 07 F11).
            let text = tl::format_timecode(*time - snapshot.start_100ns(), false);
            let (whole, frac) = tl::split_timecode(&text);
            MarkerRow {
                time: whole.into(),
                frac: frac.into(),
                label: label.into(),
            }
        })
        .collect();
    if marker_rows != *cached_marker_rows {
        *cached_marker_rows = marker_rows.clone();
        marker_row_model.set_vec(marker_rows);
    }
    *visible_markers = recorded.clone();

    let marker_spans: Vec<MarkerSpan> = recorded
        .iter()
        .enumerate()
        .filter(|(_, time)| viewport.contains(**time))
        .map(|(row, time)| MarkerSpan {
            left: viewport.percent(*time) as f32,
            row: row as i32,
            // t260927-77b8: the bar's hover label (DS Timeline).
            label: cached_marker_rows[row].label.clone(),
            time: cached_marker_rows[row].time.clone(),
        })
        .collect();
    if marker_spans != *cached_markers {
        *cached_markers = marker_spans.clone();
        marker_model.set_vec(marker_spans);
    }
}

/// The ruler over the track (t260927-77b8), replaced only when a tick moved:
/// a live session grows every poll and shifts them all, a finished one never
/// does.
fn render_ruler(vm: &ReviewVm, viewport: &Viewport, origin_100ns: i64) {
    let ticks: Vec<RulerTick> = tl::ruler_ticks(viewport, origin_100ns)
        .into_iter()
        .map(|tick| RulerTick {
            percent: tick.percent as f32,
            major: tick.major,
            label: tick.label.unwrap_or_default().into(),
        })
        .collect();
    let current = vm.get_ruler_ticks();
    if current.row_count() == ticks.len() && current.iter().eq(ticks.iter().cloned()) {
        return;
    }
    vm.set_ruler_ticks(Rc::new(VecModel::from(ticks)).into());
}

/// Whether `next` differs from what `current` holds. A new model re-creates
/// every `for` row -- a slider or button mid-press in one loses it -- and
/// while a recording is on screen `render_review` runs every drain
/// (t260928-b301), so the row lists are replaced only when they changed.
pub(super) fn model_differs<T: Clone + PartialEq + 'static>(
    current: slint::ModelRc<T>,
    next: &[T],
) -> bool {
    current.row_count() != next.len() || !current.iter().eq(next.iter().cloned())
}

pub(super) fn render_review(
    ui: &AppWindow,
    review: &mut Review,
    seg_model: &Rc<VecModel<TrackSpan>>,
    gap_model: &Rc<VecModel<GapSpan>>,
    marker_model: &Rc<VecModel<MarkerSpan>>,
    marker_row_model: &Rc<VecModel<MarkerRow>>,
    current: &AppSettings,
) {
    let render_started = Instant::now();
    review.advance_live_edge();
    let vm = ui.global::<ReviewVm>();
    let Some(snapshot) = review.snapshot.as_ref() else {
        return;
    };
    let apparent_edge = review.apparent_live_edge(snapshot);
    let viewport = review.viewport_ending_at(snapshot, apparent_edge);
    render_track_spans(
        snapshot,
        &viewport,
        review.track_width_px,
        (&mut review.cached_segments, &mut review.cached_gaps),
        (seg_model, gap_model),
    );
    render_markers(
        snapshot,
        &viewport,
        &review.markers,
        (
            &mut review.visible_markers,
            &mut review.cached_markers,
            &mut review.cached_marker_rows,
        ),
        (marker_model, marker_row_model),
    );
    vm.set_played_percent(viewport.clamped_percent(review.position) as f32);
    // How much of the visible track has video behind it: the last segment's
    // end, clamped. One number rather than a span per segment -- the recording
    // is contiguous apart from the gaps, which are drawn as seams anyway.
    vm.set_buffered_percent(
        snapshot
            .segments
            .last()
            .map(|segment| viewport.clamped_percent(segment.end_100ns) as f32)
            .unwrap_or(0.0),
    );
    // Only while the old end is on screen: zoomed away from it, nothing on
    // the track is dissolving.
    vm.set_overwriting(
        viewport.start_100ns <= snapshot.start_100ns()
            && snapshot.ring_fill(review.retention_minutes) >= 1.0,
    );
    vm.set_playhead_visible(viewport.contains(review.position));
    vm.set_playhead_percent(viewport.percent(review.position) as f32);
    vm.set_dragging(review.dragging);
    vm.set_range_dragging(review.range_dragging);

    // An end on the real edge is drawn on the apparent one (t260928-b301), or
    // the band would fall short of the growing bar by up to a segment and snap
    // back on every finalize. The panel's numbers below read the same value.
    let shown_range = tl::displayed_range(review.range, snapshot.live_edge_100ns, apparent_edge);
    match shown_range {
        Some((start, end)) => {
            vm.set_has_range(true);
            vm.set_range_start_percent(viewport.clamped_percent(start) as f32);
            vm.set_range_end_percent(viewport.clamped_percent(end) as f32);
            vm.set_range_start_handle_visible(viewport.contains(start));
            vm.set_range_end_handle_visible(viewport.contains(end));
        }
        None => vm.set_has_range(false),
    }

    vm.set_live_visible(viewport.contains(apparent_edge));
    vm.set_live_pinned(review.zoom.is_none());
    vm.set_live_percent(viewport.percent(apparent_edge) as f32);

    let start = snapshot.start_100ns();
    match review.hover {
        Some(hover) => {
            vm.set_hover_visible(true);
            // Where the pointer is, unclamped (t260928-ac18): the line stands
            // under it and the .slint keeps the time and preview on the track.
            vm.set_hover_anchor(viewport.percent(hover) as f32);
            vm.set_hover_time(tl::format_timecode(hover - start, false).into());
            let thumbnail = snapshot
                .find_covering_segment(hover)
                .and_then(|segment| review.thumbnails.get(&segment.index))
                .and_then(|image| image.clone());
            vm.set_hover_has_thumbnail(thumbnail.is_some());
            vm.set_hover_thumbnail(thumbnail.unwrap_or_default());
        }
        None => vm.set_hover_visible(false),
    }

    vm.set_playing(review.playing);
    let (position, total) = clock_texts(review, snapshot);
    set_clock(&vm, &position, &total);
    render_ruler(&vm, &viewport, start);
    let caps = super::settings_page::hotkey_caps(&current.hotkey);
    if vm.get_marker_hotkey().iter().ne(caps.iter()) {
        vm.set_marker_hotkey(caps);
    }
    // The panel's three numbers, from the same range the track's band draws --
    // they cannot disagree because they are the same `shown_range` (§4-D).
    let range = tl::range_values(
        shown_range,
        start,
        snapshot.full_duration_100ns() >= 3600 * tl::HNS_PER_SECOND,
    );
    // DS Timecode: the `.t` tail travels apart so it can be drawn faint.
    let (whole, frac) = tl::split_timecode(&range.start);
    vm.set_range_start_text(whole.into());
    vm.set_range_start_frac(frac.into());
    let (whole, frac) = tl::split_timecode(&range.end);
    vm.set_range_end_text(whole.into());
    vm.set_range_end_frac(frac.into());
    let (whole, frac) = tl::split_timecode(&range.length);
    vm.set_range_length_text(whole.into());
    vm.set_range_length_frac(frac.into());
    vm.set_range_preset_index(tl::range_preset_index(
        review.quick,
        review.range,
        review.chip,
    ));
    // DS MarkerList: the row of the marker at or just before the playhead
    // (the DS's `m.time <= position + 0.05`), and the tab's count.
    let near = review.position + tl::HNS_PER_SECOND / 20;
    vm.set_marker_current(
        review
            .visible_markers
            .iter()
            .rposition(|time| *time <= near)
            .map_or(-1, |row| row as i32),
    );
    vm.set_marker_count_text(review.visible_markers.len().to_string().into());
    render_rate(ui, review);
    vm.set_repeat_enabled(review.repeat);
    vm.set_volume_ratio((current.volume_percent as f32 / 100.0).clamp(0.0, 1.0));
    // The start-of-recording mute is a real mute as far as the ear is
    // concerned, so the button says so -- a speaker icon over silence is the
    // kind of lie that gets reported as broken audio.
    vm.set_muted(current.muted || review.live_mute.is_some());
    // The 音量 tab's levels follow the mixer state in place (t261003-83d3).
    super::review_volume::sync_levels(&vm, review);
    // 起動時に自動録画 (round13 §1). Derived here rather than at load so a
    // session switch, a language change and a list edit all land through the
    // one render every other panel row already goes through.
    //
    // task3670: the folder half needs the running process's image path, which
    // the container does not carry. `autorec_image_path` resolves it at most
    // once per (session, executable, folder list) -- see its doc -- and it is
    // stashed on the VM beside the name so the settings screen's handlers can
    // re-derive the row without a borrow of the review.
    if review.title_icon.as_deref() != Some(snapshot.session_id.as_str()) {
        review.title_icon = Some(snapshot.session_id.clone());
        ui.set_loaded_icon(
            session_app_icon(snapshot, &current.auto_capture_executables).unwrap_or_default(),
        );
    }
    let image_path = autorec_image_path(
        &mut review.autorec_path,
        &snapshot.session_id,
        snapshot.target_executable.as_deref(),
        &current.auto_capture_folders,
    )
    .unwrap_or_default();
    let autorec = auto_capture::review_toggle(
        tr_locale(),
        snapshot.target_executable.as_deref(),
        &current.auto_capture_executables,
        &current.auto_capture_folders,
        auto_capture::FolderGuard::machine(),
        image_path_arg(&image_path),
    );
    vm.set_autorec_visible(autorec.visible);
    vm.set_autorec_on(autorec.checked);
    vm.set_autorec_body(autorec.body.into());
    vm.set_autorec_executable(autorec.executable.into());
    vm.set_autorec_image_path(image_path.as_str().into());
    // round7 section 2-8: full screen folds the panel away and leaves the
    // stored preference alone, so leaving full screen restores it by itself.
    // Written here rather than in `set_fullscreen` because this runs on every
    // render and would otherwise overwrite whatever that set.
    vm.set_panel_open(current.review_panel_open && !vm.get_fullscreen());
    // `stage-aspect` is deliberately *not* set here: it rides the frame in
    // `pump_playback` so the box and the picture can never disagree (task183).

    // ---------- export (task129) ----------
    let status = review.export_status.as_ref();
    vm.set_export_busy(ex::is_busy(status));
    // React gated the button on a selected range; the pipeline needs one too.
    vm.set_export_enabled(review.range.is_some());
    // The DS ProgressBar prints its own percentage, so the label is the bare
    // word (t260927-7d08).
    vm.set_export_running_text(ex::export_running(tr_locale()).into());
    vm.set_export_percent(status.map_or(0.0, |status| ex::percent(&status.progress) as f32));
    let paths = match ex::outcome(tr_locale(), status) {
        Some(ex::Outcome::Completed(paths)) => paths,
        _ => Vec::new(),
    };
    let paths: Vec<SharedString> = paths.into_iter().map(SharedString::from).collect();
    if model_differs(vm.get_export_paths(), &paths) {
        vm.set_export_paths(Rc::new(VecModel::from(paths)).into());
    }
    // The gesture handlers paint this themselves the instant they produce it;
    // here it is for the two that have no gesture behind them -- the load that
    // puts 読み込み中 up and the segment that takes it down.
    render_stage_feedback(ui, review);
    // The clock, the LIVE frame's lag and its chip, off frames too: parked,
    // paused or just stopped, no frame arrives to run the pump, so the drain's
    // render is what keeps them moving -- and what clears the lag once the
    // recording stops (t260928-b301, 88a2's residue).
    pump_transport_chrome(&vm, review, snapshot);
    if edge_trace_on() {
        let wall_us = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| since.as_micros() as u64);
        tracing::info!(
            target: "t260930_1904_edge",
            wall_us,
            apparent_100ns = apparent_edge,
            real_100ns = snapshot.live_edge_100ns,
            dragging = review.dragging,
            pointer_100ns = review
                .pointer_ratio
                .filter(|_| review.dragging)
                .map(|ratio| viewport.time_from_ratio(ratio as f64)),
            position_100ns = review.position,
            recording = review.recording,
            hold = review.edge_clock.holding(snapshot.live_edge_100ns),
            render_us = render_started.elapsed().as_micros() as u64,
            "edge"
        );
    }
}

/// Redraws the held frame at the stage's current size (task990).
///
/// Only for a paused stage: playing, the engine's next frame arrives resampled
/// already. `last_frame` is left at full resolution -- this is what is drawn,
/// not what is kept.
pub(super) fn redraw_paused_stage(ui: &AppWindow, review: &Review) {
    if let Some(image) = paused_stage_image(review.last_frame.as_ref(), review.stage_target) {
        ui.global::<ReviewVm>().set_stage_frame(image);
    }
}

/// The held full-resolution frame, resampled to `stage_target` (or as it is
/// when there is no target yet) -- the body of `redraw_paused_stage`, shared
/// with the clips screen's paused-resize redraw (t260914-39c4). Setting it on
/// a VM stays with each screen.
pub(super) fn paused_stage_image(
    last_frame: Option<&(u32, u32, livia::playback::FullFrame)>,
    stage_target: Option<(u32, u32)>,
) -> Option<Image> {
    let (width, height, full) = last_frame?;
    // One conversion per *frame* on the NV12 half of `FullFrame`
    // (t260913-2527), not one per resize: a resize is a drag, so this runs many
    // times a second on the one frame that is held, and the ~3ms conversion at
    // 1922x1112 is the UI-thread work task990 moved off the tick.
    let rgba = full.rgba(*width, *height);
    let scaled = stage_target.and_then(|target| {
        // No spare (task1690): this is the paused-stage redraw, one frame per
        // resize, and the recycled destination belongs to the engine thread's
        // steady per-frame path.
        livia::playback::scale::downscale(
            &mut FrameScaler::default(),
            rgba,
            (*width, *height),
            target,
            None,
        )
    });
    match scaled.as_ref() {
        Some((width, height, rgba)) => image_from_rgba(*width, *height, rgba),
        None => image_from_rgba(*width, *height, rgba),
    }
}

/// Encodes the frame currently on the stage and drops it beside the video
/// exports, returning the line to show either way. React encoded in a canvas
/// and shipped base64 through the IPC boundary; here the bytes never leave the
/// process, so `save_screenshot_png` gets them directly.
/// The written file, so the corner toast has somewhere to point its
/// フォルダで表示 (task1040). It used to hand back a finished sentence, which
/// was all the notice card over the video needed.
pub(super) fn save_current_frame(review: &Review) -> Result<std::path::PathBuf, String> {
    let Some((width, height, full)) = review.last_frame.as_ref() else {
        return Err(tl::screenshot_no_frame(tr_locale()).to_owned());
    };
    // The recording's own resolution, whichever shape it was kept in. Converts
    // the NV12 a GPU-scaled frame holds unless the paused-stage redraw already
    // has, in which case this reads what that left behind.
    let rgba = full.rgba(*width, *height);
    let Some(buffer) = image::RgbaImage::from_raw(*width, *height, rgba.to_vec()) else {
        return Err(ex::screenshot_error(tr_locale()).to_owned());
    };
    let mut png = std::io::Cursor::new(Vec::new());
    if buffer.write_to(&mut png, image::ImageFormat::Png).is_err() {
        return Err(ex::screenshot_error(tr_locale()).to_owned());
    }
    let game = review
        .snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.target_title.clone())
        .unwrap_or_default();
    livia::export::save_screenshot_png(png.get_ref(), &game)
}

/// Opens the containing folder with the file selected -- the native equivalent
/// of the WebView build's `revealItemInDir` (task129). Task130 moved the actual
/// call onto `SHOpenFolderAndSelectItems`, so a path holding a comma or a quote
/// no longer has to survive explorer's own command-line parsing.
pub(super) fn reveal_in_directory(path: &std::path::Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(livia::ui_state::settings::open_directory_failed(tr_locale()).into());
    };
    super::desktop::open_in_explorer(parent, Some(path))
}

/// The slint end of [`EventSink`] (task129). The export pipeline runs on its
/// own thread and publishes through this; the UI thread is only ever woken,
/// never written to from off-thread.
pub(super) struct SlintSink {
    pub(super) status: Arc<Mutex<Option<ExportStatus>>>,
    pub(super) ui: slint::Weak<AppWindow>,
    /// Task130: the capture pipeline pushes the recording state through here as
    /// a tooltip string, from its own thread.
    pub(super) tray: slint::Weak<super::TrayIcon>,
}

impl SlintSink {
    fn ping(&self) {
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui.upgrade() {
                ui.global::<ReviewVm>().invoke_export_changed();
            }
        });
    }
}

impl EventSink for SlintSink {
    fn export_status(&self, status: &ExportStatus) {
        if let Ok(mut slot) = self.status.lock() {
            *slot = Some(status.clone());
        }
        self.ping();
    }

    fn export_progress(&self, progress: &ExportProgress) {
        if let Ok(mut slot) = self.status.lock() {
            // Progress arrives far more often than status; fold it into the
            // job it belongs to rather than letting a late packet from a
            // finished job overwrite the outcome.
            if let Some(status) = slot.as_mut() {
                if status.job_id == progress.job_id {
                    status.progress = progress.clone();
                }
            }
        }
        self.ping();
    }

    fn set_tray_tooltip(&self, tooltip: &str) {
        let tray = self.tray.clone();
        let tooltip = tooltip.to_owned();
        // The sink API carries only the tooltip; the menu's enabled state is
        // derived from it rather than widening the trait for one bool.
        //
        // The rule itself is `lifecycle::tray_stop_enabled`, next to the
        // `tray_tooltip` it has to agree with and tested against it.
        let recording = livia::ui_state::lifecycle::tray_stop_enabled(&tooltip);
        let ui = self.ui.clone();
        let _ = slint::invoke_from_event_loop(move || {
            // Task2510: mirror the tray badge onto the taskbar button. No
            // button (window hidden to tray / not yet created) means no HWND,
            // and nothing to do -- the tray icon covers that state.
            if let Some(hwnd) = ui
                .upgrade()
                .and_then(|ui| crate::shell::hwnd_of(ui.window()))
            {
                crate::desktop::set_taskbar_recording_badge(hwnd, recording, &tooltip);
            }
            if let Some(tray) = tray.upgrade() {
                tray.set_tooltip_text(tooltip.into());
                tray.set_recording(recording);
                // The icon swaps with `recording`, so the theme it has to be
                // drawn for is re-read here rather than only at start-up
                // (task2220).
                tray.set_light_taskbar(crate::desktop::system_uses_light_theme());
            }
        });
    }
}

/// The transport's own chrome: the clock, the live chip and the stage's loading state.
fn pump_transport_chrome(
    vm: &ReviewVm<'_>,
    review: &Review,
    snapshot: &livia::ui_state::timeline::TimelineSnapshot,
) {
    {
        // Task1580: every scalar the transport writes per frame, including the
        // two clock strings and the live chip's locale lookup.
        livia::insight_scope!("ui_transport_props");
        let viewport = review.viewport_of(snapshot);
        // `review.playing`, not `status.playing`: the caller has just settled
        // the two through the intent latch (task3290), and this is the write
        // the icon actually flickered on -- it runs once per presented frame,
        // where `render_review`'s own `set_playing` only runs on events. Both
        // now read the same field.
        vm.set_playing(review.playing);
        vm.set_played_percent(viewport.clamped_percent(review.position) as f32);
        vm.set_playhead_visible(viewport.contains(review.position));
        vm.set_playhead_percent(viewport.percent(review.position) as f32);
        // Set only on change (task1600). The clock's resolution is seconds and
        // the pump runs at the display's refresh, so ~60 of every 61 writes put
        // back the string that is already there -- and a slint property write
        // is unconditionally dirtying: it costs the `SharedString` allocation
        // here *and* re-layout of the text item over in the renderer, which is
        // where the bulk of the UI thread actually goes (task1580).
        let (position, total) = clock_texts(review, snapshot);
        set_clock(vm, &position, &total);
        // t260926-cc7e: the chip is there for as long as the stage session
        // records -- `review.recording` is the liveness the LIVE capsule runs
        // on (task2030 判断5) -- and says which half of Sync Mode it is in.
        // Not `at_live_edge`: that is false for a whole steady follow.
        if vm.get_live_edge_visible() != review.recording {
            vm.set_live_edge_visible(review.recording);
        }
        if vm.get_live_following() != review.follow.on {
            // One line per flip of the chip, whatever flipped it.
            tracing::info!(
                target: FOLLOW_LOG,
                on = review.follow.on,
                range_exempt = review.follow.range_exempt,
                "follow_chip"
            );
            vm.set_live_following(review.follow.on);
        }
        // The lag, `live_edge - position`, whatever the clock is set to show.
        // Only while it is the button; compared first like the clock.
        let behind = if review.recording && !review.follow.on {
            tl::format_timecode(
                -(review.apparent_live_edge(snapshot) - review.position).max(0),
                false,
            )
        } else {
            String::new()
        };
        set_timecode(
            &behind,
            (|| vm.get_live_behind_text(), |v| vm.set_live_behind_text(v)),
            (|| vm.get_live_behind_frac(), |v| vm.set_live_behind_frac(v)),
        );
        vm.set_stage_loading(pb::stage_loading(
            review.stage_has_frame,
            review.stage_feedback,
        ));
        sync_live_chip(vm, review);
    }
}

#[cfg(test)]
mod track_cache_tests {
    use super::*;
    use livia::ui_state::timeline::TimelineSegment;

    fn session(id: &str, end_100ns: i64) -> TimelineSnapshot {
        TimelineSnapshot {
            session_id: id.into(),
            segments: vec![TimelineSegment {
                index: 1,
                start_100ns: 0,
                end_100ns,
                audio_offsets_100ns: vec![0],
            }],
            gaps: Vec::new(),
            live_edge_100ns: end_100ns,
            target_title: None,
            target_executable: None,
            target_executable_path: None,
            audio_tracks: Vec::new(),
        }
    }

    fn marker(time_100ns: i64) -> MarkerRecord {
        MarkerRecord {
            time_100ns,
            label: String::new(),
            color_index: None,
        }
    }

    /// t260913-784a. The live edge poll runs ten times a second; the manifest
    /// clone (`fetch`) may only run once the extent says it grew. The grown
    /// rows are the control: the same counter sees the clone when it is due.
    #[test]
    fn the_live_edge_poll_clones_the_manifest_only_once_it_grew() {
        let current = session("a", 60_000_000);
        let calls = |extent| {
            let mut n = 0;
            manifest_if_grown(&current, extent, || {
                n += 1;
                None
            });
            n
        };
        // Not grown: same count, same end; an empty or missing session.
        assert_eq!(calls(Some((1, Some(60_000_000)))), 0);
        assert_eq!(calls(Some((0, None))), 0);
        assert_eq!(calls(None), 0);
        // Grown: one more segment, or the last one got longer.
        assert_eq!(calls(Some((2, Some(60_000_000)))), 1);
        assert_eq!(calls(Some((1, Some(60_000_001)))), 1);
    }

    /// 2026-09-22. Loading a session with no markers left the *previous*
    /// session's dots on the seek bar.
    ///
    /// `render_markers` only pushes into the slint model when what it computed
    /// differs from `cached_markers`, so that cache has to keep saying what the
    /// model actually holds. `load_review` used to empty it on every load:
    /// the next render computed `[]` for the marker-less session, compared it
    /// against the `[]` the load had just written, found no difference, and
    /// skipped the one push that would have cleared the bar. The panel's rows
    /// escaped only because `load_review` happened not to clear *their* cache.
    ///
    /// So this is about the two staying in step, which is why it renders twice
    /// with one cache rather than calling `load_review`.
    #[test]
    fn a_session_without_markers_empties_the_bar_the_previous_one_filled() {
        let spans = Rc::new(VecModel::<MarkerSpan>::default());
        let rows = Rc::new(VecModel::<MarkerRow>::default());
        let (mut visible, mut cached_spans, mut cached_rows) = (Vec::new(), Vec::new(), Vec::new());

        let first = session("a", 60_000_000);
        render_markers(
            &first,
            &Viewport::full(&first),
            &[marker(10_000_000), marker(20_000_000)],
            (&mut visible, &mut cached_spans, &mut cached_rows),
            (&spans, &rows),
        );
        assert_eq!(spans.row_count(), 2, "the first session's markers");
        assert_eq!(rows.row_count(), 2);

        // The next session, loaded over it: different timeline, no markers.
        let second = session("b", 90_000_000);
        render_markers(
            &second,
            &Viewport::full(&second),
            &[],
            (&mut visible, &mut cached_spans, &mut cached_rows),
            (&spans, &rows),
        );
        assert_eq!(spans.row_count(), 0, "the bar still holds session a's dots");
        assert_eq!(rows.row_count(), 0);
        assert!(visible.is_empty(), "and the click targets went with them");
    }

    /// The same step, the other way round: a marker-less session followed by
    /// one that has them. This one never broke -- `[]` and two spans always
    /// differ -- and it is here so a future fix for the case above cannot pass
    /// by making the push unconditional in one direction only.
    #[test]
    fn a_session_with_markers_fills_a_bar_the_previous_one_left_empty() {
        let spans = Rc::new(VecModel::<MarkerSpan>::default());
        let rows = Rc::new(VecModel::<MarkerRow>::default());
        let (mut visible, mut cached_spans, mut cached_rows) = (Vec::new(), Vec::new(), Vec::new());

        let first = session("a", 60_000_000);
        render_markers(
            &first,
            &Viewport::full(&first),
            &[],
            (&mut visible, &mut cached_spans, &mut cached_rows),
            (&spans, &rows),
        );
        assert_eq!(spans.row_count(), 0);

        let second = session("b", 90_000_000);
        render_markers(
            &second,
            &Viewport::full(&second),
            &[marker(30_000_000)],
            (&mut visible, &mut cached_spans, &mut cached_rows),
            (&spans, &rows),
        );
        assert_eq!(spans.row_count(), 1);
        assert_eq!(visible, vec![30_000_000]);
    }
    /// The guard for the load path, which no test can reach: `load_review`
    /// needs a window and a controller. What it *can* do is trip the invariant
    /// the way a reinstated `cached_markers.clear()` would, and check that the
    /// next render refuses to run on a cache that no longer describes its
    /// model.
    #[test]
    #[should_panic(expected = "something reset one without the other")]
    fn emptying_a_cache_without_its_model_is_caught() {
        let spans = Rc::new(VecModel::<MarkerSpan>::default());
        let rows = Rc::new(VecModel::<MarkerRow>::default());
        let (mut visible, mut cached_spans, mut cached_rows) = (Vec::new(), Vec::new(), Vec::new());

        let loaded = session("a", 60_000_000);
        render_markers(
            &loaded,
            &Viewport::full(&loaded),
            &[marker(10_000_000)],
            (&mut visible, &mut cached_spans, &mut cached_rows),
            (&spans, &rows),
        );
        assert_eq!(spans.row_count(), 1);

        // What the load used to do, and nothing else.
        cached_spans.clear();
        render_markers(
            &loaded,
            &Viewport::full(&loaded),
            &[],
            (&mut visible, &mut cached_spans, &mut cached_rows),
            (&spans, &rows),
        );
    }
}
