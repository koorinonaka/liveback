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
fn next_range_follows_the_live_edge_while_following() {
    let timeline = fixture();
    assert_eq!(
        timeline.next_range(Some((10_000_000, 20_000_000)), true, 166_667),
        (0, 49_999_999)
    );
    assert_eq!(timeline.next_range(None, true, 166_667), (0, 49_999_999));
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
}

#[test]
fn next_range_falls_back_to_the_live_edge_when_clamping_fails() {
    let timeline = fixture();
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
fn covering_segment_returns_the_exact_cover() {
    let timeline = thumb_fixture(vec![
        segment(0, 0, 10 * HNS_PER_SECOND),
        segment(1, 10 * HNS_PER_SECOND, 20 * HNS_PER_SECOND),
    ]);
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
fn the_ten_minute_preset_falls_back_to_the_whole_session_when_it_is_shorter() {
    // 10 minutes of buffer: the preset is exactly the session.
    let long = long_fixture();
    let ten = long.quick_range(Some(600.0), min_frame_100ns()).unwrap();
    assert_eq!(ten, (long.start_100ns(), long.live_edge_100ns - 1));

    // 5 seconds of buffer: asking for ten minutes cannot fail, it just gives
    // back everything there is.
    let short = fixture();
    let asked = short.quick_range(Some(600.0), min_frame_100ns()).unwrap();
    assert_eq!(asked, (short.start_100ns(), short.live_edge_100ns - 1));
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
fn popover_anchor_is_clamped_into_the_track() {
    assert_eq!(popover_anchor_percent(2.0), 8.0);
    assert_eq!(popover_anchor_percent(50.0), 50.0);
    assert_eq!(popover_anchor_percent(99.0), 92.0);
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
    assert_eq!(rate_labels(), ["x2", "x1.5", "x1", "x0.5", "x0.25"]);
}

/// Round5 §4-B: the colour is decided once, when the marker is made, and lives
/// on the record from then on.
#[test]
fn a_marker_keeps_the_colour_it_was_created_with() {
    // Creation order cycles through the palette and wraps.
    for created in 0..14usize {
        assert_eq!(
            marker_palette_index(Some((created % 6) as u8), 999),
            created % 6
        );
    }
    // The stored index wins over wherever the marker happens to sit now: a
    // marker made sixth keeps colour 5 even when it sorts to the front.
    assert_eq!(marker_palette_index(Some(5), 0), 5);
    // A manifest written before the field existed has no index, so the row's
    // position stands in -- stable across restarts because the order is.
    assert_eq!(marker_palette_index(None, 0), 0);
    assert_eq!(marker_palette_index(None, 7), 1);
    // A stored index from a future palette must not index out of this one.
    assert_eq!(marker_palette_index(Some(200), 0), 200 % 6);
}

/// Round5 §4-B: below 4px two rules and the space between them do not fit, so
/// the seam is drawn once instead of smearing into a false wide gap.
#[test]
fn a_gap_too_narrow_for_two_rules_collapses_to_one() {
    assert_eq!(gap_mark(0.4), GapMark::Single);
    assert_eq!(gap_mark(3.9), GapMark::Single);
    assert_eq!(gap_mark(4.0), GapMark::Edges);
    assert_eq!(gap_mark(120.0), GapMark::Edges);
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
    let whole = range_after_growth(&grown, 0, min).unwrap();
    assert_eq!(whole_at_load, (0, 50_000_000 - 1));
    assert_eq!(whole, (0, 150_000_000 - 1));

    // 直近N: the window advances rather than staying where it was opened. It
    // is computed back from the live edge by exactly the call pressing the
    // chip again would make, so the two can never disagree.
    let recent = range_after_growth(&grown, 3, min).unwrap();
    assert_eq!(recent, grown.quick_range(Some(3.0), min).unwrap());
    assert_eq!(recent, (150_000_000 - 30_000_000, 150_000_000 - 1));
    assert_ne!(recent, loaded.quick_range(Some(3.0), min).unwrap());

    // Hand-dragged: left alone, whatever the live edge does.
    assert_eq!(range_after_growth(&grown, -1, min), None);
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
    assert_eq!(playback_rate_text(1.0), "x1");
    assert_eq!(playback_rate_text(0.25), "x0.25");
}

#[test]
fn hour_long_durations_gain_an_hours_field() {
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

#[test]
fn remaining_time_counts_down_and_never_goes_negative() {
    assert_eq!(
        format_remaining(100 * HNS_PER_SECOND, 35 * HNS_PER_SECOND, false),
        "−1:05"
    );
    assert_eq!(
        format_remaining(100 * HNS_PER_SECOND, 100 * HNS_PER_SECOND, false),
        "−0:00"
    );
    assert_eq!(
        format_remaining(100 * HNS_PER_SECOND, 200 * HNS_PER_SECOND, true),
        "−0:00:00"
    );
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

/// The asymmetry with `range_follow`, spelled out because it is the one a
/// reader questions: `end` counts as *inside* for following (so a continuing
/// drag keeps pulling the playhead along) and as *reached* for resuming (so
/// the next frame's `PauseAtEnd` has nothing to undo). `start` is inside for
/// both.
#[test]
fn standing_on_the_end_blocks_a_resume_even_though_it_follows_as_inside() {
    let range = (30_000_000, 50_000_000);
    assert_eq!(range_follow(range, 50_000_000, Some(RangeEdge::End)), None);
    assert!(!may_resume_after_range_edit(range, 50_000_000));
    assert_eq!(
        range_follow(range, 30_000_000, Some(RangeEdge::Start)),
        None
    );
    assert!(may_resume_after_range_edit(range, 30_000_000));
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

/// A drag that keeps pulling one edge past the playhead keeps dragging it
/// along, one move at a time -- the property the inclusive boundary above
/// exists for.
#[test]
fn a_continuing_drag_keeps_the_playhead_pinned_to_the_edge() {
    let mut position = 40_000_000;
    for start in [45_000_000, 46_000_000, 47_000_000] {
        let follow = range_follow((start, 50_000_000), position, Some(RangeEdge::Start));
        assert_eq!(follow, Some(start), "start pulled to {start}");
        position = follow.unwrap();
    }
    // Pushed back the other way, the playhead is inside again and stays put.
    assert_eq!(
        range_follow((20_000_000, 50_000_000), position, Some(RangeEdge::Start)),
        None
    );
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

/// task3050: the playhead buttons and `i`/`o` are `move_range_edge` with the
/// current position as the target, so this pins the three cases the task names
/// on that spelling of the call.
#[test]
fn setting_an_edge_from_the_playhead_clamps_the_same_way_a_drag_does() {
    let timeline = fixture();
    let min = 1_000_000;
    let range = (0, 50_000_000 - 1);
    // Normal: the playhead sits inside the range and the start comes to it.
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::Start, 35_000_000, min),
        Some((35_000_000, 50_000_000 - 1))
    );
    // The start cannot overtake the end -- it stops one frame short.
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
    // No range yet: the caller has nothing to move, and the shape of that is
    // `move_range_edge` never being reached -- see
    // `an_edge_move_needs_a_recording_long_enough_to_hold_the_range` for the
    // empty-recording half.
    assert_eq!(
        move_range_edge(&timeline, range, RangeEdge::End, 35_000_000, min),
        Some((0, 35_000_000))
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
fn a_recent_chip_ends_at_the_playhead_and_starts_n_seconds_before_it() {
    // The ordinary case: the playhead is further in than the chip is long.
    assert_eq!(
        recent_range(50 * HNS_PER_SECOND, 30),
        (20 * HNS_PER_SECOND, 50 * HNS_PER_SECOND)
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

#[test]
fn recent_chip_labels_name_their_own_length() {
    assert_eq!(recent_chip_label(Locale::Ja, 15), "直近15秒");
    assert_eq!(recent_chip_label(Locale::En, 60), "Last 60s");
}

#[test]
fn panning_the_band_keeps_its_length_and_stops_at_the_ends() {
    let timeline = fixture();
    let min = 1_000_000;
    // 1s wide, sitting at the head of the second segment.
    let range = (30_000_000, 40_000_000);
    // Straight move, both ends recorded.
    assert_eq!(
        pan_range(&timeline, range, 35_000_000, min),
        Some((35_000_000, 45_000_000))
    );
    // Past the live edge: it runs out of road rather than shrinking.
    let panned = pan_range(&timeline, range, 99_000_000, min).unwrap();
    assert_eq!(panned.1 - panned.0, range.1 - range.0);
    assert_eq!(panned.1, 50_000_000 - 1);
    // Before the first frame, same rule from the other side.
    assert_eq!(
        pan_range(&timeline, range, -5_000_000, min).map(|(s, e)| e - s),
        Some(range.1 - range.0)
    );
    // The default selection is the whole recording: there is nowhere to pan
    // it, so the drag is a no-op rather than a resize.
    let whole = (0, 50_000_000 - 1);
    assert_eq!(
        pan_range(&timeline, whole, 20_000_000, min),
        Some(whole),
        "a full-width band has no room to move"
    );
}

/// Task1060: the mode is the most local thing on screen, so it takes Escape
/// before the window's own shape does. task3860 swapped which mode that is --
/// 「範囲を編集」 became the 確定ステップ -- and left the order alone.
#[test]
fn escape_closes_the_confirm_step_before_it_leaves_full_screen() {
    assert_eq!(
        escape_action(true, true, true),
        EscapeAction::CloseConfirmStep
    );
    // Out of the step, the old order stands: full screen, then the zoom.
    assert_eq!(
        escape_action(false, true, true),
        EscapeAction::LeaveFullscreen
    );
    assert_eq!(escape_action(false, false, true), EscapeAction::ClearZoom);
    // Nothing to leave: the key belongs to whoever else wants it.
    assert_eq!(escape_action(false, false, false), EscapeAction::Nothing);
}

/// task3860 / round19 §1's one caveat: **cancelling the confirm step does not
/// put the range back.** The comment is dropped -- it only ever lived in the
/// field that is going away -- but a boundary dragged inside the step stays
/// dragged, so 確定ステップに入る → ドラッグ → Esc → 指定フォルダに書き出し
/// still writes out what the user just framed.
#[test]
fn ending_the_confirm_step_keeps_the_range_it_was_dragged_to() {
    // The range as the drag inside the step left it, not as the step opened.
    let dragged = Some((12_000_000_i64, 34_000_000_i64));
    let ended = end_confirm_step(dragged);
    assert_eq!(
        ended.range, dragged,
        "the range is screen state, not part of the save: it survives the cancel"
    );
    // Both exits are the same function, so Enter cannot leave a different
    // range behind than Esc does.
    assert_eq!(end_confirm_step(dragged).range, ended.range);
    // Nothing selected stays nothing selected -- no range is invented on the
    // way out either.
    assert_eq!(end_confirm_step(None).range, None);
    // task3160's latch: the band's release is swallowed when `sel-touch` is
    // disabled under a held pointer, so ending the step has to clear it or
    // `owns_position` freezes the playhead for good.
    assert!(!ended.range_dragging);
    assert!(
        !ended.resume_after_range_drag,
        "an abandoned gesture does not resume playback on its own"
    );
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
/// - `clone`: `review.snapshot.clone()`, which `on_handle_moved` and
///   `on_range_panned` do on every pointer move and `on_track_moved` never
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
