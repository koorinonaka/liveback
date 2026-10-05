use super::*;

fn session(id: &str, segments: usize, bytes: u64, readable: bool) -> SessionSummary {
    SessionSummary {
        session_id: id.into(),
        start_100ns: None,
        end_100ns: None,
        segment_count: segments,
        total_bytes: bytes,
        closed: true,
        has_partial: false,
        recoverable: false,
        manifest_version: 2,
        readable,
        target_title: Some(format!("target {id}")),
        thumbnail_index: None,
        note: None,
        protected: false,
        target_executable: None,
        target_executable_path: None,
        marker_count: 0,
    }
}

/// Task1520 follow-up. A session that will not open keeps its row, and that
/// row answers nothing: the incident this comes from was a row *vanishing*
/// between the look and the right-click, which moved a user's recording under
/// the discard entry.
#[test]
fn an_unreadable_row_stays_in_the_list_and_answers_nothing() {
    let readable = session("capture-a", 3, 100, true);
    let unreadable = session("capture-b", 0, 0, false);

    assert!(has_row_menu(&readable));
    assert!(!has_row_menu(&unreadable));
    assert!(!is_row_inert(&readable));
    assert!(is_row_inert(&unreadable));
    // The two answers are the same question, so they can never disagree: a row
    // with no menu that still took a marquee would land in a bulk discard.
    assert_eq!(has_row_menu(&unreadable), !is_row_inert(&unreadable));
    // Loading was already refused for these; this only adds the rest.
    assert!(is_load_disabled(&unreadable, false, false, false));
}

/// The click path and the marquee share `is_selectable`, so the rule lives
/// there rather than at either call site. A row with no context menu that a
/// plain click could still select would arrive in a bulk discard through the
/// back door.
#[test]
fn an_unreadable_row_cannot_be_selected_by_click_or_marquee() {
    let mut state = SessionsState::default();
    state.set_sessions(vec![
        session("capture-b", 0, 0, false),
        session("capture-a", 3, 100, true),
    ]);

    assert!(!state.is_selectable("capture-b"));
    assert!(state.is_selectable("capture-a"));

    // `press` is the click path, and it filters through the same gate.
    state.press(Some("capture-b"), false, false);
    assert!(
        state.selected().is_empty(),
        "an unreadable row takes no click: {:?}",
        state.selected()
    );
    state.press(Some("capture-a"), false, false);
    assert_eq!(state.selected(), ["capture-a".to_owned()]);
}

/// t260920-bb09: the left click and the right click now answer alike on a LIVE
/// row -- both select it -- and `select_for_menu` still turns an unreadable row
/// away, which is what makes it a gate rather than a hole. Before this the
/// right click wrote `selected` with no gate at all: measured on the real
/// machine as 0 of 5 left clicks and 5 of 5 right clicks selecting the LIVE
/// row (`evidence/t260920-bb09-live-row-select-protect/`).
#[test]
fn a_live_row_takes_a_click_and_a_right_click_alike() {
    let mut state = SessionsState::default();
    state.set_sessions(vec![
        session("live", 3, 100, true),
        session("stopped", 3, 100, true),
        session("broken", 0, 0, false),
    ]);
    state.set_active_ids(vec!["live".to_owned()]);

    assert!(state.is_selectable("live"));
    state.press(Some("live"), false, false);
    assert_eq!(state.selected(), ["live".to_owned()]);

    state.press(None, false, false);
    state.select_for_menu("live");
    assert_eq!(state.selected(), ["live".to_owned()]);
    // and it stays through the list updates a recording keeps producing
    state.set_sessions(vec![
        session("live", 4, 200, true),
        session("stopped", 3, 100, true),
        session("broken", 0, 0, false),
    ]);
    assert_eq!(state.selected(), ["live".to_owned()]);

    // Control: the gate is a gate. An unreadable row is still refused by the
    // very same call, so removing the check would show up here.
    state.press(None, false, false);
    state.select_for_menu("broken");
    assert!(
        state.selected().is_empty(),
        "an unreadable row takes no right click either: {:?}",
        state.selected()
    );

    // And the marquee, which shares `is_selectable` with the click: a rectangle
    // over all three rows takes the recording and still leaves the unreadable
    // one out.
    let rects = grid_rects(3, 1, 200.0, 50.0, 6.0, 12.0);
    let rows: Vec<(String, Rect)> = state
        .rendered()
        .iter()
        .map(|session| session.session_id.clone())
        .zip(rects)
        .collect();
    state.marquee(&[], &rows, Rect::from_corners(0.0, 0.0, 300.0, 400.0));
    assert_eq!(ids(&state), ["live", "stopped"]);
}

/// t260920-bb09: a pile that holds a recording keeps its discard entry and
/// shows it disabled, on the single-row menu and the selection menu alike --
/// the reason the door is shut has to be visible now that the row can be
/// selected at all.
#[test]
fn a_menu_over_a_recording_disables_the_discard() {
    let live = session_menu(Locale::Ja, ViewMode::List, false, false, true);
    let discard = live
        .iter()
        .find(|item| item.action == SessionMenuAction::Discard)
        .expect("the entry is listed, not removed");
    assert!(discard.disabled);
    // Control: the same menu without a recording in it offers the discard.
    let stopped = session_menu(Locale::Ja, ViewMode::List, false, false, false);
    assert!(
        !stopped
            .iter()
            .find(|item| item.action == SessionMenuAction::Discard)
            .expect("listed")
            .disabled
    );
    // Nothing else in the menu closes with it: protect, load and the rest are
    // exactly what they were.
    assert_eq!(live.len(), stopped.len());
    assert!(live
        .iter()
        .zip(&stopped)
        .filter(|(l, _)| l.action != SessionMenuAction::Discard)
        .all(|(l, s)| l.disabled == s.disabled));

    assert!(selection_menu(Locale::Ja, false, true)[1].disabled);
    assert!(!selection_menu(Locale::Ja, false, false)[1].disabled);
}

/// t260920-bb09: what the screen says about a protect flag is the value the
/// toggle asked for until a listing carries it. For a recording the listing is
/// read from disk while the flag is still in the index writer's queue, so
/// without this the new value is invisible until the user's next action --
/// measured at 5 of 10 toggles on the real machine.
#[test]
fn a_staged_protect_wins_until_the_listing_agrees() {
    let mut state = SessionsState::default();
    state.set_sessions(vec![session("live", 3, 100, true)]);
    state.set_active_ids(vec!["live".to_owned()]);
    assert!(!state.protected("live"));

    state.stage_protected("live", true);
    assert!(state.protected("live"));

    // The stale listing -- the one the toggle's own `Cmd::ListSessions` gets
    // back -- does not win, and does not clear the overlay either.
    let mut stale = session("live", 3, 100, true);
    stale.protected = false;
    state.set_sessions(vec![stale]);
    assert!(state.protected("live"));

    // The listing that finally carries it drops the overlay, so the listing is
    // the only answer again.
    let mut caught_up = session("live", 3, 100, true);
    caught_up.protected = true;
    state.set_sessions(vec![caught_up]);
    assert!(state.protected("live"));
    state.stage_protected("live", true);
    state.set_sessions(vec![{
        let mut s = session("live", 3, 100, true);
        s.protected = true;
        s
    }]);
    assert!(state.protected("live"));
}

/// The two ways an optimistic value has to let go: the write came back an
/// error, and the recording stopped (the finalized container is authoritative
/// whatever it says, so a queued append the writer never managed cannot leave
/// a padlock nobody can clear).
#[test]
fn a_staged_protect_lets_go_on_an_error_and_on_a_stop() {
    let mut state = SessionsState::default();
    state.set_sessions(vec![session("live", 3, 100, true)]);
    state.set_active_ids(vec!["live".to_owned()]);

    state.stage_protected("live", true);
    state.rollback_protected("live");
    assert!(!state.protected("live"));

    state.stage_protected("live", true);
    assert!(state.protected("live"));
    // Stopped: `set_active_ids` first, then the list that follows it.
    state.set_active_ids(Vec::new());
    state.set_sessions(vec![session("live", 3, 100, true)]);
    assert!(
        !state.protected("live"),
        "the finalized session's own value is the answer once it stops"
    );
}

/// Four readable multi-segment sessions, newest-first.
fn listed() -> SessionsState {
    let mut state = SessionsState::default();
    state.set_sessions(vec![
        session("a", 4, 400, true),
        session("b", 3, 300, true),
        session("c", 2, 200, true),
        session("d", 5, 100, true),
    ]);
    state
}

fn ids(state: &SessionsState) -> Vec<&str> {
    state.selected().iter().map(String::as_str).collect()
}

#[test]
fn a_plain_click_replaces_the_selection() {
    let mut state = listed();
    state.press(Some("a"), false, false);
    assert_eq!(ids(&state), ["a"]);
    state.press(Some("c"), false, false);
    assert_eq!(ids(&state), ["c"]);
}

#[test]
fn ctrl_click_adds_and_removes_without_touching_the_rest() {
    let mut state = listed();
    state.press(Some("a"), false, false);
    state.press(Some("c"), false, true);
    assert_eq!(ids(&state), ["a", "c"]);
    state.press(Some("a"), false, true);
    assert_eq!(ids(&state), ["c"]);
}

#[test]
fn shift_click_selects_the_rendered_range_in_either_direction() {
    let mut state = listed();
    state.press(Some("c"), false, false);
    state.press(Some("a"), true, false);
    assert_eq!(ids(&state), ["a", "b", "c"]);

    let mut state = listed();
    state.press(Some("a"), false, false);
    state.press(Some("c"), true, false);
    assert_eq!(ids(&state), ["a", "b", "c"]);
}

#[test]
fn shift_without_an_anchor_behaves_like_a_plain_click() {
    let mut state = listed();
    state.press(Some("b"), true, false);
    assert_eq!(ids(&state), ["b"]);
}

#[test]
fn a_press_on_empty_space_clears_unless_it_is_additive() {
    let mut state = listed();
    state.press(Some("a"), false, false);
    let base = state.press(None, false, true);
    assert_eq!(ids(&state), ["a"]);
    assert_eq!(base, ["a"]);
    state.press(None, false, false);
    assert!(ids(&state).is_empty());
}

#[test]
fn the_marquee_adds_every_overlapping_row_to_the_base() {
    let mut state = listed();
    let rects = grid_rects(4, 1, 200.0, 50.0, 6.0, 12.0);
    let rows: Vec<(String, Rect)> = state
        .rendered()
        .iter()
        .map(|session| session.session_id.clone())
        .zip(rects)
        .collect();
    // Rows span y = 12..62, 68..118, 124..174, 180..230. A drag from y=65
    // to y=130 clears the first row and grazes the third.
    state.marquee(&[], &rows, Rect::from_corners(20.0, 65.0, 100.0, 130.0));
    assert_eq!(ids(&state), ["b", "c"]);
}

#[test]
fn the_marquee_grazes_rather_than_contains() {
    // Explorer-style: one pixel of overlap is a hit, and a rect fully
    // inside a row still selects it.
    let row = Rect {
        x: 0.0,
        y: 0.0,
        width: 100.0,
        height: 40.0,
    };
    assert!(row.overlaps(&Rect::from_corners(99.0, 39.0, 140.0, 80.0)));
    assert!(row.overlaps(&Rect::from_corners(10.0, 10.0, 20.0, 20.0)));
    assert!(!row.overlaps(&Rect::from_corners(100.0, 0.0, 140.0, 40.0)));
}

#[test]
fn the_marquee_keeps_the_base_selection_it_started_from() {
    let mut state = listed();
    state.press(Some("d"), false, false);
    let base = state.press(None, false, true);
    let rects = grid_rects(4, 1, 200.0, 50.0, 6.0, 12.0);
    let rows: Vec<(String, Rect)> = state
        .rendered()
        .iter()
        .map(|session| session.session_id.clone())
        .zip(rects)
        .collect();
    state.marquee(&base, &rows, Rect::from_corners(20.0, 0.0, 100.0, 40.0));
    assert_eq!(ids(&state), ["d", "a"]);
}

/// Task2030: several recordings run at once, so several rows are LIVE -- and
/// none of them can be selected into a bulk discard. The old rule could only
/// ever protect the newest directory on disk.
#[test]
fn every_running_recording_is_a_live_row() {
    let mut state = listed();
    state.set_active_ids(vec!["a".to_owned(), "c".to_owned()]);
    assert!(state.is_active("a"));
    assert!(state.is_active("c"));
    assert!(!state.is_active("b"));
    assert!(state.capturing());
    // Ctrl+A takes the recordings too since t260920-bb09 (the user's
    // 2026-09-19 ruling). Task2030 had excluded them here, and that exclusion
    // was the only thing keeping a recording out of a bulk discard; the guards
    // that replaced it sit on the discard instead -- the discard entry is
    // disabled, the Delete key refuses, and `discard_session` answers `Err`.
    state.select_all();
    assert_eq!(ids(&state), ["a", "b", "c", "d"]);
    // Stopping one changes nothing about what is selectable; it changes what
    // `is_active` answers, which is what those guards read.
    state.set_active_ids(vec!["c".to_owned()]);
    assert!(!state.is_active("a"));
    state.select_all();
    assert_eq!(ids(&state), ["a", "b", "c", "d"]);
    state.set_active_ids(Vec::new());
    assert!(!state.capturing());
}

/// Ctrl+A on the history (follow-up of t260916-5568): under a search it
/// takes what the search left on screen, and nothing it hid.
#[test]
fn select_all_takes_only_what_the_search_shows() {
    let mut state = listed();
    state.set_search("b");
    state.select_all();
    assert_eq!(ids(&state), ["b"]);
    state.set_search("");
    state.select_all();
    assert_eq!(ids(&state), ["a", "b", "c", "d"]);
}

#[test]
fn a_selection_is_dropped_when_its_session_leaves_the_list() {
    let mut state = listed();
    state.press(Some("a"), false, false);
    state.press(Some("c"), false, true);
    state.set_sessions(vec![session("c", 2, 200, true)]);
    assert_eq!(ids(&state), ["c"]);
}

/// t260920-bb09 (the user's 2026-09-19 ruling) reversed task2030's rule: a
/// session that starts recording keeps the selection it was in, and a LIVE row
/// can be selected outright. This test was
/// `starting_a_capture_drops_the_new_live_session_from_the_selection`.
#[test]
fn starting_a_capture_leaves_the_new_live_session_in_the_selection() {
    let mut state = listed();
    state.select_all();
    assert_eq!(ids(&state).len(), 4);
    state.set_active_ids(vec!["a".to_owned()]);
    assert_eq!(ids(&state), ["a", "b", "c", "d"]);
    // And a list update while it records does not quietly drop it either --
    // `reconcile` used to, which is why a right click's selection of a LIVE row
    // was never durable.
    state.set_sessions(vec![
        session("a", 4, 400, true),
        session("b", 3, 300, true),
        session("c", 2, 200, true),
        session("d", 1, 100, true),
    ]);
    assert_eq!(ids(&state), ["a", "b", "c", "d"]);
}

#[test]
fn loading_is_blocked_while_capturing_or_unreadable_or_already_pending() {
    let readable = session("a", 2, 20, true);
    assert!(!is_load_disabled(&readable, false, false, false));
    assert!(is_load_disabled(&readable, true, false, false));
    assert!(is_load_disabled(&readable, false, false, true));
    assert!(is_load_disabled(
        &session("b", 2, 20, false),
        false,
        false,
        false
    ));
    // LiveReview (task162): the recording session is the one row that stays
    // loadable while capturing -- and being active never rescues an unreadable
    // manifest or overrides a load already in flight.
    assert!(!is_load_disabled(&readable, true, true, false));
    assert!(is_load_disabled(&readable, true, false, false));
    assert!(is_load_disabled(&readable, true, true, true));
    assert!(is_load_disabled(
        &session("b", 2, 20, false),
        true,
        true,
        false
    ));
}

#[test]
fn byte_sizes_read_the_same_as_the_react_formatter() {
    assert_eq!(format_bytes(0), "0 B");
    assert_eq!(format_bytes(512), "512 B");
    assert_eq!(format_bytes(1024), "1.0 KB");
    assert_eq!(format_bytes(1536), "1.5 KB");
    assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
    // GB is the last unit React uses, so a terabyte keeps counting in GB.
    assert_eq!(format_bytes(2 * 1024 * 1024 * 1024 * 1024), "2048.0 GB");
}

#[test]
fn a_session_id_carries_its_own_creation_time() {
    // 0x18caa1706903c128 nanoseconds since the epoch.
    let millis = session_epoch_millis("capture-000000000000000018caa1706903c128");
    assert_eq!(millis, Some(0x18caa1706903c128i64 / 1_000_000));
    assert_eq!(session_epoch_millis("capture-not-hex"), None);
    assert_eq!(session_epoch_millis("something-else"), None);
}

#[test]
fn grid_rects_step_by_cell_plus_gap_and_wrap_at_the_column_count() {
    let rects = grid_rects(3, 2, 100.0, 60.0, 10.0, 12.0);
    assert_eq!(
        rects[0],
        Rect {
            x: 12.0,
            y: 12.0,
            width: 100.0,
            height: 60.0
        }
    );
    assert_eq!(
        rects[1],
        Rect {
            x: 122.0,
            y: 12.0,
            width: 100.0,
            height: 60.0
        }
    );
    assert_eq!(
        rects[2],
        Rect {
            x: 12.0,
            y: 82.0,
            width: 100.0,
            height: 60.0
        }
    );
}

// ---- notes and protection (task167) ----

/// Task700. Two sessions were deleted by the capacity sweep on 2026-08-19 and
/// the user found out by noticing the history was shorter. Only the capacity
/// share speaks: a session aged out by `sessionLifetimeDays` went when the
/// setting said it would, and a toast about that on every start is the kind of
/// noise that gets real warnings ignored.
#[test]
fn only_a_capacity_reclaim_says_anything() {
    assert_eq!(capacity_reclaim_notice(Locale::Ja, 0, 0), None);
    // Age-only sweeps reach this with a zero capacity share.
    assert_eq!(
        capacity_reclaim_notice(Locale::Ja, 0, 500 * 1024 * 1024),
        None
    );

    let (title, detail) = capacity_reclaim_notice(Locale::Ja, 2, 372 * 1024 * 1024)
        .expect("capacity deletions speak");
    // The DS word table (t260928-23aa): what happened, then why and how much.
    assert_eq!(title, "古いセッションを 2 件削除しました");
    assert_eq!(
        detail,
        "保存領域の上限に達したため、372.0 MB を空けました。"
    );
}

/// Success is silent on both paths: the row already shows the new value, and
/// the protect toggle is specified as no-confirmation, no-toast.
#[test]
fn only_a_failed_edit_says_anything() {
    assert_eq!(note_update_notice(Locale::Ja, &Ok(())), None);
    assert_eq!(
        note_update_notice(Locale::Ja, &Err("io".into())),
        Some(note_error(Locale::Ja))
    );
    assert_eq!(protect_update_notice(Locale::Ja, &Ok(())), None);
    assert_eq!(
        protect_update_notice(Locale::Ja, &Err("io".into())),
        Some(protect_error(Locale::Ja))
    );
    // Same shape for the protected budget (task167): it warns only when full.
    assert_eq!(protected_over_limit_notice(Locale::Ja, false), None);
    assert_eq!(
        protected_over_limit_notice(Locale::Ja, true),
        Some(protected_over_limit(Locale::Ja))
    );
}

/// W-18. Manual discard is never blocked by protection; the line exists so the
/// user knows what is in the pile before confirming.
#[test]
fn a_discard_confirmation_counts_the_protected_sessions_in_it() {
    assert_eq!(protected_in_selection_text(Locale::Ja, 0), None);
    assert_eq!(
        protected_in_selection_text(Locale::Ja, 3).as_deref(),
        Some("保護中 3 件を含みます")
    );
}

/// Round5 §6-B: the sort chips are gone and a search box took their place.
#[test]
fn the_search_box_narrows_the_list_by_title() {
    let mut state = SessionsState::default();
    let mut named = session("a", 4, 400, true);
    named.target_title = Some("Shooter Game ラン".into());
    let plain = session("b", 3, 300, true);
    state.set_sessions(vec![named, plain]);

    assert_eq!(state.rendered().len(), 2);
    state.set_search("shooter game");
    let hit: Vec<&str> = state
        .rendered()
        .iter()
        .map(|session| session.session_id.as_str())
        .collect();
    assert_eq!(hit, ["a"]);
    // An unnamed session shows its start time, which comes from the id -- so
    // the id is what a query has to match for it.
    state.set_search("b");
    assert_eq!(state.rendered().len(), 1);
    state.set_search("");
    assert_eq!(state.rendered().len(), 2);
}

/// task t260917-bd64. The search box also has to reach the note text (the
/// "コメントを編集" field), not just the title and id.
#[test]
fn the_search_box_also_matches_the_note() {
    let mut state = SessionsState::default();
    let mut noted = session("a", 4, 400, true);
    noted.target_title = None;
    noted.note = Some("Shooter Game ラン".into());
    let plain = session("b", 3, 300, true);
    state.set_sessions(vec![noted, plain]);

    assert_eq!(state.rendered().len(), 2);
    // Mixed case: the note stores "Shooter Game" with a capital O.
    state.set_search("shooter game");
    let hit: Vec<&str> = state
        .rendered()
        .iter()
        .map(|session| session.session_id.as_str())
        .collect();
    assert_eq!(hit, ["a"]);
    state.set_search("");
    assert_eq!(state.rendered().len(), 2);
}

/// A row the search hid must not stay armed for a discard the user can no
/// longer see the target of.
#[test]
fn a_search_that_hides_a_row_drops_it_from_the_selection() {
    let mut state = SessionsState::default();
    let mut named = session("a", 4, 400, true);
    named.target_title = Some("keep".into());
    state.set_sessions(vec![named, session("b", 3, 300, true)]);
    state.press(Some("b"), false, false);
    assert_eq!(ids(&state), ["b"]);
    state.set_search("keep");
    assert!(ids(&state).is_empty());
}

/// task3800. The history's nothing is two lines now, and the split is the
/// mock's: a title with no full stop, then the sentence that says what will
/// fill the screen. The search's nothing stays one line -- it is answered by
/// editing the query, not by waiting.
///
/// t260927-9a17 moved the wording to the DS HistoryScreen README's (「履歴は
/// まだありません」/「録画を止めると、ここに並びます。」), and the search's
/// nothing names the query.
#[test]
fn the_empty_history_says_it_as_a_title_and_a_second_line() {
    assert_eq!(empty(Locale::Ja), "履歴はまだありません");
    assert_eq!(empty_sub(Locale::Ja), "録画を止めると、ここに並びます。");
    assert_eq!(empty(Locale::En), "No history yet");
    assert_eq!(
        empty_sub(Locale::En),
        "Recordings appear here once you stop them."
    );
    assert_eq!(
        search_empty_text(Locale::Ja, " obs "),
        "「obs」に合うセッションはありません"
    );
    assert!(search_empty_text(Locale::En, "obs").contains("obs"));
    // The two nothings are different sentences, or the screen would say
    // "nothing was recorded" to someone who simply mistyped a query.
    assert_ne!(empty(Locale::Ja), search_empty_text(Locale::Ja, ""));
    assert_ne!(empty(Locale::En), search_empty_text(Locale::En, ""));
}

/// task3800. With no sessions the screen stops drawing its toolbar, so a query
/// from the list that was there has no field left to live in. Left behind, it
/// would filter the first recording that comes back -- an empty-looking search
/// box hiding the only row.
#[test]
fn emptying_the_list_drops_the_query_that_has_no_field_left() {
    let mut state = SessionsState::default();
    let mut named = session("a", 4, 400, true);
    named.target_title = Some("Shooter Game ラン".into());
    state.set_sessions(vec![named]);
    state.set_search("shooter game");
    assert_eq!(state.rendered().len(), 1);

    state.set_sessions(vec![]);
    assert_eq!(state.search(), "");

    // The recording that arrives next is visible, not filtered out by a query
    // nothing on screen is showing.
    state.set_sessions(vec![session("b", 3, 300, true)]);
    assert_eq!(state.rendered().len(), 1);
}

/// Round5 §6-D. The set is not fixed: コメントを編集 only exists where a note is
/// drawn, and the protect entry names the action rather than the state.
#[test]
fn the_context_menu_changes_with_the_view_and_the_protect_state() {
    let list = session_menu(Locale::Ja, ViewMode::List, false, false, false);
    let labels: Vec<&str> = list.iter().map(|item| item.label).collect();
    assert_eq!(
        labels,
        [
            menu_load(Locale::Ja),
            menu_edit_note(Locale::Ja),
            protect(Locale::Ja),
            open_directory(Locale::Ja),
            menu_discard(Locale::Ja)
        ]
    );
    // The load entry advertises the list's key that does the same thing
    // (Enter, t260928-23aa), and the note advertises F2 (it was rename's
    // until 2026-09-19).
    assert_eq!(list[0].hint, hint_open(Locale::Ja));
    assert_eq!(list[1].hint, hint_edit_note(Locale::Ja));
    // ...and discard advertises Delete, in both the single and the multi-row
    // menu. The key handler in `sessions.slint` is the other half of this:
    // an advertised accelerator that does nothing is worse than none.
    assert_eq!(list.last().unwrap().hint, hint_discard(Locale::Ja));
    assert_eq!(
        selection_menu(Locale::Ja, false, false)
            .last()
            .unwrap()
            .hint,
        hint_discard(Locale::Ja)
    );
    // Discard is the only destructive entry. Two rules, DS SessionRow order:
    // open / note, rule, protect / folder, rule, discard (2026-09-28).
    assert!(list.iter().filter(|item| item.danger).count() == 1);
    let ruled = |menu: &[SessionMenuItem]| -> Vec<SessionMenuAction> {
        menu.iter()
            .filter(|item| item.rule_above)
            .map(|item| item.action)
            .collect()
    };
    assert_eq!(
        ruled(&list),
        [SessionMenuAction::ToggleProtect, SessionMenuAction::Discard]
    );

    // A tile draws no note editor, so it offers none.
    let tile = session_menu(Locale::Ja, ViewMode::Thumbnail, false, false, false);
    assert!(!tile
        .iter()
        .any(|item| item.action == SessionMenuAction::EditNote));
    assert_eq!(tile.len(), list.len() - 1);
    assert_eq!(ruled(&tile), ruled(&list));

    // Already protected: the entry becomes the way back out (W-17).
    let protected = session_menu(Locale::Ja, ViewMode::List, true, false, false);
    assert!(protected
        .iter()
        .any(|item| item.label == unprotect(Locale::Ja)));

    // Blocked by another recording: still listed, so the row explains itself.
    let blocked = session_menu(Locale::Ja, ViewMode::List, false, true, false);
    assert!(blocked[0].disabled);
    assert!(!list[0].disabled);
}

/// 2026-09-17 (t260917-f4a1): F2 on a tile raised an editing state the tile
/// never draws, so the key appeared to do nothing. The list keeps it.
#[test]
fn f2_opens_the_note_editor_only_in_the_list_view() {
    assert!(f2_edits_note_in(ViewMode::List));
    assert!(!f2_edits_note_in(ViewMode::Thumbnail));
}

/// 2026-08-17: the bottom-right popup's three verbs moved into the same menu, so
/// which rows a menu acts on is now a rule -- and one both the render side and
/// the click side read, which is why it lives on the state.
#[test]
fn the_menu_takes_the_whole_selection_only_from_inside_it() {
    let mut state = listed();
    state.press(Some("a"), false, false);
    state.press(Some("c"), false, true);
    assert_eq!(state.menu_targets("a"), ["a", "c"]);
    // A single selection is not a pile.
    state.press(Some("b"), false, false);
    assert_eq!(state.menu_targets("b"), ["b"]);

    // Only the entries that mean something for several rows at once.
    // 「選択を解除」 went with task1120: a right-click outside the pile now
    // replaces the selection, so the entry that existed to undo one has
    // nothing left to do.
    let menu = selection_menu(Locale::Ja, false, false);
    let labels: Vec<&str> = menu.iter().map(|item| item.label).collect();
    assert_eq!(labels, [protect(Locale::Ja), menu_discard(Locale::Ja)]);
    assert!(menu.last().unwrap().danger);
    assert_eq!(
        selection_menu(Locale::Ja, true, false)[0].label,
        unprotect(Locale::Ja)
    );
}

/// Task1120 reverses the 2026-08-17 rule. A right-click outside the selection
/// used to leave it alone, which meant the menu acted on a row the user could
/// see was not picked; it now picks the row first.
#[test]
fn a_right_click_outside_the_selection_takes_the_row() {
    let mut state = listed();
    state.press(Some("a"), false, false);
    state.press(Some("c"), false, true);

    // Inside the pile: the pile stands, and the menu is the multi-row one.
    state.select_for_menu("a");
    assert_eq!(state.menu_targets("a"), ["a", "c"]);

    // Outside it: the row replaces the selection, and the menu is that row's.
    state.select_for_menu("d");
    assert!(state.is_selected("d"));
    assert!(!state.is_selected("a"));
    assert_eq!(state.menu_targets("d"), ["d"]);

    // Idempotent: the row is now selected, so a second right-click on it
    // changes nothing.
    state.select_for_menu("d");
    assert_eq!(state.menu_targets("d"), ["d"]);
}

/// t260927-9a17: the history's own tallies moved into the page (the top row's
/// count, the selection bar), in the DS HistoryScreen's words.
#[test]
fn the_history_tallies_read_as_the_ds_spells_them() {
    assert_eq!(
        selection_text(Locale::Ja, 2, 8_912_052_634),
        ("2 件".to_owned(), "を選択 · 8.3 GB".to_owned())
    );
    assert_eq!(
        selection_text(Locale::En, 2, 8_912_052_634),
        ("2 sessions".to_owned(), " selected · 8.3 GB".to_owned())
    );
    assert_eq!(discard_selection_label(Locale::Ja, 2), "2 件を破棄");
    assert_eq!(discard_selection_label(Locale::En, 2), "Discard 2 sessions");
    assert_eq!(discard_selection_label(Locale::En, 1), "Discard 1 session");
    // Protected and LIVE in the pile.
    assert_eq!(
        protected_in_selection_text(Locale::Ja, 1).as_deref(),
        Some("保護中 1 件を含みます")
    );
    assert_eq!(
        live_cannot_discard(Locale::Ja),
        "録画中のセッションは破棄できません"
    );
    assert_eq!(
        live_cannot_discard(Locale::En),
        "Sessions being recorded can't be discarded"
    );
    // No limit set: one line, no track.
    assert_eq!(
        storage_unlimited_text(Locale::Ja, 18_468_359_372, 8_912_052_634),
        "17.2 GB · 保護 8.3 GB · 上限なし"
    );
    assert_eq!(
        storage_unlimited_text(Locale::En, 18_468_359_372, 8_912_052_634),
        "17.2 GB · 8.3 GB protected · no limit"
    );
    // The toast after a discard, which still promises the bin and not a
    // freed disk (task1520 follow-up). Title and detail apart, as the DS
    // HistoryScreen toast draws them (t260927-73fd).
    assert_eq!(
        discarded_text(Locale::Ja, 2, "8.3 GB"),
        (
            "2 件のセッションを破棄しました".to_owned(),
            "ごみ箱に移しました（8.3 GB）。".to_owned()
        )
    );
    assert_eq!(
        discarded_text(Locale::En, 2, "8.3 GB"),
        (
            "Discarded 2 sessions".to_owned(),
            "Moved to the recycle bin (8.3 GB).".to_owned()
        )
    );
}

/// t260927-9a17: the UI word is 「コメント」 (the clip screen's), the
/// identifiers keep `note`.
#[test]
fn the_note_is_called_a_comment_on_screen() {
    assert_eq!(note_placeholder(Locale::Ja), "コメントを追加");
    // A toast title since t260928-e309 F4 / F15: no full stop.
    assert_eq!(note_error(Locale::Ja), "コメントを保存できませんでした");
    assert_eq!(menu_edit_note(Locale::Ja), "コメントを編集");
    assert_eq!(note_placeholder(Locale::En), "Add a comment");
    assert_eq!(note_error(Locale::En), "The comment could not be saved");
    assert_eq!(menu_edit_note(Locale::En), "Edit comment");
    assert_eq!(
        search_placeholder(Locale::Ja),
        "ウィンドウ名やコメントで検索"
    );
}

/// t260928-e029: the list's date column is 「作成日時」 / "Created", and
/// what it shows keeps the day for today and yesterday too -- the one
/// `M/D HH:MM` shape, whatever the clock says. (The tile's caption reads the
/// same text.)
#[test]
fn the_created_column_is_named_so_and_always_carries_the_day() {
    assert_eq!(column_start(Locale::Ja), "作成日時");
    assert_eq!(column_start(Locale::En), "Created");
    // No `now` goes in: today, yesterday and last week come out the same way.
    assert_eq!(
        tile_date_text("2026/09/28 09:52:12", "2026/09/28 10:00:00"),
        "9/28 09:52"
    );
    assert_eq!(
        tile_date_text("2026/09/27 23:41:00", "2026/09/28 10:00:00"),
        "9/27 23:41"
    );
    assert_eq!(
        tile_date_text("2026/09/13 22:05:00", "2026/09/28 10:00:00"),
        "9/13 22:05"
    );
    // Round5 §6-A: leading zeros drop, and a date with no time is shown whole.
    assert_eq!(
        tile_date_text("2026/11/03 09:07:00", "2026/09/28 10:00:00"),
        "11/3 09:07"
    );
    assert_eq!(
        tile_date_text("2026/08/13", "2026/09/28 10:00:00"),
        "2026/08/13"
    );
    assert_eq!(
        tile_date_text("capture-000abc", "2026/09/28 10:00:00"),
        "capture-000abc"
    );
}

fn heading_list(placed: &[crate::ui_state::clips::ClipPlacement]) -> Vec<&str> {
    placed
        .iter()
        .filter(|place| !place.heading.is_empty())
        .map(|place| place.heading.as_str())
        .collect()
}

/// t260927-9a17: 今日 / 昨日, then the clip screen's weeks. 2026-09-27 is a
/// Sunday, so its week began Monday 09-21.
#[test]
fn the_date_headings_split_at_today_yesterday_and_the_weeks() {
    let now = "2026/09/27 21:30:00";
    let stamps: Vec<String> = [
        "2026/09/27 20:00:00", // today
        "2026/09/27 00:00:01", // today, just after midnight
        "2026/09/26 23:59:59", // yesterday
        "2026/09/25 12:00:00", // this week
        "2026/09/21 00:00:00", // this week, its Monday
        "2026/09/20 23:59:59", // last week, its Sunday
        "2026/09/14 00:00:00", // last week, its Monday
        "2026/09/13 12:00:00", // 2 weeks ago
        "2026/08/30 12:00:00", // 4 weeks ago
    ]
    .iter()
    .map(|stamp| (*stamp).to_owned())
    .collect();
    let placed = place(Locale::Ja, &stamps, now, 1);
    assert_eq!(
        heading_list(&placed),
        ["今日", "昨日", "今週", "先週", "2 週間前", "4 週間前"]
    );
    // One row per line in the list; each heading adds one to the offset.
    let lines: Vec<i32> = placed.iter().map(|place| place.line).collect();
    assert_eq!(lines, [0, 1, 2, 3, 4, 5, 6, 7, 8]);
    let offsets: Vec<i32> = placed.iter().map(|place| place.heading_offset).collect();
    assert_eq!(offsets, [1, 1, 2, 3, 3, 4, 4, 5, 6]);

    let english = place(Locale::En, &stamps, now, 1);
    assert_eq!(
        heading_list(&english),
        [
            "Today",
            "Yesterday",
            "This week",
            "Last week",
            "2 weeks ago",
            "4 weeks ago"
        ]
    );

    // On a Monday, yesterday is last week's Sunday and still says 昨日.
    let monday = place(
        Locale::Ja,
        &[
            "2026/09/20 10:00:00".to_owned(),
            "2026/09/19 10:00:00".to_owned(),
        ],
        "2026/09/21 09:00:00",
        1,
    );
    assert_eq!(heading_list(&monday), ["昨日", "先週"]);
}

/// Each group starts a fresh line in the thumbnail grid, so a short group
/// leaves the rest of its line empty.
#[test]
fn a_date_group_starts_a_fresh_line_in_the_grid() {
    let now = "2026/09/27 21:30:00";
    let stamps: Vec<String> = [
        "2026/09/27 20:00:00",
        "2026/09/27 19:00:00",
        "2026/09/27 18:00:00",
        "2026/09/27 17:00:00",
        "2026/09/26 12:00:00",
    ]
    .iter()
    .map(|stamp| (*stamp).to_owned())
    .collect();
    let placed = place(Locale::Ja, &stamps, now, 3);
    let cells: Vec<(i32, i32, i32)> = placed
        .iter()
        .map(|place| (place.column, place.line, place.heading_offset))
        .collect();
    assert_eq!(
        cells,
        [(0, 0, 1), (1, 0, 1), (2, 0, 1), (0, 1, 1), (0, 2, 2)]
    );
    let metrics = GridMetrics {
        pad_x: 24.0,
        pad_top: 0.0,
        gap_x: 12.0,
        gap_y: 8.0,
        heading_pitch: 36.0,
    };
    let rects = placed_rects(&placed, metrics, 100.0, 50.0);
    assert_eq!((rects[1].x, rects[1].y), (136.0, 36.0));
    assert_eq!((rects[4].x, rects[4].y), (24.0, 2.0 * 58.0 + 72.0));
}

/// t260927-9a17: the app column's name and the monogram's hue (the DS
/// `AppIcon`'s hash over its eight hues).
#[test]
fn the_app_column_names_the_executable_and_hashes_the_monogram() {
    assert_eq!(app_name(Some("notepad.exe")).as_deref(), Some("notepad"));
    assert_eq!(
        app_name(Some("C:\\Apps\\obs64.exe")).as_deref(),
        Some("obs64")
    );
    assert_eq!(app_name(Some("tool")).as_deref(), Some("tool"));
    assert_eq!(app_name(Some("  ")), None);
    assert_eq!(app_name(None), None);
    // "A" is 65: 65 % 8 == 1. "ab" is 97 * 31 + 98 = 3105: 3105 % 8 == 1.
    assert_eq!(monogram_hue("A"), 1);
    assert_eq!(monogram_hue("ab"), 1);
    assert_eq!(monogram_hue("OBS"), ((79 * 31 + 66) * 31 + 83) % 8);
    assert_eq!(monogram_letter("notepad"), "N");
    assert_eq!(monogram_letter(""), "?");
}

/// Task243: today is a clock, yesterday says so, and everything older is a
/// date. t260928-6080 F1 (DS ClipCard): today is the bare clock, and a year
/// that is not this one is printed.
#[test]
fn the_row_timestamp_says_how_long_ago_rather_than_when() {
    let now = "2026/08/16 21:30:00";
    assert_eq!(
        row_date_text(Locale::Ja, "2026/08/16 21:04:12", now),
        "21:04"
    );
    assert_eq!(
        row_date_text(Locale::Ja, "2026/08/15 23:41:00", now),
        "昨日 23:41"
    );
    assert_eq!(
        row_date_text(Locale::Ja, "2026/08/14 22:05:00", now),
        "8/14 22:05"
    );
    assert_eq!(
        row_date_text(Locale::Ja, "2026/08/13 22:05:00", now),
        "8/13 22:05"
    );

    // Yesterday across a month boundary, and across a year: the day count is
    // a real calendar, not a subtraction on the day field.
    assert_eq!(
        row_date_text(Locale::Ja, "2026/07/31 23:41:00", "2026/08/01 00:10:00"),
        "昨日 23:41"
    );
    assert_eq!(
        row_date_text(Locale::Ja, "2025/12/31 23:41:00", "2026/01/01 00:10:00"),
        "昨日 23:41"
    );
    // Leap day, which is the case a naive month table gets wrong.
    assert_eq!(
        row_date_text(Locale::Ja, "2028/02/29 08:00:00", "2028/03/01 09:00:00"),
        "昨日 08:00"
    );
    // Last year's date carries its year (DS: 今年でないときは年を付ける).
    assert_eq!(
        row_date_text(Locale::Ja, "2025/08/16 22:05:00", now),
        "2025/8/16 22:05"
    );
    // Single-digit month and day lose their leading zeros.
    assert_eq!(
        row_date_text(Locale::Ja, "2026/01/03 09:07:00", now),
        "1/3 09:07"
    );

    // Unparseable input is passed through, as the caller falls back to a raw
    // session id when the clock conversion fails.
    assert_eq!(
        row_date_text(Locale::Ja, "capture-000abc", now),
        "capture-000abc"
    );
    assert_eq!(
        row_date_text(Locale::Ja, "2026/08/16 21:04:12", ""),
        "2026/08/16 21:04:12"
    );
}

/// Task188: the thumbnail queue follows the viewport, so the slice it asks for
/// has to stay inside the list at both ends.
#[test]
fn the_thumbnail_window_hugs_the_viewport_and_stops_at_the_lists_edges() {
    // Mid-list: a margin either side.
    assert_eq!(visible_thumbnail_range(100, 20, 400), 92..128);
    // At the top there is nothing before row 0 to reach back to.
    assert_eq!(visible_thumbnail_range(0, 20, 400), 0..28);
    assert_eq!(visible_thumbnail_range(3, 20, 400), 0..31);
    // At the bottom the margin runs out of list rather than off the end.
    assert_eq!(visible_thumbnail_range(390, 20, 400), 382..400);
    // Past the end entirely (a stale range after the list shrank).
    assert_eq!(visible_thumbnail_range(500, 20, 400), 400..400);
    // No range reported yet, or nothing to show: ask for nothing.
    assert_eq!(visible_thumbnail_range(0, 0, 400), 0..0);
    assert_eq!(visible_thumbnail_range(0, 20, 0), 0..0);
    // Whatever the numbers, the slice is usable as one.
    let range = visible_thumbnail_range(500, 20, 400);
    assert!(range.start <= range.end);
}

/// t260928-23aa: the history's wording against the DS -- the menu's first
/// entry and its key, the partial discard, the capacity sweep's notice, the
/// listing that would not read, English plurals, and last year's dates.
#[test]
fn the_history_wording_follows_the_ds() {
    assert_eq!(menu_load(Locale::Ja), "確認で開く");
    assert_eq!(hint_open(Locale::Ja), "Enter");

    // t260928-e309: the same words, split into the error toast's title (what
    // failed, no full stop) and its second line (why, and where the rest went).
    let (title, detail) = bulk_discard_partial_error(Locale::Ja, 5, 2, "5.4 GB");
    assert_eq!(title, "5 件中 2 件を破棄できませんでした");
    assert_eq!(
        detail,
        "他のアプリが使っているかもしれません。3 件をごみ箱に移しました（5.4 GB）。"
    );
    assert!(!detail.contains("解放"), "{detail}");
    let (title_en, detail_en) = bulk_discard_partial_error(Locale::En, 5, 2, "5.4 GB");
    assert!(title_en.starts_with("2 of 5 sessions"), "{title_en}");
    assert!(!detail_en.contains("freed"), "{detail_en}");

    assert_eq!(
        capacity_reclaimed_text(Locale::En, 1),
        "Deleted 1 old session"
    );
    assert_eq!(error(Locale::Ja), "一覧を取得できませんでした");
    assert!(!error_sub(Locale::Ja).is_empty());

    assert_eq!(
        discarded_text(Locale::En, 1, "1.0 GB").0,
        "Discarded 1 session"
    );
    assert_eq!(
        selection_text(Locale::En, 1, 1024),
        ("1 session".to_owned(), " selected · 1.0 KB".to_owned())
    );

    // Last year's recording carries its year; this year's does not.
    assert_eq!(
        tile_date_text("2025/08/13 22:05:09", "2026/09/28 10:00:00"),
        "2025/8/13 22:05"
    );
    assert_eq!(
        tile_date_text("2026/08/13 22:05:09", "2026/09/28 10:00:00"),
        "8/13 22:05"
    );
    // An unreadable clock costs only the year.
    assert_eq!(tile_date_text("2025/08/13 22:05:09", ""), "8/13 22:05");
}
