use super::*;
use crate::test_support::strings;

/// task t260911-66da: the 1s poll re-sent an identical list forever and
/// the receiver paid for every one of them. Every transition the picker
/// draws differently still has to get through -- including the one between
/// an empty list and a failed one, which is why `Err` is a state rather
/// than "no targets".
#[test]
fn a_list_that_did_not_move_is_not_sent_again() {
    let mut sent = SentList::default();
    let one = vec![target("1", "Shooter Game", Some("game.exe"))];
    let two = vec![
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Notepad", Some("notepad.exe")),
    ];
    assert!(sent.changed(&Ok(one.clone())), "the first list");
    assert!(!sent.changed(&Ok(one.clone())), "the same list again");
    assert!(sent.changed(&Ok(two)), "a window opened");
    let nothing: Vec<CaptureTarget> = Vec::new();
    assert!(sent.changed(&Ok(nothing.clone())), "everything closed");
    assert!(!sent.changed(&Ok(nothing)), "still nothing");
    assert!(
        sent.changed(&Err("could not list the windows".to_owned())),
        "an empty list and a failed list draw differently"
    );
    assert!(
        !sent.changed(&Err("a different wording".to_owned())),
        "one failure is one state whatever it is worded"
    );
    assert!(sent.changed(&Ok(one)), "the list came back");
    // A retitled window is what the receiver used to notice for itself.
    assert!(
        sent.changed(&Ok(vec![target("1", "Shooter Game — 2", Some("game.exe"))])),
        "the title is part of what a tile draws"
    );
}

/// The registration lists have to follow a removal made on the settings
/// screen (task1980 / task3670) -- that is the feature the throttle could
/// break, so the change has to be seen on the sending side instead.
#[test]
fn a_registration_is_sent_only_when_one_of_the_three_moved() {
    use crate::settings::{AutoCaptureApp, AutoCaptureFolder};

    let mut sent = SentRegistrations::default();
    let none: Option<String> = None;
    let no_apps: Vec<AutoCaptureApp> = Vec::new();
    let no_folders: Vec<AutoCaptureFolder> = Vec::new();
    assert!(
        sent.changed(&none, &no_apps, &no_folders),
        "the first snapshot, even an empty one"
    );
    assert!(
        !sent.changed(&none, &no_apps, &no_folders),
        "nothing registered, nothing to say"
    );

    let apps = vec![AutoCaptureApp {
        name: "game.exe".into(),
        ..AutoCaptureApp::default()
    }];
    assert!(sent.changed(&none, &apps, &no_folders), "registered");
    assert!(!sent.changed(&none, &apps, &no_folders), "same again");
    assert!(
        sent.changed(&none, &no_apps, &no_folders),
        "unregistered on the settings screen"
    );

    let folders = vec![AutoCaptureFolder {
        path: "C:\\Games".into(),
    }];
    assert!(sent.changed(&none, &no_apps, &folders), "a folder rule");
    assert!(!sent.changed(&none, &no_apps, &folders), "same again");

    assert!(
        sent.changed(&Some("game.exe".to_owned()), &no_apps, &folders),
        "the last-used executable moved on its own"
    );
    assert!(
        !sent.changed(&Some("game.exe".to_owned()), &no_apps, &folders),
        "same again"
    );
}

/// `last_capture_executable` used to reach the preselect on every list
/// poll. It has its own message now, so the preselect has to be reachable
/// without one (task t260911-66da).
#[test]
fn the_last_used_executable_can_preselect_without_a_new_list() {
    let mut state = listed(vec![
        target("1", "Notepad", Some("notepad.exe")),
        target("2", "Shooter Game", Some("game.exe")),
    ]);
    state.selected_id = None;
    assert!(state.reselect(Some("game.exe")), "the preselect moved");
    assert_eq!(state.selected_id(), Some("2"));
    assert!(!state.reselect(Some("game.exe")), "and stays where it is");
    // A selection that is still in the list outranks the preselect, the
    // same way `set_targets` has always had it.
    assert!(
        !state.reselect(Some("notepad.exe")),
        "the selection is kept"
    );
    assert_eq!(state.selected_id(), Some("2"));
    // a refresh preselects the last used executable.
    {
        let mut state = TargetsState::default();
        state.set_targets(
            vec![
                target("1", "Notepad", Some("notepad.exe")),
                target("2", "Shooter Game", Some("game.exe")),
            ],
            Some("GAME.EXE"),
        );
        assert_eq!(state.selected_id(), Some("2"));
    }
}

/// W-10 / W-11 (task165). The number is the enumeration position the user
/// counts from, so it is 1-based and the resolution is the monitor
/// rectangle -- taskbar included, because the recording includes it.
#[test]
fn a_monitor_reads_as_its_number_and_its_resolution() {
    assert_eq!(
        monitor_display_name(Locale::Ja, 1, 2560, 1440),
        "画面 1 — 2560×1440"
    );
    assert_eq!(
        monitor_display_name(Locale::Ja, 2, 1920, 1080),
        "画面 2 — 1920×1080"
    );
    // The primary badge is its own label, not part of the name: the picker
    // draws it beside the title rather than inside it.
    assert!(!monitor_display_name(Locale::Ja, 1, 800, 600).contains(monitor_primary(Locale::Ja)));
}

/// The tile draws the two halves in opposite corners (task170), so the
/// split has to survive the exact separator `monitor_display_name` writes.
#[test]
fn a_monitor_badge_takes_the_name_and_the_resolution_apart() {
    let name = monitor_display_name(Locale::Ja, 2, 1920, 1080);
    assert_eq!(monitor_name_and_resolution(&name), ("画面 2", "1920×1080"));
    // A window title is not a screen label and keeps all of itself.
    assert_eq!(
        monitor_name_and_resolution("Visual Studio Code"),
        ("Visual Studio Code", "")
    );
}

fn monitor(id: &str, number: usize) -> CaptureTarget {
    CaptureTarget {
        title: monitor_display_name(Locale::Ja, number, 2560, 1440),
        ..crate::capture::tests::test_monitor(id, number == 1)
    }
}

/// The four rules the open modal's grid lives by (design §5).
#[test]
fn the_order_the_user_is_looking_at_survives_the_one_second_poll() {
    let listed = || {
        vec![
            target("1", "Shooter Game", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
            target("3", "Code", Some("code.exe")),
        ]
    };

    // Nothing on screen yet: the list arrives in the order it was given,
    // which is the order `sort_for_open` just decided.
    let first = keep_previous_order(&[], listed());
    assert_eq!(ids(&first), ["1", "2", "3"]);

    // Existing ids keep their positions even when the fresh list disagrees.
    let shuffled = vec![
        target("3", "Code", Some("code.exe")),
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Notepad", Some("notepad.exe")),
    ];
    let held = keep_previous_order(&strings(&["1", "2", "3"]), shuffled);
    assert_eq!(ids(&held), ["1", "2", "3"]);

    // A window opened while the modal was up: it goes on the end, not
    // wherever the enumeration happened to put it.
    let mut grown = listed();
    grown.insert(0, target("4", "New", Some("new.exe")));
    let appended = keep_previous_order(&strings(&["1", "2", "3"]), grown);
    assert_eq!(ids(&appended), ["1", "2", "3", "4"]);

    // A window that closed is the only thing allowed to move the rest.
    let mut shrunk = listed();
    shrunk.remove(1);
    let dropped = keep_previous_order(&strings(&["1", "2", "3"]), shrunk);
    assert_eq!(ids(&dropped), ["1", "3"]);
}

fn ids(targets: &[CaptureTarget]) -> Vec<&str> {
    targets.iter().map(|target| target.id.as_str()).collect()
}

/// The bug this pair exists for: starting a recording rewrites
/// `last_capture_executable`, and the next poll used to sort on it and pull
/// the tile the user had just clicked to the front of the grid.
#[test]
fn only_opening_the_modal_reorders_the_grid() {
    let mut state = listed(vec![
        target("1", "Notepad", Some("notepad.exe")),
        target("2", "Shooter Game", Some("game.exe")),
    ]);
    state.sort_for_open(Some("GAME.EXE"));
    assert_eq!(ids(&state.targets), ["2", "1"]);

    // The same list, polled again with the same preference: nothing moves,
    // because the modal is already open.
    state.refresh_targets(
        vec![
            target("1", "Notepad", Some("notepad.exe")),
            target("2", "Shooter Game", Some("game.exe")),
        ],
        Some("game.exe"),
    );
    assert_eq!(ids(&state.targets), ["2", "1"]);
}

#[test]
fn each_tab_only_counts_and_shows_its_own_kind() {
    let mut state = listed(vec![
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Notepad", Some("notepad.exe")),
        monitor("m1", 1),
    ]);
    assert_eq!(state.tab(), PickerTab::Windows);
    assert_eq!(state.total(), 2);
    assert_eq!(
        ids(&state.visible().cloned().collect::<Vec<_>>()),
        ["1", "2"]
    );

    state.set_tab(PickerTab::Monitors);
    assert_eq!(state.total(), 1);
    assert_eq!(state.visible_count(), 1);

    // Both tabs search since t260927-711d (決めたこと 3; it used to leave
    // the screens alone, the field being hidden there): a window's title
    // fits no screen, and the windows tab narrows as ever.
    state.set_search("shooter game");
    assert_eq!(state.visible_count(), 0);
    state.set_tab(PickerTab::Windows);
    assert_eq!(state.visible_count(), 1);
}

#[test]
fn the_tab_index_round_trips_through_the_segment_control() {
    assert_eq!(PickerTab::from_index(0), PickerTab::Windows);
    assert_eq!(PickerTab::from_index(1), PickerTab::Monitors);
    // Anything else is the default rather than a panic: the int comes from
    // the `.slint` side, which has no enum to be wrong about.
    assert_eq!(PickerTab::from_index(7), PickerTab::Windows);
    assert_eq!(PickerTab::Monitors.index(), 1);
}

fn target(id: &str, title: &str, executable: Option<&str>) -> CaptureTarget {
    CaptureTarget {
        title: title.into(),
        executable_id: executable.map(str::to_owned),
        executable_name: executable.map(str::to_owned),
        selectable: executable.is_some(),
        ..crate::capture::tests::test_window(id)
    }
}

/// task4290: the picker's poller re-captures a motionless window every
/// 120ms and used to push each identical frame into the model, which is a
/// redraw per capture. Only a frame that differs is worth sending.
#[test]
fn only_a_changed_thumbnail_frame_is_worth_sending() {
    let mut digests = ThumbnailDigests::default();
    let still = vec![7u8; 16];

    assert!(digests.changed("1", Some((2, 2, &still))), "first sighting");
    assert!(!digests.changed("1", Some((2, 2, &still))), "same picture");
    // Another target's identical picture is still that target's first.
    assert!(digests.changed("2", Some((2, 2, &still))), "other target");

    let moved = vec![9u8; 16];
    assert!(
        digests.changed("1", Some((2, 2, &moved))),
        "the window moved"
    );
    // Same bytes, different shape: a resized window is a change.
    assert!(digests.changed("1", Some((4, 1, &moved))), "resized");
}

/// A capture failure is a value, not a gap: `None` reaches the UI as
/// `TileMedia::None`, so the trips in and out of it have to be sent or the
/// failure glyph would never appear (and never clear).
#[test]
fn a_capture_failure_counts_as_a_change_in_both_directions() {
    let mut digests = ThumbnailDigests::default();
    let picture = vec![1u8; 4];

    assert!(digests.changed("1", Some((1, 1, &picture))));
    assert!(digests.changed("1", None), "picture -> failure");
    assert!(!digests.changed("1", None), "still failing");
    assert!(digests.changed("1", Some((1, 1, &picture))), "back");
}

/// The digests are pruned against the same list the UI prunes its thumbnail
/// map against, so a target that leaves and comes back sends again --
/// otherwise its tile would sit on the loading glyph for good.
#[test]
fn a_target_that_left_the_list_sends_its_frame_again_when_it_returns() {
    let mut digests = ThumbnailDigests::default();
    let picture = vec![3u8; 4];
    let one = target("1", "One", Some("one.exe"));
    let two = target("2", "Two", Some("two.exe"));

    assert!(digests.changed("1", Some((1, 1, &picture))));
    assert!(digests.changed("2", Some((1, 1, &picture))));

    digests.retain_listed(std::slice::from_ref(&two));
    assert!(digests.changed("1", Some((1, 1, &picture))), "1 came back");
    assert!(
        !digests.changed("2", Some((1, 1, &picture))),
        "2 never left"
    );

    digests.retain_listed(&[one, two]);
    assert!(!digests.changed("1", Some((1, 1, &picture))), "both kept");
}

fn listed(targets: Vec<CaptureTarget>) -> TargetsState {
    let mut state = TargetsState::default();
    state.set_targets(targets, None);
    state
}

#[test]
fn search_matches_title_case_insensitively() {
    let mut state = listed(vec![
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Notepad", Some("notepad.exe")),
    ]);
    state.set_search("sHOOTER gAME");
    let visible: Vec<_> = state.visible().map(|target| target.id.as_str()).collect();
    assert_eq!(visible, ["1"]);
    // search also matches the executable name.
    {
        let mut state = listed(vec![
            target("1", "Shooter Game", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ]);
        state.set_search("notepad.EXE");
        let visible: Vec<_> = state.visible().map(|target| target.id.as_str()).collect();
        assert_eq!(visible, ["2"]);
    }
    // a target without an executable name is matched on its title alone.
    {
        let mut state = listed(vec![target("1", "Shooter Game", None)]);
        state.set_search("shooter");
        assert_eq!(state.visible_count(), 1);
        // `exe` is in every sibling's executable name and in no title, so a
        // target that has no executable at all cannot match it.
        state.set_search("exe");
        assert_eq!(state.visible_count(), 0);
    }
    // empty search shows everything.
    {
        let state = listed(vec![
            target("1", "Shooter Game", Some("game.exe")),
            target("2", "Notepad", Some("notepad.exe")),
        ]);
        assert_eq!(state.visible_count(), 2);
        assert_eq!(state.total(), 2);
        assert_eq!(state.empty(), None);
    }
}

#[test]
fn a_filter_that_matched_nothing_is_not_the_same_nothing_as_an_empty_list() {
    let mut state = listed(vec![target("1", "Shooter Game", Some("game.exe"))]);
    state.set_search("nothing here");
    assert_eq!(state.empty(), Some(TargetsEmpty::NoMatch));
    assert_eq!(
        search_empty_text(Locale::Ja, PickerTab::Windows, "nothing here"),
        "「nothing here」に合うウィンドウはありません"
    );
    // t260928-08df: the screens tab says screens.
    assert_eq!(
        search_empty_text(Locale::Ja, PickerTab::Monitors, "nothing here"),
        "「nothing here」に合う画面はありません"
    );
    assert_eq!(
        search_empty_text(Locale::En, PickerTab::Monitors, "x"),
        "No displays match “x”"
    );

    let state = listed(vec![]);
    assert_eq!(state.empty(), Some(TargetsEmpty::NoTargets));
}

/// t260928-08df: a refused start reads as a sentence, never as the
/// engine's English -- every kind, both languages. The disk refusal is
/// worded from its numbers (t261101-5ff3).
#[test]
fn a_refused_start_is_told_in_words_not_in_the_engines_detail() {
    let raw = "the capture state lock is poisoned";
    for locale in [Locale::Ja, Locale::En] {
        for error in [
            StartError::Internal(raw.to_owned()),
            StartError::Codec(raw.to_owned()),
            StartError::BufferFolder(raw.to_owned()),
        ] {
            let text = start_failure_text(locale, &error);
            assert!(!text.is_empty() && !text.contains(raw), "{error:?}: {text}");
        }
    }
    assert_eq!(
        start_failure_text(Locale::Ja, &StartError::BufferFolder(raw.to_owned())),
        "録画バッファのフォルダを使えません。設定で保存先を確かめてください。"
    );
    assert_eq!(
        start_failure_text(
            Locale::Ja,
            &StartError::NoSpace(crate::capture::DiskRefusal {
                required_bytes: 0,
                free_bytes: 0,
                already_running: 0,
            })
        ),
        "空き容量が 10 GB 以下のため録画を開始できません。"
    );
    assert!(!target_closed(Locale::Ja).contains("選択"));
    assert!(!target_closed(Locale::Ja).contains("再試行"));
}

#[test]
fn unselectable_targets_carry_their_own_reason_and_fall_back_to_a_generic_one() {
    let mut unavailable = target("1", "Shooter Game", None);
    assert_eq!(
        blocked_reason(Locale::Ja, &unavailable),
        Some(super::unavailable(Locale::Ja))
    );
    assert!(!super::unavailable(Locale::Ja).contains("選択"));
    unavailable.unavailable_reason = Some("プロセス情報を取得できません".into());
    // Even minimized: the enumeration only leaves the reason empty when
    // being minimized is the whole of it (task2430).
    unavailable.minimized = true;
    assert_eq!(
        blocked_reason(Locale::Ja, &unavailable),
        Some("プロセス情報を取得できません")
    );
    assert_eq!(
        blocked_reason(Locale::Ja, &target("2", "Notepad", Some("n.exe"))),
        None
    );
}

/// task2430. Minimized is both: the bottom pill still names the state,
/// and -- since the encoder refuses the title-bar-sized client rect a
/// minimized window reports -- it is now also why the tile cannot be
/// picked. Every start path is gated on `selectable`, so clearing that one
/// flag is what makes the tile inert; the pill is what keeps a dimmed tile
/// from being unexplained.
#[test]
fn minimized_is_the_reason_you_cannot_pick_it() {
    let mut minimized = target("1", "Shooter Game", Some("game.exe"));
    minimized.minimized = true;
    minimized.selectable = false;
    assert_eq!(
        blocked_reason(Locale::Ja, &minimized),
        Some(minimized_unavailable(Locale::Ja))
    );
    let state = listed(vec![minimized]);
    assert!(!state.can_start("1"));
    assert_eq!(state.tile_action("1"), TileAction::None);
}

/// The start path demanded an `executable_id` from every target, which a
/// screen never has, so screen tiles were selectable but inert (task165).
#[test]
fn a_screen_starts_without_an_executable_identity_but_a_window_does_not() {
    assert!(!TargetsState::needs_executable(CaptureTargetKind::Monitor));
    assert!(TargetsState::needs_executable(CaptureTargetKind::Window));
}

#[test]
fn only_selectable_targets_can_start_and_recording_freezes_only_its_own_tile() {
    let mut state = listed(vec![
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Ghost", None),
        target("3", "Notepad", Some("notepad.exe")),
    ]);
    assert!(state.can_start("1"));
    assert!(!state.can_start("2"), "no process identity, no capture");
    assert!(!state.can_start("missing"));

    // Recording tile 1: every other tile still starts one more beside it
    // (task2030 判断1), and the one already recording is the stop half of
    // the toggle rather than a second start.
    state.set_running(vec![("sess-a".into(), "0x1".into())]);
    assert!(!state.can_start("1"));
    assert_eq!(state.recording_session("1"), Some("sess-a"));
    assert!(state.can_start("3"));
    assert!(!state.can_start("2"), "still no process identity");

    // A second capture beside the first freezes neither of the others.
    state.set_running(vec![
        ("sess-a".into(), "0x1".into()),
        ("sess-b".into(), "0x3".into()),
    ]);
    assert_eq!(state.running_count(), 2);
    assert!(!state.can_start("1"));
    assert!(!state.can_start("3"));
    assert_eq!(state.recording_session("3"), Some("sess-b"));

    // A capture that ended leaves nothing inert behind.
    state.set_running(Vec::new());
    assert_eq!(state.recording_session("1"), None);
    assert!(!state.capturing());
    assert!(state.can_start("1"));
}

/// Task2030. The poll is ground truth: a click's optimism lasts exactly
/// until the next one, whether or not the start it hoped for landed.
#[test]
fn a_clicked_tile_reads_as_recording_until_the_next_poll_says_otherwise() {
    let mut state = listed(vec![target("1", "Shooter Game", Some("game.exe"))]);
    state.set_capturing_target("1");
    assert!(state.capturing());
    assert_eq!(state.tile_action("1"), TileAction::Stop);
    // ...but there is no session to stop yet, so the click has nothing to
    // send and the handler falls through rather than stopping the wrong one.
    assert_eq!(state.recording_session("1"), None);
    // A start that was refused: the poll comes back empty and the tile
    // offers to start again.
    state.set_running(Vec::new());
    assert_eq!(state.tile_action("1"), TileAction::Capture);
    assert!(state.can_start("1"));
}

#[test]
fn a_selection_that_left_the_list_does_not_survive_the_refresh() {
    let mut state = TargetsState::default();
    state.set_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
    state.select("1");
    state.set_targets(vec![target("2", "Shooter Game", Some("game.exe"))], None);
    assert_eq!(state.selected_id(), None);
}

#[test]
fn an_unselectable_window_is_never_preselected() {
    let mut unselectable = target("1", "Shooter Game", Some("game.exe"));
    unselectable.selectable = false;
    let mut state = TargetsState::default();
    state.set_targets(vec![unselectable], Some("game.exe"));
    assert_eq!(state.selected_id(), None);
}

/// Round5 §5 + task175: the tile being recorded is the one that stops.
#[test]
fn only_the_recording_tile_offers_to_stop() {
    let mut state = listed(vec![
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Ghost", None),
        target("3", "Notepad", Some("notepad.exe")),
    ]);

    // Nothing recording: every selectable tile starts one.
    assert_eq!(state.tile_action("1"), TileAction::Capture);
    assert_eq!(state.tile_action("3"), TileAction::Capture);
    // No process identity, so no action at all -- unchanged by task175.
    assert_eq!(state.tile_action("2"), TileAction::None);
    assert_eq!(state.tile_action("missing"), TileAction::None);

    state.set_running(vec![("sess-a".into(), "0x1".into())]);
    assert_eq!(state.tile_action("1"), TileAction::Stop);
    // The others start one more beside it (task2030 判断1); they do not stop.
    assert_eq!(state.tile_action("3"), TileAction::Capture);
    assert_eq!(state.tile_action("2"), TileAction::None);

    // Two running: two stop offers, and no tile is inert for the count.
    state.set_running(vec![
        ("sess-a".into(), "0x1".into()),
        ("sess-b".into(), "0x3".into()),
    ]);
    assert_eq!(state.tile_action("1"), TileAction::Stop);
    assert_eq!(state.tile_action("3"), TileAction::Stop);

    // And the offer goes away with the recording.
    state.set_running(Vec::new());
    assert_eq!(state.tile_action("1"), TileAction::Capture);
}

/// task a015. The list poll re-sends the same windows once a second, and
/// the drain must be able to tell that apart from a window appearing.
#[test]
fn the_same_list_twice_is_not_a_change() {
    let mut state = TargetsState::default();
    let listed = vec![
        target("1", "Notepad", Some("notepad.exe")),
        monitor("m1", 1),
    ];
    let first = state.refresh_targets(listed.clone(), None);
    assert!(first.listed, "the first list is new by definition");
    let again = state.refresh_targets(listed, None);
    assert!(!again.any(), "an identical poll moves nothing: {again:?}");
    assert_eq!(again.first_diff, None);
}

/// A window opening is a change, and the instrument names which one -- a
/// count alone cannot tell a new window from a title that ticked.
#[test]
fn a_window_appearing_is_a_change_and_names_itself() {
    let mut state = TargetsState::default();
    state.refresh_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
    let change = state.refresh_targets(
        vec![
            target("1", "Notepad", Some("notepad.exe")),
            target("2", "Shooter Game", Some("game.exe")),
        ],
        None,
    );
    assert!(change.listed);
    assert_eq!(change.first_diff.as_deref(), Some("Shooter Game"));
}

/// The same window under a new title is a change: the tile draws the title.
#[test]
fn a_retitled_window_is_a_change() {
    let mut state = TargetsState::default();
    state.refresh_targets(vec![target("1", "Notepad", Some("notepad.exe"))], None);
    let change = state.refresh_targets(vec![target("1", "Notepad *", Some("notepad.exe"))], None);
    assert!(change.listed);
    assert_eq!(change.first_diff.as_deref(), Some("Notepad *"));
}

/// The order ruling, written down as a test (task a015 Step 4). The poll's
/// own enumeration order is not stable, and `refresh_targets` normalises it
/// back onto the order the user is looking at -- so a reordered poll of the
/// same windows draws the identical grid and is not a change.
#[test]
fn a_reordered_poll_of_the_same_windows_is_not_a_change() {
    let mut state = TargetsState::default();
    let first = target("1", "Notepad", Some("notepad.exe"));
    let second = target("2", "Shooter Game", Some("game.exe"));
    state.refresh_targets(vec![first.clone(), second.clone()], None);
    let change = state.refresh_targets(vec![second, first], None);
    assert!(
        !change.any(),
        "keep_previous_order puts them back: {change:?}"
    );
}

/// `set_targets` -- the open-modal path does reshuffle, so there the same
/// windows in a new order *are* a different grid.
#[test]
fn a_reordered_list_through_set_targets_is_a_change() {
    let mut state = TargetsState::default();
    let first = target("1", "Notepad", Some("notepad.exe"));
    let second = target("2", "Shooter Game", Some("game.exe"));
    state.set_targets(vec![first.clone(), second.clone()], None);
    assert!(state.set_targets(vec![second, first], None).listed);
}

/// The preselection moving is a change even when the list did not.
#[test]
fn the_preselection_moving_is_a_change_on_its_own() {
    let mut state = TargetsState::default();
    let listed = vec![target("1", "Shooter Game", Some("game.exe"))];
    assert!(state.set_targets(listed.clone(), None).listed);
    let change = state.set_targets(listed, Some("game.exe"));
    assert!(!change.listed, "the same list");
    assert!(change.selection, "but a tile is preselected now");
}

/// `Msg::Capturing` arrives every 1s tick (the list only while someone
/// needs it, t261025-b28f), so it answers the same question.
#[test]
fn an_unchanged_running_list_is_not_a_change() {
    let mut state = TargetsState::default();
    let running = vec![("sess-a".to_owned(), "0x1".to_owned())];
    assert!(state.set_running(running.clone()), "nothing to something");
    assert!(!state.set_running(running.clone()), "the same poll again");
    assert!(state.set_running(Vec::new()), "the recording stopped");
    // The optimistic tile from a click is dropped here, and that is a
    // change however still the running map is.
    state.set_capturing_target("1");
    assert!(state.set_running(Vec::new()), "pending_start was cleared");
    assert!(!state.set_running(Vec::new()));
}

/// The banner is the other half of what the picker pane draws.
#[test]
fn the_alert_reports_only_the_transitions() {
    let mut alert = TargetsAlert::default();
    assert!(!alert.listed_ok(), "nothing to clear");
    assert!(alert.list_failed("a".to_owned()), "the failure appeared");
    assert!(
        !alert.list_failed("a".to_owned()),
        "it was already saying that"
    );
    assert!(
        alert.list_failed("b".to_owned()),
        "a new reason is a change"
    );
    assert!(alert.listed_ok(), "the failure went away");
    assert!(!alert.listed_ok());
}

/// t260927-711d 決めたこと 3: the search works on 画面全体 too, against the
/// screen's own title -- its number, and メイン on the primary one.
#[test]
fn the_search_reaches_the_screens_tab_too() {
    let mut state = listed(vec![
        target("1", "Shooter Game", Some("game.exe")),
        monitor("m1", 1),
        monitor("m2", 2),
    ]);
    state.set_tab(PickerTab::Monitors);
    state.set_search("画面 2");
    assert_eq!(ids(&state.visible().cloned().collect::<Vec<_>>()), ["m2"]);
    state.set_search("メイン");
    assert_eq!(ids(&state.visible().cloned().collect::<Vec<_>>()), ["m1"]);
    state.set_search("main");
    assert_eq!(ids(&state.visible().cloned().collect::<Vec<_>>()), ["m1"]);
    // A query that fits no screen empties this tab, and says so as the
    // filter's nothing, not as "nothing to record".
    state.set_search("shooter");
    assert_eq!(state.visible_count(), 0);
    assert_eq!(state.empty(), Some(TargetsEmpty::NoMatch));
}

/// The tabs carry their counts (DS Tabs `count`): everything of that kind,
/// before the search narrows it.
#[test]
fn each_tab_counts_its_whole_kind_whatever_the_search() {
    let mut state = listed(vec![
        target("1", "Shooter Game", Some("game.exe")),
        target("2", "Notepad", Some("notepad.exe")),
        monitor("m1", 1),
    ]);
    state.set_search("notepad");
    assert_eq!(state.tab_count(PickerTab::Windows), 2);
    assert_eq!(state.tab_count(PickerTab::Monitors), 1);
    assert_eq!(state.visible_count(), 1);
}

/// DS TargetTile 「画面のタイトル」: the number and メイン in words, no
/// primary badge. A window keeps its own title.
#[test]
fn a_screen_tile_names_its_number_and_main_in_words() {
    assert_eq!(tile_title(Locale::Ja, &monitor("m1", 1)), "画面 1 · メイン");
    assert_eq!(tile_title(Locale::Ja, &monitor("m2", 2)), "画面 2");
    let mut en = monitor("m1", 1);
    en.title = monitor_display_name(Locale::En, 1, 1920, 1080);
    assert_eq!(tile_title(Locale::En, &en), "Screen 1 · Main");
    assert_eq!(
        tile_title(Locale::Ja, &target("1", "Shooter Game", Some("game.exe"))),
        "Shooter Game"
    );
}

/// The DS's own words (CaptureScreen / TargetTile README, Version 14).
#[test]
fn the_flyout_speaks_the_ds_words() {
    assert_eq!(TileAction::Capture.label(Locale::Ja), "録画する");
    assert_eq!(TileAction::Stop.label(Locale::Ja), "録画を止める");
    assert_eq!(TileAction::Capture.label(Locale::En), "Record");
    assert_eq!(TileAction::Stop.label(Locale::En), "Stop recording");
    assert_eq!(TileAction::None.label(Locale::Ja), "");
    assert_eq!(rec(Locale::Ja), "REC");
    assert_eq!(search_placeholder(Locale::Ja), "タイトルやアプリ名で検索");
    assert_eq!(
        search_empty_text(Locale::Ja, PickerTab::Windows, "zzz"),
        "「zzz」に合うウィンドウはありません"
    );
    assert_eq!(empty_title(Locale::Ja), "録画できるウィンドウがありません");
    assert_eq!(empty_sub(Locale::Ja), "一覧は自動で更新されます。");
    assert_eq!(list_error_title(Locale::Ja), "一覧を取得できませんでした");
    assert_eq!(start_failed_title(Locale::Ja), "録画を開始できませんでした");
    for locale in [Locale::Ja, Locale::En] {
        for text in [
            rec(locale),
            search_placeholder(locale),
            empty_title(locale),
            empty_sub(locale),
            list_error_title(locale),
            start_failed_title(locale),
            foot_note(locale, PickerTab::Windows, 0).0.as_str(),
        ] {
            assert!(!text.is_empty());
        }
    }
    assert!(search_empty_text(Locale::En, PickerTab::Monitors, "zzz").contains("zzz"));
}

/// DS CaptureScreen 下端 and 「状態」: what gets recorded, per tab, until
/// three or more are running -- then the line is the warning instead.
#[test]
fn the_foot_says_what_is_recorded_until_three_run() {
    assert_eq!(
        foot_note(Locale::Ja, PickerTab::Windows, 2),
        (
            "選んだウィンドウだけを録ります。通知や他のアプリは映りません。".to_owned(),
            false
        )
    );
    assert_eq!(
        foot_note(Locale::Ja, PickerTab::Monitors, 0),
        (
            "画面全体では、その画面に映るものがすべて録画されます。".to_owned(),
            false
        )
    );
    assert_eq!(
        foot_note(Locale::Ja, PickerTab::Windows, 3),
        (
            "3 本を録画中です。同時に録る本数が多いと、録画が重くなることがあります。".to_owned(),
            true
        )
    );
    assert!(foot_note(Locale::En, PickerTab::Monitors, 4).1);
}

/// DS CaptureScreen 「一覧を取れない」: the reason travels with the failure.
#[test]
fn a_list_failure_keeps_its_reason_until_a_good_list() {
    let mut alert = TargetsAlert::default();
    assert_eq!(alert.text(), None);
    assert!(alert.list_failed("EnumWindows failed".to_owned()));
    assert_eq!(alert.text(), Some("EnumWindows failed"));
    assert!(!alert.list_failed("EnumWindows failed".to_owned()));
    assert!(alert.listed_ok());
    assert_eq!(alert.text(), None);
}
