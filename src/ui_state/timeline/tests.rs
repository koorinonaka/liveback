use super::*;

fn segment(index: u64, start: i64, end: i64) -> TimelineSegment {
    TimelineSegment {
        index,
        start_100ns: start,
        end_100ns: end,
        audio_offsets_100ns: vec![0],
    }
}

/// The fixture from `timeline.test.ts`: [0,2s) and [3s,5s) with a gap
/// between.
fn fixture() -> TimelineSnapshot {
    TimelineSnapshot {
        audio_tracks: Vec::new(),
        session_id: "session-test".into(),
        segments: vec![
            segment(1, 0, 20_000_000),
            segment(2, 30_000_000, 50_000_000),
        ],
        gaps: vec![TimelineGap {
            start_100ns: 20_000_000,
            end_100ns: 30_000_000,
            reason: "最小化".into(),
        }],
        live_edge_100ns: 50_000_000,
        target_title: None,
        target_executable: None,
        target_executable_path: None,
    }
}

#[test]
fn exposes_duration_live_edge_and_recorded_markers() {
    let timeline = fixture();
    assert_eq!(timeline.start_100ns(), 0);
    assert_eq!(timeline.full_duration_100ns(), 50_000_000);
    assert_eq!(timeline.live_edge_100ns, 50_000_000);
    assert_eq!(
        timeline.recorded_markers(&[10_000_000, 25_000_000]),
        vec![10_000_000, 25_000_000]
    );
    // A marker past the live edge has no recorded time and is dropped.
    assert_eq!(timeline.recorded_markers(&[60_000_000]), Vec::<i64>::new());
}

#[test]
fn clamps_quick_ranges_like_the_react_version() {
    let timeline = fixture();
    assert_eq!(
        timeline.quick_range(Some(1.0), 166_667),
        Some((40_000_000, 49_999_999))
    );
    assert_eq!(
        timeline.quick_range(Some(2.5), 166_667),
        Some((30_000_000, 49_999_999))
    );
    assert_eq!(
        timeline.quick_range(Some(300.0), 166_667),
        Some((0, 49_999_999))
    );
    assert_eq!(timeline.quick_range(None, 166_667), Some((0, 49_999_999)));
}

#[test]
fn next_range_clamps_the_current_selection_when_not_following() {
    let timeline = fixture();
    assert_eq!(
        timeline.next_range(Some((5_000_000, 40_000_000)), false, 166_667),
        (5_000_000, 40_000_000)
    );
    assert_eq!(
        timeline.next_range(Some((25_000_000, 40_000_000)), false, 166_667),
        (30_000_000, 40_000_000)
    );
    // Following the live edge ignores the selection.
    assert_eq!(
        timeline.next_range(Some((10_000_000, 20_000_000)), true, 166_667),
        (0, 49_999_999)
    );
    assert_eq!(timeline.next_range(None, true, 166_667), (0, 49_999_999));
    // A selection that cannot be clamped falls back to the live range.
    assert_eq!(
        timeline.next_range(Some((0, 0)), false, 166_667),
        (0, 49_999_999)
    );
    assert_eq!(timeline.next_range(None, false, 166_667), (0, 49_999_999));
}

// ---- findCoveringSegment (thumbnails.test.ts) ----

fn thumb_fixture(segments: Vec<TimelineSegment>) -> TimelineSnapshot {
    TimelineSnapshot {
        audio_tracks: Vec::new(),
        session_id: "session-thumb".into(),
        live_edge_100ns: segments.last().map(|s| s.end_100ns).unwrap_or(0),
        segments,
        gaps: Vec::new(),
        target_title: None,
        target_executable: None,
        target_executable_path: None,
    }
}

#[test]
fn covering_segment_start_inclusive_end_exclusive() {
    let timeline = thumb_fixture(vec![
        segment(0, 0, 10 * HNS_PER_SECOND),
        segment(1, 10 * HNS_PER_SECOND, 20 * HNS_PER_SECOND),
    ]);
    assert_eq!(
        timeline
            .find_covering_segment(10 * HNS_PER_SECOND)
            .unwrap()
            .index,
        1
    );
    assert_eq!(timeline.find_covering_segment(0).unwrap().index, 0);
    // Interior points resolve to their own segment.
    assert_eq!(
        timeline
            .find_covering_segment(5 * HNS_PER_SECOND)
            .unwrap()
            .index,
        0
    );
    assert_eq!(
        timeline
            .find_covering_segment(15 * HNS_PER_SECOND)
            .unwrap()
            .index,
        1
    );
}

#[test]
fn covering_segment_is_none_inside_a_gap_or_outside() {
    let timeline = thumb_fixture(vec![
        segment(0, 0, 10 * HNS_PER_SECOND),
        segment(1, 20 * HNS_PER_SECOND, 30 * HNS_PER_SECOND),
    ]);
    assert!(timeline
        .find_covering_segment(15 * HNS_PER_SECOND)
        .is_none());
    assert!(timeline
        .find_covering_segment(40 * HNS_PER_SECOND)
        .is_none());
    assert!(thumb_fixture(vec![]).find_covering_segment(0).is_none());
}

// ---- viewport / zoom / pan ----

fn long_fixture() -> TimelineSnapshot {
    thumb_fixture(vec![segment(0, 0, 600 * HNS_PER_SECOND)])
}

#[test]
fn the_range_jumps_read_the_selection_and_never_move_it() {
    let timeline = fixture();
    // A selection: its own edges.
    assert_eq!(
        range_jump_targets(&timeline, Some((30_000_000, 40_000_000))),
        (30_000_000, 40_000_000)
    );
    // No selection: the session's head and its last recorded tick -- the live
    // edge itself is exclusive and has nothing to decode.
    assert_eq!(
        range_jump_targets(&timeline, None),
        (0, timeline.live_edge_100ns - 1)
    );
}

#[test]
fn percent_maps_the_viewport_onto_the_track() {
    let viewport = Viewport {
        start_100ns: 100 * HNS_PER_SECOND,
        duration_100ns: 100 * HNS_PER_SECOND,
    };
    assert_eq!(viewport.percent(150 * HNS_PER_SECOND), 50.0);
    assert_eq!(viewport.clamped_percent(0), 0.0);
    assert_eq!(viewport.clamped_percent(1000 * HNS_PER_SECOND), 100.0);
    assert!(viewport.contains(100 * HNS_PER_SECOND));
    assert!(!viewport.contains(201 * HNS_PER_SECOND));
    assert!(viewport.overlaps(0, 100 * HNS_PER_SECOND));
    assert!(!viewport.overlaps(0, 99 * HNS_PER_SECOND));
}

#[test]
fn time_from_ratio_rounds_to_the_100ns_quantum() {
    let viewport = Viewport {
        start_100ns: 0,
        duration_100ns: 3,
    };
    assert_eq!(viewport.time_from_ratio(0.5), 2); // 1.5 rounds up
    assert_eq!(viewport.time_from_ratio(-1.0), 0);
    assert_eq!(viewport.time_from_ratio(2.0), 3);
}

#[test]
fn zoom_keeps_the_anchor_under_the_cursor() {
    let timeline = long_fixture();
    let full = Viewport::full(&timeline);
    let cursor_ratio = 0.25;
    let anchor = full.time_from_ratio(cursor_ratio);
    let zoomed = zoom_at(&timeline, full, cursor_ratio, true).unwrap();
    // The anchor's ratio inside the new viewport is unchanged.
    let new_ratio = (anchor - zoomed.start_100ns) as f64 / zoomed.duration_100ns as f64;
    assert!((new_ratio - cursor_ratio).abs() < 0.001, "{new_ratio}");
    assert_eq!(
        zoomed.duration_100ns,
        (600.0 * HNS_PER_SECOND as f64 / ZOOM_STEP).round() as i64
    );
}

#[test]
fn zoom_out_past_full_drops_the_zoom() {
    let timeline = long_fixture();
    let nearly_full = Viewport {
        start_100ns: 0,
        duration_100ns: 590 * HNS_PER_SECOND,
    };
    assert_eq!(zoom_at(&timeline, nearly_full, 0.5, false), None);
}

#[test]
fn zoom_in_stops_at_the_five_second_floor() {
    let timeline = long_fixture();
    let tight = Viewport {
        start_100ns: 0,
        duration_100ns: MIN_ZOOM_DURATION_100NS,
    };
    let zoomed = zoom_at(&timeline, tight, 0.5, true).unwrap();
    assert_eq!(zoomed.duration_100ns, MIN_ZOOM_DURATION_100NS);
}

#[test]
fn pan_clamps_to_the_recording() {
    let timeline = long_fixture();
    let viewport = Viewport {
        start_100ns: 0,
        duration_100ns: 100 * HNS_PER_SECOND,
    };
    let panned = pan(&timeline, viewport, 1);
    assert_eq!(panned.start_100ns, 20 * HNS_PER_SECOND);
    assert_eq!(pan(&timeline, viewport, -1).start_100ns, 0);
    let at_end = Viewport {
        start_100ns: 500 * HNS_PER_SECOND,
        duration_100ns: 100 * HNS_PER_SECOND,
    };
    assert_eq!(pan(&timeline, at_end, 1).start_100ns, 500 * HNS_PER_SECOND);
}

#[test]
fn an_unnamed_pin_says_so() {
    // DS Timeline: `m.label || '名前なし'` (t260928-ac18, review 06 F21).
    assert_eq!(marker_unnamed(Locale::Ja), "名前なし");
    assert_eq!(marker_unnamed(Locale::En), "Unnamed");
    assert_ne!(
        marker_unnamed(Locale::Ja),
        marker_label_placeholder(Locale::Ja)
    );
}

#[test]
fn track_boxes_are_viewport_filtered_and_keep_a_minimum_width() {
    let mut segments = Vec::new();
    for index in 0..500u64 {
        let start = index as i64 * 2 * HNS_PER_SECOND;
        segments.push(segment(index, start, start + 2 * HNS_PER_SECOND));
    }
    let timeline = thumb_fixture(segments);
    let full = Viewport::full(&timeline);
    let boxes = segment_boxes(&timeline, &full);
    assert_eq!(boxes.len(), 500);
    // 2s of 1000s is 0.2% -- floored to 0.5%.
    assert!(boxes.iter().all(|b| b.width >= 0.5));
    // Zoomed to 100s starting at 500s, only ~51 segments overlap.
    let zoomed = Viewport {
        start_100ns: 500 * HNS_PER_SECOND,
        duration_100ns: 100 * HNS_PER_SECOND,
    };
    let visible = segment_boxes(&timeline, &zoomed);
    assert!(visible.len() <= 52, "{}", visible.len());
}

#[test]
fn playback_rate_steps_clamp_at_the_ends() {
    // Five rates since task238 dropped 4x, so a step from 1x is a step to
    // 1.5x and the top of the list is 2x.
    assert_eq!(shift_playback_rate(1.0, 1), 1.5);
    assert_eq!(shift_playback_rate(1.0, -1), 0.5);
    assert_eq!(shift_playback_rate(2.0, 1), 2.0);
    // 4x is no longer listed, so it re-anchors at 1x like any other stranger.
    assert_eq!(shift_playback_rate(4.0, 1), 1.5);
    assert_eq!(shift_playback_rate(0.25, -1), 0.25);
    // An unlisted rate re-anchors at 1.0x -- which now includes the 1.25 /
    // 1.75 steps the list dropped.
    assert_eq!(shift_playback_rate(3.0, 1), 1.5);
    assert_eq!(shift_playback_rate(1.25, -1), 0.5);
    assert_eq!(rate_index(1.0), 2);
    assert_eq!(rate_index(0.25), 4);
    assert_eq!(rate_index(9.9), 2);
    // Fastest first: the menu reads top-to-bottom, high to low.
    assert_eq!(rate_labels(), ["×2", "×1.5", "×1", "×0.5", "×0.25"]);
}

/// Task390: a live session's selection went stale the moment it was loaded,
/// because the range was only ever rebuilt when the load had found an empty
/// timeline. The chip that is showing decides who follows the live edge.
#[test]
fn a_growing_live_session_moves_the_ranges_that_follow_it() {
    let loaded = fixture();
    // The same session 10s later: the second segment ran on to 15s.
    let mut grown = fixture();
    grown.segments[1] = segment(2, 30_000_000, 150_000_000);
    grown.live_edge_100ns = 150_000_000;
    let min = min_frame_100ns();

    // 全体: the end follows, so the seekbar and `End` reach the live edge.
    let whole_at_load = loaded.next_range(None, true, min);
    let whole = range_after_growth(&grown, 0, None, min).unwrap();
    assert_eq!(whole_at_load, (0, 50_000_000 - 1));
    assert_eq!(whole, (0, 150_000_000 - 1));

    // 直近N: the window advances rather than staying where it was opened. It
    // is computed back from the live edge by exactly the call pressing the
    // chip again would make, so the two can never disagree.
    let recent = range_after_growth(&grown, 3, None, min).unwrap();
    assert_eq!(recent, grown.quick_range(Some(3.0), min).unwrap());
    assert_eq!(recent, (150_000_000 - 30_000_000, 150_000_000 - 1));
    assert_ne!(recent, loaded.quick_range(Some(3.0), min).unwrap());

    // Hand-dragged: left alone, whatever the live edge does.
    assert_eq!(
        range_after_growth(&grown, -1, Some((10_000_000, 40_000_000)), min),
        None
    );
}

/// t260928-b301: an end let go on the live edge keeps following it; the start
/// stays the user's.
#[test]
fn a_pinned_range_end_follows_the_live_edge_and_keeps_its_start() {
    let loaded = fixture();
    let mut grown = fixture();
    grown.segments[1] = segment(2, 30_000_000, 150_000_000);
    grown.live_edge_100ns = 150_000_000;
    let min = min_frame_100ns();
    let pinned = (10_000_000, loaded.live_edge_100ns - 1);
    assert_eq!(
        range_after_growth(&grown, QUICK_END_PINNED, Some(pinned), min),
        Some((10_000_000, 150_000_000 - 1))
    );
    // No range to keep a start from: nothing to follow.
    assert_eq!(
        range_after_growth(&grown, QUICK_END_PINNED, None, min),
        None
    );
}

/// t260928-b301: "on the live edge" is within one frame of the snapshot's end.
#[test]
fn a_range_end_reaches_the_live_edge_within_one_frame() {
    let snapshot = fixture();
    let min = min_frame_100ns();
    let edge = snapshot.live_edge_100ns;
    assert!(end_reaches_live(&snapshot, edge - 1, min));
    assert!(end_reaches_live(&snapshot, edge - min, min));
    assert!(!end_reaches_live(&snapshot, edge - min - 1, min));
    assert!(!end_reaches_live(&snapshot, edge - 10_000_000, min));
}

/// t260928-b301: the live edge's target runs ahead of the last finalized
/// segment by the time since it landed, and is the real edge when not
/// recording or not yet grown. t260930-1904 took b301's one-segment cap off:
/// it is what stopped the bar whenever a segment came late (the drawn edge,
/// `LiveEdgeClock`, has its own hold measured from the real edge).
#[test]
fn the_live_edge_target_runs_ahead_by_elapsed_time_uncapped() {
    use std::time::Duration;
    let edge = 50_000_000;
    let ms = Duration::from_millis;
    assert_eq!(live_edge_target(edge, true, Some(ms(0)), None), edge);
    assert_eq!(
        live_edge_target(edge, true, Some(ms(700)), None),
        edge + 7_000_000
    );
    // Past one segment it keeps going (b301 stopped here at edge + 2 s).
    assert_eq!(
        live_edge_target(edge, true, Some(ms(2_300)), None),
        edge + 23_000_000
    );
    // Not recording, or no growth seen since the load: the real edge.
    assert_eq!(live_edge_target(edge, false, Some(ms(700)), None), edge);
    assert_eq!(live_edge_target(edge, true, None, None), edge);
}

/// t260928-faac: before the first growth the target runs from the load; a
/// growth clock, once there is one, wins; not recording is the real edge.
#[test]
fn the_live_edge_target_runs_from_the_load_until_the_first_growth() {
    use std::time::Duration;
    let edge = 50_000_000;
    let ms = Duration::from_millis;
    assert_eq!(
        live_edge_target(edge, true, None, Some(ms(700))),
        edge + 7_000_000
    );
    assert_eq!(
        live_edge_target(edge, true, Some(ms(100)), Some(ms(1_500))),
        edge + 1_000_000
    );
    assert_eq!(live_edge_target(edge, false, None, Some(ms(700))), edge);
}

/// t260929-e171: a live load's clock starts at the recorder's fold, so a load
/// late in the segment draws the edge that far ahead at once, and the first
/// growth moves it on by no more than the growth poll's lag -- never back.
/// The contrast: the same load clocked from the load itself steps 1.6 s.
#[test]
fn the_live_edge_target_from_a_phased_load_does_not_step_at_the_first_growth() {
    use std::time::Duration;
    let edge = 50_000_000;
    let ms = Duration::from_millis;
    let segment = crate::encoder::SEGMENT_DURATION_100NS;
    // Loaded 1.5 s after the fold: the drawn edge is already 1.5 s ahead.
    assert_eq!(
        live_edge_target(edge, true, None, Some(ms(1_500))),
        edge + 15_000_000
    );
    // The next fold comes 2 s after the last; the growth poll sees it 100 ms on.
    let before_growth = live_edge_target(edge, true, None, Some(ms(2_000)));
    let after_growth = live_edge_target(edge + segment, true, Some(ms(100)), Some(ms(2_100)));
    assert!(after_growth >= before_growth);
    assert!(after_growth - before_growth <= 2_000_000);
    // Clocked from the load (1.5 s after the fold) instead: a 1.6 s step.
    let from_load = live_edge_target(edge, true, None, Some(ms(500)));
    assert_eq!(after_growth - from_load, 16_000_000);
}

/// t260930-1904 harness: drives a `LiveEdgeClock` at 60 fps against a
/// recorder whose segments land at `arrivals` (ms from 0, each 2 s of real
/// edge), from `from_ms` to `to_ms`. Returns (t_ms, drawn, target, real) per
/// frame.
fn run_edge_clock(arrivals: &[u64], to_ms: u64) -> Vec<(u64, i64, i64, i64)> {
    use std::time::{Duration, Instant};
    let base = Instant::now();
    let segment = crate::encoder::SEGMENT_DURATION_100NS;
    let mut clock = LiveEdgeClock::default();
    let mut out = Vec::new();
    let mut t = 0;
    while t <= to_ms {
        let landed = arrivals.iter().filter(|&&at| at <= t).count() as i64;
        let real = landed * segment;
        let since = arrivals
            .iter()
            .rev()
            .find(|&&at| at <= t)
            .map(|&at| Duration::from_millis(t - at));
        let target = live_edge_target(real, true, since, Some(Duration::from_millis(t)));
        let drawn = clock.advance(base + Duration::from_millis(t), real, target, true, false);
        out.push((t, drawn, target, real));
        t += 16;
    }
    out
}

fn assert_grows_within_rate(frames: &[(u64, i64, i64, i64)]) {
    for pair in frames.windows(2) {
        let (t0, e0, ..) = pair[0];
        let (t1, e1, _, real) = pair[1];
        let dt = ((t1 - t0) * 10_000) as i64;
        let de = e1 - e0;
        if e0 >= real + LIVE_EDGE_HOLD_AHEAD_100NS {
            continue;
        }
        assert!(de > 0, "stalled at {t1} ms: {e0} -> {e1}");
        assert!(
            de as f64 <= (1.0 + LIVE_EDGE_RATE_SPAN) * dt as f64 + 1.0,
            "jumped at {t1} ms: {de} in {dt}"
        );
    }
}

/// (a) A segment 1.5 s early moves the real edge forward 1.5 s at once. 1904
/// had the drawn edge chase it at 1.3x, which left it behind the real edge
/// for a while -- a playhead pinned to LIVE then drew outside the band. The
/// user's ruling (2026-09-30, follow-up of 1904) puts a floor there instead:
/// the drawn edge is never below the real edge, so it lifts onto it in one
/// step. Otherwise it never steps, never exceeds 1.3x, never goes back.
#[test]
fn an_early_segment_lifts_the_drawn_live_edge_onto_the_real_edge() {
    // Due at 2000 and 4000; the second lands at 2500 (1.5 s early).
    let frames = run_edge_clock(&[0, 2_000, 2_500, 4_500, 6_500, 8_500], 9_600);
    for &(t, drawn, _, real) in &frames {
        assert!(
            drawn >= real,
            "behind the real edge at {t} ms: {drawn} < {real}"
        );
    }
    let mut lifts = 0;
    for pair in frames.windows(2) {
        let (t0, e0, ..) = pair[0];
        let (t1, e1, _, real) = pair[1];
        let dt = ((t1 - t0) * 10_000) as f64;
        assert!(e1 > e0, "stalled or stepped back at {t1} ms: {e0} -> {e1}");
        if (e1 - e0) as f64 > (1.0 + LIVE_EDGE_RATE_SPAN) * dt + 1.0 {
            assert_eq!(e1, real, "jumped at {t1} ms past the real edge");
            lifts += 1;
        }
    }
    assert_eq!(lifts, 1, "the early segment is the only lift");
    let (_, drawn, target, _) = *frames.last().unwrap();
    assert!(
        (target - drawn).abs() < 166_670,
        "still {} behind",
        target - drawn
    );
}

/// (b) A segment 0.5 s late puts the target back 0.5 s: the drawn edge slows
/// to no less than 0.7x, never stops or steps back, and meets the target.
#[test]
fn the_drawn_live_edge_absorbs_a_late_segment_by_running_slower() {
    let frames = run_edge_clock(&[0, 2_000, 4_500, 6_500, 8_500], 8_400);
    assert_grows_within_rate(&frames);
    let late = frames.iter().find(|f| f.0 >= 4_500).unwrap();
    assert!(late.1 > late.2, "the target did fall behind the drawn edge");
    let (_, drawn, target, _) = *frames.last().unwrap();
    assert!(
        (target - drawn).abs() < 166_670,
        "still {} apart",
        drawn - target
    );
}

/// (c) A recorder that has really stopped delivering: the drawn edge grows
/// until it is two segments past the real edge and holds there.
#[test]
fn the_drawn_live_edge_holds_two_segments_past_a_stalled_recorder() {
    let frames = run_edge_clock(&[0, 2_000], 12_000);
    let real = frames.last().unwrap().3;
    let (_, drawn, target, _) = *frames.last().unwrap();
    assert_eq!(drawn, real + LIVE_EDGE_HOLD_AHEAD_100NS);
    assert!(target > drawn);
    let mut clock = LiveEdgeClock::default();
    let now = std::time::Instant::now();
    clock.advance(now, 0, LIVE_EDGE_HOLD_AHEAD_100NS, true, false);
    assert!(clock.holding(0));
    assert!(!clock.holding(1));
}

/// (d) After the stop the drawn edge reaches the real final edge within the
/// settle, in one direction, forwards or back, and then stays.
#[test]
fn the_drawn_live_edge_settles_on_the_final_edge_after_the_stop() {
    use std::time::Duration;
    let base = std::time::Instant::now();
    for (drawn_at_stop, real) in [(30_000_000, 28_000_000), (26_000_000, 28_000_000)] {
        let mut clock = LiveEdgeClock::default();
        clock.advance(base, real, drawn_at_stop, true, false);
        let mut seen = Vec::new();
        for ms in (16..=400).step_by(16) {
            seen.push(clock.advance(base + Duration::from_millis(ms), real, real, false, false));
        }
        let settled_by = seen.iter().position(|&edge| edge == real).unwrap();
        assert!(settled_by * 16 <= 250 + 16, "settled at frame {settled_by}");
        assert!(seen[settled_by..].iter().all(|&edge| edge == real));
        let forward = real > drawn_at_stop;
        assert!(seen
            .windows(2)
            .all(|w| if forward { w[1] >= w[0] } else { w[1] <= w[0] }));
        assert!(!clock.moving(real));
    }
    // The tail segment landing after the settle is settled onto too, not jumped.
    let mut clock = LiveEdgeClock::default();
    clock.advance(base, 28_000_000, 30_000_000, true, false);
    clock.advance(
        base + Duration::from_millis(16),
        28_000_000,
        28_000_000,
        false,
        false,
    );
    let done = clock.advance(
        base + Duration::from_millis(300),
        28_000_000,
        28_000_000,
        false,
        false,
    );
    assert_eq!(done, 28_000_000);
    let next = clock.advance(
        base + Duration::from_millis(316),
        30_000_000,
        30_000_000,
        false,
        false,
    );
    assert_eq!(next, 28_000_000);
    assert!(clock.moving(30_000_000));
}

/// (e) Not recording and never drawn: the real edge, and nothing kept.
#[test]
fn a_session_that_is_not_recording_draws_the_real_edge() {
    let mut clock = LiveEdgeClock::default();
    let now = std::time::Instant::now();
    assert_eq!(clock.advance(now, 5_000, 9_000, false, false), 5_000);
    assert_eq!(clock.shown(), None);
    assert!(!clock.moving(5_000));
}

/// User decision c: a scrub holds the drawn edge still. On release it eases
/// onto its target over `LIVE_EDGE_SETTLE` -- the user's ruling 2026-09-30
/// (follow-up of 1904) replaced "carries on at the absorbing rate", which
/// after a 3 s+ hold fell into the resync jump (measured 4.3 s in one frame).
/// Segments keep landing during the hold, so the frozen edge is behind the
/// real one at release: the ease is not floored, or it would jump there.
#[test]
fn a_scrub_freezes_the_drawn_edge_and_the_release_eases_back() {
    use std::time::Duration;
    let base = std::time::Instant::now();
    let at = |ms| base + Duration::from_millis(ms);
    let segment = crate::encoder::SEGMENT_DURATION_100NS;
    // Real edge in 2 s steps (a segment at every whole 2 s), target runs on.
    let edges = |ms: u64| {
        let real = (ms / 2_000) as i64 * segment;
        (real, real + (ms % 2_000) as i64 * 10_000)
    };
    let mut clock = LiveEdgeClock::default();
    let (real, target) = edges(0);
    let pressed = clock.advance(at(0), real, target, true, false);
    for ms in (16..4_500).step_by(16) {
        let (real, target) = edges(ms);
        assert_eq!(clock.advance(at(ms), real, target, true, true), pressed);
    }
    let (mut prev, mut frames) = (pressed, 0);
    let mut ms = 4_500;
    loop {
        let (real, target) = edges(ms);
        assert!(
            real > pressed,
            "the scenario must release behind the real edge"
        );
        let drawn = clock.advance(at(ms), real, target, true, false);
        assert!(drawn > prev, "stalled or stepped back at {ms} ms");
        assert!(
            drawn - prev < 10_000_000,
            "jumped {} at {ms} ms",
            drawn - prev
        );
        prev = drawn;
        frames += 1;
        if drawn == target {
            break;
        }
        assert!(ms <= 4_484 + 250 + 16, "not on the target by {ms} ms");
        ms += 16;
    }
    assert!(
        frames >= 10,
        "landed in {frames} frames: a jump, not an ease"
    );
}

/// Nobody drew the edge for a while (hidden, another pane): the next read
/// goes straight to the target instead of crawling at 1.3x.
#[test]
fn a_long_gap_between_reads_resyncs_the_drawn_edge() {
    use std::time::Duration;
    let base = std::time::Instant::now();
    let mut clock = LiveEdgeClock::default();
    clock.advance(base, 0, 10_000_000, true, false);
    let later = base + LIVE_EDGE_RESYNC + Duration::from_millis(30_000);
    assert_eq!(
        clock.advance(later, 300_000_000, 310_000_000, true, false),
        310_000_000
    );
}

/// A live load that found no segment draws from 0; the first segment's
/// stamps are the recorder's clock (measured 4.6e12 on the desk, t260930-1904).
/// That is a different edge, not a drift: jump to it rather than chase it at
/// 1.3x for ever (the first real-machine run did exactly that).
#[test]
fn the_first_segment_after_an_empty_load_is_jumped_to_not_chased() {
    use std::time::Duration;
    let base = std::time::Instant::now();
    let mut clock = LiveEdgeClock::default();
    clock.advance(base, 0, 4_000_000, true, false);
    let real = 4_663_057_945_565;
    let next = clock.advance(
        base + Duration::from_millis(16),
        real,
        real + 1_000,
        true,
        false,
    );
    assert_eq!(next, real + 1_000);
}

/// t260928-faac: an end handle let go on the live edge after being held still
/// while a segment landed is still pinned, and is put back on the edge as it
/// is now. The release-time test b301 used calls it off the edge -- that was
/// the bug. Pulled back off the edge: not pinned.
#[test]
fn an_end_held_on_the_live_edge_while_it_grew_is_pinned_on_release() {
    let loaded = fixture();
    let mut grown = fixture();
    grown.segments[1] = segment(2, 30_000_000, 70_000_000);
    grown.live_edge_100ns = 70_000_000;
    let min = min_frame_100ns();
    let held = (10_000_000, loaded.live_edge_100ns - 1);
    let on_live_at_move = end_reaches_live(&loaded, held.1, min);
    assert!(on_live_at_move);
    assert!(!end_reaches_live(&grown, held.1, min));
    assert_eq!(
        release_pinned_range(&grown, held, on_live_at_move, min),
        Some((10_000_000, 70_000_000 - 1))
    );
    let pulled_back = (10_000_000, loaded.live_edge_100ns - 20_000_000);
    let on_live_at_move = end_reaches_live(&loaded, pulled_back.1, min);
    assert!(!on_live_at_move);
    assert_eq!(
        release_pinned_range(&grown, pulled_back, on_live_at_move, min),
        None
    );
}

/// t260928-970c: a range whose end rides the live edge (全体, pinned) is
/// shown -- band and panel numbers alike -- ending on the apparent edge; a
/// range fixed short of it shows its real end.
#[test]
fn a_range_on_the_live_edge_is_shown_to_the_apparent_edge_and_a_fixed_one_is_not() {
    let snapshot = fixture();
    let edge = snapshot.live_edge_100ns;
    let apparent = edge + 7_000_000;
    let frame = 166_667;
    let whole = range_after_growth(&snapshot, 0, None, frame);
    let pinned = range_after_growth(
        &snapshot,
        QUICK_END_PINNED,
        Some((10_000_000, edge - 1)),
        frame,
    );
    for range in [whole, pinned] {
        let (start, _) = range.expect("a range");
        assert_eq!(
            displayed_range(range, edge, apparent),
            Some((start, apparent - 1))
        );
    }
    let shown = range_values(displayed_range(pinned, edge, apparent), 0, false);
    assert_eq!(shown.end, format_range_time(apparent - 1, false));
    assert_eq!(
        shown.length,
        format_range_time(apparent - 1 - 10_000_000, false)
    );
    // Fixed short of the edge: the real end, however far the bar has run.
    let fixed = Some((10_000_000, 40_000_000));
    assert_eq!(displayed_range(fixed, edge, apparent), fixed);
    // Not recording: the apparent edge is the real one and nothing moves.
    assert_eq!(displayed_range(whole, edge, edge), whole);
    assert_eq!(displayed_range(None, edge, apparent), None);
}

/// Task380: an unreported track width is not a 0px-wide track. The first paint
/// after a session loads has no width yet, and collapsing there turned a
/// 739px gap into a 2px line that only a window resize could undo.
#[test]
fn a_gap_on_a_track_of_unknown_width_is_not_collapsed() {
    // 69.7% of a 1060px track: the reproduction session's hole.
    assert_eq!(gap_mark_on_track(69.7, 0.0), GapMark::Edges);
    assert_eq!(gap_mark_on_track(69.7, 1060.0), GapMark::Edges);
    // A hair-thin one still collapses once the width is known, which is the
    // behaviour task171 asked for and this must not lose.
    assert_eq!(gap_mark_on_track(0.2, 1060.0), GapMark::Single);
    assert_eq!(gap_mark_on_track(0.4, 1060.0), GapMark::Edges);
    // ...and is drawn wide for the one frame before that, deliberately.
    assert_eq!(gap_mark_on_track(0.2, 0.0), GapMark::Edges);
    // Round5 §4-B: below 4px two rules and the space between them do not fit,
    // so the seam is drawn once instead of smearing into a false wide gap.
    assert_eq!(gap_mark(0.4), GapMark::Single);
    assert_eq!(gap_mark(3.9), GapMark::Single);
    assert_eq!(gap_mark(4.0), GapMark::Edges);
    assert_eq!(gap_mark(120.0), GapMark::Edges);
}

/// Round5 §4-D: the right panel's block shows three separate values, and says
/// so with dashes rather than zeroes when there is no selection at all.
#[test]
fn the_range_block_reads_from_the_sessions_own_start() {
    let none = range_values(None, 0, false);
    assert_eq!(none.start, range_unset(Locale::Ja));
    assert_eq!(none.end, range_unset(Locale::Ja));
    assert_eq!(none.length, range_unset(Locale::Ja));

    // Times are relative to the session's first frame, not to the absolute
    // timeline coordinate the engine works in.
    let origin = 100 * HNS_PER_SECOND;
    let values = range_values(
        Some((origin + 30 * HNS_PER_SECOND, origin + 95 * HNS_PER_SECOND)),
        origin,
        false,
    );
    // Tenths since round8 §2: the panel's own clock, not the transport's.
    assert_eq!(values.start, "0:30.0");
    assert_eq!(values.end, "1:35.0");
    assert_eq!(values.length, "1:05.0");

    // An hour-long session pads all three, so the columns stay put.
    let long = range_values(Some((0, 65 * HNS_PER_SECOND)), 0, true);
    assert_eq!(long.start, "0:00:00.0");
    assert_eq!(long.length, "0:01:05.0");
}

#[test]
fn durations_format_like_the_react_version() {
    assert_eq!(format_duration(0), "0:00");
    assert_eq!(format_duration(65 * HNS_PER_SECOND), "1:05");
    assert_eq!(format_duration(-5), "0:00");
    assert_eq!(playback_rate_text(1.0), "×1");
    assert_eq!(playback_rate_text(0.25), "×0.25");
    // An hour-long duration gains an hours field.
    assert_eq!(format_duration(3599 * HNS_PER_SECOND), "59:59");
    assert_eq!(format_duration(3600 * HNS_PER_SECOND), "1:00:00");
    assert_eq!(format_duration(4530 * HNS_PER_SECOND), "1:15:30");
    assert_eq!(format_position(300 * HNS_PER_SECOND, true), "0:05:00");
    assert_eq!(format_position(300 * HNS_PER_SECOND, false), "5:00");
}

#[test]
fn digit_keys_jump_to_tenths_of_the_recording() {
    let timeline = fixture();
    assert_eq!(digit_jump_100ns(&timeline, 0), 0);
    assert_eq!(digit_jump_100ns(&timeline, 5), 25_000_000);
    assert_eq!(digit_jump_100ns(&timeline, 9), 45_000_000);
    assert_eq!(digit_jump_100ns(&timeline, 12), 45_000_000);
}

// t260927-77b8: the DS Timecode. Tenths always, hours only past an hour (or
// forced to keep a column aligned), a U+2212 minus for a negative value.
#[test]
fn timecodes_carry_tenths_and_a_real_minus() {
    assert_eq!(format_timecode(0, false), "0:00.0");
    assert_eq!(
        format_timecode(761 * HNS_PER_SECOND + 3_000_000, false),
        "12:41.3"
    );
    assert_eq!(format_timecode(1800 * HNS_PER_SECOND, false), "30:00.0");
    // t260928-ac18 (review 06 F13): no hours column inside a long session.
    assert_eq!(format_timecode(300 * HNS_PER_SECOND, false), "5:00.0");
    assert_eq!(format_timecode(4800 * HNS_PER_SECOND, false), "1:20:00.0");
    // Hours only past an hour, unless forced.
    assert_eq!(format_timecode(3599 * HNS_PER_SECOND, false), "59:59.0");
    assert_eq!(format_timecode(3600 * HNS_PER_SECOND, false), "1:00:00.0");
    assert_eq!(format_timecode(300 * HNS_PER_SECOND, true), "0:05:00.0");
    // The lag behind the live edge and the remaining time are negative.
    assert_eq!(
        format_timecode(-(138 * HNS_PER_SECOND + 7_000_000), false),
        "\u{2212}2:18.7"
    );
    assert!(!format_timecode(-HNS_PER_SECOND, false).starts_with('-'));
    // The tail the DS draws faint.
    assert_eq!(split_timecode("12:41.3"), ("12:41", ".3"));
    assert_eq!(split_timecode("\u{2212}2:18.7"), ("\u{2212}2:18", ".7"));
    assert_eq!(split_timecode(""), ("", ""));
}

// t260927-77b8: the ruler picks the smallest DS step that keeps the labels to
// seven or fewer, whatever the length.
#[test]
fn ruler_labels_stay_at_seven_or_fewer() {
    let cases = [(30, 5), (5 * 60, 60), (30 * 60, 300), (2 * 3600, 1800)];
    for (seconds, expected_major) in cases {
        let viewport = Viewport {
            start_100ns: 0,
            duration_100ns: seconds * HNS_PER_SECOND,
        };
        let (major, minor) = ruler_steps(viewport.duration_100ns);
        assert_eq!(major, expected_major, "{seconds}s");
        assert_eq!(major % minor, 0, "{seconds}s");
        let ticks = ruler_ticks(&viewport, 0);
        let labels = ticks.iter().filter(|tick| tick.label.is_some()).count();
        assert!((1..=7).contains(&labels), "{seconds}s: {labels} labels");
        assert!(ticks
            .iter()
            .all(|tick| (0.0..=100.0).contains(&tick.percent)));
    }
    // 5 minutes: labelled every minute from 0:00, minor ticks in between.
    let five = ruler_ticks(
        &Viewport {
            start_100ns: 0,
            duration_100ns: 300 * HNS_PER_SECOND,
        },
        0,
    );
    let labels: Vec<_> = five.iter().filter_map(|tick| tick.label.clone()).collect();
    assert_eq!(labels, ["0:00", "1:00", "2:00", "3:00", "4:00"]);
    assert!(five.iter().any(|tick| !tick.major));
}

// A zoomed track reads in the session's own clock, not the viewport's.
#[test]
fn ruler_labels_count_from_the_session_start() {
    let origin = 1_000 * HNS_PER_SECOND;
    let viewport = Viewport {
        start_100ns: origin + 62 * HNS_PER_SECOND,
        duration_100ns: 30 * HNS_PER_SECOND,
    };
    let ticks = ruler_ticks(&viewport, origin);
    let labels: Vec<_> = ticks.iter().filter_map(|tick| tick.label.clone()).collect();
    assert_eq!(labels.first().map(String::as_str), Some("1:05"));
    assert!(ticks.first().is_some_and(|tick| tick.percent >= 0.0));
}

// ---- LiveReview playlist growth (task162) ----

/// A session that grew by one segment while the review screen watched.
fn grown(extra: Vec<TimelineSegment>) -> TimelineSnapshot {
    let mut snapshot = fixture();
    snapshot.segments.extend(extra);
    snapshot.live_edge_100ns = snapshot
        .segments
        .last()
        .map(|segment| segment.end_100ns)
        .unwrap_or(0);
    snapshot
}

#[test]
fn a_grown_session_reports_only_the_segments_past_the_tail() {
    let latest = grown(vec![
        segment(3, 50_000_000, 70_000_000),
        segment(4, 70_000_000, 90_000_000),
    ]);
    let extension = playlist_extension(&fixture(), &latest).expect("the session grew");
    assert_eq!(
        extension.segments,
        vec![
            segment(3, 50_000_000, 70_000_000),
            segment(4, 70_000_000, 90_000_000),
        ]
    );
    assert_eq!(extension.live_edge_100ns, 90_000_000);
}

#[test]
fn an_unchanged_or_rewound_snapshot_reports_nothing() {
    let current = fixture();
    assert_eq!(playlist_extension(&current, &fixture()), None);
    // Shorter than what the engine already has: the front of a live session can
    // still be pruned, and dropping segments is not something Extend can do.
    let mut pruned = fixture();
    pruned.segments.remove(0);
    assert_eq!(playlist_extension(&current, &pruned), None);
}

#[test]
fn another_session_is_never_folded_into_this_one() {
    let mut other = grown(vec![segment(3, 50_000_000, 70_000_000)]);
    other.session_id = "session-other".into();
    assert_eq!(playlist_extension(&fixture(), &other), None);
}

/// Loading a live session before its first segment finalizes is the normal
/// case for the review hotkey: the playlist starts empty and everything the
/// recording produces is new.
#[test]
fn an_empty_playlist_takes_every_segment() {
    let empty = TimelineSnapshot {
        audio_tracks: Vec::new(),
        session_id: "session-test".into(),
        segments: Vec::new(),
        gaps: Vec::new(),
        live_edge_100ns: 0,
        target_title: None,
        target_executable: None,
        target_executable_path: None,
    };
    let extension = playlist_extension(&empty, &fixture()).expect("the session grew");
    assert_eq!(extension.segments.len(), 2);
    assert_eq!(extension.live_edge_100ns, 50_000_000);
}

/// The live edge can move on its own: the last segment's end is re-stated when
/// a recording stops, with no new segment behind it.
#[test]
fn a_live_edge_that_moved_alone_still_reports() {
    let mut stretched = fixture();
    stretched.live_edge_100ns = 55_000_000;
    let extension = playlist_extension(&fixture(), &stretched).expect("the edge moved");
    assert!(extension.segments.is_empty());
    assert_eq!(extension.live_edge_100ns, 55_000_000);
}

// ---------- round8 §2: the panel's clock, its parser, and the edges ----------

#[test]
fn the_panels_clock_carries_tenths() {
    assert_eq!(format_range_time(0, false), "0:00.0");
    assert_eq!(format_range_time(4_613_000_000, false), "7:41.3");
    // Truncates rather than rounds: 7:41.39 is still inside the tenth that
    // started at 7:41.3, and rounding up would show a time the range has not
    // reached.
    assert_eq!(format_range_time(4_613_900_000, false), "7:41.3");
    assert_eq!(format_range_time(-5, false), "0:00.0");
    assert_eq!(format_range_time(4_613_000_000, true), "0:07:41.3");
    assert_eq!(format_range_time(36_610_000_000, false), "1:01:01.0");
}

#[test]
fn the_parser_takes_what_the_clock_prints() {
    assert_eq!(parse_range_time("7:41.3"), Some(4_613_000_000));
    assert_eq!(parse_range_time("07:41.3"), Some(4_613_000_000));
    assert_eq!(parse_range_time(" 5:00 "), Some(3_000_000_000));
    assert_eq!(parse_range_time("0:07:41.3"), Some(4_613_000_000));
    // Past the tenth is truncated, not refused.
    assert_eq!(parse_range_time("7:41.35"), Some(4_613_000_000));
}

#[test]
fn the_parser_refuses_everything_else() {
    for bad in [
        "", "abc", "90", "5:", ":30", "5:60", "1:60:00", "-1:00", "5:0a", "5:00.", "5:00.x",
        "1:2:3:4", "1e2:00",
    ] {
        assert_eq!(parse_range_time(bad), None, "{bad} should not parse");
    }
}

/// task3160. The whole rule in one table: an edge carries the playhead only
/// 通り過ぎたとき, a pan always shows the range's start, and both boundaries
/// count as inside -- which is what lets a continuing drag keep following
/// (each move parks the playhead *on* the edge, and the next one leaves it
/// just outside again).
#[test]
fn a_range_edit_carries_the_playhead_only_when_it_left_it_outside() {
    let range = (30_000_000, 50_000_000);
    let start_edge = Some(RangeEdge::Start);
    let end_edge = Some(RangeEdge::End);
    let cases: &[(&str, Option<RangeEdge>, i64, Option<i64>)] = &[
        // Inside: neither edge moves it. The pan still does.
        ("start edge, inside", start_edge, 40_000_000, None),
        ("end edge, inside", end_edge, 40_000_000, None),
        ("pan, inside", None, 40_000_000, Some(30_000_000)),
        // Before the start.
        (
            "start edge, before",
            start_edge,
            20_000_000,
            Some(30_000_000),
        ),
        ("end edge, before", end_edge, 20_000_000, Some(50_000_000)),
        ("pan, before", None, 20_000_000, Some(30_000_000)),
        // After the end.
        (
            "start edge, after",
            start_edge,
            60_000_000,
            Some(30_000_000),
        ),
        ("end edge, after", end_edge, 60_000_000, Some(50_000_000)),
        ("pan, after", None, 60_000_000, Some(30_000_000)),
        // Standing exactly on a boundary is inside, both of them.
        ("start edge, on start", start_edge, 30_000_000, None),
        ("end edge, on start", end_edge, 30_000_000, None),
        ("start edge, on end", start_edge, 50_000_000, None),
        ("end edge, on end", end_edge, 50_000_000, None),
        ("pan, on start", None, 30_000_000, Some(30_000_000)),
    ];
    for (name, edge, position, expected) in cases {
        assert_eq!(range_follow(range, *position, *edge), *expected, "{name}");
    }
}

/// task3390. The companion table: given where `range_follow` left the
/// playhead, may the drag's pause be paid back? One rule for all three
/// gestures -- 終了に達していないなら再開する -- so the rows below are named
/// after the gesture that produces each landing.
#[test]
fn a_range_edit_resumes_unless_the_follow_parked_the_playhead_on_the_end() {
    let range = (30_000_000, 50_000_000);
    let cases: &[(&str, i64, bool)] = &[
        // 終了端 with the playhead still inside: `range_follow` says `None`,
        // so the position is untouched -- the case task3390 changes.
        ("end edge, playhead stayed inside", 40_000_000, true),
        // 終了端 that pushed the playhead out: parked exactly on the new end.
        // Resuming here would only flicker, so it stays stopped (task3160).
        ("end edge, playhead pushed to the end", 50_000_000, false),
        // 開始端 that overtook the playhead: pulled forward to `start`.
        ("start edge, playhead pulled to the start", 30_000_000, true),
        // 移動: always parked on the new start.
        ("pan, playhead carried to the start", 30_000_000, true),
        // Past the end at all -- reachable while the selection shrinks under
        // a playhead the engine is still draining into.
        ("beyond the end", 60_000_000, false),
        // Before the start: `JumpToStart` will carry it in and keep playing,
        // so there is nothing to hold back.
        ("before the start", 20_000_000, true),
    ];
    for (name, position, expected) in cases {
        assert_eq!(
            may_resume_after_range_edit(range, *position),
            *expected,
            "{name}"
        );
    }
}

/// A degenerate selection has no inside, so -- exactly as `range_guard`
/// refuses to act on one -- nothing would stop playback and the resume is
/// allowed wherever the playhead stands.
#[test]
fn a_degenerate_selection_never_holds_a_resume_back() {
    for position in [0, 30_000_000, 60_000_000] {
        assert!(may_resume_after_range_edit(
            (30_000_000, 30_000_000),
            position
        ));
        assert!(may_resume_after_range_edit(
            (50_000_000, 30_000_000),
            position
        ));
    }
}

#[test]
fn a_shift_sweep_is_the_press_and_the_pointer_in_either_order() {
    let timeline = fixture();
    let min = 1_000_000;
    // Forward and backward sweeps over the same two points agree.
    assert_eq!(
        sweep_range(&timeline, 5_000_000, 40_000_000, min),
        Some((5_000_000, 40_000_000))
    );
    assert_eq!(
        sweep_range(&timeline, 40_000_000, 5_000_000, min),
        Some((5_000_000, 40_000_000))
    );
    // Past either end of the recording is cut to it.
    assert_eq!(
        sweep_range(&timeline, 10_000_000, -90_000_000, min),
        Some((0, 10_000_000))
    );
    assert_eq!(
        sweep_range(&timeline, 10_000_000, 99_000_000, min),
        Some((10_000_000, 50_000_000 - 1))
    );
    // An end inside the gap (2s..3s) snaps forward to recorded time.
    assert_eq!(
        sweep_range(&timeline, 25_000_000, 45_000_000, min),
        Some((30_000_000, 45_000_000))
    );
    // Closer than a frame is no range: the caller keeps its own.
    assert_eq!(sweep_range(&timeline, 10_000_000, 10_000_000, min), None);
    assert_eq!(sweep_range(&timeline, 10_000_000, 10_500_000, min), None);
}

#[test]
fn a_shift_press_moves_the_nearer_edge() {
    let range = Some((10_000_000, 40_000_000));
    // Nearer the start (or inside, left of centre): the end stays put.
    assert_eq!(sweep_anchor(range, 5_000_000), 40_000_000);
    assert_eq!(sweep_anchor(range, 20_000_000), 40_000_000);
    // Nearer the end (inside or past it): the start stays put.
    assert_eq!(sweep_anchor(range, 30_000_000), 10_000_000);
    assert_eq!(sweep_anchor(range, 45_000_000), 10_000_000);
    // No range: the press is the anchor.
    assert_eq!(sweep_anchor(None, 20_000_000), 20_000_000);
}

#[test]
fn the_comment_placeholder_says_a_comment_can_be_added() {
    assert_eq!(
        clip_comment_placeholder(Locale::Ja),
        "コメントを付けられます"
    );
    assert_eq!(clip_comment_placeholder(Locale::En), "Add a comment");
}

#[test]
fn moving_an_edge_clamps_against_the_other_and_the_recording() {
    let timeline = fixture();
    let min = 1_000_000;
    let range = (0, 50_000_000 - 1);
    // A start dragged past the end stops one frame short of it.
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::Start, 90_000_000, min),
        Some((50_000_000 - 1 - min, 50_000_000 - 1))
    );
    // An end dragged before the start does the same from the other side.
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::End, -90_000_000, min),
        Some((0, min))
    );
    // Past the live edge is pinned to the last recorded tick.
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::End, 99_000_000, min),
        Some((0, 50_000_000 - 1))
    );
    // A landing inside the gap snaps forward to recorded time (10ms of gap,
    // 2s..3s, snaps to 3s).
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::Start, 25_000_000, min),
        Some((30_000_000, 50_000_000 - 1))
    );
    // task3050: the playhead buttons and `i`/`o` are this call with the
    // current position as the target. Inside the range, either edge comes to it.
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::Start, 35_000_000, min),
        Some((35_000_000, 50_000_000 - 1))
    );
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::End, 35_000_000, min),
        Some((0, 35_000_000))
    );
    // The start cannot overtake a shorter range's end either.
    assert_eq!(
        move_range_edge(
            &timeline,
            (0, 40_000_000),
            RangeEdge::Start,
            45_000_000,
            min
        ),
        Some((40_000_000 - min, 40_000_000))
    );
}

#[test]
fn an_edge_move_needs_a_recording_long_enough_to_hold_the_range() {
    let empty = TimelineSnapshot {
        audio_tracks: Vec::new(),
        session_id: "empty".into(),
        segments: vec![],
        gaps: vec![],
        live_edge_100ns: 0,
        target_title: None,
        target_executable: None,
        target_executable_path: None,
    };
    assert_eq!(
        move_range_edge(&empty, (0, 0), RangeEdge::Start, 0, 1_000_000),
        None
    );
}

#[test]
fn a_marker_takes_thirty_seconds_either_side_and_stops_at_the_ends() {
    // 100s of recording in one segment, so the ends are the only clamp.
    let long = TimelineSnapshot {
        audio_tracks: Vec::new(),
        session_id: "long".into(),
        segments: vec![segment(1, 0, 100 * HNS_PER_SECOND)],
        gaps: vec![],
        live_edge_100ns: 100 * HNS_PER_SECOND,
        target_title: None,
        target_executable: None,
        target_executable_path: None,
    };
    let min = 1_000_000;
    let half = MARKER_RANGE_HALF_SECONDS * HNS_PER_SECOND;
    // Middle: the full 60s window.
    assert_eq!(
        marker_range(&long, 50 * HNS_PER_SECOND, min),
        Some((20 * HNS_PER_SECOND, 80 * HNS_PER_SECOND))
    );
    // Near the head: the front edge wins, the back half stays 30s.
    assert_eq!(
        marker_range(&long, 5 * HNS_PER_SECOND, min),
        Some((0, 5 * HNS_PER_SECOND + half))
    );
    // Near the tail: the live edge wins, exclusive as everywhere else.
    assert_eq!(
        marker_range(&long, 98 * HNS_PER_SECOND, min),
        Some((98 * HNS_PER_SECOND - half, 100 * HNS_PER_SECOND - 1))
    );
    // A recording shorter than the window is selected whole rather than refused.
    assert_eq!(
        marker_range(&fixture(), 40_000_000, min),
        Some((0, 50_000_000 - 1))
    );
}

#[test]
fn a_recent_chip_shorter_than_the_playhead_is_taken_from_the_head() {
    // Less than N seconds in: the start is the head of the timeline rather
    // than a negative tick, and the end stays on the playhead -- the chip
    // gives what there is, it does not refuse (design round19 §2-3).
    assert_eq!(
        recent_range(10 * HNS_PER_SECOND, 60),
        (0, 10 * HNS_PER_SECOND)
    );
    // The ordinary case: the playhead is further in than the chip is long.
    assert_eq!(
        recent_range(50 * HNS_PER_SECOND, 30),
        (20 * HNS_PER_SECOND, 50 * HNS_PER_SECOND)
    );
}

#[test]
fn a_recent_chip_at_the_head_is_a_zero_length_range() {
    // The playhead at 0 leaves nothing to select. Reported as a zero-length
    // range and refused by `clamp_range`'s `min_frame_100ns`, not by this --
    // the unclamped answer is what keeps every other case arithmetic.
    for seconds in RECENT_CHIP_SECONDS {
        assert_eq!(recent_range(0, seconds), (0, 0));
    }
    assert_eq!(
        fixture().clamp_range(0, 0, min_frame_100ns()),
        None,
        "a zero-length range is below one frame, so the chip is a no-op"
    );
}

/// t260929-40a4: 全体 heads the row, and each drawn label's index is the one
/// `range_preset_at` reads back.
#[test]
fn the_preset_row_is_whole_then_the_recent_chips() {
    assert_eq!(
        range_preset_labels(Locale::Ja),
        ["全体", "15 秒", "30 秒", "60 秒"]
    );
    assert_eq!(
        range_preset_labels(Locale::En),
        ["All", "15s", "30s", "60s"]
    );
    assert_eq!(range_preset_at(0), Some(RangePreset::Whole));
    for (i, seconds) in RECENT_CHIP_SECONDS.iter().enumerate() {
        assert_eq!(
            range_preset_at(i as i32 + 1),
            Some(RangePreset::Recent(*seconds))
        );
    }
    assert_eq!(range_preset_at(-1), None);
    assert_eq!(range_preset_at(RECENT_CHIP_SECONDS.len() as i32 + 1), None);
}

/// t260929-40a4: 全体 is lit exactly while `quick == 0`; a chip while the
/// range is still the one it made, and "none" stays -1 rather than landing on
/// 全体.
#[test]
fn whole_is_lit_only_for_the_whole_range() {
    let s = HNS_PER_SECOND;
    let at = 100 * s;
    let thirty = recent_range(at, 30);
    // The load's whole range, even one that happens to be a chip's.
    assert_eq!(range_preset_index(0, Some(thirty), Some((2, thirty))), 0);
    assert_eq!(range_preset_index(0, None, None), 0);
    // A chip's range, also once the playhead has walked on past it.
    assert_eq!(range_preset_index(-1, Some(thirty), Some((2, thirty))), 2);
    // A clamp that trimmed it at the head of the recording is still the press.
    let head = recent_range(10 * s, 30);
    assert_eq!(range_preset_index(-1, Some(head), Some((2, head))), 2);
    // A handle moved after the press: nothing, not 全体.
    assert_eq!(
        range_preset_index(-1, Some((71 * s, at)), Some((2, thirty))),
        -1
    );
    assert_eq!(range_preset_index(-1, Some(thirty), None), -1);
    assert_eq!(range_preset_index(-1, None, Some((2, thirty))), -1);
    // End pinned to the live edge with a chosen start (t260928-b301): not 全体.
    assert_eq!(
        range_preset_index(QUICK_END_PINNED, Some((71 * s, at)), None),
        -1
    );
}

/// t260927-7d08: with the 確定ステップ gone, full screen comes first, then
/// the zoom -- the order task1060 gave everything after the mode.
#[test]
fn escape_leaves_full_screen_before_it_clears_the_zoom() {
    assert_eq!(escape_action(true, true), EscapeAction::LeaveFullscreen);
    assert_eq!(escape_action(true, false), EscapeAction::LeaveFullscreen);
    assert_eq!(escape_action(false, true), EscapeAction::ClearZoom);
    // Nothing to leave: the key belongs to whoever else wants it.
    assert_eq!(escape_action(false, false), EscapeAction::Nothing);
}

/// task2350: the review screen only ever sees a `TimelineSnapshot`, so the
/// executable has to make the last hop off the manifest too.
#[test]
fn from_manifest_carries_the_target_executable() {
    let mut manifest = SessionManifest {
        version: crate::ring_buffer::MANIFEST_VERSION,
        audio_tracks: Vec::new(),
        session_id: "task2350".into(),
        closed: true,
        retention_minutes: 15,
        segments: Vec::new(),
        gaps: Vec::new(),
        target_title: Some("ペイント".into()),
        target_executable: Some("mspaint.exe".into()),
        target_executable_path: None,
        markers: Vec::new(),
        note: None,
        protected: false,
    };
    assert_eq!(
        TimelineSnapshot::from_manifest(&manifest)
            .target_executable
            .as_deref(),
        Some("mspaint.exe")
    );
    // A monitor recording, and every container written before the record
    // existed, arrive here as `None`.
    manifest.target_executable = None;
    assert_eq!(
        TimelineSnapshot::from_manifest(&manifest).target_executable,
        None
    );
}

// ---------- task3400: range drag vs scrub, per move ----------

/// A session of `count` two-second segments, the shape `from_manifest` builds:
/// one audio offset per segment, so the per-segment heap allocation a
/// `TimelineSnapshot::clone` pays is real rather than elided.
fn synthetic_session(count: usize) -> TimelineSnapshot {
    const SEGMENT_100NS: i64 = 2 * HNS_PER_SECOND;
    let segments: Vec<TimelineSegment> = (0..count)
        .map(|index| TimelineSegment {
            index: index as u64,
            start_100ns: index as i64 * SEGMENT_100NS,
            end_100ns: (index as i64 + 1) * SEGMENT_100NS,
            audio_offsets_100ns: vec![0],
        })
        .collect();
    TimelineSnapshot {
        session_id: "task3400-probe".into(),
        live_edge_100ns: count as i64 * SEGMENT_100NS,
        segments,
        gaps: Vec::new(),
        target_title: Some("probe".into()),
        target_executable: Some("charmap.exe".into()),
        target_executable_path: None,
        audio_tracks: vec!["charmap.exe".into()],
    }
}

/// Runs `body` over targets swept across the whole timeline and returns the
/// mean nanoseconds per call. Sweeping matters: `snap_to_recorded_time` is a
/// linear scan that returns as soon as it reaches the covering segment, so a
/// fixed target measures one arbitrary point on that ramp instead of the
/// average a real drag pays.
fn mean_ns(snapshot: &TimelineSnapshot, iterations: usize, mut body: impl FnMut(i64)) -> u128 {
    let span = snapshot.live_edge_100ns - 1;
    let started = std::time::Instant::now();
    for step in 0..iterations {
        let target = span * step as i64 / iterations.max(1) as i64;
        body(target);
    }
    started.elapsed().as_nanos() / iterations.max(1) as u128
}

/// task3400: the Rust-side cost a range-handle move pays that a track scrub
/// move does not.
///
/// Not an assertion about behaviour -- a measurement, which is why it is
/// `#[ignore]`d (timings are not a pass/fail criterion and would flake on a
/// loaded machine). Run it by hand, in **release**, the way the README runs
/// the other by-hand probes:
///
/// ```text
/// cargo test --release --lib task3400 -- --ignored --nocapture
/// ```
///
/// What it separates:
///
/// - `clone`: `review.snapshot.clone()`, which `on_handle_moved` does
///   on every pointer move and `on_track_moved` never
///   does. A deep copy of the segment vector plus one heap allocation per
///   segment's `audio_offsets_100ns`.
/// - `range move`: that clone plus `move_range_edge` (two
///   `snap_to_recorded_time` scans through `clamp_range`) plus the
///   `classify_seek` inside `seek_clamped` (one more scan).
/// - `scrub move`: the same `classify_seek` plus the `find_covering_segment`
///   the scrub does for its thumbnail request -- a binary search, so it is
///   not what separates the two.
///
/// Neither number includes `render_review` or slint's own redraw; both
/// gestures pay those identically, which is exactly why they are left out.
/// The on-device `task127_scrub` / `task3400_range_drag` lines are what
/// measure the whole handler.
#[test]
#[ignore = "task3400 measurement, not a pass/fail criterion: run with --ignored --nocapture"]
fn task3400_what_a_range_move_costs_that_a_scrub_move_does_not() {
    use crate::ui_state::playback::classify_seek;
    use std::hint::black_box;

    const ITERATIONS: usize = 4_000;
    let min_frame = min_frame_100ns();

    println!("task3400 per-move cost (mean ns over {ITERATIONS} sweeps)");
    println!("segments | clone | range move | scrub move | delta");
    // 150: the five-minute ring task3460 used. 3598: the two-hour full ring
    // task3460 measured on the user's own container.
    for count in [150usize, 3598] {
        let snapshot = synthetic_session(count);
        let range = (0i64, snapshot.live_edge_100ns - 1);

        let clone_ns = mean_ns(&snapshot, ITERATIONS, |_| {
            black_box(snapshot.clone());
        });
        let range_ns = mean_ns(&snapshot, ITERATIONS, |target| {
            let owned = snapshot.clone();
            if let Some(next) = move_range_edge(&owned, range, RangeEdge::End, target, min_frame) {
                black_box(classify_seek(&owned, next.1));
            }
            black_box(owned);
        });
        let scrub_ns = mean_ns(&snapshot, ITERATIONS, |target| {
            black_box(classify_seek(&snapshot, target));
            black_box(snapshot.find_covering_segment(target));
        });
        println!(
            "{count:>8} | {clone_ns:>5} | {range_ns:>10} | {scrub_ns:>10} | {:>5}",
            range_ns as i128 - scrub_ns as i128
        );
    }
}

// ---- t260927-9865: the empty review screen's drift ----

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-5
}

#[test]
fn drift_phases_follow_their_periods() {
    use std::time::Duration;
    let at = |s: u64| drift_phases(Duration::from_secs(s));
    assert_eq!(
        at(0),
        DriftPhases {
            back: 0.0,
            front: 0.0,
            pulse: 0.0
        }
    );
    // 22s: half a front lap, 22/80 of a back lap.
    assert!(close(at(22).front, 0.5), "{:?}", at(22));
    assert!(close(at(22).back, 22.0 / 80.0), "{:?}", at(22));
    // 44s: the front belt is back where it began, the back one is not.
    assert!(close(at(44).front, 0.0), "{:?}", at(44));
    assert!(close(at(44).back, 44.0 / 80.0), "{:?}", at(44));
    // 80s: the back belt has done one lap; the front one 80 - 44 = 36s in.
    assert!(close(at(80).back, 0.0), "{:?}", at(80));
    assert!(close(at(80).front, 36.0 / 44.0), "{:?}", at(80));
}

#[test]
fn drift_pulse_swells_over_2_4s_and_comes_back_on_the_standard_curve() {
    use std::time::Duration;
    let pulse = |ms: u64| drift_phases(Duration::from_millis(ms)).pulse;
    assert!(close(pulse(0), 0.0));
    assert!(close(pulse(2400), 1.0));
    assert!(close(pulse(4800), 0.0));
    // t260928-ac18 (review 08 F7): the DS's `lb-now 2.4s easing-standard
    // alternate` -- quick off each end, slow into the swell. A straight
    // line (the old triangle wave) reads 0.5 half way.
    assert!(pulse(1200) > 0.75, "{}", pulse(1200));
    // `alternate` runs the curve backwards on the way down, so the two
    // halves mirror each other.
    assert!(close(pulse(1200), pulse(3600)));
    assert!(pulse(600) < pulse(1200) && pulse(1200) < pulse(1800));
}

#[test]
fn drift_runs_only_while_shown_on_screen_without_reduced_motion() {
    for shown in [false, true] {
        for on_screen in [false, true] {
            for reduce in [false, true] {
                assert_eq!(
                    drift_runs(shown, on_screen, reduce),
                    shown && on_screen && !reduce,
                    "shown={shown} on_screen={on_screen} reduce={reduce}"
                );
            }
        }
    }
    // The positive control: exactly one of the eight runs.
    let running = [false, true]
        .iter()
        .flat_map(|&a| [false, true].map(move |b| (a, b)))
        .flat_map(|(a, b)| [false, true].map(move |c| drift_runs(a, b, c)))
        .filter(|&r| r)
        .count();
    assert_eq!(running, 1);
}

#[test]
fn drift_clock_resumes_where_it_stopped() {
    use std::time::{Duration, Instant};
    let t0 = Instant::now();
    let at = |s: u64| t0 + Duration::from_secs(s);
    let mut clock = DriftClock::default();
    assert_eq!(clock.tick(at(0), true), Duration::ZERO);
    assert_eq!(clock.tick(at(10), true), Duration::from_secs(10));
    // Hidden for 100s: the clock holds.
    assert_eq!(clock.tick(at(11), false), Duration::from_secs(10));
    assert_eq!(clock.tick(at(110), false), Duration::from_secs(10));
    // Shown again: the first tick does not jump by the hidden stretch...
    assert_eq!(clock.tick(at(111), true), Duration::from_secs(10));
    // ...and time runs on from there.
    assert_eq!(clock.tick(at(115), true), Duration::from_secs(14));
}

#[test]
fn empty_stage_words_are_the_ds_ones() {
    assert_eq!(empty_stage_title(Locale::Ja), "まだ何も録画していません");
    assert_eq!(
        empty_stage_body(Locale::Ja),
        // t260928-ac18 (review 08 F2): a break after the first sentence, so
        // the second never splits mid-word (「録画しなが／ら」).
        "録画する対象を選ぶと、ここに映像が流れはじめます。\n録画しながら、いつでも巻き戻して見返せます。"
    );
    assert_eq!(empty_stage_action(Locale::Ja), "対象を選ぶ");
    assert!(!empty_stage_action(Locale::En).is_empty());
}

/// t260928-2f77: one 10-minute session measured against its retention.
fn span_of(seconds: i64) -> TimelineSnapshot {
    TimelineSnapshot {
        segments: vec![segment(0, 0, seconds * HNS_PER_SECOND)],
        live_edge_100ns: seconds * HNS_PER_SECOND,
        ..Default::default()
    }
}

#[test]
fn ring_fill_is_the_recorded_span_over_the_retention_window() {
    // Half the window.
    assert_eq!(span_of(300).ring_fill(10), 0.5);
    // A tick short of full is not full: the old end is not dissolving yet.
    let almost = TimelineSnapshot {
        segments: vec![segment(0, 0, 600 * HNS_PER_SECOND - 1)],
        live_edge_100ns: 600 * HNS_PER_SECOND - 1,
        ..Default::default()
    };
    assert!(almost.ring_fill(10) < 1.0);
    // Exactly the window is full.
    assert_eq!(span_of(600).ring_fill(10), 1.0);
    // Past it (a segment not pruned yet) clamps to full.
    assert_eq!(span_of(700).ring_fill(10), 1.0);
    // Measured from the current head, so a pruned ring stays full.
    let pruned = TimelineSnapshot {
        segments: vec![segment(7, 400 * HNS_PER_SECOND, 1_010 * HNS_PER_SECOND)],
        live_edge_100ns: 1_010 * HNS_PER_SECOND,
        ..Default::default()
    };
    assert_eq!(pruned.ring_fill(10), 1.0);
    // No retention limit never fills, however long it records.
    assert_eq!(span_of(100_000).ring_fill(0), 0.0);
    // An empty session is empty.
    assert_eq!(TimelineSnapshot::default().ring_fill(10), 0.0);
}
