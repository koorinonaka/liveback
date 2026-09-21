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
use slint::{ComponentHandle, Image, SharedString, VecModel};

use super::tr_locale;
use super::{
    image_from_rgba, AppWindow, GapSpan, MarkerRow, MarkerSpan, MixerTrack, RecentChip, ReviewVm,
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
    pub(super) engine: Option<PlaybackEngine>,
    /// Per-track level and mute for the mixer, in track order (task1300).
    /// Session-lived, like the engine's own copy: opening a recording again
    /// starts everything at full volume.
    pub(super) track_volumes: Vec<(i64, bool)>,
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
    /// The clip button has turned into its comment field (task2970).
    pub(super) clip_editing: bool,
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
    // task1060's `range_editing` stood here. task3860 (round19 §1) folded the
    // mode into the 確定ステップ, so `clip_editing` above is the one flag that
    // says the boundaries are editable -- the steppers, the value editors and
    // the track band's drag all read it now. Off, they are a readout again and
    // a press on the band falls through to a plain seek, exactly as task1060
    // wound task940 back to.
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
    /// The band drag started while playing, so its release owes a `Play`
    /// (task3160). Only 開始端 and 移動 pay it back -- a 終了端 drag is a
    /// request to stop at the new end, so it stays stopped.
    pub(super) resume_after_range_drag: bool,
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
    pub(super) cached_segments: Vec<TrackSpan>,
    pub(super) cached_gaps: Vec<GapSpan>,
    pub(super) cached_markers: Vec<MarkerSpan>,
    pub(super) cached_marker_rows: Vec<MarkerRow>,
}

impl Review {
    pub(super) fn viewport(&self) -> Option<Viewport> {
        let snapshot = self.snapshot.as_ref()?;
        Some(self.zoom.unwrap_or_else(|| Viewport::full(snapshot)))
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
            match pb::seek_plan(
                self.playing,
                skimming,
                gesturing,
                self.repeat,
                self.range,
                position,
                |start| pb::classify_seek(snapshot, start).position(),
            ) {
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
            self.send(PlaybackCommand::Pause);
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
            self.send(PlaybackCommand::Pause);
            return;
        }
        if let Some((start, _)) = self.range {
            // Any verdict but `None` means the playhead is outside the
            // selection, whichever side and whether or not repeat is on.
            // Never a skim: the centre hold does not come through here at all
            // (task650/1010 write the transport themselves), and the double
            // press that does clears `gesture` before calling this.
            if pb::range_guard(true, false, self.repeat, self.range, self.position)
                != pb::RangeGuard::None
            {
                self.seek_clamped(start, true);
            }
        }
        self.send(PlaybackCommand::Play);
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
            PlaybackCommand::Pause => self.play_intent = Some((false, Instant::now())),
            _ => {}
        }
        if let Some(engine) = &self.engine {
            engine.send(command);
        }
    }
}

/// Resets the review screen onto a freshly loaded session's manifest and
/// starts a fresh playback engine for it. Dropping the old engine joins its
/// thread, which releases its lease -- so prune is never left blocked by a
/// session the user moved on from.
///
/// `live` is a session that is still being recorded (task162): it opens parked
/// at the live edge, watching what is happening now, rather than autoplaying
/// from a head the user has already seen.
pub(super) fn load_review(
    review: &mut Review,
    manifest: &ring_buffer::SessionManifest,
    controller: &CaptureController,
    settings: &AppSettings,
    ui: &AppWindow,
    live: bool,
    fresh: bool,
) {
    let snapshot = TimelineSnapshot::from_manifest(manifest);
    // A recording the user started seconds ago has no live edge worth parking
    // at, so it opens at the head and plays -- for a session this young the two
    // are the same picture, and parking is what left the stage black. Every
    // other live load keeps task162's park. Muted too: see `live_mute`.
    let start = if live && !fresh {
        (snapshot.live_edge_100ns - 1).max(snapshot.start_100ns())
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
    // Autoplay (task149). The initial position is the session's head, so this
    // starts from the beginning rather than parking at the live edge -- except
    // for a live session, which is opened *to* watch the live edge.
    review.playing = !live || fresh;
    review.show_remaining = false;
    review.repeat = false;
    // Speed is deliberately not persisted (task101): a new session starts at
    // 1x, so a forgotten 0.25x can't silently follow the user around.
    review.rate = 1.0;
    // Same rule for the mixer (task1300): a recording opens sounding the way
    // it was recorded, not the way the last one was left.
    review.track_volumes.clear();
    review.hover = None;
    review.status = None;
    review.dragging = false;
    // A session change mid-gesture (task3160): the band drag's state belongs
    // to the range it was editing, latch included.
    review.range_dragging = false;
    review.resume_after_range_drag = false;
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
    review.cached_segments.clear();
    review.cached_gaps.clear();
    review.cached_markers.clear();
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
    review.live_mute = fresh.then_some((settings.volume_percent, settings.muted));
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
    review.engine = Some(PlaybackEngine::start(
        controller.clone(),
        snapshot.clone(),
        start,
        settings.volume_percent,
        settings.muted || fresh,
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
    // After the engine is in place: the command channel is unbounded, so this
    // cannot block, and the engine picks it up as soon as its worker is up.
    if !live || fresh {
        review.send(PlaybackCommand::Play);
    }
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
    review.dragging = false;
    review.range_dragging = false;
    review.resume_after_range_drag = false;
    review.hover = None;
    review.status = None;
    review.last_frame = None;
    // The field belongs to the range that is going away (task2970). The job,
    // if one is running, does not -- it owns the file it is writing and still
    // owes it a comment.
    review.clip_editing = false;
    review.track_volumes.clear();
    review.thumbnails.clear();
    livia::insight_images!(Review, 0);
    review.thumbnails_requested.clear();
    review.cached_segments.clear();
    review.cached_gaps.clear();
    review.cached_markers.clear();
    review.cached_marker_rows.clear();
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
/// added, removed or monitor-toggled on the settings screen).
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
pub(super) fn follow_live_edge(review: &mut Review, controller: &CaptureController) -> bool {
    let Some(current) = review.snapshot.as_ref() else {
        return false;
    };
    let Some(manifest) = controller.timeline_for(&current.session_id) else {
        return false;
    };
    // Cheap gate first: rebuilding a snapshot ten times a second for a closed
    // session that will never grow is the common case, and it is pure waste.
    let grew = manifest.segments.len() > current.segments.len()
        || manifest
            .segments
            .last()
            .is_some_and(|segment| segment.end_100ns > current.live_edge_100ns);
    if !grew {
        return false;
    }
    let latest = TimelineSnapshot::from_manifest(&manifest);
    let Some(extension) = tl::playlist_extension(current, &latest) else {
        return false;
    };
    review.send(PlaybackCommand::ExtendPlaylist {
        segments: extension.segments,
        live_edge_100ns: extension.live_edge_100ns,
    });
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
    if let Some(range) = tl::range_after_growth(&latest, review.quick, tl::min_frame_100ns()) {
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
pub(super) fn clock_texts(review: &Review, snapshot: &TimelineSnapshot) -> (String, String) {
    let total = snapshot.full_duration_100ns();
    let elapsed = review.position - snapshot.start_100ns();
    let force_hours = total >= 3600 * tl::HNS_PER_SECOND;
    let position = if review.show_remaining {
        tl::format_remaining(total, elapsed, force_hours)
    } else {
        tl::format_position(elapsed, force_hours)
    };
    (position, tl::format_position(total, force_hours))
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
        if let Some(image) = image_from_rgba(frame.width, frame.height, &frame.rgba) {
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
    let Some(snapshot) = review.snapshot.as_ref() else {
        return started_playing;
    };
    pump_transport_chrome(&vm, review, snapshot, &status);
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
    // Both accounts have to say playback is running. `status.playing` alone
    // leaves a window: the release of a skim that started paused sends `Pause`,
    // and until the worker takes it the status still says true -- so
    // `JumpToStart` would fire on the very next frame and undo the skim the
    // moment the button came up. `review.playing` alone is no good either,
    // because a `Play` the engine has not taken (parked at the live edge, which
    // the entry above lets through) would then fire the boundaries the doc on
    // `pb::range_guard` forbids from a parked engine. Both, and the only frames
    // that change are the ones just after a pause this side issued.
    let guard = pb::range_guard(
        status.playing && review.playing,
        skimming,
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
                review.send(PlaybackCommand::Pause);
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
}

pub(super) fn init_review_labels(ui: &AppWindow) {
    let vm = ui.global::<ReviewVm>();
    // round8 §2: the four quick chips and the way back are gone. What is
    // left of them is the reset's tooltip and the steppers' four.
    vm.set_range_start_back_hint(tl::range_step_start_back(tr_locale()).into());
    vm.set_range_start_forward_hint(tl::range_step_start_forward(tr_locale()).into());
    vm.set_range_end_back_hint(tl::range_step_end_back(tr_locale()).into());
    vm.set_range_end_forward_hint(tl::range_step_end_forward(tr_locale()).into());
    vm.set_range_reset_hint(tl::range_reset_hint(tr_locale()).into());
    // task3050: the tail column's third button and the marker row's second.
    vm.set_range_start_playhead_hint(tl::range_playhead_start(tr_locale()).into());
    vm.set_range_end_playhead_hint(tl::range_playhead_end(tr_locale()).into());
    vm.set_marker_range_hint(tl::marker_range_hint(tr_locale()).into());
    vm.set_add_marker_label(tl::add_marker(tr_locale()).into());
    vm.set_marker_placeholder(tl::marker_label_placeholder(tr_locale()).into());
    vm.set_export_label(tl::export_selection(tr_locale()).into());
    vm.set_clip_label(tl::clip_save(tr_locale()).into());
    vm.set_clip_hint(tl::clip_comment_hint(tr_locale()).into());
    // task3870 (design round19 §2-3): the three 直近N chips above the clip
    // button. A label push like the rest of this function -- the row is fixed,
    // so nothing per-render rebuilds it, and the only thing that can change it
    // is the language, which re-runs this whole function.
    let recent_chips: Vec<RecentChip> = tl::RECENT_CHIP_SECONDS
        .iter()
        .map(|seconds| RecentChip {
            label: tl::recent_chip_label(tr_locale(), *seconds).into(),
            seconds: *seconds as i32,
        })
        .collect();
    vm.set_recent_chips(Rc::new(VecModel::from(recent_chips)).into());
    // The live chip and its confirmation (task237, round8 §1). The chip's
    // resting word is rewritten every frame by `sync_live_chip`; the capsule's
    // stop half belongs here.
    //
    // 停止, not the picker tile's キャプチャを停止: inside a capsule that
    // already says LIVE, the noun is on screen twice (round8 §1).
    vm.set_stop_button_label(lifecycle::live_chip_stop(tr_locale()).into());
    vm.set_minimized_title(tl::minimized_title(tr_locale()).into());
    vm.set_minimized_body(tl::minimized_body(tr_locale()).into());
    vm.set_fullscreen_exit_label(tl::fullscreen_exit(tr_locale()).into());
    // ---------- right panel + empty state (round5 §4-A / §4-D) ----------
    vm.set_panel_toggle_label(tl::panel_toggle(tr_locale()).into());
    vm.set_range_section_label(tl::panel_range_section(tr_locale()).into());
    vm.set_export_section_label(tl::panel_export_section(tr_locale()).into());
    vm.set_still_section_label(tl::panel_still_section(tr_locale()).into());
    vm.set_marker_section_label(tl::panel_marker_section(tr_locale()).into());
    vm.set_mixer_section_label(pb::mixer_section(tr_locale()).into());
    vm.set_range_start_label(tl::range_start_label(tr_locale()).into());
    vm.set_range_end_label(tl::range_end_label(tr_locale()).into());
    vm.set_range_length_label(tl::range_length_label(tr_locale()).into());
    vm.set_markers_empty_text(tl::markers_empty(tr_locale()).into());
    // round13 §1. Only the fixed half: the body names the session's executable
    // and is written by `render_review`.
    vm.set_autorec_label(auto_capture::review_toggle_label(tr_locale()).into());
    render_empty_stage(ui);
    vm.set_export_cancel_label(ex::export_cancel(tr_locale()).into());
    vm.set_screenshot_label(tl::screenshot(tr_locale()).into());
    vm.set_rate_text(tl::playback_rate_text(1.0).into());
    vm.set_rate_labels(
        Rc::new(VecModel::from(
            tl::rate_labels()
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    vm.set_stage_zone_seconds(pb::stage_zone_seconds(tr_locale()).into());
    vm.set_stage_hint_text(pb::stage_hint(tr_locale()).into());
    vm.set_live_edge_text(pb::live_edge_badge(tr_locale()).into());
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
            pb::tap_seconds_text(direction, seconds),
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
            pb::hold_seconds_text(tr_locale(), direction, seconds),
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
    // Only markers that land on recorded time survive, and the palette index
    // is decided from the record rather than the row so a marker keeps its
    // colour when the list around it changes (round5 §4-B).
    let recorded = snapshot.recorded_markers(
        &markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
    );
    let coloured: Vec<(i64, usize)> = recorded
        .iter()
        .enumerate()
        .map(|(position, time)| {
            let stored = markers
                .iter()
                .find(|marker| marker.time_100ns == *time)
                .and_then(|marker| marker.color_index);
            (*time, tl::marker_palette_index(stored, position))
        })
        .collect();

    // The panel lists every recorded marker; the bar only draws the ones the
    // viewport can reach. The row index is the panel's list, which is what the
    // click callbacks carry.
    let marker_rows: Vec<MarkerRow> = coloured
        .iter()
        .map(|(time, colour)| {
            let label = markers
                .iter()
                .find(|marker| marker.time_100ns == *time)
                .map(|marker| marker.label.clone())
                .unwrap_or_default();
            MarkerRow {
                time: tl::format_duration(*time - snapshot.start_100ns()).into(),
                label: label.into(),
                color_index: *colour as i32,
            }
        })
        .collect();
    if marker_rows != *cached_marker_rows {
        *cached_marker_rows = marker_rows.clone();
        marker_row_model.set_vec(marker_rows);
    }
    *visible_markers = coloured.iter().map(|(time, _)| *time).collect();

    let marker_spans: Vec<MarkerSpan> = coloured
        .iter()
        .enumerate()
        .filter(|(_, (time, _))| viewport.contains(*time))
        .map(|(row, (time, colour))| MarkerSpan {
            left: viewport.percent(*time) as f32,
            row: row as i32,
            color_index: *colour as i32,
        })
        .collect();
    if marker_spans != *cached_markers {
        *cached_markers = marker_spans.clone();
        marker_model.set_vec(marker_spans);
    }
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
    let vm = ui.global::<ReviewVm>();
    let Some(snapshot) = review.snapshot.as_ref() else {
        return;
    };
    let viewport = review.zoom.unwrap_or_else(|| Viewport::full(snapshot));
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
    vm.set_playhead_visible(viewport.contains(review.position));
    vm.set_playhead_percent(viewport.percent(review.position) as f32);
    vm.set_dragging(review.dragging);
    vm.set_range_dragging(review.range_dragging);

    match review.range {
        Some((start, end)) => {
            vm.set_has_range(true);
            vm.set_range_start_percent(viewport.clamped_percent(start) as f32);
            vm.set_range_end_percent(viewport.clamped_percent(end) as f32);
            vm.set_range_start_handle_visible(viewport.contains(start));
            vm.set_range_end_handle_visible(viewport.contains(end));
        }
        None => vm.set_has_range(false),
    }

    vm.set_live_visible(viewport.contains(snapshot.live_edge_100ns));
    vm.set_live_pinned(review.zoom.is_none());
    vm.set_live_percent(viewport.percent(snapshot.live_edge_100ns) as f32);

    let start = snapshot.start_100ns();
    match review.hover {
        Some(hover) => {
            vm.set_hover_visible(true);
            vm.set_hover_anchor(tl::popover_anchor_percent(viewport.percent(hover)) as f32);
            vm.set_hover_time(tl::format_duration(hover - start).into());
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
    vm.set_position_text(position.into());
    vm.set_total_text(total.into());
    // The panel's three numbers, from the same range the track's band draws --
    // they cannot disagree because they are the same `review.range` (§4-D).
    let range = tl::range_values(
        review.range,
        start,
        snapshot.full_duration_100ns() >= 3600 * tl::HNS_PER_SECOND,
    );
    vm.set_range_start_text(range.start.into());
    vm.set_range_end_text(range.end.into());
    vm.set_range_length_text(range.length.into());
    render_rate(ui, review);
    vm.set_repeat_enabled(review.repeat);
    vm.set_volume_ratio((current.volume_percent as f32 / 100.0).clamp(0.0, 1.0));
    // The start-of-recording mute is a real mute as far as the ear is
    // concerned, so the button says so -- a speaker icon over silence is the
    // kind of lie that gets reported as broken audio.
    vm.set_muted(current.muted || review.live_mute.is_some());
    // The mixer (task1300). Absent for a single-track recording, which is
    // every recording made before task1260 -- one row would just be a second
    // copy of the master slider beside it.
    let mixer = pb::mixer_tracks(tr_locale(), &snapshot.audio_tracks, &review.track_volumes);
    vm.set_audio_tracks(slint::ModelRc::new(slint::VecModel::from(
        mixer
            .into_iter()
            .map(|track| MixerTrack {
                name: track.name.into(),
                volume_ratio: track.volume_ratio,
                muted: track.muted,
            })
            .collect::<Vec<_>>(),
    )));
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
    // Task2970: only ever true while the range it was opened over still
    // exists, so a session swap cannot leave a field floating over nothing.
    vm.set_clip_editing(review.clip_editing && review.range.is_some());
    vm.set_export_running_text(ex::running_text(tr_locale(), status).into());
    vm.set_export_percent(status.map_or(0.0, |status| ex::percent(&status.progress) as f32));
    let paths = match ex::outcome(tr_locale(), status) {
        Some(ex::Outcome::Completed(paths)) => paths,
        _ => Vec::new(),
    };
    vm.set_export_paths(
        Rc::new(VecModel::from(
            paths
                .into_iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    // Task169: a job that failed or was cancelled says so in the corner toast
    // now, so the card is back to carrying only the screen's own failures.
    vm.set_status_text(review.status.clone().unwrap_or_default().into());
    // The gesture handlers paint this themselves the instant they produce it;
    // here it is for the two that have no gesture behind them -- the load that
    // puts 読み込み中 up and the segment that takes it down.
    render_stage_feedback(ui, review);
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
    status: &livia::playback::PlaybackStatus,
) {
    {
        // Task1580: every scalar the transport writes per frame, including the
        // two clock strings and the live chip's locale lookup.
        livia::insight_scope!("ui_transport_props");
        let viewport = review.zoom.unwrap_or_else(|| Viewport::full(snapshot));
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
        if vm.get_position_text().as_str() != position {
            vm.set_position_text(position.into());
        }
        if vm.get_total_text().as_str() != total {
            vm.set_total_text(total.into());
        }
        // Gated on the stage session still being recorded (task2610):
        // `at_live_edge` also rises for an ended session parked at its end,
        // where 追従中 would be a lie. `review.recording` is the same
        // liveness the LIVE capsule runs on (task2030 判断5).
        vm.set_live_edge_visible(pb::live_edge_badge_visible(
            status.at_live_edge,
            review.recording,
        ));
        vm.set_stage_loading(pb::stage_loading(
            review.stage_has_frame,
            review.stage_feedback,
        ));
        sync_live_chip(vm, review);
    }
}
