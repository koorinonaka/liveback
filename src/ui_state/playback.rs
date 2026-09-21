//! Pure playback state for the slint review screen (task128). The Media
//! Foundation engine (`crate::playback`) does the decoding; everything that
//! can be decided without touching MF -- transport transitions, seek
//! classification, gap crossing, live-edge clamping, and the video stage's
//! gesture/feedback machine (task119/round2 §10) -- lives here with tests.

use std::time::{Duration, Instant};

use crate::ui_state::locale::Locale;
use crate::ui_state::timeline::{TimelineSnapshot, HNS_PER_SECOND};

// ---------- stage constants (ReviewTimeline.tsx) ----------

/// Side zones are the outer 25% of the stage.
pub const STAGE_SIDE_ZONE_RATIO: f64 = 0.25;
pub const STAGE_TAP_SECONDS: i64 = 10;
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
/// pairing is done here instead and this is the window it uses.
pub const STAGE_DOUBLE_PRESS_MS: u64 = 400;
pub const STAGE_HINT_MAX_SHOWINGS: u32 = 3;
pub const STAGE_HINT_DELAY_MS: u64 = 1200;
pub const STAGE_HINT_DURATION_MS: u64 = 2800;

/// One row of the review screen's track mixer (task1300).
#[derive(Clone, Debug, PartialEq)]
pub struct MixerTrack {
    pub name: String,
    /// 0.0 to 1.0, as the slider draws it.
    pub volume_ratio: f32,
    pub muted: bool,
}

crate::tr! {
    /// Track 0 is always the capture target, and a recording that has extra
    /// tracks is usually recording a game -- naming the row after the
    /// executable would be the least useful thing on the panel.
    mixer_section { ja: "音声トラック", en: "Audio tracks" }
    mixer_target_track { ja: "対象アプリ", en: "Capture target" }
    /// A track whose executable the manifest did not record. Should not
    /// happen; says so plainly rather than showing an empty row.
    mixer_unnamed_track { ja: "トラック", en: "Track" }
}

/// The mixer rows for a session: one per recorded audio track, with whatever
/// levels this playback session has been given.
///
/// Fewer than two tracks means no mixer at all -- one track is what every
/// recording had before task1260, and a mixer with one row is a second volume
/// slider for the same sound.
pub fn mixer_tracks(locale: Locale, names: &[String], volumes: &[(i64, bool)]) -> Vec<MixerTrack> {
    if names.len() < 2 {
        return Vec::new();
    }
    names
        .iter()
        .enumerate()
        .map(|(track, name)| {
            let (percent, muted) = volumes.get(track).copied().unwrap_or((100, false));
            MixerTrack {
                name: if track == 0 {
                    mixer_target_track(locale).to_string()
                } else if name.trim().is_empty() {
                    format!("{} {}", mixer_unnamed_track(locale), track + 1)
                } else {
                    name.clone()
                },
                volume_ratio: (percent.clamp(0, 100) as f32) / 100.0,
                muted,
            }
        })
        .collect()
}

crate::tr! {
    stage_zone_seconds { ja: "10秒", en: "10s" }
    stage_hint {
        ja: "左右をクリックで10秒移動、長押しで早送り。中央で再生／一時停止、長押しで倍速。",
        en: "Click either side to jump 10s, hold to scan. Click the middle to play or pause, hold for double speed."
    }
    stage_clamped_start { ja: "先頭です", en: "At the start" }
    stage_clamped_live_edge { ja: "最新位置です", en: "At the live edge" }
    stage_gap_skipped { ja: "録画のない区間を飛ばしました", en: "Skipped a stretch with no recording" }
    stage_buffering { ja: "読み込み中", en: "Loading" }
    // "Waiting" until task1530, when live follow stopped waiting: the reserve
    // keeps a finalized segment ahead of the playhead, so the transport plays
    // continuously about a segment behind the newest frame instead of parking
    // on every crossing. The badge stands for both -- what it claims is that
    // there is nothing newer this transport can show.
    live_edge_badge { ja: "ライブ端を追従中", en: "Following the live edge" }
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

// ---------- range guard ----------

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
pub fn range_guard(
    playing: bool,
    skimming: bool,
    repeat: bool,
    range: Option<(i64, i64)>,
    position_100ns: i64,
) -> RangeGuard {
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
        } else if constrained {
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

/// Whether the clip player should loop back to the head (t260916-0ee4).
///
/// [`range_guard`] cannot answer this, and the reason is not the missing
/// selection: hand it the whole clip as its range and it still says nothing,
/// because it keys the end on `position_100ns >= end` and the engine never
/// leaves the playhead there. `Worker::park_at_live_edge` puts it on
/// `playable_edge_100ns() - 1`, one tick *short* of the length, at exactly the
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
    playing: bool,
    skimming: bool,
    gesturing: bool,
    repeat: bool,
    range: Option<(i64, i64)>,
    landing_100ns: i64,
    wrapped: impl FnOnce(i64) -> Option<i64>,
) -> SeekPlan {
    if !playing {
        return SeekPlan::Go;
    }
    match range_guard(true, skimming, repeat, range, landing_100ns) {
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
                    if range_guard(true, skimming, repeat, range, landing) == RangeGuard::None =>
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

/// Whether the transport shows the ライブ端を追従中 pill (task2610).
///
/// `at_live_edge` really means "parked at the playable edge": the worker raises
/// it for an ended session that ran out of playlist too, and the whole-session
/// repeat wrap depends on that reading. The pill is the one consumer that means
/// it literally -- there is no live edge to follow once the recording stopped --
/// so the liveness gate lives here, not in the worker.
pub fn live_edge_badge_visible(at_live_edge: bool, session_is_live: bool) -> bool {
    at_live_edge && session_is_live
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

/// Refresh rates outside this range are not believable answers from the
/// display, and are treated as "no answer" rather than trusted.
pub const REFRESH_HZ_RANGE: std::ops::RangeInclusive<u32> = 20..=1000;

impl StageGate {
    /// A gate for a display refreshing at `hz`. Anything implausible leaves it
    /// wide open: a frame gate that is wrong in the other direction stalls the
    /// picture, and that is much worse than paying the cost this saves.
    pub fn for_refresh_hz(hz: u32) -> Self {
        Self {
            period: if REFRESH_HZ_RANGE.contains(&hz) {
                Duration::from_nanos(1_000_000_000 / u64::from(hz))
            } else {
                Duration::default()
            },
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

pub fn tap_seconds_text(direction: i64, seconds: i64) -> String {
    format!("{}{}", if direction < 0 { "−" } else { "+" }, seconds)
}

pub fn hold_seconds_text(locale: Locale, direction: i64, seconds: i64) -> String {
    let sign = if direction < 0 { "−" } else { "+" };
    match locale {
        Locale::Ja => format!("{sign}{seconds}秒"),
        Locale::En => format!("{sign}{seconds}s"),
    }
}

/// One tap's delta in 100ns.
pub fn tap_delta_100ns(zone: StageZone) -> i64 {
    zone.direction() * STAGE_TAP_SECONDS * HNS_PER_SECOND
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

#[cfg(test)]
mod mixer_tests {
    use super::*;

    #[test]
    fn a_single_track_recording_has_no_mixer_at_all() {
        // Every recording made before task1260, and every monitor recording.
        assert!(mixer_tracks(Locale::Ja, &[], &[]).is_empty());
        assert!(mixer_tracks(Locale::Ja, &["ffxiv_dx11.exe".into()], &[]).is_empty());
    }

    #[test]
    fn track_zero_is_named_for_its_role_and_the_rest_for_their_executable() {
        let rows = mixer_tracks(
            Locale::Ja,
            &["ffxiv_dx11.exe".into(), "Discord.exe".into()],
            &[],
        );
        assert_eq!(rows.len(), 2);
        // The target's own row says what it *is*: a recording with extra
        // tracks is usually recording a game, and its executable name is the
        // least useful label on the panel.
        assert_eq!(rows[0].name, "対象アプリ");
        assert_eq!(rows[1].name, "Discord.exe");
        // Untouched tracks are full volume, unmuted.
        assert!(rows.iter().all(|row| row.volume_ratio == 1.0 && !row.muted));
    }

    #[test]
    fn levels_and_mutes_come_from_the_session_state() {
        let rows = mixer_tracks(
            Locale::En,
            &["game.exe".into(), "chrome.exe".into(), "chat.exe".into()],
            &[(100, false), (40, false), (100, true)],
        );
        assert_eq!(rows[0].volume_ratio, 1.0);
        assert!((rows[1].volume_ratio - 0.4).abs() < 1e-6);
        assert!(rows[2].muted);
        assert_eq!(rows[0].name, "Capture target");
    }

    /// A manifest that recorded a track without a name still gets a row --
    /// silently dropping it would renumber every track after it.
    #[test]
    fn a_nameless_track_still_gets_a_row() {
        let rows = mixer_tracks(Locale::En, &["game.exe".into(), "  ".into()], &[]);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].name, "Track 2");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_state::timeline::TimelineSegment;
    use std::cell::Cell;

    #[test]
    fn stage_target_scales_the_logical_size_by_the_scale_factor() {
        assert_eq!(stage_target(1200.0, 675.0, 1.0), Some((1200, 675)));
        assert_eq!(stage_target(800.0, 450.0, 1.5), Some((1200, 675)));
        assert_eq!(stage_target(600.0, 337.5, 2.0), Some((1200, 675)));
    }

    /// Slint reports a `length` at subpixel precision, so the physical size
    /// rounds rather than truncating -- a stage 0.6px wider is a pixel wider.
    #[test]
    fn stage_target_rounds_to_whole_physical_pixels() {
        assert_eq!(stage_target(1199.6, 674.6, 1.0), Some((1200, 675)));
        assert_eq!(stage_target(1199.4, 674.4, 1.0), Some((1199, 674)));
    }

    #[test]
    fn stage_target_is_absent_until_the_stage_has_a_size() {
        assert_eq!(stage_target(0.0, 0.0, 1.0), None);
        assert_eq!(stage_target(1200.0, 0.0, 1.0), None);
        assert_eq!(stage_target(0.0, 675.0, 1.0), None);
        assert_eq!(stage_target(-5.0, 675.0, 1.0), None);
    }

    /// Task2610: the pill needs both the park and a session that is still
    /// recording -- an ended session parked at its end shows nothing.
    #[test]
    fn the_live_edge_badge_needs_both_the_edge_and_a_live_session() {
        assert!(live_edge_badge_visible(true, true));
        assert!(!live_edge_badge_visible(true, false));
        assert!(!live_edge_badge_visible(false, true));
        assert!(!live_edge_badge_visible(false, false));
    }

    #[test]
    fn the_stage_only_waits_when_a_loaded_session_has_no_frame_yet() {
        // Loaded, nothing decoded yet: this is the whole reason the ring exists.
        assert!(stage_loading(false, None));
        // The first frame landed.
        assert!(!stage_loading(true, None));
        // A seek's circle is already in the middle of the picture; two marks
        // stacked there read as a glitch.
        assert!(!stage_loading(
            false,
            Some(StageFeedback::Tap {
                direction: 1,
                seconds: 5
            })
        ));
        assert!(!stage_loading(false, Some(StageFeedback::Buffering)));
    }

    #[test]
    fn a_window_crossing_the_stage_does_not_count_as_hidden() {
        // task2800: alt-tab, a toast, a dialog opening and closing. Each is a
        // cover that ends well inside the grace period, and pausing on every
        // one of them would be worse than never pausing.
        let mut gate = CoverGate::default();
        let start = Instant::now();
        assert!(!gate.hidden(true, start));
        assert!(!gate.hidden(true, start + Duration::from_millis(300)));
        assert!(!gate.hidden(false, start + Duration::from_millis(400)));
        // The next cover starts its own stretch rather than resuming the last.
        let second = start + Duration::from_millis(500);
        assert!(!gate.hidden(true, second));
        assert!(!gate.hidden(true, second + Duration::from_millis(400)));
    }

    #[test]
    fn a_cover_that_holds_for_the_grace_period_counts_as_hidden() {
        let mut gate = CoverGate::default();
        let start = Instant::now();
        assert!(!gate.hidden(true, start));
        assert!(gate.hidden(true, start + COVER_GRACE));
        // Still covered on later ticks: still hidden, so the caller's
        // `offscreen_transport` sees a steady state and does not re-send.
        assert!(gate.hidden(true, start + COVER_GRACE * 4));
    }

    #[test]
    fn uncovering_is_believed_at_once() {
        let mut gate = CoverGate::default();
        let start = Instant::now();
        gate.hidden(true, start);
        assert!(gate.hidden(true, start + COVER_GRACE));
        // No grace on the way back -- the wait exists to avoid pausing too
        // eagerly, and resuming late would be its own bug.
        assert!(!gate.hidden(false, start + COVER_GRACE));
    }

    #[test]
    fn a_cover_that_holds_pauses_and_returning_resumes() {
        // The gate feeding `offscreen_transport` is the whole task2800 path:
        // covered long enough -> Pause (and the resume is owed), uncovered ->
        // Resume.
        let mut gate = CoverGate::default();
        let start = Instant::now();
        let on_review = |gate: &mut CoverGate, covered, now| !gate.hidden(covered, now);

        assert_eq!(
            offscreen_transport(on_review(&mut gate, true, start), true, false),
            OffscreenTransport::Leave
        );
        assert_eq!(
            offscreen_transport(on_review(&mut gate, true, start + COVER_GRACE), true, false),
            OffscreenTransport::Pause
        );
        assert_eq!(
            offscreen_transport(
                on_review(&mut gate, false, start + COVER_GRACE),
                false,
                true
            ),
            OffscreenTransport::Resume
        );
    }

    #[test]
    fn a_pause_the_user_asked_for_survives_a_cover() {
        // `owed` stays false because this pause was never ours to give back.
        let mut gate = CoverGate::default();
        let start = Instant::now();
        assert_eq!(
            offscreen_transport(!gate.hidden(true, start), false, false),
            OffscreenTransport::Leave
        );
        assert_eq!(
            offscreen_transport(!gate.hidden(true, start + COVER_GRACE), false, false),
            OffscreenTransport::Leave
        );
        assert_eq!(
            offscreen_transport(!gate.hidden(false, start + COVER_GRACE * 2), false, false),
            OffscreenTransport::Leave
        );
    }

    #[test]
    fn offscreen_pauses_playback_and_gives_it_back_on_return() {
        // Playing, then hidden -> pause, and the return owes a resume.
        assert_eq!(
            offscreen_transport(false, true, false),
            OffscreenTransport::Pause
        );
        assert_eq!(
            offscreen_transport(true, false, true),
            OffscreenTransport::Resume
        );
        // Already paused by hand: leaving owes nothing, so returning must not
        // start playing something the user stopped.
        assert_eq!(
            offscreen_transport(false, false, false),
            OffscreenTransport::Leave
        );
        assert_eq!(
            offscreen_transport(true, false, false),
            OffscreenTransport::Leave
        );
        // Still hidden on a later tick: the pause is not re-sent.
        assert_eq!(
            offscreen_transport(false, true, true),
            OffscreenTransport::Leave
        );
    }

    /// [0,2s) + gap + [3s,5s), live edge 5s.
    fn fixture() -> TimelineSnapshot {
        TimelineSnapshot {
            audio_tracks: Vec::new(),
            session_id: "session-playback".into(),
            segments: vec![
                TimelineSegment {
                    index: 1,
                    start_100ns: 0,
                    end_100ns: 20_000_000,
                    audio_offsets_100ns: vec![0],
                },
                TimelineSegment {
                    index: 2,
                    start_100ns: 30_000_000,
                    end_100ns: 50_000_000,
                    audio_offsets_100ns: vec![0],
                },
            ],
            gaps: Vec::new(),
            live_edge_100ns: 50_000_000,
            target_title: None,
            target_executable: None,
            target_executable_path: None,
        }
    }

    #[test]
    fn the_ui_owns_position_until_the_engine_acks_the_seek() {
        // Nothing sent yet: the engine owns it, as it always did.
        assert!(!owns_position(false, None, None));
        assert!(!owns_position(false, None, Some(10)));
        // Sent and unacknowledged -- the status still holds the old position.
        assert!(owns_position(false, Some(20), None));
        assert!(owns_position(false, Some(20), Some(10)));
        // Acknowledged: hand it back.
        assert!(!owns_position(false, Some(20), Some(20)));
        // Dragging is unchanged by any of it.
        assert!(owns_position(true, None, None));
        assert!(owns_position(true, Some(20), Some(20)));
    }

    #[test]
    fn the_ui_owns_playing_until_the_engine_catches_up_or_the_latch_expires() {
        // Nothing sent: the engine owns the transport, as it always did.
        assert!(!owns_playing(None, 0, true));
        assert!(!owns_playing(None, 0, false));
        assert!(!owns_playing(None, 10_000, true));

        // A `Pause` the engine has not run yet -- the flicker this exists for.
        assert!(owns_playing(Some(false), 0, true));
        assert!(owns_playing(Some(false), 16, true));
        // It ran: the latch is spent, whatever the clock says.
        assert!(!owns_playing(Some(false), 0, false));
        assert!(!owns_playing(Some(false), 16, false));
        // Never ran: the display goes back to the truth rather than lying on.
        assert!(!owns_playing(Some(false), PLAY_INTENT_MS + 1, true));

        // Symmetric for `Play` -- the live-edge route that may never start.
        assert!(owns_playing(Some(true), 0, false));
        assert!(owns_playing(Some(true), 16, false));
        assert!(!owns_playing(Some(true), 0, true));
        assert!(!owns_playing(Some(true), 16, true));
        assert!(!owns_playing(Some(true), PLAY_INTENT_MS + 1, false));
    }

    /// The deadline is inclusive, and it is the last millisecond of the lie.
    #[test]
    fn the_playing_latch_expires_at_its_deadline() {
        assert_eq!(PLAY_INTENT_MS, 250);
        assert!(owns_playing(Some(false), PLAY_INTENT_MS - 1, true));
        assert!(owns_playing(Some(false), PLAY_INTENT_MS, true));
        assert!(!owns_playing(Some(false), PLAY_INTENT_MS + 1, true));
        // However long the engine stays wrong, the display does not follow it
        // there.
        assert!(!owns_playing(Some(true), u64::MAX, false));
    }

    #[test]
    fn classify_seek_clamps_gaps_and_snaps() {
        let snapshot = fixture();
        assert_eq!(
            classify_seek(&snapshot, 10_000_000),
            SeekOutcome::Ok(10_000_000)
        );
        assert_eq!(
            classify_seek(&snapshot, 25_000_000),
            SeekOutcome::Gap(30_000_000)
        );
        assert_eq!(
            classify_seek(&snapshot, -5),
            SeekOutcome::Clamped {
                position: 0,
                at_start: true
            }
        );
    }

    /// A readable manifest with zero segments is loadable, and a seek against
    /// it must not panic on an inverted clamp range (live edge 0).
    #[test]
    fn classify_seek_survives_an_empty_timeline() {
        let snapshot = TimelineSnapshot {
            audio_tracks: Vec::new(),
            session_id: "session-empty".into(),
            segments: Vec::new(),
            gaps: Vec::new(),
            live_edge_100ns: 0,
            target_title: None,
            target_executable: None,
            target_executable_path: None,
        };
        assert_eq!(classify_seek(&snapshot, 0), SeekOutcome::Buffering);
    }

    /// The full-screen double press puts the playhead back only when the
    /// gesture actually moved it and left the transport paused (task3270).
    #[test]
    fn the_double_press_restores_only_a_paused_playhead_that_moved() {
        let cases = [
            // (playing, saved, position, expected)
            (false, Some(10), 50, Some(10)),
            (false, Some(10), 10, None),
            (false, None, 50, None),
            (true, Some(10), 50, None),
            (true, Some(10), 10, None),
            (true, None, 50, None),
        ];
        for (playing, saved, position, expected) in cases {
            assert_eq!(
                restore_after_double_press(playing, saved, position),
                expected,
                "playing={playing} saved={saved:?} position={position}"
            );
        }
    }

    #[test]
    fn repeat_wraps_only_at_the_selection_end() {
        let wrap = |range, position| range_guard(true, false, true, range, position);
        assert_eq!(wrap(None, 100), RangeGuard::None);
        assert_eq!(wrap(Some((10, 50)), 49), RangeGuard::None);
        assert_eq!(wrap(Some((10, 50)), 50), RangeGuard::WrapToStart);
        assert_eq!(wrap(Some((10, 50)), 80), RangeGuard::WrapToStart);
        // A degenerate (empty) selection must not seek-loop forever.
        assert_eq!(wrap(Some((10, 10)), 10), RangeGuard::None);
    }

    #[test]
    fn the_selection_holds_playback_inside_itself_with_repeat_off() {
        let guard = |range, position| range_guard(true, false, false, range, position);
        // No selection: playback runs to the end of the session as it always did.
        assert_eq!(guard(None, 100), RangeGuard::None);
        assert_eq!(guard(Some((10, 50)), 10), RangeGuard::None);
        assert_eq!(guard(Some((10, 50)), 49), RangeGuard::None);
        // Before the start: forward to it, still playing.
        assert_eq!(guard(Some((10, 50)), 9), RangeGuard::JumpToStart);
        assert_eq!(guard(Some((10, 50)), 0), RangeGuard::JumpToStart);
        // At or past the end: stop there. This is what repeat off used to miss.
        assert_eq!(guard(Some((10, 50)), 50), RangeGuard::PauseAtEnd);
        assert_eq!(guard(Some((10, 50)), 80), RangeGuard::PauseAtEnd);
        assert_eq!(guard(Some((10, 10)), 10), RangeGuard::None);
    }

    #[test]
    fn repeat_jumps_forward_to_the_start_too() {
        assert_eq!(
            range_guard(true, false, true, Some((10, 50)), 9),
            RangeGuard::JumpToStart
        );
    }

    #[test]
    fn a_parked_engine_only_ever_wraps() {
        // Paused at the live edge past the end with repeat off: nothing, or the
        // 100ms drain would drag the playhead back to the end every tick.
        assert_eq!(
            range_guard(false, false, false, Some((10, 50)), 80),
            RangeGuard::None
        );
        assert_eq!(
            range_guard(false, false, false, Some((10, 50)), 9),
            RangeGuard::None
        );
        // Repeat on, though, restarts the loop from a parked engine (task153).
        assert_eq!(
            range_guard(false, false, true, Some((10, 50)), 80),
            RangeGuard::WrapToStart
        );
    }

    #[test]
    fn a_seek_out_of_the_selection_stops_playback_first() {
        // task3380: the rule is asked *before* the seek goes out, so the
        // engine never sounds the frame outside the selection.
        let range = Some((10, 50));
        // A healthy timeline: the selection's start is recorded, so
        // `classify_seek` leaves it where it is. Non-capturing, hence `Copy`,
        // so the one closure serves every row.
        let lands = |start| Some(start);
        for repeat in [false, true] {
            let leaves = |target| seek_plan(true, false, false, repeat, range, target, lands);
            // Inside the selection: playback carries on untouched.
            assert_eq!(leaves(10), SeekPlan::Go, "repeat={repeat}");
            assert_eq!(leaves(30), SeekPlan::Go, "repeat={repeat}");
            assert_eq!(leaves(49), SeekPlan::Go, "repeat={repeat}");
            // Before the start -- never a wrap, whatever repeat says:
            // `JumpToStart` is not the loop point (task3570).
            assert_eq!(leaves(9), SeekPlan::PauseFirst, "repeat={repeat}");
            assert_eq!(leaves(0), SeekPlan::PauseFirst, "repeat={repeat}");
            // At or past the end -- `end` is the first position outside. With
            // repeat on this is task3570's wrap instead of task3380's stop.
            let past_the_end = if repeat {
                SeekPlan::WrapTo(10)
            } else {
                SeekPlan::PauseFirst
            };
            assert_eq!(leaves(50), past_the_end, "repeat={repeat}");
            assert_eq!(leaves(80), past_the_end, "repeat={repeat}");
            // No selection, and a degenerate one, constrain nothing.
            assert_eq!(
                seek_plan(true, false, false, repeat, None, 80, lands),
                SeekPlan::Go,
                "repeat={repeat}"
            );
            assert_eq!(
                seek_plan(true, false, false, repeat, Some((10, 10)), 10, lands),
                SeekPlan::Go,
                "repeat={repeat}"
            );
            // Already paused: this is the user scrubbing outside the
            // selection, and there is no playback left to stop -- nor, since
            // task3570, to wrap.
            assert_eq!(
                seek_plan(false, false, false, repeat, range, 9, lands),
                SeekPlan::Go,
                "repeat={repeat}"
            );
            assert_eq!(
                seek_plan(false, false, false, repeat, range, 80, lands),
                SeekPlan::Go,
                "repeat={repeat}"
            );
        }
    }

    #[test]
    fn repeat_now_changes_what_a_seek_past_the_end_does() {
        // Was `repeat_does_not_move_the_edge_a_seek_has_to_cross`, asserting
        // the opposite: task3380 deliberately answered the same either way,
        // reasoning that seeking past the end with repeat on would otherwise
        // keep playing and wrap a frame later -- the very flash 3380 removes.
        //
        // The user reversed that judgement on 2026-09-07: repeat on has to
        // wrap. 3380's guarantee is kept a different way -- the wrap replaces
        // the target *before* the `Seek` goes out, so the engine still never
        // decodes a frame outside the selection and there is no later frame to
        // flash.
        //
        // The edge itself has not moved: 50 is the first position outside for
        // both values of repeat. Only the verdict there differs.
        let range = Some((10, 50));
        let lands = |start| Some(start);
        for target in [0, 9, 10, 30, 49] {
            assert_eq!(
                seek_plan(true, false, false, false, range, target, lands),
                seek_plan(true, false, false, true, range, target, lands),
                "target={target}"
            );
        }
        for target in [50, 80] {
            assert_eq!(
                seek_plan(true, false, false, false, range, target, lands),
                SeekPlan::PauseFirst,
                "target={target}"
            );
            assert_eq!(
                seek_plan(true, false, false, true, range, target, lands),
                SeekPlan::WrapTo(10),
                "target={target}"
            );
        }
    }

    #[test]
    fn a_paused_seek_out_of_the_selection_is_left_alone_with_repeat_on() {
        // task3150's "look outside the selection" survives task3570: the wrap
        // is gated on playback actually running, so scrubbing past the end
        // while paused still lands where it was asked to and stays there.
        // Without the gate there would be no way left to see outside the
        // selection with repeat on.
        let range = Some((10, 50));
        let lands = |start| Some(start);
        for target in [0, 9, 50, 80] {
            assert_eq!(
                seek_plan(false, false, false, true, range, target, lands),
                SeekPlan::Go,
                "target={target}"
            );
        }
    }

    #[test]
    fn a_wrap_that_lands_outside_too_gives_up_instead_of_hopping_again() {
        // The pathological selection: a gap swallows all of it, so
        // `classify_seek` carries its start forward to or past the end.
        // Re-entering `seek_clamped` there would ask the same question, get
        // the same answer and recurse until the stack ran out -- so the hop is
        // one deep and falls back to task3380's stop instead. Before task3570
        // this range merely span an idle seek per frame through
        // `poll_range_guard_with`; it must not get worse than that.
        let range = Some((10, 50));
        assert_eq!(
            seek_plan(true, false, false, true, range, 80, |_| Some(50)),
            SeekPlan::PauseFirst
        );
        assert_eq!(
            seek_plan(true, false, false, true, range, 80, |_| Some(99)),
            SeekPlan::PauseFirst
        );
        // Nothing recorded at or after the start at all (`SeekOutcome::
        // Buffering`, so no landing): same fallback.
        assert_eq!(
            seek_plan(true, false, false, true, range, 80, |_| None),
            SeekPlan::PauseFirst
        );
        // The healthy row, for contrast -- and a gap just after the start
        // still wraps, because 30 is inside the selection.
        assert_eq!(
            seek_plan(true, false, false, true, range, 80, |_| Some(10)),
            SeekPlan::WrapTo(10)
        );
        assert_eq!(
            seek_plan(true, false, false, true, range, 80, |_| Some(30)),
            SeekPlan::WrapTo(30)
        );
    }

    #[test]
    fn a_seek_taken_during_a_skim_is_not_stopped_by_the_selection() {
        // task3580. The centre hold's x2 sends no seek of its own, but a key
        // tapped while it is held comes through `seek_clamped` like any other
        // -- and task3380's stop would end the skim mid-gesture. task3280's
        // 案(A) settled that the exemption is about the *gesture*, so it has to
        // reach this side too, not only the guard's own polling.
        let range = Some((10, 50));
        let lands = |start| Some(start);
        let skim = |repeat, target| seek_plan(true, true, false, repeat, range, target, lands);
        let no_skim = |repeat, target| seek_plan(true, false, false, repeat, range, target, lands);

        // repeat OFF: neither side of the selection stops a skim.
        assert_eq!(skim(false, 80), SeekPlan::Go, "past the end");
        assert_eq!(skim(false, 9), SeekPlan::Go, "before the start");
        // ...and the exemption really is what does it: the same rows without
        // the gesture still stop, which is task3380 untouched.
        assert_eq!(no_skim(false, 80), SeekPlan::PauseFirst);
        assert_eq!(no_skim(false, 9), SeekPlan::PauseFirst);

        // repeat ON, past the end: `WrapToStart` is the one verdict `skimming`
        // does not clear (`range_guard`'s doc), so the skim is turned round to
        // the selection's start by task3570 and carries on rather than either
        // stopping or running off -- the same shape `poll_range_guard_with`
        // already gave a skim that *ran* over the end.
        assert_eq!(skim(true, 80), SeekPlan::WrapTo(10));
        assert_eq!(skim(true, 50), SeekPlan::WrapTo(10));
        // repeat ON, before the start, is `JumpToStart` and so is cleared.
        assert_eq!(skim(true, 9), SeekPlan::Go);

        // Paused: `seek_plan` gives up before it ever looks at `skimming`, so a
        // skim changes nothing there. (A hold that started paused is running by
        // the time it skims -- task1010's repause -- so this row is about a
        // stale gesture, not about a skim in flight.)
        for repeat in [false, true] {
            for target in [9, 80] {
                assert_eq!(
                    seek_plan(false, true, false, repeat, range, target, lands),
                    seek_plan(false, false, false, repeat, range, target, lands),
                    "repeat={repeat} target={target}"
                );
            }
        }

        // Inside the selection there was never anything to exempt.
        for target in [10, 30, 49] {
            for repeat in [false, true] {
                assert_eq!(skim(repeat, target), SeekPlan::Go, "target={target}");
            }
        }
    }

    #[test]
    fn the_wraps_recursion_bound_stops_a_seek_but_not_a_skim() {
        // task3660. The bound itself is unchanged and still task3570's: a gap
        // swallowing the whole selection carries the start to or past the end,
        // that landing is `WrapToStart` whatever `skimming` says, and the hop
        // is given up rather than tried again. Only where giving up *lands*
        // moved. task3580 wrote this test asserting the stop for both values --
        // that assertion is what task3660 deliberately inverts on the skim
        // side, because inheriting the seek's stop contradicts task3280's
        // 案(A): the exemption is about the gesture, not about the route that
        // moved the playhead inside it. The non-skim rows are untouched, so
        // 3380/3570's guarantee still has its coverage here.
        let range = Some((10, 50));
        for skimming in [false, true] {
            let expected = if skimming {
                SeekPlan::Go
            } else {
                SeekPlan::PauseFirst
            };
            // The wrap lands *on* the end: outside the selection.
            assert_eq!(
                seek_plan(true, skimming, false, true, range, 80, |_| Some(50)),
                expected,
                "skimming={skimming}"
            );
            // Nothing recorded at or after the start at all: no landing.
            assert_eq!(
                seek_plan(true, skimming, false, true, range, 80, |_| None),
                expected,
                "skimming={skimming}"
            );
            // The wrap that *succeeds* is unaffected either way -- the split
            // is only in the fallback.
            assert_eq!(
                seek_plan(true, skimming, false, true, range, 80, |_| Some(30)),
                SeekPlan::WrapTo(30),
                "skimming={skimming}"
            );
        }
    }

    #[test]
    fn the_wrap_is_tried_once_before_a_skim_is_let_through() {
        // `Go` on the fallback (task3660) must not be reachable without having
        // *attempted* the wrap -- otherwise a skim past the end would run
        // straight out of the selection with repeat on, which is not what
        // `range_guard` says. So count the calls: exactly one on the
        // `WrapToStart` row, and it is asked about the selection's start.
        //
        // "never twice" is not observable from here: `wrapped` is `FnOnce`, so
        // a second call is a compile error rather than a test failure. That is
        // the stronger guarantee, and it is why this test only pins 1 vs 0.
        let range = Some((10, 50));
        for skimming in [false, true] {
            let calls = Cell::new(0u32);
            let asked = Cell::new(0i64);
            let plan = seek_plan(true, skimming, false, true, range, 80, |start| {
                calls.set(calls.get() + 1);
                asked.set(start);
                Some(50)
            });
            assert_eq!(calls.get(), 1, "skimming={skimming}");
            assert_eq!(asked.get(), 10, "skimming={skimming}");
            assert_eq!(
                plan,
                if skimming {
                    SeekPlan::Go
                } else {
                    SeekPlan::PauseFirst
                },
                "skimming={skimming}"
            );
        }

        // The contrast: every other verdict answers without asking at all.
        // repeat OFF past the end is `PauseAtEnd`, before the start is
        // `JumpToStart`, and inside the selection is `None`.
        for (repeat, target) in [(false, 80), (false, 9), (true, 9), (true, 30)] {
            let calls = Cell::new(0u32);
            seek_plan(true, false, false, repeat, range, target, |_| {
                calls.set(calls.get() + 1);
                Some(10)
            });
            assert_eq!(calls.get(), 0, "repeat={repeat} target={target}");
        }
    }

    #[test]
    fn a_gesture_in_flight_does_not_wrap_it_stops() {
        // task3650. `on_track_moved` calls `seek_clamped` on every pointer
        // move, so with repeat on a drag past the end wrapped on each one and
        // the playhead sat on the selection's start for the whole gesture.
        // While the gesture is in flight the wrap is not attempted and the
        // seek takes task3380's stop instead, so the playhead follows the
        // pointer after the first crossing stops playback.
        let range = Some((10, 50));
        let lands = |start| Some(start);
        assert_eq!(
            seek_plan(true, false, true, true, range, 80, lands),
            SeekPlan::PauseFirst,
            "gesturing"
        );
        assert_eq!(
            seek_plan(true, false, false, true, range, 80, lands),
            SeekPlan::WrapTo(10),
            "not gesturing"
        );

        // The wrap is not merely discarded, it is never asked for: the gate
        // returns before `wrapped` runs.
        let calls = Cell::new(0u32);
        seek_plan(true, false, true, true, range, 80, |start| {
            calls.set(calls.get() + 1);
            Some(start)
        });
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn the_gesture_gate_reaches_the_wrap_row_and_nothing_else() {
        // It changes where `WrapToStart` lands and no other verdict: `None`
        // still goes, `JumpToStart` / `PauseAtEnd` still stop, and a paused
        // transport still takes the early `Go` that task3150's "look outside
        // the selection" rests on.
        let range = Some((10, 50));
        let lands = |start| Some(start);
        for gesturing in [false, true] {
            assert_eq!(
                seek_plan(true, false, gesturing, true, range, 30, lands),
                SeekPlan::Go,
                "inside, gesturing={gesturing}"
            );
            assert_eq!(
                seek_plan(true, false, gesturing, true, range, 9, lands),
                SeekPlan::PauseFirst,
                "before the start, gesturing={gesturing}"
            );
            assert_eq!(
                seek_plan(true, false, gesturing, false, range, 80, lands),
                SeekPlan::PauseFirst,
                "repeat off past the end, gesturing={gesturing}"
            );
            assert_eq!(
                seek_plan(false, false, gesturing, true, range, 80, lands),
                SeekPlan::Go,
                "paused, gesturing={gesturing}"
            );
        }
    }

    #[test]
    fn a_skim_beats_the_gesture_gate() {
        // The gate is `gesturing && !skimming`, and `!skimming` is the half
        // this test exists to keep. A skim *is* a gesture, so without it the
        // gate would fire on every skim past the end -- stopping it on the
        // recursion bound, which is exactly what task3660 fixed and what
        // task3280's 案(A) (the exemption is about the gesture) rules out.
        // `range_guard` can answer `WrapToStart` whatever `skimming` says (the
        // gap snap), so the two really do meet here.
        //
        // In the app the centre hold and a track drag are not held at once;
        // this is a type-level pin, not a scenario. Drop `!skimming` from the
        // gate and both halves below go red.
        let range = Some((10, 50));
        assert_eq!(
            seek_plan(true, true, true, true, range, 80, Some),
            SeekPlan::WrapTo(10),
            "the skim still wraps"
        );
        assert_eq!(
            seek_plan(true, true, true, true, range, 80, |_| None),
            SeekPlan::Go,
            "task3660's fallback still lets the skim run"
        );
    }

    #[test]
    fn the_guards_own_seeks_do_not_stop_themselves() {
        // `JumpToStart` / `WrapToStart` both seek to `start`, which is inside
        // the selection, so they never trip task3380's rule. `PauseAtEnd`
        // seeks to `end`, which does trip it -- deliberately: that seek and
        // the branch want the same stop, and the branch skips its own once
        // `seek_clamped` has issued it.
        //
        // task3570 leaves that intact. `range_guard` answers `WrapToStart`
        // before it ever reaches `PauseAtEnd`, so `poll_range_guard_with`'s
        // `PauseAtEnd` arm is only ever entered with repeat *off* -- and with
        // repeat off its own `seek_clamped(end)` still comes back
        // `PauseFirst`, exactly as before.
        let range = Some((10, 50));
        let lands = |start| Some(start);
        for repeat in [false, true] {
            assert_eq!(
                seek_plan(true, false, false, repeat, range, 10, lands),
                SeekPlan::Go,
                "repeat={repeat}"
            );
        }
        assert_eq!(
            seek_plan(true, false, false, false, range, 50, lands),
            SeekPlan::PauseFirst
        );
        // With repeat on, the wrap branch's own seek turns round to the very
        // landing it was already asking for.
        assert_eq!(
            seek_plan(true, false, false, true, range, 50, lands),
            SeekPlan::WrapTo(10)
        );
    }

    #[test]
    fn only_a_held_centre_gesture_counts_as_a_skim() {
        // The x2 skim is the centre hold and nothing else: the side zones hold
        // to scan frames (task650), which is not this, and an unheld press is
        // still on its way to being a tap.
        for repause in [false, true] {
            assert!(stage_skimming(StageZone::Center, true, repause));
            assert!(!stage_skimming(StageZone::Center, false, repause));
            assert!(!stage_skimming(StageZone::Left, true, repause));
            assert!(!stage_skimming(StageZone::Right, true, repause));
        }
        // 案(A), not 案(C): a hold begun while playing is exempt exactly like
        // one begun paused.
        assert_eq!(
            stage_skimming(StageZone::Center, true, false),
            stage_skimming(StageZone::Center, true, true)
        );
    }

    #[test]
    fn the_selection_lets_go_while_the_centre_hold_skims() {
        // task3280: while the centre hold runs, the selection constrains
        // nothing -- outside it, and on the way out of it.
        for repause in [false, true] {
            let skimming = stage_skimming(StageZone::Center, true, repause);
            let skim = |playing, repeat, position| {
                range_guard(playing, skimming, repeat, Some((10, 50)), position)
            };
            let held = |playing, repeat, position| {
                range_guard(
                    playing,
                    stage_skimming(StageZone::Center, false, repause),
                    repeat,
                    Some((10, 50)),
                    position,
                )
            };
            for playing in [false, true] {
                // Before the start: the skim is not dragged forward to it.
                assert_eq!(skim(playing, false, 9), RangeGuard::None, "{repause}");
                // Inside: nothing to do either way.
                assert_eq!(skim(playing, false, 30), RangeGuard::None, "{repause}");
                // Running out of the end: not stopped (2026-09-07 判断, 帰結1).
                assert_eq!(skim(playing, false, 50), RangeGuard::None, "{repause}");
                assert_eq!(skim(playing, false, 80), RangeGuard::None, "{repause}");
                // Repeat's wrap is untouched by the exemption (task3280 は
                // 折り返しの規則に触らない).
                assert_eq!(
                    skim(playing, true, 80),
                    RangeGuard::WrapToStart,
                    "{repause}"
                );
                assert_eq!(skim(playing, true, 9), RangeGuard::None, "{repause}");
            }
            // Not held: 3150's boundaries are exactly as they were.
            assert_eq!(held(true, false, 9), RangeGuard::JumpToStart, "{repause}");
            assert_eq!(held(true, false, 80), RangeGuard::PauseAtEnd, "{repause}");
            assert_eq!(held(true, true, 80), RangeGuard::WrapToStart, "{repause}");
            assert_eq!(held(false, false, 80), RangeGuard::None, "{repause}");
        }
    }

    #[test]
    fn centre_hold_borrows_x2_and_gives_the_rate_back() {
        assert_eq!(
            stage_hold_rate(true, 1.0),
            StageHold {
                applied: 2.0,
                restore: 1.0,
                repause: false
            }
        );
        // Whatever it was pressed at is what release must restore.
        assert_eq!(stage_hold_rate(true, 0.5).restore, 0.5);
        assert_eq!(stage_hold_rate(true, 1.5).restore, 1.5);
        // Already there: still a no-op round trip, never x4.
        assert_eq!(stage_hold_rate(true, 2.0).applied, 2.0);
    }

    #[test]
    fn a_hold_that_started_paused_plays_at_x2_and_stops_again() {
        assert_eq!(
            stage_hold_rate(false, 1.0),
            StageHold {
                applied: 2.0,
                restore: 1.0,
                repause: true
            }
        );
        // The rate it was paused at is still what comes back.
        assert_eq!(stage_hold_rate(false, 0.5).restore, 0.5);
    }

    #[test]
    fn zones_are_the_outer_quarters() {
        assert_eq!(stage_zone(0.1), StageZone::Left);
        assert_eq!(stage_zone(0.25), StageZone::Center);
        assert_eq!(stage_zone(0.5), StageZone::Center);
        assert_eq!(stage_zone(0.75), StageZone::Center);
        assert_eq!(stage_zone(0.9), StageZone::Right);
    }

    #[test]
    fn a_press_on_the_picture_reports_where_it_landed() {
        // Ratios are the picture's own, so the zone split lands on the outer
        // quarters of the picture rather than of the pane.
        assert_eq!(stage_hit(0.0, 0.0, 800.0, 450.0), Some(0.0));
        assert_eq!(stage_hit(400.0, 225.0, 800.0, 450.0), Some(0.5));
        assert_eq!(stage_hit(600.0, 449.0, 800.0, 450.0), Some(0.75));
        assert_eq!(
            stage_hit(700.0, 225.0, 800.0, 450.0).map(stage_zone),
            Some(StageZone::Right)
        );
    }

    #[test]
    fn a_press_on_the_letterbox_is_not_a_press_on_the_picture() {
        // Pillarbox: negative x is the black to the left, >= width the right.
        assert_eq!(stage_hit(-1.0, 225.0, 800.0, 450.0), None);
        assert_eq!(stage_hit(800.0, 225.0, 800.0, 450.0), None);
        // Letterbox above and below.
        assert_eq!(stage_hit(400.0, -1.0, 800.0, 450.0), None);
        assert_eq!(stage_hit(400.0, 450.0, 800.0, 450.0), None);
        // No picture yet: everything is letterbox.
        assert_eq!(stage_hit(0.0, 0.0, 0.0, 0.0), None);
    }

    #[test]
    fn a_second_centre_press_soon_after_a_centre_tap_is_a_double_click() {
        assert!(stage_double_press(Some(0), StageZone::Center));
        assert!(stage_double_press(
            Some(STAGE_DOUBLE_PRESS_MS),
            StageZone::Center
        ));
        // Too slow: two separate play/pause taps.
        assert!(!stage_double_press(
            Some(STAGE_DOUBLE_PRESS_MS + 1),
            StageZone::Center
        ));
        // No tap before it at all.
        assert!(!stage_double_press(None, StageZone::Center));
        // The side zones seek twice instead (task650).
        assert!(!stage_double_press(Some(0), StageZone::Left));
        assert!(!stage_double_press(Some(0), StageZone::Right));
    }

    #[test]
    fn taps_accumulate_on_the_same_side_and_reset_on_crossing() {
        let first = fold_tap(None, SeekOutcome::Ok(0), 10);
        assert_eq!(
            first,
            StageFeedback::Tap {
                direction: 1,
                seconds: 10
            }
        );
        let second = fold_tap(Some(first), SeekOutcome::Ok(0), 10);
        assert_eq!(
            second,
            StageFeedback::Tap {
                direction: 1,
                seconds: 20
            }
        );
        let crossed = fold_tap(Some(second), SeekOutcome::Ok(0), -10);
        assert_eq!(
            crossed,
            StageFeedback::Tap {
                direction: -1,
                seconds: 10
            }
        );
    }

    #[test]
    fn outcome_feedback_replaces_the_tap_circle() {
        assert_eq!(fold_tap(None, SeekOutcome::Gap(0), 10), StageFeedback::Gap);
        assert_eq!(
            fold_tap(
                None,
                SeekOutcome::Clamped {
                    position: 0,
                    at_start: true
                },
                -10
            ),
            StageFeedback::Clamped { at_start: true }
        );
        assert_eq!(
            StageFeedback::Clamped { at_start: true }.line(Locale::Ja),
            stage_clamped_start(Locale::Ja)
        );
        assert_eq!(
            StageFeedback::Gap.line(Locale::Ja),
            stage_gap_skipped(Locale::Ja)
        );
    }

    #[test]
    fn holds_report_running_totals_and_dot_cycles() {
        // Seven repeats at the ramped stride: 5x1s + 2x2s = 9s of travel.
        let total: i64 = (1..=7).map(hold_step_seconds).sum();
        assert_eq!(total, 9);
        let feedback = fold_hold(SeekOutcome::Ok(0), 1, total, 7);
        assert_eq!(
            feedback,
            StageFeedback::Hold {
                direction: 1,
                seconds: 9,
                steps: 7
            }
        );
        assert_eq!(hold_dots_on(0), 0);
        assert_eq!(hold_dots_on(3), 3);
        assert_eq!(hold_dots_on(5), 5);
        assert_eq!(hold_dots_on(6), 1);
        assert_eq!(tap_seconds_text(-1, 10), "−10");
        assert_eq!(hold_seconds_text(Locale::Ja, 1, 9), "+9秒");
    }

    #[test]
    fn hold_stride_doubles_each_dot_cycle_and_caps_at_8s() {
        assert_eq!(hold_step_seconds(1), 1);
        assert_eq!(hold_step_seconds(5), 1);
        assert_eq!(hold_step_seconds(6), 2);
        assert_eq!(hold_step_seconds(10), 2);
        assert_eq!(hold_step_seconds(11), 4);
        assert_eq!(hold_step_seconds(15), 4);
        assert_eq!(hold_step_seconds(16), 8);
        assert_eq!(hold_step_seconds(100), 8);
        // Direction still comes from the zone.
        assert_eq!(hold_step_100ns(StageZone::Right, 6), 2 * HNS_PER_SECOND);
        assert_eq!(hold_step_100ns(StageZone::Left, 16), -8 * HNS_PER_SECOND);
    }

    #[test]
    fn key_scan_gates_repeats_to_the_200ms_cadence() {
        // The first repeat of a hold has no previous step and goes through.
        assert!(key_scan_due(None));
        assert!(!key_scan_due(Some(0)));
        assert!(!key_scan_due(Some(KEY_SCAN_INTERVAL_MS - 1)));
        assert!(key_scan_due(Some(KEY_SCAN_INTERVAL_MS)));
        assert!(key_scan_due(Some(KEY_SCAN_INTERVAL_MS * 10)));
    }

    #[test]
    fn key_scan_stride_ramps_5_10_20_40_and_caps() {
        assert_eq!(key_scan_step_seconds(1), 5);
        assert_eq!(key_scan_step_seconds(5), 5);
        assert_eq!(key_scan_step_seconds(6), 10);
        assert_eq!(key_scan_step_seconds(10), 10);
        assert_eq!(key_scan_step_seconds(11), 20);
        assert_eq!(key_scan_step_seconds(15), 20);
        assert_eq!(key_scan_step_seconds(16), 40);
        assert_eq!(key_scan_step_seconds(100), 40);
    }

    #[test]
    fn the_stage_gate_paces_frames_at_the_display_refresh() {
        let mut gate = StageGate::for_refresh_hz(60);
        let t0 = Instant::now();
        // Nothing published yet: the first frame goes straight through.
        assert_eq!(gate.take_turn(t0), None);
        gate.published(t0);
        // x2 on a 57fps recording arrives every ~8.8ms; the one landing inside
        // the refresh period waits out the rest of it. t260916-e610: "the rest
        // of it" stops an eighth of a period short, so a frame that is merely
        // 2.1ms early is not made to sit out a whole refresh.
        let period = Duration::from_nanos(16_666_666);
        let early = t0 + Duration::from_millis(9);
        assert_eq!(
            gate.take_turn(early),
            Some(period - period / StageGate::EARLY_ENOUGH - Duration::from_millis(9))
        );
        // ...and the frame after that is due, so it goes.
        let due = t0 + Duration::from_millis(17);
        assert_eq!(gate.take_turn(due), None);
        gate.published(due);
        // x1 at 57fps clears the period every time.
        assert_eq!(gate.take_turn(due + Duration::from_millis(18)), None);
    }

    #[test]
    fn the_gate_keeps_its_phase_instead_of_restarting_it_on_every_frame() {
        // The recordings this desk makes play back at ~61.5 frames a second
        // against a 16.667ms period, so each publish lands a hair *inside* the
        // period the previous hand-over opened. A gate that re-anchors on `now`
        // therefore turns every other frame away for ever -- t260916-e610
        // measured exactly that on the real machine, 33 frames reaching the
        // stage out of 61 published, with `ui_stage_gate_turned_away` carrying
        // the other 28.
        let step = Duration::from_nanos(16_260_000);
        let mut gate = StageGate::for_refresh_hz(60);
        let t0 = Instant::now();
        let mut taken = 0;
        for index in 0..60u32 {
            let now = t0 + step * index;
            if gate.take_turn(now).is_none() {
                taken += 1;
                gate.published(now);
            }
        }
        // Every one of them is within `EARLY_ENOUGH` of its refresh, so every
        // one goes through. A gate with no tolerance takes 30.
        assert!(taken >= 57, "took {taken} of 60 publishes");
    }

    #[test]
    fn keeping_the_phase_does_not_turn_the_gate_into_a_pass_through() {
        // The control for the test above: 120 publishes at x2 still have to
        // come out as a display's worth, not as 120. Without this the phase
        // fix could "pass" by simply never gating.
        let step = Duration::from_nanos(8_333_333);
        let mut gate = StageGate::for_refresh_hz(60);
        let t0 = Instant::now();
        let mut taken = 0;
        for index in 0..120u32 {
            let now = t0 + step * index;
            if gate.take_turn(now).is_none() {
                taken += 1;
                gate.published(now);
            }
        }
        assert!(taken <= 61, "took {taken} in one second on a 60Hz gate");
    }

    #[test]
    fn a_late_hand_over_does_not_shorten_the_next_frames_window() {
        // The other half of t260916-e610: the grid, not the hand-over, is what
        // the next window is measured from. A frame handed over 3.3ms late must
        // not push the one behind it out of its own refresh -- with the mark on
        // `now` that second frame reads as 13ms early and is turned away, which
        // is how the lateness compounded in the first place.
        let mut gate = StageGate::for_refresh_hz(60);
        let t0 = Instant::now();
        gate.published(t0);
        let late = t0 + Duration::from_millis(20);
        assert_eq!(gate.take_turn(late), None);
        gate.published(late);
        assert_eq!(
            gate.take_turn(t0 + Duration::from_millis(33)),
            None,
            "the frame after a late one is still due on the grid"
        );
    }

    #[test]
    fn a_pause_does_not_leave_the_gate_owing_the_stage_a_burst() {
        let mut gate = StageGate::for_refresh_hz(60);
        let t0 = Instant::now();
        gate.published(t0);
        // Three seconds paused, then one frame: it goes straight through...
        let woke = t0 + Duration::from_secs(3);
        assert_eq!(gate.take_turn(woke), None);
        gate.published(woke);
        // ...and the one a millisecond behind it still waits its period out,
        // rather than collecting on 180 periods of credit.
        assert!(
            gate.take_turn(woke + Duration::from_millis(1)).is_some(),
            "the frame after a pause has to wait the period out"
        );
    }

    #[test]
    fn a_display_that_gives_no_believable_refresh_rate_leaves_the_gate_open() {
        let now = Instant::now();
        for hz in [0, 1, 19, 1001, u32::MAX] {
            let mut gate = StageGate::for_refresh_hz(hz);
            gate.published(now);
            assert_eq!(gate.take_turn(now), None, "hz={hz} must not gate");
        }
        // Same for the derived-by-`Default` gate, which is what `Review`
        // starts with before the display has been asked.
        let mut gate = StageGate::default();
        gate.published(now);
        assert_eq!(gate.take_turn(now), None);
        // A 144Hz display gets a 144Hz gate, not a 60Hz one.
        assert_eq!(
            StageGate::for_refresh_hz(144).take_turn(Instant::now()),
            None
        );
    }

    #[test]
    fn the_stage_takes_its_shape_from_the_frame_and_falls_back_to_16_9() {
        assert_eq!(stage_aspect(1920, 1080), 16.0 / 9.0);
        // A portrait window keeps its own shape rather than being widened.
        assert!(stage_aspect(720, 1280) < 1.0);
        assert_eq!(stage_aspect(720, 1280), 720.0 / 1280.0);
        // No frame yet / a decode that produced nothing: 16:9, never a divide
        // by zero and never a zero-width box.
        assert_eq!(stage_aspect(0, 1080), DEFAULT_STAGE_ASPECT);
        assert_eq!(stage_aspect(1920, 0), DEFAULT_STAGE_ASPECT);
        assert_eq!(stage_aspect(0, 0), DEFAULT_STAGE_ASPECT);
    }

    /// t260916-0ee4. The clip player's loop point. `range_guard` cannot be
    /// asked this question at all (see `clip_repeat_wrap`), so the rule is its
    /// own function and this is the one arrangement that fires it: repeat on,
    /// the transport was running, it is not running now, the engine parked on
    /// the end, and nothing this side sent is still in flight.
    #[test]
    fn a_repeating_clip_wraps_when_the_engine_parks_on_its_end() {
        assert!(clip_repeat_wrap(true, true, false, true, false));
    }

    /// The control for the assert above: one input moved at a time, so a rule
    /// that ignored the input it is named after could not stay green here.
    #[test]
    fn every_input_of_the_clip_wrap_can_veto_it() {
        // Repeat off: the park is the end of playback, exactly as before.
        assert!(!clip_repeat_wrap(false, true, false, true, false));
        // Not running on the previous pump: the user paused on the last frame,
        // and turning repeat on must not restart it under them.
        assert!(!clip_repeat_wrap(true, false, false, true, false));
        // Still running: nothing has ended yet.
        assert!(!clip_repeat_wrap(true, true, true, true, false));
        // Paused somewhere that is not the end.
        assert!(!clip_repeat_wrap(true, true, false, false, false));
        // A seek this side sent is unanswered, so the position -- and the edge
        // flag with it -- still describes where the transport was before it.
        assert!(!clip_repeat_wrap(true, true, false, true, true));
    }
}
