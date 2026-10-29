use super::*;
// [0,2s) + gap + [3s,5s), live edge 5s.
use crate::ui_state::timeline::seconds_text;
use crate::ui_state::timeline::tests::fixture;
use std::cell::Cell;

#[test]
fn stage_target_scales_the_logical_size_by_the_scale_factor() {
    assert_eq!(stage_target(1200.0, 675.0, 1.0), Some((1200, 675)));
    assert_eq!(stage_target(800.0, 450.0, 1.5), Some((1200, 675)));
    assert_eq!(stage_target(600.0, 337.5, 2.0), Some((1200, 675)));
    // Slint reports a `length` at subpixel precision, so the physical size
    // rounds rather than truncating -- a stage 0.6px wider is a pixel wider.
    assert_eq!(stage_target(1199.6, 674.6, 1.0), Some((1200, 675)));
    assert_eq!(stage_target(1199.4, 674.4, 1.0), Some((1199, 674)));
    // Absent until the stage has a size.
    assert_eq!(stage_target(0.0, 0.0, 1.0), None);
    assert_eq!(stage_target(1200.0, 0.0, 1.0), None);
    assert_eq!(stage_target(0.0, 675.0, 1.0), None);
    assert_eq!(stage_target(-5.0, 675.0, 1.0), None);
}

/// t260926-cc7e: the mode's transitions, one event at a time.
#[test]
fn the_follow_mode_is_joined_by_its_own_landings_and_left_by_the_user() {
    let off = Follow::default();
    let on = off.apply(FollowEvent::Join);
    assert_eq!(
        on,
        Follow {
            on: true,
            range_exempt: true
        }
    );
    // Catching up by itself (x2, or a forward seek into the last
    // segment) joins exactly as the chip does.
    assert_eq!(off.apply(FollowEvent::Arrive), on);
    // A seek drops both the mode and the range exemption.
    assert_eq!(on.apply(FollowEvent::UserSeek), off);
    // A pause drops the mode but keeps the exemption: pausing and playing
    // again carries on where it is, not at the selection's start.
    let paused = Follow {
        on: false,
        range_exempt: true,
    };
    assert_eq!(on.apply(FollowEvent::UserPause), paused);
    // Only a rate other than x1 leaves it.
    assert_eq!(on.apply(FollowEvent::Rate(1.0)), on);
    assert_eq!(on.apply(FollowEvent::Rate(2.0)), paused);
    assert_eq!(on.apply(FollowEvent::Rate(0.5)), paused);
    // And the next user seek is what gives the selection its stops back.
    assert_eq!(paused.apply(FollowEvent::UserSeek), off);
}

#[test]
fn a_follow_rejoins_only_past_the_steady_lag_and_never_twice_in_flight() {
    let on = Follow::default().apply(FollowEvent::Join);
    let lag = follow_rejoin_lag_100ns();
    // t261012-5b97: the threshold sits between the steady follow's worst
    // lag, D + m, and the 2D a park leaves it stuck at.
    let d = crate::encoder::segment_duration_100ns();
    let steady_max = d + crate::playback::LIVE_FOLLOW_MARGIN_100NS;
    let jitter = 1_000_000; // 100ms late on top of it
    assert!(!follow_rejoin_due(
        on,
        true,
        true,
        steady_max + jitter,
        false
    ));
    assert!(follow_rejoin_due(on, true, true, 2 * d, false));
    assert!(follow_rejoin_due(on, true, true, lag + 1, false));
    // Exactly the threshold is still following: 超えたら.
    assert!(!follow_rejoin_due(on, true, true, lag, false));
    // The previous seek has not reached the engine yet.
    assert!(!follow_rejoin_due(on, true, true, lag * 4, true));
    // The cover gate's pause freezes the position; its resume rejoins.
    assert!(!follow_rejoin_due(on, true, false, lag * 4, false));
    // Off, or the recording stopped: never.
    assert!(!follow_rejoin_due(
        Follow::default(),
        true,
        true,
        lag * 4,
        false
    ));
    assert!(!follow_rejoin_due(on, false, true, lag * 4, false));
}

#[test]
fn only_the_engine_parking_out_of_playback_counts_as_arriving() {
    // Playing, parked at the edge, nothing in flight.
    assert!(follow_arrives(true, true, true, false));
    // The user paused first: the UI's own account is already false.
    assert!(!follow_arrives(true, false, true, false));
    // A seek away from the park that the engine has not taken yet.
    assert!(!follow_arrives(true, true, true, true));
    // Not at the edge, or an ended session at its end.
    assert!(!follow_arrives(true, true, false, false));
    assert!(!follow_arrives(false, true, true, false));
    // An x2 catch-up joins like any other arrival (the caller puts the
    // rate back to x1).
    let arrived = Follow::default()
        .apply(FollowEvent::Rate(2.0))
        .apply(FollowEvent::Arrive);
    assert!(arrived.on);
}

/// t260921-6ba9: the hotkey takes the playhead only with all four inputs
/// in its favour; any one missing falls back to the live edge.
#[test]
fn the_marker_hotkey_takes_the_playhead_only_when_every_condition_holds() {
    use HotkeyMarkTarget::{LiveEdge, Playhead};
    assert_eq!(hotkey_mark_target(true, true, true, false), Playhead);
    assert_eq!(hotkey_mark_target(true, true, true, true), LiveEdge);
    assert_eq!(hotkey_mark_target(false, true, true, false), LiveEdge);
    assert_eq!(hotkey_mark_target(true, false, true, false), LiveEdge);
    assert_eq!(hotkey_mark_target(true, true, false, false), LiveEdge);
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
    // On screen and playing: nothing to do (task2800).
    assert_eq!(
        offscreen_transport(true, true, false),
        OffscreenTransport::Leave
    );
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
    let wrap = |range, position| {
        range_guard(
            TransportState {
                playing: true,
                repeat: true,
                ..TransportState::default()
            },
            false,
            range,
            position,
        )
    };
    assert_eq!(wrap(None, 100), RangeGuard::None);
    assert_eq!(wrap(Some((10, 50)), 49), RangeGuard::None);
    assert_eq!(wrap(Some((10, 50)), 50), RangeGuard::WrapToStart);
    assert_eq!(wrap(Some((10, 50)), 80), RangeGuard::WrapToStart);
    // A degenerate (empty) selection must not seek-loop forever.
    assert_eq!(wrap(Some((10, 10)), 10), RangeGuard::None);
    // repeat jumps forward to the start too.
    {
        assert_eq!(
            range_guard(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                false,
                Some((10, 50)),
                9
            ),
            RangeGuard::JumpToStart
        );
    }
}

#[test]
fn the_selection_holds_playback_inside_itself_with_repeat_off() {
    let guard = |range, position| {
        range_guard(
            TransportState {
                playing: true,
                ..TransportState::default()
            },
            false,
            range,
            position,
        )
    };
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
fn a_parked_engine_only_ever_wraps() {
    // Paused at the live edge past the end with repeat off: nothing, or the
    // 100ms drain would drag the playhead back to the end every tick.
    assert_eq!(
        range_guard(
            TransportState {
                ..TransportState::default()
            },
            false,
            Some((10, 50)),
            80
        ),
        RangeGuard::None
    );
    assert_eq!(
        range_guard(
            TransportState {
                ..TransportState::default()
            },
            false,
            Some((10, 50)),
            9
        ),
        RangeGuard::None
    );
    // Repeat on, though, restarts the loop from a parked engine (task153).
    assert_eq!(
        range_guard(
            TransportState {
                repeat: true,
                ..TransportState::default()
            },
            false,
            Some((10, 50)),
            80
        ),
        RangeGuard::WrapToStart
    );
}

#[test]
fn a_live_park_arms_the_end_hold_like_playback() {
    // t260926-c0b3: the engine's park reports playing false, yet it is
    // playback the user never stopped.
    assert!(arms_end_hold(true, false, false));
    assert!(arms_end_hold(true, true, true));
    assert!(arms_end_hold(false, true, true));
    // Paused away from the edge, or at the end of a session no longer
    // recording (never resumed): nothing to keep playing past.
    assert!(!arms_end_hold(false, false, true));
    assert!(!arms_end_hold(false, true, false));
    assert!(!arms_end_hold(false, false, false));
}

#[test]
fn the_end_hold_clears_only_the_stop_at_the_end() {
    // task4230: the end set to the playhead while playing. Control first:
    // the same call without the hold stops, so the rows below are not
    // passing on an input that never reached `PauseAtEnd`.
    let range = Some((10, 50));
    let guard = |playing, hold, repeat, position| {
        range_guard(
            TransportState {
                playing,
                repeat,
                ..TransportState::default()
            },
            hold,
            range,
            position,
        )
    };
    for position in [50, 51, 80] {
        assert_eq!(guard(true, false, false, position), RangeGuard::PauseAtEnd);
        assert_eq!(guard(true, true, false, position), RangeGuard::None);
        // Repeat's wrap wins over the hold, as it does over `skimming`.
        assert_eq!(guard(true, true, true, position), RangeGuard::WrapToStart);
        assert_eq!(guard(false, true, true, position), RangeGuard::WrapToStart);
        // Not playing: the hold changes nothing.
        assert_eq!(
            guard(false, true, false, position),
            guard(false, false, false, position)
        );
    }
    // Before the start the hold is not an exemption: still dragged forward.
    assert_eq!(guard(true, true, false, 9), RangeGuard::JumpToStart);
    assert_eq!(guard(true, true, false, 30), RangeGuard::None);
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
        let leaves = |target| {
            seek_plan(
                TransportState {
                    playing: true,
                    repeat,
                    ..TransportState::default()
                },
                false,
                range,
                target,
                lands,
            )
        };
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
            seek_plan(
                TransportState {
                    playing: true,
                    repeat,
                    ..TransportState::default()
                },
                false,
                None,
                80,
                lands
            ),
            SeekPlan::Go,
            "repeat={repeat}"
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat,
                    ..TransportState::default()
                },
                false,
                Some((10, 10)),
                10,
                lands
            ),
            SeekPlan::Go,
            "repeat={repeat}"
        );
        // Already paused: this is the user scrubbing outside the
        // selection, and there is no playback left to stop -- nor, since
        // task3570, to wrap.
        assert_eq!(
            seek_plan(
                TransportState {
                    repeat,
                    ..TransportState::default()
                },
                false,
                range,
                9,
                lands
            ),
            SeekPlan::Go,
            "repeat={repeat}"
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    repeat,
                    ..TransportState::default()
                },
                false,
                range,
                80,
                lands
            ),
            SeekPlan::Go,
            "repeat={repeat}"
        );
    }
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
    let skim = |repeat, target| {
        seek_plan(
            TransportState {
                playing: true,
                skimming: true,
                repeat,
            },
            false,
            range,
            target,
            lands,
        )
    };
    let no_skim = |repeat, target| {
        seek_plan(
            TransportState {
                playing: true,
                repeat,
                ..TransportState::default()
            },
            false,
            range,
            target,
            lands,
        )
    };

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
                seek_plan(
                    TransportState {
                        skimming: true,
                        repeat,
                        ..TransportState::default()
                    },
                    false,
                    range,
                    target,
                    lands
                ),
                seek_plan(
                    TransportState {
                        repeat,
                        ..TransportState::default()
                    },
                    false,
                    range,
                    target,
                    lands
                ),
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
            seek_plan(
                TransportState {
                    playing: true,
                    skimming,
                    repeat: true
                },
                false,
                range,
                80,
                |_| Some(50)
            ),
            expected,
            "skimming={skimming}"
        );
        // Nothing recorded at or after the start at all: no landing.
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    skimming,
                    repeat: true
                },
                false,
                range,
                80,
                |_| None
            ),
            expected,
            "skimming={skimming}"
        );
        // The wrap that *succeeds* is unaffected either way -- the split
        // is only in the fallback.
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    skimming,
                    repeat: true
                },
                false,
                range,
                80,
                |_| Some(30)
            ),
            SeekPlan::WrapTo(30),
            "skimming={skimming}"
        );
    }
    // a wrap that lands outside too gives up instead of hopping again.
    {
        // The pathological selection: a gap swallows all of it, so
        // `classify_seek` carries its start forward to or past the end.
        // Re-entering `seek_clamped` there would ask the same question, get
        // the same answer and recurse until the stack ran out -- so the hop is
        // one deep and falls back to task3380's stop instead. Before task3570
        // this range merely span an idle seek per frame through
        // `poll_range_guard_with`; it must not get worse than that.
        let range = Some((10, 50));
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                false,
                range,
                80,
                |_| Some(50)
            ),
            SeekPlan::PauseFirst
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                false,
                range,
                80,
                |_| Some(99)
            ),
            SeekPlan::PauseFirst
        );
        // Nothing recorded at or after the start at all (`SeekOutcome::
        // Buffering`, so no landing): same fallback.
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                false,
                range,
                80,
                |_| None
            ),
            SeekPlan::PauseFirst
        );
        // The healthy row, for contrast -- and a gap just after the start
        // still wraps, because 30 is inside the selection.
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                false,
                range,
                80,
                |_| Some(10)
            ),
            SeekPlan::WrapTo(10)
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                false,
                range,
                80,
                |_| Some(30)
            ),
            SeekPlan::WrapTo(30)
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
        let plan = seek_plan(
            TransportState {
                playing: true,
                skimming,
                repeat: true,
            },
            false,
            range,
            80,
            |start| {
                calls.set(calls.get() + 1);
                asked.set(start);
                Some(50)
            },
        );
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
        seek_plan(
            TransportState {
                playing: true,
                repeat,
                ..TransportState::default()
            },
            false,
            range,
            target,
            |_| {
                calls.set(calls.get() + 1);
                Some(10)
            },
        );
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
        seek_plan(
            TransportState {
                playing: true,
                repeat: true,
                ..TransportState::default()
            },
            true,
            range,
            80,
            lands
        ),
        SeekPlan::PauseFirst,
        "gesturing"
    );
    assert_eq!(
        seek_plan(
            TransportState {
                playing: true,
                repeat: true,
                ..TransportState::default()
            },
            false,
            range,
            80,
            lands
        ),
        SeekPlan::WrapTo(10),
        "not gesturing"
    );

    // The wrap is not merely discarded, it is never asked for: the gate
    // returns before `wrapped` runs.
    let calls = Cell::new(0u32);
    seek_plan(
        TransportState {
            playing: true,
            repeat: true,
            ..TransportState::default()
        },
        true,
        range,
        80,
        |start| {
            calls.set(calls.get() + 1);
            Some(start)
        },
    );
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
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                gesturing,
                range,
                30,
                lands
            ),
            SeekPlan::Go,
            "inside, gesturing={gesturing}"
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    repeat: true,
                    ..TransportState::default()
                },
                gesturing,
                range,
                9,
                lands
            ),
            SeekPlan::PauseFirst,
            "before the start, gesturing={gesturing}"
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    playing: true,
                    ..TransportState::default()
                },
                gesturing,
                range,
                80,
                lands
            ),
            SeekPlan::PauseFirst,
            "repeat off past the end, gesturing={gesturing}"
        );
        assert_eq!(
            seek_plan(
                TransportState {
                    repeat: true,
                    ..TransportState::default()
                },
                gesturing,
                range,
                80,
                lands
            ),
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
        seek_plan(
            TransportState {
                playing: true,
                skimming: true,
                repeat: true
            },
            true,
            range,
            80,
            Some
        ),
        SeekPlan::WrapTo(10),
        "the skim still wraps"
    );
    assert_eq!(
        seek_plan(
            TransportState {
                playing: true,
                skimming: true,
                repeat: true
            },
            true,
            range,
            80,
            |_| None
        ),
        SeekPlan::Go,
        "task3660's fallback still lets the skim run"
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
            range_guard(
                TransportState {
                    playing,
                    skimming,
                    repeat,
                },
                false,
                Some((10, 50)),
                position,
            )
        };
        let held = |playing, repeat, position| {
            range_guard(
                TransportState {
                    playing,
                    skimming: stage_skimming(StageZone::Center, false, repause),
                    repeat,
                },
                false,
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
    // a hold that started paused plays at x2 and stops again.
    {
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
    // DS Stage README (t260928-ac18, review 06 F2 / F3): 「10 秒」,
    // the direction left to the arrow, a half-width space before the
    // unit; the taps add up to 「20 秒」.
    assert_eq!(seconds_text(Locale::Ja, 10), "10 秒");
    assert_eq!(seconds_text(Locale::Ja, 20), "20 秒");
    assert_eq!(seconds_text(Locale::Ja, 30), "30 秒");
    assert_eq!(seconds_text(Locale::En, 30), "30s");
    // The first-run hint names the jump the setting chose, not a fixed 10.
    assert!(stage_hint(Locale::Ja, 15).contains(&seconds_text(Locale::Ja, 15)));
    assert!(stage_hint(Locale::En, 15).contains(&seconds_text(Locale::En, 15)));
}

#[test]
fn the_first_run_hint_shows_once() {
    // DS Stage README: 「はじめての 1 回だけ」 (t260928-ac18, review 06 F11).
    assert_eq!(STAGE_HINT_MAX_SHOWINGS, 1);
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
    // a repeating clip wraps when the engine parks on its end.
    // t260916-0ee4. The clip player's loop point. `range_guard` cannot be
    // asked this question at all (see `clip_repeat_wrap`), so the rule is its
    // own function and this is the one arrangement that fires it: repeat on,
    // the transport was running, it is not running now, the engine parked on
    // the end, and nothing this side sent is still in flight.
    {
        assert!(clip_repeat_wrap(true, true, false, true, false));
    }
}

mod live_skip {
    use super::*;

    #[test]
    fn a_seek_onto_the_live_end_is_not_stopped() {
        assert!(lands_on_live_end(false, Some((0, 599)), 600, 599));
        // Repeat wraps, a shorter selection stops, and so does no selection
        // -- that one has no stop to skip anyway.
        assert!(!lands_on_live_end(true, Some((0, 599)), 600, 599));
        assert!(!lands_on_live_end(false, Some((0, 400)), 600, 599));
        assert!(!lands_on_live_end(false, Some((0, 599)), 600, 300));
        assert!(!lands_on_live_end(false, None, 600, 599));
    }

    #[test]
    fn switching_the_stage_to_another_session_does_not() {
        assert!(!stop_skip_arms(Some("a"), Some("b"), false, true));
        assert!(!stop_skip_arms(Some("a"), None, false, true));
        // Still recording, and never was: no edge either.
        assert!(!stop_skip_arms(Some("a"), Some("a"), true, true));
        assert!(!stop_skip_arms(None, Some("a"), false, true));
        // the same session stopping while following arms the skip.
        {
            assert!(stop_skip_arms(Some("a"), Some("a"), false, true));
        }
        // Not following -- rewound and playing, or paused (either drops the
        // mode, t260926-cc7e) -- stays put.
        {
            assert!(!stop_skip_arms(Some("a"), Some("a"), false, false));
        }
    }

    #[test]
    fn an_arm_skips_at_once_then_again_when_the_tail_lands() {
        let armed = live_skip(Some(("a", i64::MIN)), "a", 600, None);
        assert_eq!(
            armed,
            LiveSkip {
                seek_to: Some(599),
                stay_armed: true
            }
        );
        // Nothing new: no second seek.
        assert_eq!(live_skip(Some(("a", 600)), "a", 600, None).seek_to, None);
        // The tail grew the snapshot, and the manifest is closed with it.
        assert_eq!(
            live_skip(Some(("a", 600)), "a", 700, Some(700)),
            LiveSkip {
                seek_to: Some(699),
                stay_armed: false
            }
        );
        // a closed manifest the snapshot has not caught up with stays armed.
        {
            let tick = live_skip(Some(("a", 600)), "a", 600, Some(700));
            assert_eq!(tick.seek_to, None);
            assert!(tick.stay_armed);
        }
    }

    #[test]
    fn another_session_on_the_stage_drops_the_arm_without_seeking() {
        assert_eq!(
            live_skip(Some(("a", i64::MIN)), "b", 600, None),
            LiveSkip {
                seek_to: None,
                stay_armed: false
            }
        );
        assert!(!live_skip(None, "a", 600, None).stay_armed);
    }
}
