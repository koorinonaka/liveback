//! The video stage's press / hold / release machine (task108/119), out of the
//! bin's timer closures (t261102-918c). Every transition takes what it reads
//! as arguments -- the clock, `playing`, `rate`, `position` -- and returns the
//! effects in the order the bin applies them. The bin owns the timers, the
//! engine and the paint; nothing here names either.

use std::time::{Duration, Instant};

use super::{
    fold_hold, fold_tap, hold_step_100ns, hold_step_seconds, stage_double_press, stage_hold_rate,
    stage_tap_seconds, tap_delta_100ns, SeekOutcome, StageFeedback, StageZone,
    STAGE_FEEDBACK_FADE_MS, STAGE_FEEDBACK_HIDE_MS, STAGE_HOLD_DELAY_MS, STAGE_REPEAT_INTERVAL_MS,
    STAGE_TAP_RESET_MS,
};

/// A stage gesture in flight: zone fixed at pointerdown, hold bookkeeping
/// accumulated by the repeat timer. Read-only outside this module.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StageGesture {
    pub zone: StageZone,
    /// When the press landed: a centre tap's double-click window counts from
    /// here, not from its release.
    pub pressed_at: Instant,
    pub held: bool,
    pub hold_seconds: i64,
    pub hold_steps: u32,
    /// The rate the centre hold borrowed x2 from (task650), owed back on
    /// release or cancel. `None` for every other gesture.
    pub restore_rate: Option<f64>,
    /// The centre hold started on a paused video, so release owes a pause as
    /// well as the rate (task1010).
    pub repause: bool,
}

/// What a transition asks the bin to do, in order.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum StageEffect {
    /// Start the hold timer: one shot, then `StageGesture::hold_fired`.
    ArmHold(Duration),
    /// Restart the hold timer as a repeat of `StageGesture::hold_step`.
    ArmRepeat(Duration),
    StopHold,
    /// Set the rate and send it to the engine.
    SetRate(f64),
    Play,
    Pause {
        reason: &'static str,
    },
    TogglePlay,
    /// Arm the double-click pairing with the playhead as it stands now
    /// (task3270): the bin reads the position, before the toggle that follows.
    RememberCenterTap(Instant),
    /// The second press of a double click: put back the position saved with
    /// the opening tap, through `restore_after_double_press` once the toggle
    /// has landed (task3270).
    RestoreBeforeTap(Option<i64>),
    ToggleFullscreen,
    /// Cancel the pending precise settle (task2580).
    StopSettle,
    /// Book the precise settle (task2580).
    ArmSettle,
    /// Stop the band's pending dwell and fade (t260929-40a4).
    StopFeedbackExit,
    Feedback(Option<StageFeedback>),
    /// A coarse seek whose outcome folds into the band.
    Seek {
        target: i64,
        fold: SeekFold,
    },
    /// Book the band's exit (`feedback_exit`).
    BookFeedbackExit,
}

/// How a seek's outcome becomes the band.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SeekFold {
    Tap {
        delta_seconds: i64,
    },
    Hold {
        direction: i64,
        seconds: i64,
        steps: u32,
    },
}

impl SeekFold {
    pub fn apply(self, previous: Option<StageFeedback>, outcome: SeekOutcome) -> StageFeedback {
        match self {
            SeekFold::Tap { delta_seconds } => fold_tap(previous, outcome, delta_seconds),
            SeekFold::Hold {
                direction,
                seconds,
                steps,
            } => fold_hold(outcome, direction, seconds, steps),
        }
    }
}

/// A press inside the picture. `last_tap` is the bin's pairing slot, taken:
/// every press clears it. Either the second half of a centre double click
/// (no gesture, full screen) or a new gesture with its hold armed.
pub fn stage_press(
    last_tap: Option<(Instant, i64)>,
    zone: StageZone,
    now: Instant,
) -> (Option<StageGesture>, Vec<StageEffect>) {
    let since = last_tap.map(|(at, _)| now.saturating_duration_since(at).as_millis() as u64);
    if stage_double_press(since, zone) {
        return (
            None,
            vec![
                StageEffect::TogglePlay,
                StageEffect::RestoreBeforeTap(last_tap.map(|(_, position)| position)),
                StageEffect::Feedback(None),
                StageEffect::StopHold,
                StageEffect::ToggleFullscreen,
            ],
        );
    }
    let gesture = StageGesture {
        zone,
        pressed_at: now,
        held: false,
        hold_seconds: 0,
        hold_steps: 0,
        restore_rate: None,
        repause: false,
    };
    (
        Some(gesture),
        vec![StageEffect::ArmHold(Duration::from_millis(
            STAGE_HOLD_DELAY_MS,
        ))],
    )
}

impl StageGesture {
    /// The hold delay ran out with the button still down. Centre: x2 for as
    /// long as it is held (task650), playing even from paused (task1010).
    /// Side: the first seek step, then the repeat.
    pub fn hold_fired(&mut self, playing: bool, rate: f64, position: i64) -> Vec<StageEffect> {
        if self.zone != StageZone::Center {
            let mut effects = self.hold_step(position);
            effects.push(StageEffect::ArmRepeat(Duration::from_millis(
                STAGE_REPEAT_INTERVAL_MS,
            )));
            return effects;
        }
        // Held, so the release cannot fall through to the play/pause tap.
        let hold = stage_hold_rate(playing, rate);
        self.held = true;
        self.restore_rate = Some(hold.restore);
        self.repause = hold.repause;
        let mut effects = vec![StageEffect::SetRate(hold.applied)];
        if hold.repause {
            effects.push(StageEffect::Play);
        }
        // t260929-40a4: an earlier tap's exit would take the band down mid-hold.
        effects.push(StageEffect::StopFeedbackExit);
        effects.push(StageEffect::Feedback(Some(StageFeedback::RateHold)));
        effects
    }

    /// One side-hold step from the engine's real position -- not a private
    /// cursor, which walks off to somewhere the decoder never reaches.
    pub fn hold_step(&mut self, position: i64) -> Vec<StageEffect> {
        self.held = true;
        self.hold_steps += 1;
        self.hold_seconds += hold_step_seconds(self.hold_steps);
        vec![
            // A stray settle must not slip a precise decode into the scan (task2580).
            StageEffect::StopSettle,
            StageEffect::Seek {
                target: position + hold_step_100ns(self.zone, self.hold_steps),
                fold: SeekFold::Hold {
                    direction: self.zone.direction(),
                    seconds: self.hold_seconds,
                    steps: self.hold_steps,
                },
            },
            // t260929-40a4: an earlier tap's pending exit must not blank a step.
            StageEffect::StopFeedbackExit,
        ]
    }

    /// The button came up: give back what a centre hold borrowed, book the
    /// settle after any side gesture, and fire the tap only if nothing was held.
    pub fn release(self, position: i64) -> Vec<StageEffect> {
        let mut effects = Vec::new();
        // The x2 lasts exactly as long as the button is down (task650).
        if let Some(rate) = self.restore_rate {
            effects.push(StageEffect::SetRate(rate));
            effects.push(StageEffect::Feedback(None));
        }
        // Started paused: hand back a paused video (task1010).
        if self.repause {
            effects.push(StageEffect::Pause {
                reason: "skim_release",
            });
        }
        // Any side release -- tap or hold -- books the settle (task2580).
        if self.zone != StageZone::Center {
            effects.push(StageEffect::ArmSettle);
        }
        // A hold already did the seeking; releasing it must not also tap.
        if !self.held {
            match self.zone {
                StageZone::Center => {
                    effects.push(StageEffect::RememberCenterTap(self.pressed_at));
                    effects.push(StageEffect::TogglePlay);
                    effects.push(StageEffect::Feedback(None));
                }
                zone => {
                    // Read once: the band's sum and the seek must agree.
                    let seconds = stage_tap_seconds();
                    effects.push(StageEffect::Seek {
                        target: position + tap_delta_100ns(zone, seconds),
                        fold: SeekFold::Tap {
                            delta_seconds: zone.direction() * seconds,
                        },
                    });
                }
            }
        }
        effects.push(StageEffect::BookFeedbackExit);
        effects
    }

    /// The compositor abandoned the gesture: owe back the borrowed rate and
    /// pause (task650 / task1010), and nothing else -- no tap, no band.
    pub fn cancel(self) -> Vec<StageEffect> {
        let Some(rate) = self.restore_rate else {
            return Vec::new();
        };
        let mut effects = vec![StageEffect::SetRate(rate)];
        if self.repause {
            effects.push(StageEffect::Pause {
                reason: "gesture_abandoned",
            });
        }
        effects.push(StageEffect::Feedback(None));
        effects
    }
}

/// The band's exit (task1020): dwell, then hide and fade, then clear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedbackExit {
    pub dwell: Duration,
    pub fade: Duration,
}

pub fn feedback_exit(feedback: Option<StageFeedback>) -> FeedbackExit {
    let dwell = match feedback {
        // A tap's accumulator (±10 → ±20) has to outlast the double tap it counts.
        Some(StageFeedback::Tap { .. }) => STAGE_TAP_RESET_MS,
        _ => STAGE_FEEDBACK_HIDE_MS,
    };
    FeedbackExit {
        dwell: Duration::from_millis(dwell),
        fade: Duration::from_millis(STAGE_FEEDBACK_FADE_MS),
    }
}

/// The settle's precise seek (task2540/2580): paused only -- playing, the
/// engine already walks to the target and pays the precise cost itself.
pub fn settle_confirm(playing: bool, position: i64) -> Option<i64> {
    (!playing).then_some(position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_state::playback::{STAGE_DOUBLE_PRESS_MS, STAGE_HOLD_RATE};
    use crate::ui_state::timeline::HNS_PER_SECOND;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn pressed(zone: StageZone, now: Instant) -> StageGesture {
        let (gesture, effects) = stage_press(None, zone, now);
        assert_eq!(effects, vec![StageEffect::ArmHold(ms(STAGE_HOLD_DELAY_MS))]);
        gesture.expect("a plain press starts a gesture")
    }

    /// task650: the centre hold borrows x2 and the release gives the old rate back.
    #[test]
    fn task650_a_centre_hold_borrows_x2_and_the_release_returns_the_rate() {
        let mut gesture = pressed(StageZone::Center, Instant::now());
        let fired = gesture.hold_fired(true, 1.5, 0);
        assert_eq!(
            fired,
            vec![
                StageEffect::SetRate(STAGE_HOLD_RATE),
                StageEffect::StopFeedbackExit,
                StageEffect::Feedback(Some(StageFeedback::RateHold)),
            ]
        );
        assert_eq!(
            gesture.release(0),
            vec![
                StageEffect::SetRate(1.5),
                StageEffect::Feedback(None),
                StageEffect::BookFeedbackExit,
            ]
        );
    }

    /// task1010: a centre hold from paused plays, and its release pauses again.
    #[test]
    fn task1010_a_paused_centre_hold_plays_and_its_release_pauses() {
        let mut gesture = pressed(StageZone::Center, Instant::now());
        let fired = gesture.hold_fired(false, 1.0, 0);
        assert!(fired.contains(&StageEffect::Play));
        let released = gesture.release(0);
        assert!(released.contains(&StageEffect::Pause {
            reason: "skim_release"
        }));
        // No play/pause tap on top of the skim: the hold already decided.
        assert!(!released.contains(&StageEffect::TogglePlay));
        // Control: the same hold from playing owes no pause.
        let mut playing = pressed(StageZone::Center, Instant::now());
        assert!(!playing
            .hold_fired(true, 1.0, 0)
            .contains(&StageEffect::Play));
        assert!(!playing
            .release(0)
            .iter()
            .any(|e| matches!(e, StageEffect::Pause { .. })));
    }

    /// task2580: side-hold steps add up their seconds; the release books the
    /// settle and does not also fire the tap's seek.
    #[test]
    fn task2580_a_side_hold_accumulates_steps_and_its_release_does_not_tap() {
        let mut gesture = pressed(StageZone::Right, Instant::now());
        let first = gesture.hold_fired(true, 1.0, 0);
        assert_eq!(first[0], StageEffect::StopSettle);
        assert_eq!(
            first.last(),
            Some(&StageEffect::ArmRepeat(ms(STAGE_REPEAT_INTERVAL_MS)))
        );
        for _ in 1..6 {
            gesture.hold_step(0);
        }
        // Steps 1-5 jump 1s each, step 6 doubles to 2s.
        assert_eq!((gesture.hold_steps, gesture.hold_seconds), (6, 7));
        assert!(matches!(
            gesture.hold_step(100)[1],
            StageEffect::Seek { target, fold: SeekFold::Hold { seconds: 9, steps: 7, direction: 1 } }
                if target == 100 + 2 * HNS_PER_SECOND
        ));
        let released = gesture.release(0);
        assert!(released.contains(&StageEffect::ArmSettle));
        assert!(!released
            .iter()
            .any(|e| matches!(e, StageEffect::Seek { .. })));
        // Control: a side tap (never held) does seek, and books the settle too.
        let tap = pressed(StageZone::Left, Instant::now()).release(0);
        assert!(tap.contains(&StageEffect::ArmSettle));
        assert!(tap.iter().any(|e| matches!(
            e,
            StageEffect::Seek { fold: SeekFold::Tap { delta_seconds }, .. } if *delta_seconds < 0
        )));
    }

    /// task650 / task1010: a cancel returns the borrowed rate and pause and
    /// nothing else -- no tap, no band of its own.
    #[test]
    fn a_cancel_only_returns_what_the_hold_borrowed() {
        let mut gesture = pressed(StageZone::Center, Instant::now());
        gesture.hold_fired(false, 0.5, 0);
        assert_eq!(
            gesture.cancel(),
            vec![
                StageEffect::SetRate(0.5),
                StageEffect::Pause {
                    reason: "gesture_abandoned"
                },
                StageEffect::Feedback(None),
            ]
        );
        // A tap that never became a hold has nothing to give back.
        assert!(pressed(StageZone::Center, Instant::now())
            .cancel()
            .is_empty());
        assert!(pressed(StageZone::Right, Instant::now())
            .cancel()
            .is_empty());
    }

    /// task3270: the second press inside the window toggles full screen and
    /// restores the opening tap's position; outside the window it is a press.
    #[test]
    fn task3270_a_double_press_goes_full_screen_and_restores_the_first_position() {
        let t0 = Instant::now();
        let released = pressed(StageZone::Center, t0).release(42);
        assert!(released.contains(&StageEffect::RememberCenterTap(t0)));
        // The bin records (t0, position) on that effect.
        let (gesture, effects) = stage_press(
            Some((t0, 42)),
            StageZone::Center,
            t0 + ms(STAGE_DOUBLE_PRESS_MS),
        );
        assert_eq!(gesture, None);
        assert!(effects.contains(&StageEffect::ToggleFullscreen));
        assert!(effects.contains(&StageEffect::RestoreBeforeTap(Some(42))));
        assert!(effects.contains(&StageEffect::StopHold));
        // Outside the window: an ordinary press.
        let (gesture, effects) = stage_press(
            Some((t0, 42)),
            StageZone::Center,
            t0 + ms(STAGE_DOUBLE_PRESS_MS + 1),
        );
        assert!(gesture.is_some());
        assert_eq!(effects, vec![StageEffect::ArmHold(ms(STAGE_HOLD_DELAY_MS))]);
    }

    /// t260929-40a4: every hold step stops the band's pending exit.
    #[test]
    fn t260929_40a4_every_hold_step_stops_the_feedback_exit() {
        let mut side = pressed(StageZone::Left, Instant::now());
        assert!(side
            .hold_fired(true, 1.0, 0)
            .contains(&StageEffect::StopFeedbackExit));
        assert!(side.hold_step(0).contains(&StageEffect::StopFeedbackExit));
        let mut centre = pressed(StageZone::Center, Instant::now());
        assert!(centre
            .hold_fired(true, 1.0, 0)
            .contains(&StageEffect::StopFeedbackExit));
    }

    /// task1020: a tap's band dwells for the tap window, every other band less.
    #[test]
    fn the_feedback_exit_dwells_longer_for_a_tap() {
        let tap = Some(StageFeedback::Tap {
            direction: 1,
            seconds: 10,
        });
        assert_eq!(feedback_exit(tap).dwell, ms(STAGE_TAP_RESET_MS));
        assert_eq!(
            feedback_exit(Some(StageFeedback::RateHold)).dwell,
            ms(STAGE_FEEDBACK_HIDE_MS)
        );
        assert_eq!(feedback_exit(None).fade, ms(STAGE_FEEDBACK_FADE_MS));
    }
}
