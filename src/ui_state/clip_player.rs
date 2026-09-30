//! The clip player's own state (task3510), ahead of the surface that draws it
//! (task3520) and the hover preview that borrows one rule from it (task3540).
//!
//! Separate from `playback` for the same reason `ClipVm` is separate from
//! `ReviewVm`: the review engine outlives leaving its pane -- it only pauses
//! (`playback::offscreen_transport`) and keeps the session's state -- so a clip
//! writing into that state would collide with the picture the review comes back
//! to. What is *identical* is borrowed rather than copied: there is deliberately
//! no `offscreen` function here, because `playback::offscreen_transport` already
//! answers the same question from the same three inputs and a second copy would
//! be a second rule to keep in step.
//!
//! Time is in 100ns ticks, as everywhere else in this crate. Ratios come from
//! the `.slint` side (a click on the seek bar is a fraction of its width) and go
//! back as ticks, because a tick does not fit slint's 32-bit int.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::playback::StageZone;

/// How long the transport stays up after the last activity while a clip is
/// playing. The review screen spells the same 3 seconds as a `Timer` interval in
/// `ui/review.slint` (`idle`, task151); the clip player decides it here instead,
/// so the rule has a test.
pub const CONTROLS_HIDE_MS: u64 = 3_000;

/// One clip, loaded and playing (or paused). Nothing here is optional: the
/// absence of a clip is `ClipPlayer::current() == None`, not a state with empty
/// fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipPlayerState {
    /// Row of the clip model the grid draws from -- the same index
    /// `ClipVm.row` carries and `ui_state::clips::place` lays out.
    pub row: usize,
    pub path: PathBuf,
    pub playing: bool,
    pub position_100ns: i64,
    pub duration_100ns: i64,
    pub fullscreen: bool,
}

/// The player as the shell holds it: at most one clip at a time.
///
/// A holder rather than a bare `Option<ClipPlayerState>` so `open` / `close` /
/// `toggle_play` / `seek_ratio` are one vocabulary with one place to test them,
/// instead of the caller matching on an option at every call site and each match
/// getting the clamping slightly differently.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ClipPlayer {
    clip: Option<ClipPlayerState>,
}

impl ClipPlayer {
    /// Loads `path` for grid row `row`. Starts playing: the gesture that opens a
    /// clip is "play this one" (round20 §2-1), not "select it". Task3520 owns
    /// the wiring and may pause on a decode failure -- it flips `playing`, it
    /// does not need a second constructor.
    ///
    /// A negative `duration_100ns` (an unreadable file) is stored as 0, which
    /// makes every seek land at 0 rather than somewhere derived from a negative
    /// span.
    pub fn open(&mut self, row: usize, path: PathBuf, duration_100ns: i64) {
        self.clip = Some(ClipPlayerState {
            row,
            path,
            playing: true,
            position_100ns: 0,
            duration_100ns: duration_100ns.max(0),
            fullscreen: false,
        });
    }

    /// Unloads whatever was open. Full screen goes with it: the clip owning the
    /// display is a property of the clip being open, and leaving the flag set
    /// would fold the window chrome away with nothing playing.
    pub fn close(&mut self) {
        self.clip = None;
    }

    pub fn current(&self) -> Option<&ClipPlayerState> {
        self.clip.as_ref()
    }

    pub fn current_mut(&mut self) -> Option<&mut ClipPlayerState> {
        self.clip.as_mut()
    }

    pub fn is_open(&self) -> bool {
        self.clip.is_some()
    }

    /// Open and stopped -- the one state the context menu's 「再生」 has to
    /// resume by itself (task3530 follow-up): `clips::sync_player` returns
    /// early when the engine is already on the clip, so an expanded tile that
    /// was paused would otherwise see the menu entry do nothing.
    ///
    /// Deliberately not `!is_playing()`: a closed player is neither playing nor
    /// paused, and answering `true` for it would have the menu send a transport
    /// command over no engine.
    pub fn is_paused(&self) -> bool {
        self.clip.as_ref().is_some_and(|clip| !clip.playing)
    }

    /// Flips play/pause, returning the new value. `None` with nothing open --
    /// the space bar reaching a closed player is not a pause.
    pub fn toggle_play(&mut self) -> Option<bool> {
        let clip = self.clip.as_mut()?;
        clip.playing = !clip.playing;
        Some(clip.playing)
    }

    /// Where a seek-bar ratio lands, in 100ns ticks, and moves the position
    /// there. `None` with nothing open.
    ///
    /// The ratio is whatever the pointer produced, so it is clamped rather than
    /// trusted: a drag off the left edge of the bar gives a negative one, and a
    /// zero-width bar can produce a NaN, which `f64::clamp` would pass straight
    /// through.
    pub fn seek_ratio(&mut self, ratio: f64) -> Option<i64> {
        let clip = self.clip.as_mut()?;
        clip.position_100ns = clip.seek_target(ratio);
        Some(clip.position_100ns)
    }
}

impl ClipPlayerState {
    /// The tick a ratio points at, without moving anything.
    pub fn seek_target(&self, ratio: f64) -> i64 {
        let ratio = if ratio.is_nan() {
            0.0
        } else {
            ratio.clamp(0.0, 1.0)
        };
        let duration = self.duration_100ns.max(0);
        ((ratio * duration as f64).round() as i64).clamp(0, duration)
    }

    /// How much of the clip is behind the playhead, 0..=1, for the seek bar's
    /// fill. 0 for a clip with no length rather than a division by zero.
    pub fn played_ratio(&self) -> f64 {
        if self.duration_100ns <= 0 {
            return 0.0;
        }
        (self.position_100ns as f64 / self.duration_100ns as f64).clamp(0.0, 1.0)
    }
}

/// Whether the transport is up. The review screen's rule, stated once
/// (`ui/review.slint`'s `controls-shown` / `controls-pinned` pair, task151):
///
/// - anything the user is in the middle of *pins* the bar -- a drag, an open
///   menu, a hover over the bar itself. The caller folds those into `pinned`.
/// - a paused clip keeps the bar: there is nothing to watch under it.
/// - otherwise it lives for `CONTROLS_HIDE_MS` after the last activity.
///
/// Exactly at the deadline the bar is gone: the review's `Timer` fires *at* its
/// interval, so the boundary belongs to the hidden side.
pub fn controls_visible(playing: bool, last_activity: Instant, now: Instant, pinned: bool) -> bool {
    if !playing || pinned {
        return true;
    }
    now.saturating_duration_since(last_activity) < Duration::from_millis(CONTROLS_HIDE_MS)
}

/// What a key press on the clip pane asks the player to do (t260916-956e).
///
/// Only the four operations `ClipVm` already carries: round20 §2-2 fixed the
/// in-place controls at play/pause, seek, mute and full screen, so there is
/// nothing for a rate or range key to reach.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKeyAction {
    TogglePlay,
    ToggleMute,
    ToggleFullscreen,
    /// The same jump the stage's left/right 25% tap makes -- the zone is what
    /// carries the direction, so the seconds stay stated once
    /// (`playback::tap_delta_100ns`).
    Seek(StageZone),
}

/// Key text -> what the clip player does with it, or `None` to let the press
/// through (t260916-956e).
///
/// The strings are the review pane's (`review_wiring.rs`'s `match
/// text.as_str()`), because they are slint's: the arrows arrive as the private
/// -use codepoints `\u{F702}` / `\u{F703}`. Deliberately *narrower* than the
/// review's set -- `\u{F700}`/`\u{F701}` (volume), `s`, `<`/`>`, `,`/`.`, Home
/// and End all reach operations this player does not have, so they fall through
/// rather than being silently swallowed.
///
/// Unlike the review there is no shift and no repeat: the clip player has no
/// coarse scan and no 30s step (the reason task3510 gave `key-pressed-cb` one
/// argument), so `j`/`l` and the arrows share the stage tap's single step.
pub fn clip_key_action(text: &str) -> Option<ClipKeyAction> {
    match text {
        " " | "k" | "K" => Some(ClipKeyAction::TogglePlay),
        "m" | "M" => Some(ClipKeyAction::ToggleMute),
        "f" | "F" => Some(ClipKeyAction::ToggleFullscreen),
        "\u{F702}" | "j" | "J" => Some(ClipKeyAction::Seek(StageZone::Left)),
        "\u{F703}" | "l" | "L" => Some(ClipKeyAction::Seek(StageZone::Right)),
        _ => None,
    }
}

/// Whether a hover has rested long enough to start the preview (task3540).
/// `hover_since` is `None` when the pointer is not on a tile at all.
///
/// The dwell is inclusive: at exactly `dwell` the preview starts. A timer fired
/// at the deadline must not have to wait for the next tick to agree with it.
pub fn hover_preview_should_start(
    hover_since: Option<Instant>,
    now: Instant,
    dwell: Duration,
) -> bool {
    match hover_since {
        None => false,
        Some(since) => now.saturating_duration_since(since) >= dwell,
    }
}

/// The hover storyboard's dwell (round20 §7 追記: 250ms, no indicator).
pub const STORYBOARD_DWELL: Duration = Duration::from_millis(250);

/// How soon a declined start asks again, and how many times (t260925-2509):
/// 200 x 50ms = 10s of a resting pointer, after which the next move re-asks.
pub const STORYBOARD_RETRY: Duration = Duration::from_millis(50);
pub const STORYBOARD_RETRIES: u32 = 200;

/// After the dwell's timer asked to start: ask again later? `started` is the
/// state of the same hover (`None` once it ended or another tile took over).
/// An expanded tile shows no preview, so it is not waited out either.
pub fn hover_preview_should_retry(started: Option<bool>, expanded: bool, tries_left: u32) -> bool {
    started == Some(false) && !expanded && tries_left > 0
}

/// How many frames a tile of this width scrubs through: one per ~16px of
/// pointer travel (round20 §7 追記), never fewer than one.
pub fn storyboard_count(tile_width_px: f32) -> usize {
    ((tile_width_px / 16.0).floor() as usize).max(1)
}

/// The frame under the pointer, `ratio` being `mouse-x / width`.
pub fn storyboard_index(ratio: f32, count: usize) -> usize {
    let count = count.max(1);
    ((ratio.clamp(0.0, 1.0) * count as f32).floor() as usize).min(count - 1)
}

/// The generated frame nearest `index` -- itself when it is there, the lower
/// neighbour on a tie. `None` while nothing has arrived (the tile keeps its
/// thumbnail).
pub fn storyboard_nearest<T>(frames: &[Option<T>], index: usize) -> Option<usize> {
    frames
        .iter()
        .enumerate()
        .filter(|(_, frame)| frame.is_some())
        .min_by_key(|(at, _)| (at.abs_diff(index), *at))
        .map(|(at, _)| at)
}

/// The order frames are generated in: the one under the pointer first, then
/// outward, alternating right and left.
pub fn storyboard_order(start: usize, count: usize) -> Vec<usize> {
    let start = start.min(count.saturating_sub(1));
    let mut order = Vec::with_capacity(count);
    if count == 0 {
        return order;
    }
    order.push(start);
    for step in 1..count {
        if start + step < count {
            order.push(start + step);
        }
        if step <= start {
            order.push(start - step);
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storyboard_count_is_one_frame_per_16px_and_at_least_one() {
        assert_eq!(storyboard_count(256.0), 16);
        assert_eq!(storyboard_count(255.9), 15);
        assert_eq!(storyboard_count(10.0), 1);
        assert_eq!(storyboard_count(0.0), 1);
    }

    #[test]
    fn storyboard_index_covers_both_edges_and_n_equals_one() {
        assert_eq!(storyboard_index(0.0, 16), 0);
        assert_eq!(storyboard_index(1.0, 16), 15);
        assert_eq!(storyboard_index(1.5, 16), 15);
        assert_eq!(storyboard_index(-0.2, 16), 0);
        assert_eq!(storyboard_index(0.5, 16), 8);
        assert_eq!(storyboard_index(0.99, 1), 0);
        assert_eq!(storyboard_index(0.5, 0), 0);
    }

    #[test]
    fn storyboard_nearest_stands_in_for_a_hole() {
        let frames = [None, Some(1), None, None, Some(4)];
        assert_eq!(storyboard_nearest(&frames, 1), Some(1));
        assert_eq!(storyboard_nearest(&frames, 0), Some(1));
        assert_eq!(storyboard_nearest(&frames, 3), Some(4));
        // Equidistant: the lower one.
        assert_eq!(storyboard_nearest(&[Some(0), None, Some(2)], 1), Some(0));
        assert_eq!(storyboard_nearest::<u8>(&[None, None], 1), None);
    }

    #[test]
    fn storyboard_order_starts_under_the_pointer_and_covers_every_frame() {
        assert_eq!(storyboard_order(2, 5), vec![2, 3, 1, 4, 0]);
        assert_eq!(storyboard_order(0, 3), vec![0, 1, 2]);
        assert_eq!(storyboard_order(9, 3), vec![2, 1, 0]);
        assert_eq!(storyboard_order(0, 1), vec![0]);
        assert!(storyboard_order(0, 0).is_empty());
    }

    const SECOND: i64 = 10_000_000;

    fn opened(duration_100ns: i64) -> ClipPlayer {
        let mut player = ClipPlayer::default();
        player.open(3, PathBuf::from(r"C:\clips\a.mp4"), duration_100ns);
        player
    }

    #[test]
    fn opening_starts_at_zero_and_playing() {
        let player = opened(30 * SECOND);
        let clip = player.current().expect("a clip is open");
        assert_eq!(clip.row, 3);
        assert_eq!(clip.path, PathBuf::from(r"C:\clips\a.mp4"));
        assert!(clip.playing);
        assert_eq!(clip.position_100ns, 0);
        assert_eq!(clip.duration_100ns, 30 * SECOND);
        assert!(!clip.fullscreen);
        // an unreadable length becomes zero rather than a negative span.
        {
            let player = opened(-1);
            assert_eq!(player.current().unwrap().duration_100ns, 0);
        }
    }

    #[test]
    fn closing_drops_the_clip_and_its_full_screen_with_it() {
        let mut player = opened(30 * SECOND);
        player.current_mut().unwrap().fullscreen = true;
        player.close();
        assert!(!player.is_open());
        assert_eq!(player.toggle_play(), None, "nothing to pause");
        assert_eq!(player.seek_ratio(0.5), None, "nothing to seek");
    }

    #[test]
    fn toggling_play_returns_the_new_value() {
        let mut player = opened(30 * SECOND);
        assert_eq!(player.toggle_play(), Some(false));
        assert!(!player.current().unwrap().playing);
        assert_eq!(player.toggle_play(), Some(true));
        assert!(player.current().unwrap().playing);
    }

    /// task3530 follow-up: the three states the menu's 「再生」 asks about, in
    /// one test, so hollowing `is_paused` out to either constant shows up here
    /// rather than passing on whichever row happens to match.
    #[test]
    fn only_an_open_and_stopped_player_counts_as_paused() {
        let mut player = ClipPlayer::default();
        assert!(!player.is_paused(), "nothing open is not paused");
        player.open(0, PathBuf::from("a.mp4"), 30 * SECOND);
        assert!(!player.is_paused(), "a clip opens playing");
        player.toggle_play();
        assert!(player.is_paused(), "stopped with the clip still open");
        player.close();
        assert!(!player.is_paused(), "closing is not pausing");
    }

    #[test]
    fn a_seek_ratio_lands_on_the_tick_it_points_at() {
        let mut player = opened(30 * SECOND);
        assert_eq!(player.seek_ratio(0.0), Some(0));
        assert_eq!(player.seek_ratio(0.5), Some(15 * SECOND));
        assert_eq!(player.seek_ratio(1.0), Some(30 * SECOND));
        assert_eq!(player.current().unwrap().position_100ns, 30 * SECOND);
        // a seek ratio off either end is clamped.
        {
            let mut player = opened(30 * SECOND);
            assert_eq!(player.seek_ratio(-0.4), Some(0));
            assert_eq!(player.seek_ratio(4.0), Some(30 * SECOND));
        }
        // a nan ratio seeks to the start.
        // A zero-width seek bar divides by zero on the `.slint` side and hands a
        // NaN over. `f64::clamp` passes NaN through, so it is caught by name.
        {
            let mut player = opened(30 * SECOND);
            assert_eq!(player.seek_ratio(0.5), Some(15 * SECOND));
            assert_eq!(player.seek_ratio(f64::NAN), Some(0));
        }
    }

    #[test]
    fn every_seek_in_a_zero_length_clip_lands_at_zero() {
        let mut player = opened(0);
        assert_eq!(player.seek_ratio(0.5), Some(0));
        assert_eq!(player.seek_ratio(1.0), Some(0));
        assert_eq!(player.current().unwrap().played_ratio(), 0.0);
    }

    #[test]
    fn the_played_ratio_follows_the_position() {
        let mut player = opened(40 * SECOND);
        player.seek_ratio(0.25);
        assert!((player.current().unwrap().played_ratio() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn a_playing_clip_hides_its_controls_at_the_deadline_and_not_before() {
        let start = Instant::now();
        let hide = Duration::from_millis(CONTROLS_HIDE_MS);
        assert!(controls_visible(true, start, start, false));
        assert!(controls_visible(
            true,
            start,
            start + hide - Duration::from_millis(1),
            false
        ));
        assert!(
            !controls_visible(true, start, start + hide, false),
            "the review's 3s Timer fires *at* the interval"
        );
        assert!(!controls_visible(
            true,
            start,
            start + hide + Duration::from_millis(1),
            false
        ));
        // a paused clip keeps its controls however long it sits.
        {
            let start = Instant::now();
            let much_later = start + Duration::from_secs(600);
            assert!(controls_visible(false, start, much_later, false));
        }
        // something pinning the bar keeps it up while playing.
        {
            let start = Instant::now();
            let much_later = start + Duration::from_secs(600);
            assert!(controls_visible(true, start, much_later, true));
        }
        // a clock that went backwards does not hide the controls.
        {
            let start = Instant::now();
            assert!(controls_visible(
                true,
                start + Duration::from_secs(10),
                start,
                false
            ));
        }
    }

    #[test]
    fn the_preview_starts_at_the_dwell_and_not_before() {
        let since = Instant::now();
        let dwell = Duration::from_millis(400);
        assert!(!hover_preview_should_start(Some(since), since, dwell));
        assert!(!hover_preview_should_start(
            Some(since),
            since + dwell - Duration::from_millis(1),
            dwell
        ));
        assert!(hover_preview_should_start(
            Some(since),
            since + dwell,
            dwell
        ));
        assert!(hover_preview_should_start(
            Some(since),
            since + dwell + Duration::from_secs(5),
            dwell
        ));
        // a pointer on nothing never starts the preview.
        {
            let now = Instant::now();
            assert!(!hover_preview_should_start(
                None,
                now,
                Duration::from_millis(400)
            ));
        }
        // a zero dwell starts immediately.
        // A zero dwell is "start on contact", not "never start".
        {
            let since = Instant::now();
            assert!(hover_preview_should_start(
                Some(since),
                since,
                Duration::ZERO
            ));
        }
    }

    /// t260925-2509. Only a hover still waiting to start is asked again.
    #[test]
    fn a_declined_start_is_asked_again_only_while_the_same_hover_waits() {
        assert!(hover_preview_should_retry(Some(false), false, 1));
        assert!(!hover_preview_should_retry(None, false, 1));
        assert!(!hover_preview_should_retry(Some(true), false, 1));
        assert!(!hover_preview_should_retry(Some(false), true, 1));
        assert!(!hover_preview_should_retry(Some(false), false, 0));
    }

    /// t260916-956e. The whole table at once, so hollowing the match shows up
    /// as one failure per row rather than one test that stops at the first.
    #[test]
    fn the_transport_keys_reach_the_operations_the_clip_player_has() {
        for text in [" ", "k", "K"] {
            assert_eq!(
                clip_key_action(text),
                Some(ClipKeyAction::TogglePlay),
                "{text:?}"
            );
        }
        for text in ["m", "M"] {
            assert_eq!(
                clip_key_action(text),
                Some(ClipKeyAction::ToggleMute),
                "{text:?}"
            );
        }
        for text in ["f", "F"] {
            assert_eq!(
                clip_key_action(text),
                Some(ClipKeyAction::ToggleFullscreen),
                "{text:?}"
            );
        }
        // The arrows are slint's private-use codepoints, and they share the
        // branch j/l take -- which is what makes j/l a stand-in for them on a
        // harness that can only send characters (`send-keys.ps1`).
        for text in ["\u{F702}", "j", "J"] {
            assert_eq!(
                clip_key_action(text),
                Some(ClipKeyAction::Seek(StageZone::Left)),
                "{text:?}"
            );
        }
        for text in ["\u{F703}", "l", "L"] {
            assert_eq!(
                clip_key_action(text),
                Some(ClipKeyAction::Seek(StageZone::Right)),
                "{text:?}"
            );
        }
    }

    /// The absence half, with the positives above as its control: a match that
    /// answered `Some` for everything would fail here, and one that answered
    /// `None` for everything would fail there.
    ///
    /// `\u{F700}` / `\u{F701}` (up/down) and `s` are the review pane's keys for
    /// volume and the screenshot save -- operations `ClipVm` does not carry, so
    /// they are the ones most likely to be copied across by mistake. `\u{1b}`
    /// is Esc, which the `FocusScope` handles itself before asking.
    #[test]
    fn keys_the_clip_player_has_no_operation_for_fall_through() {
        for text in [
            "a", "\u{1b}", "\u{F700}", "\u{F701}", "s", "S", "<", ">", ",", ".", "\u{F729}",
            "\u{F72B}", "", "kk",
        ] {
            assert_eq!(clip_key_action(text), None, "{text:?}");
        }
    }
}
