//! Pure playback state for the slint review screen (task128). The Media
//! Foundation engine (`crate::playback`) does the decoding; everything that
//! can be decided without touching MF -- transport transitions, seek
//! classification, gap crossing, live-edge clamping, and the video stage's
//! gesture/feedback machine (task119/round2 §10) -- lives here with tests.

use std::time::{Duration, Instant};

use crate::ui_state::locale::Locale;
use crate::ui_state::timeline::{TimelineSnapshot, HNS_PER_SECOND};

pub mod stage_gesture;
pub use stage_gesture::{
    feedback_exit, settle_confirm, stage_press, FeedbackExit, SeekFold, StageEffect, StageGesture,
};

// ---------- stage constants (ReviewTimeline.tsx) ----------

/// Side zones are the outer 25% of the stage.
pub const STAGE_SIDE_ZONE_RATIO: f64 = 0.25;
/// What a side tap and J/L jump: `settings.seek_seconds`, mirrored here so the
/// stage and key handlers need no settings lock (the same split as
/// `locale::active`). Only the bin reads it; the functions below take the
/// seconds as an argument.
static STAGE_TAP_SECONDS: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(crate::settings::DEFAULT_SEEK_SECONDS as i64);

pub fn set_stage_tap_seconds(seconds: u8) {
    STAGE_TAP_SECONDS.store(i64::from(seconds), std::sync::atomic::Ordering::Relaxed);
}

pub fn stage_tap_seconds() -> i64 {
    STAGE_TAP_SECONDS.load(std::sync::atomic::Ordering::Relaxed)
}
pub const STAGE_HOLD_DELAY_MS: u64 = 350;
/// React used 3s/500ms because five MSE seeks per second outran its fetch
/// window (recorded in ReviewTimeline.tsx). The native engine's coarse seek
/// lands in a few ms, so the original 200ms cadence works again -- measured
/// in task128's Execution Log.
pub const STAGE_REPEAT_INTERVAL_MS: u64 = 200;
pub const STAGE_TAP_RESET_MS: u64 = 800;
pub const STAGE_FEEDBACK_HIDE_MS: u64 = 720;
/// How long the mark takes to fade once its dwell is up (task1020). The dwell
/// above ends by taking the opacity down; the state behind it is only cleared
/// once this has run, so the glyph is still in the circle while it goes.
pub const STAGE_FEEDBACK_FADE_MS: u64 = 180;
pub const STAGE_HOLD_DOTS: u32 = 5;
/// What a centre hold plays at (task650). A fixed rate, not a multiple of the
/// current one: the gesture is "let me skim this bit", and doubling an already
/// chosen x2 lands on a speed nothing else in the app can reach.
pub const STAGE_HOLD_RATE: f64 = 2.0;
/// How long after a centre tap a second press still counts as a double click
/// (task1010). Slint's own `double-clicked` fires on the second *release*,
/// which is a whole press-and-hold too late for a full-screen toggle, so the
/// pairing is done here instead and this is the window it uses. Press to
/// press, like the OS's own double click: 400ms counted from the first tap's
/// *release* let a play click followed by a second click ~300-500ms later
/// leave full screen (user report, 2026-10-15).
pub const STAGE_DOUBLE_PRESS_MS: u64 = 300;
/// The first-run hint shows once (DS Stage README 「はじめての 1 回だけ」,
/// t260928-ac18).
pub const STAGE_HINT_MAX_SHOWINGS: u32 = 1;
pub const STAGE_HINT_DELAY_MS: u64 = 1200;
pub const STAGE_HINT_DURATION_MS: u64 = 2800;

crate::trf! {
    // t260927-77b8: the DS Stage README's words.
    stage_hint(seconds) {
        ja: "左右を押すと {seconds} 秒移動、長押しで早送り。中央で再生 / 一時停止、長押しで倍速。",
        en: "Press either side to jump {seconds}s, hold to scan. Press the middle to play or pause, hold for double speed."
    }
}

crate::tr! {
    // t260927-77b8: the DS Stage README's words.
    stage_clamped_start { ja: "先頭です", en: "At the start" }
    stage_clamped_live_edge { ja: "ライブ位置です", en: "At the live edge" }
    stage_gap_skipped { ja: "録画のない区間を飛ばしました", en: "Skipped a stretch with no recording" }
    stage_buffering { ja: "読み込み中", en: "Loading" }

    // t260927-77b8: the transport's tooltips (DS TransportBar), in
    // `transport_tips` order. The Sync Mode chip's words (t260926-cc7e) went
    // with the chip: LiveButton says LIVE and its tip says where it goes.
    tip_frame_back { ja: "1 フレーム戻る", en: "Previous frame" }
    tip_play { ja: "再生", en: "Play" }
    tip_pause { ja: "一時停止", en: "Pause" }
    tip_frame_forward { ja: "1 フレーム進む", en: "Next frame" }
    tip_repeat { ja: "繰り返し", en: "Repeat" }
    tip_range_start { ja: "範囲の先頭へ", en: "To the range start" }
    tip_range_end { ja: "範囲の末尾へ", en: "To the range end" }
    tip_add_marker { ja: "マーカーを追加", en: "Add marker" }
    tip_rate { ja: "再生速度", en: "Playback speed" }
    tip_mute { ja: "ミュート", en: "Mute" }
    tip_unmute { ja: "ミュートを解除", en: "Unmute" }
    tip_go_live { ja: "ライブへ", en: "Go live" }
    tip_fullscreen { ja: "全画面", en: "Full screen" }
    tip_fullscreen_exit { ja: "全画面を終了", en: "Leave full screen" }
    tip_panel_open { ja: "パネルを開く", en: "Open the panel" }
    tip_panel_close { ja: "パネルを閉じる", en: "Close the panel" }
    tip_behind { ja: "ライブ位置からの遅れ", en: "Behind the live edge" }
}

/// The transport's tooltip words. `review.slint`'s `tip(i)` reads them by
/// index, so the order is part of the contract: 0 frame back, 1 play, 2 pause,
/// 3 frame forward, 4 repeat, 5 range start, 6 range end, 7 add marker,
/// 8 rate, 9 mute, 10 unmute, 11 go live, 12 full screen, 13 leave full
/// screen, 14 open panel, 15 close panel, 16 behind the live edge. One list
/// rather than a property each: seventeen words that change together, once
/// per locale.
pub fn transport_tips(locale: Locale) -> Vec<&'static str> {
    vec![
        tip_frame_back(locale),
        tip_play(locale),
        tip_pause(locale),
        tip_frame_forward(locale),
        tip_repeat(locale),
        tip_range_start(locale),
        tip_range_end(locale),
        tip_add_marker(locale),
        tip_rate(locale),
        tip_mute(locale),
        tip_unmute(locale),
        tip_go_live(locale),
        tip_fullscreen(locale),
        tip_fullscreen_exit(locale),
        tip_panel_open(locale),
        tip_panel_close(locale),
        tip_behind(locale),
    ]
}

// ---------- seek classification (React `seekClamped`) ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekOutcome {
    Ok(i64),
    Clamped {
        position: i64,
        at_start: bool,
    },
    /// The request landed inside a gap and was carried forward.
    Gap(i64),
    /// Nothing recorded at or after the request.
    Buffering,
}

impl SeekOutcome {
    pub fn position(&self) -> Option<i64> {
        match self {
            SeekOutcome::Ok(position)
            | SeekOutcome::Clamped { position, .. }
            | SeekOutcome::Gap(position) => Some(*position),
            SeekOutcome::Buffering => None,
        }
    }
}

/// Clamp into [start, live-1], snap forward, and report what actually
/// happened -- the feedback layer has to say 先頭です rather than silently
/// doing nothing (task119).
pub fn classify_seek(snapshot: &TimelineSnapshot, target_100ns: i64) -> SeekOutcome {
    let low = snapshot.start_100ns();
    // `.max(low)`: a manifest with zero segments has `live_edge_100ns == 0`,
    // and `clamp` panics when its bounds invert. Such a session is loadable
    // (`is_load_disabled` only checks `readable`), so this must degrade to a
    // zero-width range instead.
    let high = (snapshot.live_edge_100ns - 1).max(low);
    let clamped = target_100ns.clamp(low, high);
    let Some(snapped) = snapshot.snap_to_recorded_time(clamped) else {
        return SeekOutcome::Buffering;
    };
    if target_100ns < low || target_100ns > high {
        return SeekOutcome::Clamped {
            position: snapped,
            at_start: target_100ns < low,
        };
    }
    if snapped != clamped {
        return SeekOutcome::Gap(snapped);
    }
    SeekOutcome::Ok(snapped)
}

/// Who owns `position` between the UI and the engine (task2910).
///
/// A UI-issued seek is not instant: until the worker has run it, the shared
/// status still carries the position from *before* it. Writing that back
/// rewinds the playhead a whole step, and the next key tap -- which derives
/// its target from `position` -- recomputes the target just sent, so the
/// press is swallowed (measured: 16 taps at 120ms on a cold session produced
/// 9 moves). The UI therefore keeps `position` from the moment it sends a
/// seek until the engine echoes that exact target back.
///
/// `dragging` keeps its own ownership unchanged: a scrub owns the position
/// for the whole gesture, not just while one seek is in flight.
pub fn owns_position(dragging: bool, last_sent: Option<i64>, acked: Option<i64>) -> bool {
    dragging || (last_sent.is_some() && last_sent != acked)
}

/// Where the full-screen double press has to put the playhead back (task3270).
///
/// The gesture is meant to be full screen and nothing else: the tap that opens
/// the pair is undone by the second press, so the whole thing has to leave the
/// transport -- position included -- where it found it. It no longer does on
/// its own, because task3150 made `Review::toggle_play` start playback at the
/// selection's start when the playhead is outside it: the opening tap jumps
/// there, and the undo only pauses.
///
/// `saved` is the position from just before that opening tap, `position` the
/// one the undo left behind. `None` means leave it alone:
///
/// - `playing`: the pair started from playing, so the undo *resumed* playback
///   and the gesture is over with the transport running. Seeking here would
///   hitch the picture for nothing -- and `saved` can differ from `position`
///   even in this case, because the pause the opening tap sent is answered on
///   the worker thread and the position keeps moving until it lands.
/// - `saved == position`: nothing moved, so there is nothing to undo. Skipping
///   spares the precise decode a seek to the current frame would still cost.
pub fn restore_after_double_press(playing: bool, saved: Option<i64>, position: i64) -> Option<i64> {
    let saved = saved?;
    (!playing && saved != position).then_some(saved)
}

/// How long the UI's own play/pause intent outranks the engine's status
/// (task3290).
///
/// Roughly fifteen frames at 60Hz: long enough that the whole unacknowledged
/// window is covered, short enough that a lie nobody intended is over before
/// it can be read as the transport being broken.
pub const PLAY_INTENT_MS: u64 = 250;

/// Who owns `playing` between the UI and the engine (task3290).
///
/// The same lag `owns_position` covers applies to the transport itself: a
/// `Pause` reaches the worker on another thread, and until it runs there the
/// shared status still says `playing == true`. Every frame in between, the
/// pump writes that back over the `false` the UI just set, and the play/pause
/// icon flickers. Seen on a range-band drag, but the window has always been
/// there -- `Review::toggle_play` opens the same one.
///
/// Unlike the position, there is no `acked_playing` to compare against: the
/// status carries no echo of the last transport command, and adding one means
/// designing a second discipline into the worker for what is a display
/// artefact. So this latch expires on a clock instead, and `elapsed_ms` is a
/// parameter rather than an `Instant::now()` in here so the whole table is
/// testable.
///
/// The deadline is not a detail -- it is the point. A latch that only ended on
/// agreement would hold forever wherever the engine never takes the command:
/// `Play` at the live edge is an existing such route, and an icon stuck
/// claiming playback that is not happening is worse than the flicker it
/// replaced.
pub fn owns_playing(intent: Option<bool>, elapsed_ms: u64, engine_playing: bool) -> bool {
    match intent {
        // Nothing sent, or a latch already spent: the engine has always owned
        // this and still does.
        None => false,
        // Agreement retires the latch early -- the whole reason it existed is
        // that the two disagreed.
        Some(intent) => intent != engine_playing && elapsed_ms <= PLAY_INTENT_MS,
    }
}

/// What `playing` shows this tick, given the UI's latch (the last transport
/// command and when it was sent) and the engine's own flag.
///
/// The latch wins only while [`owns_playing`] says so; once it is spent it is
/// cleared here and the engine's value is returned. Both players (review and
/// clips) settle their glyph through this.
pub fn settle_intent(latch: &mut Option<(bool, Instant)>, engine_playing: bool) -> bool {
    let held = latch.and_then(|(intent, sent)| {
        owns_playing(
            Some(intent),
            sent.elapsed().as_millis() as u64,
            engine_playing,
        )
        .then_some(intent)
    });
    if held.is_none() {
        *latch = None;
    }
    held.unwrap_or(engine_playing)
}

// ---------- range guard ----------

/// The transport flags the selection rules read ([`range_guard`],
/// [`seek_plan`]): whether it plays, whether the centre hold's x2 skim is on,
/// and whether the selection repeats.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransportState {
    pub playing: bool,
    pub skimming: bool,
    pub repeat: bool,
}

/// What the selection asks of the transport at `position_100ns` (task3150).
///
/// Repeat used to be the only reader of the boundaries (task153), so with
/// repeat off playback ran straight past the end and on to the tail of the
/// session. The selection now holds playback inside itself either way.
///
/// What that no longer covers is a *seek* out of the selection: since task3380
/// those stop the transport where they land instead of being pulled back here
/// a frame later, and since task3570 a seek that would land past the end with
/// repeat on is turned round to the start before it is even sent (see
/// [`seek_plan`]). So the verdicts below now speak for playback that *ran* out
/// of the selection -- or a skim released outside it -- rather than for a
/// playhead the user moved by hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeGuard {
    /// Nothing to do: no selection, inside it, degenerate, or a case that
    /// only applies while the engine is playing and it is not.
    None,
    /// Playing from before the start: forward to it, and keep playing.
    JumpToStart,
    /// Repeat's loop point: back to the start. `poll_range_guard_with` also
    /// restarts the engine if it was parked rather than playing.
    ///
    /// That branch stopped being the only reader in task3570: [`seek_plan`]
    /// asks for this same verdict about a *seek's* landing, and turns the seek
    /// round to the start rather than letting task3380's rule stop it outside
    /// the selection.
    WrapToStart,
    /// Repeat off and the end reached: stop, leaving the position on the end.
    PauseAtEnd,
}

/// Pure so the whole table is testable off the bin side -- including the rows
/// that only exist while the engine is *not* playing.
///
/// `playing` is "playback is running by both accounts" -- the engine's own
/// status *and* the UI's settled view of it (`owns_playing`). Only
/// `WrapToStart` may fire while it is false: parked at the live edge the engine
/// renders nothing, and a whole-session loop has to start it again or it would
/// run exactly once. `JumpToStart` / `PauseAtEnd` from there would instead drag
/// a user who paused at the live edge back to the selection's end on every
/// 100ms drain.
///
/// `skimming` is the centre hold's x2 (task3280): while it is held the
/// selection stops constraining the playhead at all, so a skim may sit outside
/// the selection *and* run out of it without being caught. It is deliberately
/// the same lever as `playing` -- both gate exactly `JumpToStart` and
/// `PauseAtEnd`, and neither touches `WrapToStart`, whose rules task3280 leaves
/// alone.
///
/// `hold_at_end` is the end-set-to-the-playhead latch (task4230, user ruling
/// 2026-09-09 案(B)): the chip / `i` `o` / playhead-button family put `end`
/// on the playhead without seeking, so the very next tick reads `position >=
/// end` -- and without this, playback stopped the instant the user said
/// 「ここまで」 while watching (round19 §3's 「再生しながら」). It clears
/// `PauseAtEnd` and nothing else: `JumpToStart` is not reachable from it (the
/// family never puts the playhead before the start) and `WrapToStart` wins
/// before it, exactly as it does over `skimming`. The latch has to be sticky --
/// the tick after it the position is past the end and the condition still
/// holds -- so its release rules live beside the field, `Review::hold_at_end`.
///
/// Read the same coordinate the other way in
/// [`crate::ui_state::timeline::may_resume_after_range_edit`]: a band drag's
/// release standing on `end` stays stopped. Both are right, because the two
/// got there differently -- see that doc's 2026-09-25 note.
///
/// [`seek_plan`] always passes `false`: a seek's landing is a different
/// question (task3380), and any seek releases the latch anyway.
///
/// `transport` carries the three flags [`range_guard`] and [`seek_plan`] both
/// read, so a call site cannot swap two adjacent bools (t261025-730e).
pub fn range_guard(
    transport: TransportState,
    hold_at_end: bool,
    range: Option<(i64, i64)>,
    position_100ns: i64,
) -> RangeGuard {
    let TransportState {
        playing,
        skimming,
        repeat,
    } = transport;
    let Some((start, end)) = range else {
        return RangeGuard::None;
    };
    // A degenerate (empty) selection has no inside; acting on it seek-loops.
    if start >= end {
        return RangeGuard::None;
    }
    // A skim is an explicit look at what is where, so it is exempt for the
    // same reason task3150 left an explicit seek while paused exempt: take
    // that away and there is no way left to see outside the selection.
    let constrained = playing && !skimming;
    if position_100ns >= end {
        return if repeat {
            RangeGuard::WrapToStart
        } else if constrained && !hold_at_end {
            RangeGuard::PauseAtEnd
        } else {
            RangeGuard::None
        };
    }
    if position_100ns < start && constrained {
        return RangeGuard::JumpToStart;
    }
    RangeGuard::None
}

/// Whether a chip / `i` `o` / playhead-button press arms the end hold of
/// [`range_guard`] (t260926-c0b3, user ruling 2026-09-26 on task4230's
/// residue).
///
/// `playing` is the UI's account, and a live park drops it: parked at the live
/// edge the engine reports `playing: false` until the recording grows and it
/// resumes on its own (`park` -> `park_resume`, about 3 s after End). A press
/// in that window used to arm nothing, so the resume ran straight into
/// `PauseAtEnd` 37-40ms later (task4230's live-o.ps1, 2/2). The park is still
/// playback the user never stopped, so it arms too -- but only while
/// `recording`: an ended session parked at its end is never resumed by the
/// engine, and `at_live_edge` alone cannot tell a park from a paused seek to
/// the edge (the worker's `parked_from_play` note). A deliberate pause there
/// arming the hold is harmless anyway: release (1) drops it as soon as the
/// edge moves on, and any Play from the end seeks.
pub fn arms_end_hold(playing: bool, at_live_edge: bool, recording: bool) -> bool {
    playing || (at_live_edge && recording)
}

/// Whether the clip player should loop back to the head (t260916-0ee4).
///
/// [`range_guard`] cannot answer this, and the reason is not the missing
/// selection: hand it the whole clip as its range and it still says nothing,
/// because it keys the end on `position_100ns >= end` and the engine never
/// leaves the playhead there. `Worker::park_at_live_edge` puts it on
/// `live_edge_100ns - 1`, one tick *short* of the length, at exactly the
/// moment repeat has to fire. What does say so is `at_live_edge`, which the
/// park raises and which `range_guard` does not take.
///
/// Each input can veto, and each is needed:
///
/// - `repeat`: the toggle. Off, the park is the end of playback, as before.
/// - `was_playing`: what the transport said on the *previous* pump. This is
///   what tells a park apart from a pause, the engine parking only out of
///   playback -- without it, a clip the user deliberately paused on its last
///   frame would be restarted by turning the toggle on.
/// - `playing`: the transport now, folded the way the glyph is
///   ([`owns_playing`]'s latch over the engine's own flag) rather than the raw
///   status. The latch is what keeps this from firing again on every ring
///   between the `Play` it causes and the worker taking it: for that whole
///   window `at_live_edge` is still up and the raw flag still false.
/// - `at_live_edge`: the park itself.
/// - `owns_position`: a seek this side sent that the engine has not echoed
///   leaves the status describing where the transport was before it
///   (task2910), `at_live_edge` included.
///
/// What the caller does with a `true` is send `Play` and nothing else: from a
/// transport parked on the last playable instant that is already a restart
/// from the head ([`Worker::restarts_from_head`]'s own rule, which is why
/// pressing play there has always started the clip over).
pub fn clip_repeat_wrap(
    repeat: bool,
    was_playing: bool,
    playing: bool,
    at_live_edge: bool,
    owns_position: bool,
) -> bool {
    repeat && was_playing && !playing && at_live_edge && !owns_position
}

/// What [`seek_plan`] tells `Review::seek_clamped` to do with a seek whose
/// landing the selection has something to say about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekPlan {
    /// Send the seek as classified; the transport is untouched.
    ///
    /// Since task3660 this is also where a *skim* ends up when repeat's wrap
    /// cannot find a landing inside the selection: the recursion bound still
    /// refuses a second hop, but refusing it no longer stops the gesture.
    Go,
    /// task3380: stop playback *before* the `Seek` goes out, then seek to the
    /// classified landing and stay there, so it can be scrubbed.
    ///
    /// Only for a seek, never for a skim in flight -- the exemption reaches
    /// every row that would answer this, including task3570's wrap fallback
    /// since task3660.
    PauseFirst,
    /// task3570: repeat is on and playback is running, so the seek goes to the
    /// selection's start instead and playback carries on. The payload is where
    /// `classify_seek` actually puts that start, not the raw boundary.
    WrapTo(i64),
}

/// What a seek landing on `landing_100ns` has to do about the selection
/// (task3380, task3570).
///
/// Leaving the selection while playing used to be answered a frame or two
/// *after* the seek: the engine decoded the new position and sounded it, and
/// the next [`range_guard`] verdict dragged the playhead back to the
/// selection's start. The user rejected that on 2026-09-07 -- a seek out of the
/// selection should pause and stay where it landed, so it can be scrubbed.
/// Deciding it here, before the `Seek` is sent, is what keeps that frame
/// silent; deciding it afterwards is exactly the bug.
///
/// The same user, the same day, took one row back out of that rule: with
/// repeat on, a seek past the end should *wrap* and keep playing rather than
/// stop outside the selection. 3380's guarantee survives it, because the answer
/// is still given before the `Seek` goes out -- the target is replaced, not
/// chased a frame later -- so the engine still never decodes or sounds a frame
/// outside the selection.
///
/// It is [`range_guard`]'s own question asked about the seek's landing instead
/// of the current position, so the two can never disagree about where
/// "outside" is.
///
/// The one argument pinned on the way through:
///
/// - `playing` is *this side's* transport and is deliberately not passed on as
///   `range_guard`'s own `playing`. `WrapToStart` is not gated by that flag (a
///   parked engine still wraps, task153), so threading it would make a seek
///   issued while paused, with repeat on, wrap -- and a seek out of the
///   selection while paused is task3150's explicit "look outside it", which
///   the gate below keeps.
///
/// `skimming` used to be pinned false beside it, on the reasoning that the
/// centre hold's x2 never seeks -- it only changes the rate -- so this path had
/// no skim to exempt. That was wrong about the gesture rather than about the
/// hold: the hold itself sends no seek, but **a key pressed while it is held
/// does**, and it arrives right here. So a skim that ran off the end because
/// the user tapped -> was stopped by task3380 mid-gesture, which is the one
/// thing task3280's 案(A) settled against -- **the exemption is about the
/// gesture, not about which route inside it moved the playhead**. task3580
/// threads the real value, and `Review::skimming` is the single place both this
/// and the guard's own polling compute it.
///
/// It reaches both `range_guard` calls below, and since task3660 the wrap's
/// own fallback as well. The exemption clears exactly `JumpToStart` and
/// `PauseAtEnd`, never `WrapToStart`, so with repeat on a skim past the end
/// still turns round to the selection's start and carries on -- the same shape
/// `poll_range_guard_with` has always given a skim that ran over the end.
///
/// `gesturing` is task3650's own flag and is deliberately *not* folded into
/// `skimming`: the two say different things. A skim is exempt from
/// [`range_guard`]'s verdict itself; `gesturing` leaves every verdict alone and
/// changes only where `WrapToStart` lands -- no wrap is attempted while a
/// gesture is in flight, and the seek takes the stop. The set is the one
/// `poll_range_guard_with` already excludes from `owns_position`
/// (`dragging || range_dragging`), for the same reason: the selection's rules
/// apply once the gesture is finished, not during it. Where they meet, the
/// skim wins (`gesturing && !skimming`).
///
/// `wrapped` resolves where `classify_seek` actually puts the selection's
/// start; only the bin side holds the snapshot to ask. **It is the recursion
/// bound, in code**: `FnOnce`, called at most once and only on the
/// `WrapToStart` row, and its answer is put to `range_guard` here rather than
/// fed back into the caller. A gap swallowing the whole selection snaps the
/// start past the end, and that second verdict is `WrapToStart` all over
/// again -- so the wrap is given up instead of re-entering `seek_clamped`,
/// which would recurse until the stack ran out. The hop is one deep and cannot
/// be two, without leaning on "the start is inside the selection" -- an
/// assumption the gap snap is free to break.
///
/// task3660 kept that bound and changed only where giving up lands: a seek
/// still takes `PauseFirst`, but a skim takes `Go` and keeps running. Stopping
/// it would have let one pathological selection end a gesture the exemption
/// exists to protect -- and the bound is about recursion, not about the
/// transport.
pub fn seek_plan(
    transport: TransportState,
    gesturing: bool,
    range: Option<(i64, i64)>,
    landing_100ns: i64,
    wrapped: impl FnOnce(i64) -> Option<i64>,
) -> SeekPlan {
    let TransportState {
        playing, skimming, ..
    } = transport;
    if !playing {
        return SeekPlan::Go;
    }
    match range_guard(transport, false, range, landing_100ns) {
        RangeGuard::None => SeekPlan::Go,
        RangeGuard::JumpToStart | RangeGuard::PauseAtEnd => SeekPlan::PauseFirst,
        RangeGuard::WrapToStart => {
            // A verdict at all implies a selection, so the fallback is
            // unreachable; it takes the stop rather than a panic.
            let Some((start, _)) = range else {
                return SeekPlan::PauseFirst;
            };
            // task3650's gate, and the only thing it touches: while a track or
            // range-edge gesture is in flight the wrap is not attempted at all,
            // and the seek takes task3380's stop instead. `on_track_moved`
            // calls `seek_clamped` on every pointer move, so a `WrapTo` here
            // pinned the playhead to the selection's start for the whole drag
            // -- the behaviour task3570 introduced and this undoes. The rule of
            // the selection applies once the gesture is over, which is the same
            // reason `poll_range_guard_with` excludes `dragging ||
            // range_dragging` from `owns_position`; the set is deliberately the
            // same one.
            //
            // `&& !skimming` is load-bearing: a skim IS a gesture, and the gap
            // snap can answer `WrapToStart` whatever `skimming` says (see
            // `range_guard` above). Letting the gate win there would stop a
            // skim on the recursion bound -- precisely what task3660 fixed and
            // what task3280's 案(A) rules out. The skim wins, so the gate is
            // asked second.
            //
            // What it costs, decided by the user on 2026-09-09 (task3650): a
            // press cannot be told apart from the start of a drag, so a plain
            // track *click* past the end is caught too and stops at the click
            // point rather than wrapping. That withdraws task3570's real-machine
            // scenario 3 for the track click alone; the range panel's 終了へ,
            // keys and markers set no `dragging` and still wrap.
            if gesturing && !skimming {
                return SeekPlan::PauseFirst;
            }
            // `skimming` again, for one contract rather than two: the same
            // gesture is still in flight, so the same rules apply to where the
            // wrap lands. It barely bites -- the only landing that can fail
            // this check in practice is a start the gap snap carried to or past
            // the end, which is `WrapToStart` whatever `skimming` says, so both
            // values reach the fallback below. That fallback is task3570's
            // recursion bound: the hop is given up rather than tried again.
            //
            // Where it lands is what task3660 split. The bound stays -- one
            // hop, never two -- but a skim is not stopped by it: it is a
            // gesture, and task3280's 案(A) exempts the gesture rather than the
            // route that moved the playhead inside it. So the skim takes `Go`
            // and carries on to the landing it was classified for; only a
            // non-skim seek still falls back to task3380's stop, which is what
            // keeps 3380/3570's guarantee that the engine never decodes or
            // sounds a frame the user did not ask for.
            //
            // What `Go` costs, so nobody "fixes" it back: the skim lands
            // outside the selection, the next ack makes
            // `poll_range_guard_with` answer `WrapToStart` there, that calls
            // `seek_clamped(start)`, the gap snap carries it outside again and
            // the answer is `Go` once more. The skim does not stop -- it sticks
            // to the edge of the gap. That is exactly the state this
            // pathological selection was in *before* task3570 (an idle seek
            // per frame), so it is not a regression and not a defect; the
            // ping-pong belongs to `poll_range_guard_with`'s branches, not
            // here.
            match wrapped(start) {
                Some(landing)
                    if range_guard(transport, false, range, landing) == RangeGuard::None =>
                {
                    SeekPlan::WrapTo(landing)
                }
                _ if skimming => SeekPlan::Go,
                _ => SeekPlan::PauseFirst,
            }
        }
    }
}

/// Whether a stage gesture in flight is the centre hold's skim (task3280),
/// which [`range_guard`] is exempt from.
///
/// Since task3580 that exemption covers the whole gesture rather than only the
/// guard's own polling: [`seek_plan`] takes this same answer, so a seek issued
/// *during* a hold -- a key tap, the only route that can move the playhead
/// while the centre is held -- is no longer stopped by task3380's rule for
/// landing outside the selection. Nothing about the gesture changed; the set of
/// places that ask about it did.
///
/// task3580 left one place still stopping a skim, and task3660 closed it: when
/// repeat's wrap cannot land inside the selection, `seek_plan` gives the hop up
/// (task3570's recursion bound, untouched) but now falls to `Go` for a skim
/// instead of the stop. So every reader of this answer -- the guard's polling,
/// the seek's landing, and the wrap's fallback -- exempts the same gesture.
///
/// `_repause` -- whether the hold started on a paused video -- is taken and
/// deliberately not read. 案(C) (exempt only the skim that started paused,
/// because a hold begun while playing is "a kind of playback") was put to the
/// user on 2026-09-07 and rejected in favour of 案(A): the exemption is about
/// the gesture, not about what the transport happened to be doing when it
/// started. Keeping the parameter is what lets the table below assert that
/// both values give the same answer, rather than leaving it unwritten.
pub fn stage_skimming(zone: StageZone, held: bool, _repause: bool) -> bool {
    held && matches!(zone, StageZone::Center)
}

// ---------- stage gestures (task108 + task119) ----------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageZone {
    Left,
    Center,
    Right,
}

/// Where a press landed inside the picture, as a fraction of the picture's
/// width (task1010). `x`/`y` are relative to the video box's top-left corner,
/// which the stage's `contain` fit leaves centred in a field of letterbox.
///
/// `None` for the letterbox itself: the black either side is not the video,
/// and a tap there used to seek ±10s because the zone split was measured
/// against the whole stage. Measuring against the picture is also what puts
/// the ±10s zones back on the outer quarters of what is actually on screen.
pub fn stage_hit(x: f32, y: f32, width: f32, height: f32) -> Option<f64> {
    (width > 0.0 && height > 0.0 && (0.0..width).contains(&x) && (0.0..height).contains(&y))
        .then(|| f64::from(x / width))
}

/// Whether this press pairs with the last centre tap into a double click
/// (task1010). Only the centre: the side zones are ±10s a tap, so two quick
/// taps there mean ±20s and nothing else.
pub fn stage_double_press(since_tap_ms: Option<u64>, zone: StageZone) -> bool {
    zone == StageZone::Center && since_tap_ms.is_some_and(|ms| ms <= STAGE_DOUBLE_PRESS_MS)
}

/// Zone is fixed at pointerdown so sliding mid-gesture can't turn a rewind
/// into a fast-forward.
pub fn stage_zone(ratio: f64) -> StageZone {
    if ratio < STAGE_SIDE_ZONE_RATIO {
        StageZone::Left
    } else if ratio > 1.0 - STAGE_SIDE_ZONE_RATIO {
        StageZone::Right
    } else {
        StageZone::Center
    }
}

impl StageZone {
    pub fn direction(&self) -> i64 {
        match self {
            StageZone::Left => -1,
            StageZone::Right => 1,
            StageZone::Center => 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StageFeedback {
    Tap {
        direction: i64,
        seconds: i64,
    },
    Hold {
        direction: i64,
        seconds: i64,
        steps: u32,
    },
    Clamped {
        at_start: bool,
    },
    Gap,
    Buffering,
    /// The centre hold's x2 (task650). Carries no rate: it is always
    /// `STAGE_HOLD_RATE`, and an `f64` in here would cost the enum its `Eq`.
    /// Unlike `Hold`, this one may wear its rate -- round2 §10 forbids a rate
    /// badge on repeated seeking, which promises playback it cannot deliver;
    /// this is that playback.
    RateHold,
}

impl StageFeedback {
    /// The one line under the circle for the three neutral outcomes.
    pub fn line(&self, locale: Locale) -> &'static str {
        match self {
            StageFeedback::Clamped { at_start: true } => stage_clamped_start(locale),
            StageFeedback::Clamped { at_start: false } => stage_clamped_live_edge(locale),
            StageFeedback::Gap => stage_gap_skipped(locale),
            StageFeedback::Buffering => stage_buffering(locale),
            _ => "",
        }
    }
}

/// Whether the stage should be showing its loading ring (task239).
///
/// Reaching here already means a session is open: the pane with nothing loaded
/// is `ReviewEmpty`, a different component entirely, so "no frame yet" here can
/// only mean the first one is still on its way. Any other stage feedback owns
/// the middle of the picture while it is up, so the ring stands down for it.
pub fn stage_loading(has_frame: bool, feedback: Option<StageFeedback>) -> bool {
    !has_frame && feedback.is_none()
}

/// Sync Mode (t260926-cc7e): the review screen following a recording's live
/// edge on its own, until the user does something that says otherwise.
///
/// It replaces reading "following" off the engine. `at_live_edge` does not
/// say "following": from task1800 to t261012-5b97 it was false for the whole of
/// a steady follow (two to three segments behind the newest frame), so a chip
/// built on it said 追従中 for the few seconds after End and then went dark
/// again; since then it flickers, true while the transport plays the newest
/// segment (about (D - m) / D of a steady follow) and false between. The mode is
/// what the user asked for instead, and the automatic moves (the cover gate's
/// resume, the stop skip, the marker hotkey's target) ask it rather than
/// guessing from whether the transport happens to be playing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Follow {
    /// Following the live edge: the chip reads ● ライブ追従中.
    pub on: bool,
    /// The selection's stops are off (`range_guard`'s `PauseAtEnd` /
    /// `JumpToStart`, and `toggle_play`'s jump to the start). Raised with the
    /// mode and dropped only by a seek the user makes -- a pause drops the mode
    /// but not this, so pausing and playing again carries on from where it is.
    pub range_exempt: bool,
}

/// What moves [`Follow`] (user ruling 2026-09-26, t260926-cc7e Context).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FollowEvent {
    /// The mode's own landing: the chip, End while recording, a live load,
    /// the cover gate's resume and the automatic rejoin.
    Join,
    /// The engine parked at the live edge out of playback -- the transport
    /// caught up by itself (x2, or a forward seek into the last segment).
    Arrive,
    /// A seek the user made: the track, a marker, the keys, a stage tap, a
    /// frame step. Not the mode's own seeks, the cover gate's, or the stop
    /// skip's.
    UserSeek,
    /// A pause the user asked for (the button, Space, a centre tap, a frame
    /// step). Not the cover gate's.
    UserPause,
    /// A rate chosen from the menu or `<` `>`. Not the centre hold's skim.
    Rate(f64),
}

impl Follow {
    pub fn apply(self, event: FollowEvent) -> Self {
        match event {
            FollowEvent::Join | FollowEvent::Arrive => Follow {
                on: true,
                range_exempt: true,
            },
            FollowEvent::UserSeek => Follow::default(),
            FollowEvent::UserPause => Follow { on: false, ..self },
            FollowEvent::Rate(rate) if rate != 1.0 => Follow { on: false, ..self },
            FollowEvent::Rate(_) => self,
        }
    }
}

/// How far behind the live edge a follow may fall before it is put back
/// (t260926-cc7e): `3D / 2 + m` since t261012-5b97, D being
/// `encoder::segment_duration_100ns()` and m `playback::LIVE_FOLLOW_MARGIN_100NS`
/// (3.25 s at 2 s segments, 1.75 s at 1 s).
///
/// It has to sit strictly between two lags:
/// - the most a steady follow reaches, D + m: the resume lead, which the lag
///   is back down to m by the time the next segment arrives;
/// - the lag a follow is stuck at after a park, 2D: a park at the edge resumes
///   only on the second arrival (one gives D, short of D + m), and from there
///   the lead runs between 2D and D, a segment further back than the landing.
///
/// The interval is not empty only while m < D / 2 -- recheck this if D or m
/// changes. `follow_rejoin_due` compares strictly (`lag > T`), so 2D itself
/// would never fire on the stuck follow. Rejoining from there lands on
/// `live_edge - (D + m) + phase`, a jump forward of about D - m (1.75 s at 2 s
/// segments). The steady follow's headroom is T - (D + m) = D / 2: a crossing
/// that stalls longer than that rejoins, which reading the newest segment from
/// memory is what makes rare.
///
/// Was 4D (a fixed 8 s before t261012-38b1) while the follow sat 2D-3D
/// behind a one-segment reserve. 2D + m, the old "steady maximum plus one
/// segment" shape, is not used: it never fires on the 2D a park leaves behind,
/// so that ~4.2 s lag would last until the user rejoined by hand.
pub fn follow_rejoin_lag_100ns() -> i64 {
    3 * crate::encoder::segment_duration_100ns() / 2 + crate::playback::LIVE_FOLLOW_MARGIN_100NS
}

/// Whether this tick puts a follow that fell behind back on its landing.
///
/// `playing` is required: the cover gate's pause freezes the position while
/// the edge moves on, and its own resume does the rejoin. `in_flight` is a
/// seek this side sent that the engine has not echoed (`owns_position`) --
/// a stalled engine is not handed a second seek on every tick, one per lag.
pub fn follow_rejoin_due(
    follow: Follow,
    recording: bool,
    playing: bool,
    lag_100ns: i64,
    in_flight: bool,
) -> bool {
    follow.on && recording && playing && !in_flight && lag_100ns > follow_rejoin_lag_100ns()
}

/// Whether the transport reached the live edge by itself this tick
/// ([`FollowEvent::Arrive`]).
///
/// `ui_playing` is the UI's own account *before* the pump folds the engine's
/// status into it: a park drops `status.playing`, and one frame later the UI
/// follows -- but a pause the user asked for has already dropped the UI's
/// account, so it never reads as an arrival. `in_flight` for the same reason
/// as [`follow_rejoin_due`]: after a seek away from a park the status still
/// says `at_live_edge` until the engine takes it.
pub fn follow_arrives(
    recording: bool,
    ui_playing: bool,
    at_live_edge: bool,
    in_flight: bool,
) -> bool {
    recording && ui_playing && at_live_edge && !in_flight
}

/// Where the marker hotkey lands (t260921-6ba9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyMarkTarget {
    /// The review pane's playhead -- the user rewound and is watching.
    Playhead,
    /// The capture's live edge -- task1090's "mark this moment".
    LiveEdge,
}

impl HotkeyMarkTarget {
    pub fn name(self) -> &'static str {
        match self {
            Self::Playhead => "playhead",
            Self::LiveEdge => "live_edge",
        }
    }
}

/// The playhead only when all four hold: the window is in front, the review
/// pane is the one showing, it has a session loaded, and the chip does not
/// read ● ライブ追従中. Since t260926-cc7e `live_badge_visible` is the Sync
/// Mode -- `Follow::on` of a session still recording -- rather than
/// `at_live_edge`, which comes and goes through a steady follow (true only
/// while the newest segment plays, t261012-5b97). An ended session
/// has no chip, and takes the playhead rather than being refused as "not
/// recording".
pub fn hotkey_mark_target(
    foreground: bool,
    on_review_pane: bool,
    loaded: bool,
    live_badge_visible: bool,
) -> HotkeyMarkTarget {
    if foreground && on_review_pane && loaded && !live_badge_visible {
        HotkeyMarkTarget::Playhead
    } else {
        HotkeyMarkTarget::LiveEdge
    }
}

/// Keeps the stage from being handed more than one frame per display refresh
/// (task1580).
///
/// On average, not strictly: `published` marks the period grid rather than the
/// moment of the hand-over (t260916-e610), so two frames 16.4ms apart on a
/// 16.67ms period are both allowed when the first one was itself late.
///
/// The engine ticks at the playback rate -- 114/s at x2 on a 57fps recording --
/// while the window redraws at the display's cadence. Every hand-over past that
/// replaces a picture nobody saw, and measured full screen it is not free:
/// `ui_set_stage_frame` costs 0.03ms when the display keeps up (windowed at any
/// rate, full screen at x1) and **3.75ms** when it does not (full screen at
/// x2), which alone was 58% of the UI thread. Frames turned away are not lost:
/// the engine's slot is latest-wins, so skipping shows a newer picture instead.
///
/// The gate is paced off the wall clock rather than off the renderer. Since
/// 68d0d0c0 (wgpu-30 renderer) `set_rendering_notifier` does fire on the
/// Direct3D path (measured: `RenderingSetup` n=1, `AfterRendering` n>=3),
/// but it still never fires under `LIVEBACK_RENDERER=software` (n=0) -- see
/// `.agents/docs/slint-ui-migration.md`. The period therefore comes from the
/// display's own refresh rate, not from a hardcoded 60Hz.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StageGate {
    /// Zero means no gating at all -- which is what an implausible refresh
    /// rate falls back to, so a bad answer can never freeze the stage.
    period: Duration,
    last_publish: Option<Instant>,
}

pub use crate::playback::{refresh_period, REFRESH_HZ_RANGE};

impl StageGate {
    /// A gate for a display refreshing at `hz`. Anything implausible leaves it
    /// wide open: a frame gate that is wrong in the other direction stalls the
    /// picture, and that is much worse than paying the cost this saves.
    pub fn for_refresh_hz(hz: u32) -> Self {
        Self {
            period: refresh_period(hz).unwrap_or_default(),
            last_publish: None,
        }
    }

    /// How early a frame may be and still count as belonging to the next
    /// refresh rather than to the one already on screen (t260916-e610).
    ///
    /// Without it the gate is a *hard* cap, and a hard cap fed at exactly its
    /// own rate loses whatever the jitter is: the engine's skip clamps publishes
    /// to the display period, so there is exactly one candidate per refresh and
    /// every one that arrives a hair early costs that refresh entirely --
    /// measured 2026-09-18, 60.8 publishes a second reaching the stage as 53-57.
    /// An eighth of a period is 2.1ms at 60Hz, far below a refresh and far above
    /// the jitter measured here.
    const EARLY_ENOUGH: u32 = 8;

    /// `None` to hand the stage the frame now. `Some(wait)` to leave it in the
    /// engine's slot, with how long until the gate opens -- the caller arms a
    /// timer for that, because when playback is paused a seek publishes exactly
    /// one frame and no further pump would ever come to collect it.
    pub fn take_turn(&mut self, now: Instant) -> Option<Duration> {
        if self.period.is_zero() {
            return None;
        }
        let opens = self.period - self.period / Self::EARLY_ENOUGH;
        match self.last_publish {
            Some(last) => {
                let since = now.saturating_duration_since(last);
                (since < opens).then(|| opens - since)
            }
            None => None,
        }
    }

    /// The stage has just been handed a picture.
    ///
    /// The mark goes on the period *grid*, not on `now`: `now` carries the wake
    /// jitter and, worse, the wait this gate itself just imposed, so a frame
    /// that was handed over late would shorten the window the next one gets.
    /// Stepping the grid instead means a late hand-over is self-correcting.
    ///
    /// Together with `EARLY_ENOUGH` this is what t260916-e610 fixed. Measured
    /// 2026-09-18 on a 60Hz primary: 61 publishes a second reached the stage as
    /// **33**, and the `ui_stage_gate_turned_away` counter accounted for every
    /// single one of the other 28 -- each with a frame sitting in the slot.
    /// It is the same correction `should_skip_frame` already applies on the
    /// engine side by measuring between deadlines rather than between wake
    /// times (`src/playback/worker.rs`).
    pub fn published(&mut self, now: Instant) {
        let (Some(last), false) = (self.last_publish, self.period.is_zero()) else {
            self.last_publish = Some(now);
            return;
        };
        let period = self.period.as_nanos();
        let since = now.saturating_duration_since(last).as_nanos();
        // The last grid line at or before `now` -- floor, never rounded up. A
        // mark rounded into the future would cost the frame behind this one its
        // own window, which is the beat this is here to stop. The one exception
        // is `max(1)`: a frame taken inside its tolerance has `since` under a
        // period, which floors to nothing, and its mark belongs on the line it
        // was early *for* -- up to `period / EARLY_ENOUGH` past `now`, never
        // more.
        let steps = (since / period).max(1);
        let stepped = u64::try_from(steps.saturating_mul(period)).unwrap_or(u64::MAX);
        self.last_publish = Some(
            last.checked_add(Duration::from_nanos(stepped))
                .unwrap_or(now),
        );
    }
}

/// What the review pane going off screen (another pane, or the whole window
/// stashed in the tray) does to the transport.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OffscreenTransport {
    Pause,
    Resume,
    Leave,
}

/// Decoding a 1080p session nobody is looking at costs the UI thread an 8MB
/// frame ~50 times a second, which is what a game on the same machine cannot
/// spare. Pause while the stage is out of sight and give the transport back on
/// return -- but only the pause *this* took: a pause the user asked for has to
/// survive any number of trips through the history.
pub fn offscreen_transport(on_screen: bool, playing: bool, owed: bool) -> OffscreenTransport {
    match (on_screen, playing, owed) {
        (false, true, false) => OffscreenTransport::Pause,
        (true, _, true) => OffscreenTransport::Resume,
        _ => OffscreenTransport::Leave,
    }
}

/// How long another window has to sit on top of ours before the transport
/// believes nobody is looking (task2800).
///
/// A window merely crossing the stage -- an alt-tab, a toast, a file dialog
/// opening and closing -- is not somebody leaving, and pausing on every pass
/// would be worse than never pausing at all. Uncovering is believed at once,
/// so coming back is never delayed by this.
pub const COVER_GRACE: Duration = Duration::from_millis(500);

/// Debounces the per-tick "something is fully on top of us" observation that
/// `desktop::is_window_covered` samples off Win32.
///
/// Split from that sampling so the rule lives here as a pure function: the
/// Win32 hit test is untestable, one uninterrupted stretch of `COVER_GRACE`
/// is not.
#[derive(Default)]
pub struct CoverGate {
    covered_since: Option<Instant>,
}

impl CoverGate {
    /// Feeds this tick's observation and answers whether the stage now counts
    /// as out of sight. `covered = false` clears the wait, so the grace period
    /// only ever measures one unbroken stretch -- a window that crosses and
    /// leaves starts the next one from scratch.
    pub fn hidden(&mut self, covered: bool, now: Instant) -> bool {
        if !covered {
            self.covered_since = None;
            return false;
        }
        let since = *self.covered_since.get_or_insert(now);
        now.saturating_duration_since(since) >= COVER_GRACE
    }
}

/// Folds a tap outcome into the previous feedback: repeated taps on the same
/// side add up; crossing to the other side starts over rather than
/// subtracting, because the number names the move about to happen.
pub fn fold_tap(
    previous: Option<StageFeedback>,
    outcome: SeekOutcome,
    delta_seconds: i64,
) -> StageFeedback {
    let direction = if delta_seconds < 0 { -1 } else { 1 };
    let magnitude = delta_seconds.abs();
    match outcome {
        SeekOutcome::Buffering => StageFeedback::Buffering,
        SeekOutcome::Clamped { at_start, .. } => StageFeedback::Clamped { at_start },
        SeekOutcome::Gap(_) => StageFeedback::Gap,
        SeekOutcome::Ok(_) => match previous {
            Some(StageFeedback::Tap {
                direction: previous_direction,
                seconds,
            }) if previous_direction == direction => StageFeedback::Tap {
                direction,
                seconds: seconds + magnitude,
            },
            _ => StageFeedback::Tap {
                direction,
                seconds: magnitude,
            },
        },
    }
}

/// A hold reports its running total instead, and never a rate: this is
/// repeated seeking, and a "5x" badge would promise continuous playback it
/// cannot deliver (round2 §10).
pub fn fold_hold(
    outcome: SeekOutcome,
    direction: i64,
    cumulative_seconds: i64,
    steps: u32,
) -> StageFeedback {
    match outcome {
        SeekOutcome::Buffering => StageFeedback::Buffering,
        SeekOutcome::Clamped { at_start, .. } => StageFeedback::Clamped { at_start },
        _ => StageFeedback::Hold {
            direction,
            seconds: cumulative_seconds,
            steps,
        },
    }
}

/// How many of the five dots are lit for `steps` hold repeats.
pub fn hold_dots_on(steps: u32) -> u32 {
    if steps == 0 {
        0
    } else {
        let rem = steps % STAGE_HOLD_DOTS;
        if rem == 0 {
            STAGE_HOLD_DOTS
        } else {
            rem
        }
    }
}

/// One tap's delta in 100ns.
pub fn tap_delta_100ns(zone: StageZone, seconds: i64) -> i64 {
    zone.direction() * seconds * HNS_PER_SECOND
}

/// Seconds the Nth hold repeat jumps (task2520). Doubles every five steps --
/// one full dot cycle per speed tier -- and caps at 8s, so a long hold ramps
/// 5x -> 40x while the 200ms cadence stays put. `step` is 1-based.
pub fn hold_step_seconds(step: u32) -> i64 {
    1i64 << ((step - 1) / 5).min(3)
}

pub fn hold_step_100ns(zone: StageZone, step: u32) -> i64 {
    zone.direction() * hold_step_seconds(step) * HNS_PER_SECOND
}

/// Milliseconds between arrow-key scan steps (task2540). The OS repeats at
/// ~30Hz; the scan walks at the stage hold's proven 5Hz so every step's
/// coarse frame is actually seen.
pub const KEY_SCAN_INTERVAL_MS: u64 = 200;

/// Delay after a transport key is released before the precise settle seek
/// confirms the exact frame (task2540). Shorter than the OS's initial repeat
/// delay would be wrong the other way round: the settle is armed on release
/// only, so a hold can never race it.
pub const KEY_SETTLE_MS: u64 = 250;

/// Whether the next key repeat may take a scan step (task2540): `None` is the
/// first step of a hold, otherwise the 200ms cadence gates it.
pub fn key_scan_due(since_last_ms: Option<u64>) -> bool {
    since_last_ms.is_none_or(|ms| ms >= KEY_SCAN_INTERVAL_MS)
}

/// Seconds the Nth key-scan step jumps (task2540): the tap's 5s base ramped
/// by the stage hold's tier multiplier (x1 -> x2 -> x4 -> x8 every five
/// steps, task2520) -- 5/10/20/40s, 25x -> 200x at the 200ms cadence.
/// Not task2520's 1s base: a tap moves 5s, and a hold that opened slower
/// than a tap would feel like a brake. `step` is 1-based.
pub fn key_scan_step_seconds(step: u32) -> i64 {
    5 * hold_step_seconds(step)
}

/// What a centre hold does to the transport (task650, revised task1010).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageHold {
    /// The rate to play at for as long as the button is down.
    pub applied: f64,
    /// The rate at pointerdown, owed back on release -- so a release puts
    /// back x0.5 or x1.5 rather than assuming x1.
    pub restore: f64,
    /// Whether the release also has to stop again. A hold that started paused
    /// is a skim, not a play command: it runs at x2 while held and hands back
    /// a paused video at whatever position it reached (task1010). Task650 let
    /// a paused hold do nothing at all, which read as the gesture being broken.
    pub repause: bool,
}

pub fn stage_hold_rate(playing: bool, current: f64) -> StageHold {
    StageHold {
        applied: STAGE_HOLD_RATE,
        restore: current,
        repause: !playing,
    }
}

/// 16:9, the shape the stage assumes until a frame says otherwise.
pub const DEFAULT_STAGE_ASPECT: f32 = 16.0 / 9.0;

/// The video box's shape (round5 §4-A-2). The overlay bar sits at the inside
/// bottom of that box, so an aspect that disagrees with the frame being painted
/// puts the bar across the middle of the picture -- which is why the caller
/// sets this from the same frame it hands to the stage, not on its own
/// schedule. A portrait source returns a portrait ratio; a zero dimension
/// (no frame yet, or a decode that produced nothing) falls back to 16:9.
pub fn stage_aspect(width: u32, height: u32) -> f32 {
    if width == 0 || height == 0 {
        return DEFAULT_STAGE_ASPECT;
    }
    width as f32 / height as f32
}

/// The physical pixels the stage draws the picture into, from the logical size
/// the layout reports and the window's scale factor (task990).
///
/// The playback engine resamples frames to this, so what reaches skia is
/// already the size it is drawn at. `None` while the stage has no size yet --
/// mid-layout, or a window being restored -- which leaves frames at full
/// resolution rather than resizing them to nothing.
pub fn stage_target(width: f32, height: f32, scale_factor: f32) -> Option<(u32, u32)> {
    let physical = |logical: f32| (logical * scale_factor).round().max(0.0) as u32;
    let (width, height) = (physical(width), physical(height));
    (width > 0 && height > 0).then_some((width, height))
}

/// Whether a seek lands on the live edge of a selection that ends there --
/// `>>` or End while a recording plays. [`seek_plan`] would stop it
/// (`PauseAtEnd`), but the engine parks there on its own and resumes as the
/// recording grows, so the stop only breaks that follow. Repeat keeps its wrap;
/// a hand-dragged end short of the edge keeps its stop.
pub fn lands_on_live_end(
    repeat: bool,
    range: Option<(i64, i64)>,
    live_edge_100ns: i64,
    landing_100ns: i64,
) -> bool {
    let live_end = live_edge_100ns - 1;
    !repeat && landing_100ns >= live_end && range.is_some_and(|(_, end)| end == live_end)
}

/// The session on the stage just stopped recording (t260917-fe49, user
/// ruling 2026-09-17: a stop skips to the live edge). `was_recording` is the
/// stage session id the previous poll saw recording; the edge needs the *same*
/// id now not recording. A bool alone would also fall when the stage switches
/// from a live session to a closed one, and landing an old recording at its end
/// the moment it opens is a bug of its own.
///
/// Only while following (t260926-cc7e, user ruling 2026-09-26): a transport
/// the user rewound keeps playing where it is, and a paused one stays put as
/// t260924-c0c5 ruled -- a user pause drops the mode, so that ruling is kept
/// by construction. The gate's pause does not drop it (fe49: the gate's pause
/// is not the user's pause), and neither does a live park, whose
/// `review.playing` is already false -- which is why this takes the mode
/// rather than `playing`. Judged once, here at the stop edge: the first skip
/// lands stopped at the end, so a per-tick check would drop the arm before the
/// tail.
pub fn stop_skip_arms(
    was_recording: Option<&str>,
    loaded: Option<&str>,
    recording: bool,
    following: bool,
) -> bool {
    !recording && following && was_recording.is_some_and(|id| loaded == Some(id))
}

/// What one tick does with an armed stop skip (t260917-fe49).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveSkip {
    /// Seek here -- the End key's own target, `live_edge - 1`.
    pub seek_to: Option<i64>,
    pub stay_armed: bool,
}

/// The stop is observed before the tail lands: `CaptureController` takes the
/// capture out of `active` first and only then joins the writer, so the last
/// segment reaches the manifest after `running` has already lost the id. The
/// skip is therefore armed at the stop and fired on every tick the live edge
/// moved past the last skip (`skipped_to`, `i64::MIN` when just armed), until
/// the manifest is closed *and* the snapshot holds its last segment
/// (`final_edge`, `Some` only for a closed manifest). A different session on
/// the stage drops the arm without seeking it.
pub fn live_skip(
    armed: Option<(&str, i64)>,
    session_id: &str,
    live_edge_100ns: i64,
    final_edge_100ns: Option<i64>,
) -> LiveSkip {
    let Some((id, skipped_to)) = armed else {
        return LiveSkip {
            seek_to: None,
            stay_armed: false,
        };
    };
    if id != session_id {
        return LiveSkip {
            seek_to: None,
            stay_armed: false,
        };
    }
    LiveSkip {
        seek_to: (skipped_to != live_edge_100ns).then_some(live_edge_100ns - 1),
        stay_armed: final_edge_100ns != Some(live_edge_100ns),
    }
}

#[cfg(test)]
mod tests;
