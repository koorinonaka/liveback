use super::*;
use crate::test_support::strings;
use crate::ui_state::selection::{ClickOutcome, PlainClick, Selection};
use std::path::PathBuf;

/// 2026-09-06 is a **Sunday**, so it is the last day of the week that
/// started Monday 2026-08-31. Every week fixture below is anchored on it,
/// which is what pins the Monday start down: a Sunday-start calendar would
/// put 09/06 in a fresh week and every one of these would move.
const NOW: &str = "2026/09/06 21:04:00";

/// t260927-bb89: the card's monogram reads the app off the export's own
/// `{game}_{YYYYMMDD-HHMMSS}.mp4`.
#[test]
fn the_app_is_read_off_the_clip_file_name() {
    assert_eq!(
        app_from_file_name("Overwatch2_20260926-210412.mp4"),
        Some("Overwatch2")
    );
    // A game name with `_` of its own, and the counter a taken name gets.
    assert_eq!(
        app_from_file_name("Tom_Clancy_20260926-210412_02.mp4"),
        Some("Tom_Clancy")
    );
    assert_eq!(
        app_from_file_name("game_20260926-210412_part01.MP4"),
        Some("game")
    );
    // Names that do not follow the rule have no app.
    for name in [
        "arena-2026-09-06.mp4",
        "_20260926-210412.mp4",
        "game_20260926-2104.mp4",
        "game_2026092x-210412.mp4",
        "game_20260926-210412x.mp4",
        "game_20260926-210412.mkv",
        "mp4",
    ] {
        assert_eq!(app_from_file_name(name), None, "{name}");
    }
}

/// t260927-bb89: the DS ClipsScreen's words for the empty folder, the
/// empty search, the selection's delete and the toasts. (The top row's
/// count went in t260928-80e7.)
#[test]
fn the_clip_screen_speaks_the_ds_words() {
    assert_eq!(empty(Locale::Ja), "クリップはまだありません");
    assert_eq!(
        empty_sub(Locale::Ja),
        "確認画面の「クリップに保存」で、ここに並びます。"
    );
    assert_eq!(
        search_empty_text(Locale::Ja, " 戦車 "),
        "「戦車」に合うクリップはありません"
    );
    assert_eq!(
        search_empty_text(Locale::En, "tank"),
        "No clips match \u{201c}tank\u{201d}"
    );
    assert_eq!(delete_selection_label(Locale::Ja, 2), "2 件を削除");
    assert_eq!(delete_selection_label(Locale::En, 2), "Delete 2");
    assert_eq!(hold_to_delete(Locale::Ja), "長押しで削除");
    assert_eq!(open_folder(Locale::Ja), "フォルダを開く");
    assert_eq!(deleted(Locale::Ja), "クリップを削除しました");
    // t260928-6080 F8: a title and a detail line, as the history's.
    assert_eq!(
        bulk_deleted(Locale::Ja, 2, "84.0 MB"),
        (
            "2 件のクリップを削除しました".to_owned(),
            "ごみ箱に移しました（84.0 MB）。".to_owned()
        )
    );
    assert_eq!(
        bulk_deleted(Locale::En, 2, "84.0 MB"),
        (
            "Deleted 2 clips".to_owned(),
            "Moved to the recycle bin (84.0 MB).".to_owned()
        )
    );
    // F7: the count alone, no size.
    assert_eq!(
        selection_text(Locale::Ja, 2),
        ("2 件".to_owned(), "を選択".to_owned())
    );
    assert_eq!(
        selection_text(Locale::En, 1),
        ("1 clip".to_owned(), " selected".to_owned())
    );
    // F13 / F9: what failed, in the past tense; the reason is line two.
    assert_eq!(delete_failed(Locale::Ja), "クリップを削除できませんでした");
    assert_eq!(
        comment_save_failed(Locale::Ja),
        "コメントを保存できませんでした"
    );
    assert_eq!(no_duration(Locale::Ja), "クリップの長さを読めませんでした");
    assert_eq!(list_failed(Locale::Ja), "一覧を取得できませんでした");
}

/// The pre-footprint call: nothing expanded, every footprint 1×1
/// (task3880). The existing cases below go through it unchanged, which is
/// what makes them the regression net for 「配置は従来と1画素も変わらない」.
fn place_plain(
    locale: Locale,
    grouping: ClipGrouping,
    stamps: &[String],
    now: &str,
    columns: usize,
) -> Vec<ClipPlacement> {
    place(locale, grouping, stamps, now, columns, None, 1)
}

pub(crate) fn headings(placed: &[ClipPlacement]) -> Vec<&str> {
    placed
        .iter()
        .filter(|entry| !entry.heading.is_empty())
        .map(|entry| entry.heading.as_str())
        .collect()
}

/// Monday is the boundary, and 2026-08-31 is one: a clip from that Monday
/// is 今週 next to 09/06, and the Sunday before it is 先週.
#[test]
fn the_week_turns_over_on_monday() {
    let placed = place_plain(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&["2026/08/31 00:10:00", "2026/08/30 23:50:00"]),
        NOW,
        4,
    );
    assert_eq!(headings(&placed), vec!["今週", "先週"]);
    // The second group opens a line of its own, under a second heading.
    assert_eq!((placed[0].line, placed[0].heading_offset), (0, 1));
    assert_eq!((placed[1].line, placed[1].heading_offset), (1, 2));
}

#[test]
fn older_weeks_keep_counting() {
    let placed = place_plain(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&["2026/08/17 12:00:00", "2026/07/06 12:00:00"]),
        NOW,
        4,
    );
    assert_eq!(headings(&placed), vec!["2 週間前", "8 週間前"]);

    let english = place_plain(
        Locale::En,
        ClipGrouping::Week,
        &strings(&[
            "2026/09/06 12:00:00",
            "2026/08/30 12:00:00",
            "2026/08/17 12:00:00",
        ]),
        NOW,
        4,
    );
    assert_eq!(
        headings(&english),
        vec!["This week", "Last week", "2 weeks ago"]
    );
}

#[test]
fn the_month_grouping_breaks_where_the_month_does() {
    let placed = place_plain(
        Locale::Ja,
        ClipGrouping::Month,
        &strings(&[
            "2026/09/01 00:05:00",
            "2026/08/31 23:55:00",
            "2026/08/01 00:00:00",
            "2025/12/31 23:00:00",
        ]),
        NOW,
        4,
    );
    assert_eq!(
        headings(&placed),
        vec!["2026年9月", "2026年8月", "2025年12月"]
    );
    assert_eq!((placed[1].line, placed[1].heading_offset), (1, 2));
    assert_eq!((placed[2].column, placed[2].line), (1, 1));
    assert_eq!((placed[3].line, placed[3].heading_offset), (2, 3));

    let english = place_plain(
        Locale::En,
        ClipGrouping::Month,
        &strings(&["2026/09/01 00:05:00"]),
        NOW,
        4,
    );
    assert_eq!(headings(&english), vec!["September 2026"]);
}

/// Never folded into the group above it, and never left without a heading
/// -- an empty heading is how the `.slint` side spells "no heading here".
#[test]
fn a_stamp_that_will_not_parse_stands_alone() {
    let placed = place_plain(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&["capture-0000", "2026/09/06 12:00:00", "capture-0001"]),
        NOW,
        4,
    );
    assert_eq!(
        headings(&placed),
        vec!["capture-0000", "今週", "capture-0001"]
    );
    assert_eq!(
        placed.iter().map(|e| e.line).collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
}

/// A clock that would not convert costs the relative wording, not the
/// grouping: the weeks still break where they break.
#[test]
fn week_headings_fall_back_to_the_month_when_now_is_unreadable() {
    let placed = place_plain(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&["2026/09/06 12:00:00", "2026/08/30 12:00:00"]),
        "",
        4,
    );
    assert_eq!(headings(&placed), vec!["2026年9月", "2026年8月"]);
}

#[test]
fn an_empty_list_places_nothing() {
    assert!(place_plain(Locale::Ja, ClipGrouping::Week, &[], NOW, 4).is_empty());
}

/// Six clips of one week, expanded 0..3 apart. Read as
/// `(column, line, span)`.
fn grid(placed: &[ClipPlacement]) -> Vec<(i32, i32, i32)> {
    placed
        .iter()
        .map(|entry| (entry.column, entry.line, entry.span))
        .collect()
}

const ONE_WEEK: [&str; 6] = [
    "2026/09/06 21:04:00",
    "2026/09/06 20:04:00",
    "2026/09/06 19:04:00",
    "2026/09/06 18:04:00",
    "2026/09/06 17:04:00",
    "2026/09/06 16:04:00",
];

/// Nothing expanded is the layout this function had before the footprint
/// existed -- reading order, one cell each -- and `line_count` then agrees
/// with the old `last.line + 1` derivation.
#[test]
fn nothing_expanded_lays_the_grid_out_exactly_as_before() {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&ONE_WEEK),
        NOW,
        4,
        None,
        // 3, the shipping footprint (`liveback::clips::EXPAND_SPAN`). Spelled
        // out rather than imported: the constant belongs to the binary that
        // owns the grid, and a second named copy here would be a second place
        // the number could be changed.
        3,
    );
    assert_eq!(
        grid(&placed),
        vec![
            (0, 0, 1),
            (1, 0, 1),
            (2, 0, 1),
            (3, 0, 1),
            (0, 1, 1),
            (1, 1, 1)
        ]
    );
    assert_eq!(line_count(&placed), 2);
    // The `span` argument alone changes nothing: it is the `expanded`
    // index that opens a footprint.
    assert_eq!(
        placed,
        place_plain(Locale::Ja, ClipGrouping::Week, &strings(&ONE_WEEK), NOW, 4)
    );
}

/// The B案 picture: the tiles after the expanded one are pushed aside and
/// re-fill the holes its footprint leaves, rather than being covered over.
/// The group grows, and `line_count` reports it.
#[test]
fn an_expanded_tile_pushes_the_rest_of_the_group_around_it() {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&ONE_WEEK),
        NOW,
        4,
        Some(1),
        2,
    );
    assert_eq!(
        grid(&placed),
        vec![
            (0, 0, 1),
            // 2×2, so it owns columns 1-2 of lines 0-1.
            (1, 0, 2),
            (3, 0, 1),
            (0, 1, 1),
            (3, 1, 1),
            (0, 2, 1)
        ]
    );
    assert_eq!(line_count(&placed), 3);

    // The shipping span (task3880: `EXPAND_SPAN` is 3 against `COLUMNS`
    // of 4), which is what makes the remaining tiles stack in the one
    // column left of the grid -- round20 §1's 「右の1列に元サイズで積み」.
    let shipping = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&ONE_WEEK[..4]),
        NOW,
        4,
        Some(0),
        // 3, the shipping footprint (`liveback::clips::EXPAND_SPAN`). Spelled
        // out rather than imported: the constant belongs to the binary that
        // owns the grid, and a second named copy here would be a second place
        // the number could be changed.
        3,
    );
    assert_eq!(
        grid(&shipping),
        vec![(0, 0, 3), (3, 0, 1), (3, 1, 1), (3, 2, 1)]
    );
}

/// The 2026-09-13 ruling, in the shape the user reported it: 1,2,3 with the
/// third pressed used to lay out as 1,2 / 3 -- the footprint fell a whole
/// line because the first hole wide enough for it was down there. It now
/// stays on line 0 and pushes 2 down instead, which is 1,3 / 2.
///
/// The pressed tile's *line* is the assertion that matters; its column
/// moves (2 -> 1) because three columns of footprint cannot start at column
/// 2 of a four-column grid.
#[test]
fn the_expanded_tile_keeps_the_line_it_had_in_the_list() {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&ONE_WEEK),
        NOW,
        4,
        Some(2),
        // 3, the shipping footprint (`liveback::clips::EXPAND_SPAN`). Spelled
        // out rather than imported: the constant belongs to the binary that
        // owns the grid, and a second named copy here would be a second place
        // the number could be changed.
        3,
    );
    assert_eq!(
        grid(&placed),
        vec![
            // 1 keeps column 0 of line 0 ...
            (0, 0, 1),
            // ... 2 is pushed off line 0 ...
            (0, 1, 1),
            // ... and 3 opens where it already was.
            (1, 0, 3),
            (0, 2, 1),
            (0, 3, 1),
            (1, 3, 1)
        ]
    );
    assert_eq!(line_count(&placed), 4);

    // A tile on the second line keeps *its* line the same way -- the rule
    // is "the line it had in the list", not "line 0".
    let second_line = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&ONE_WEEK),
        NOW,
        4,
        Some(5),
        3,
    );
    assert_eq!(
        second_line.get(5).map(|entry| (entry.column, entry.line)),
        Some((1, 1))
    );
}

/// The regression net `content-height` has and the `.slint` side cannot
/// get: with a footprint the last tile fills a hole beside it rather than
/// sitting on the bottom line, so `last.line + 1` -- what `clips.slint`
/// derived before task3880 -- undercounts and the grid refuses to scroll
/// to its own bottom.
#[test]
fn the_line_count_outgrows_the_last_tiles_own_line() {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&ONE_WEEK[..3]),
        NOW,
        4,
        Some(0),
        // 3, the shipping footprint (`liveback::clips::EXPAND_SPAN`). Spelled
        // out rather than imported: the constant belongs to the binary that
        // owns the grid, and a second named copy here would be a second place
        // the number could be changed.
        3,
    );
    // Three clips, the first of them 3×3: the two others stack in the one
    // free column beside it, so the last one sits on line 1 -- two lines
    // above the bottom of the footprint.
    assert_eq!(placed.last().map(|entry| entry.line), Some(1));
    assert_eq!(line_count(&placed), 3);
    // Which is the whole point: the old derivation would have said 2.
    assert!(line_count(&placed) > placed.last().map(|entry| entry.line + 1).unwrap());
}

/// The footprint never straddles a heading. Expanding the *first* tile of
/// the second group keeps it entirely on that group's own lines, and the
/// group below is pushed down by the footprint's height, not by one line.
#[test]
fn a_footprint_stays_inside_its_own_group() {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&[
            "2026/09/06 21:04:00",
            "2026/09/06 20:04:00",
            "2026/08/30 21:04:00",
            "2026/08/30 20:04:00",
            "2026/08/30 19:04:00",
            "2026/08/30 18:04:00",
            "2026/08/17 12:00:00",
        ]),
        NOW,
        4,
        Some(2),
        2,
    );
    assert_eq!(headings(&placed), vec!["今週", "先週", "2 週間前"]);
    assert_eq!(
        grid(&placed),
        vec![
            (0, 0, 1),
            (1, 0, 1),
            // The expanded tile opens its group on line 1 and covers
            // lines 1-2 -- never line 0, which belongs to 今週.
            (0, 1, 2),
            (2, 1, 1),
            (3, 1, 1),
            (2, 2, 1),
            // The third group starts below the footprint, not below the
            // line the previous group's last tile sat on.
            (0, 3, 1)
        ]
    );
    assert_eq!(
        placed.iter().map(|e| e.heading_offset).collect::<Vec<_>>(),
        vec![1, 1, 2, 2, 2, 2, 3]
    );
    assert_eq!(line_count(&placed), 4);
}

/// The gesture task3890 will attach is a toggle, and this is the whole of
/// what it does -- the `.slint` side only forwards the press.
#[test]
fn pressing_the_expanded_tile_again_collapses_it() {
    // Nothing expanded: the press opens that entry.
    assert_eq!(toggle_expanded(None, Some(2)), Some(2));
    // The same entry again closes it.
    assert_eq!(toggle_expanded(Some(2), Some(2)), None);
    // A different one moves the expansion rather than opening a second.
    assert_eq!(toggle_expanded(Some(2), Some(5)), Some(5));
    // A press that resolved to no entry at all -- an index the grid moved
    // out from under -- collapses instead of leaving a stale one open.
    assert_eq!(toggle_expanded(Some(2), None), None);
    assert_eq!(toggle_expanded(None, None), None);
}

/// Four clips: two with comments, one whose comment has not been read yet,
/// and one that was saved without any.
fn folder() -> Vec<(&'static str, Option<&'static str>)> {
    vec![
        ("arena-2026-09-06.mp4", Some("エース取った直後のリテイク")),
        ("sprint-2026-09-05.mp4", Some("ACE clutch")),
        ("arena-2026-09-04.mp4", None),
        ("mspaint-2026-09-03.mp4", Some("")),
    ]
}

#[test]
fn the_search_matches_the_comment_and_the_file_name() {
    assert_eq!(filter("", folder()), vec![0, 1, 2, 3]);
    // Nothing but whitespace is the same nothing: a field the user cleared
    // with the space bar must not empty the grid.
    assert_eq!(filter("   ", folder()), vec![0, 1, 2, 3]);
    assert!(filter("", Vec::new()).is_empty());
    // The comment, in the other case and with the query's own spaces
    // around it.
    assert_eq!(filter("  ACE  ", folder()), vec![1]);
    assert_eq!(filter("ace clutch", folder()), vec![1]);
    // Japanese, where `to_lowercase` changes nothing and a substring is
    // still a substring.
    assert_eq!(filter("リテイク", folder()), vec![0]);
    // A clip whose comment has not been read yet, and one saved without
    // one, are simply not matched by a comment query.
    assert!(filter("リテイク", folder()).iter().all(|&i| i != 2));
    // Matches the name of two clips, the comment of neither.
    assert_eq!(filter("arena", folder()), vec![0, 2]);
    // The name is where a clip with no comment at all is still findable.
    assert_eq!(filter("mspaint", folder()), vec![3]);
    // Either side is enough: `sprint` is only a name, `ACE` only a comment,
    // and both leave the same clip standing.
    assert_eq!(filter("SPRINT", folder()), vec![1]);
    // And a query that is in neither leaves nothing -- which is the
    // 「一致するクリップがありません」 case, not the empty-folder one.
    assert!(filter("cs2", folder()).is_empty());
}

fn edit(row: usize, path: &str) -> ClipEdit {
    ClipEdit {
        row,
        path: PathBuf::from(path),
        original: String::new(),
    }
}

#[test]
fn an_editor_only_survives_while_its_row_holds_its_own_file() {
    let listing = vec![PathBuf::from("a.mp4"), PathBuf::from("b.mp4")];
    assert!(edit_survives(&edit(1, "b.mp4"), &listing));

    // The file was deleted: the row now holds whatever moved up into it.
    let after_delete = vec![PathBuf::from("b.mp4")];
    assert!(!edit_survives(&edit(1, "b.mp4"), &after_delete));

    // Nothing was deleted, but a newer clip landed on top -- the list is
    // newest-first, so every index below it moved by one.
    let after_save = vec![
        PathBuf::from("new.mp4"),
        PathBuf::from("a.mp4"),
        PathBuf::from("b.mp4"),
    ];
    assert!(!edit_survives(&edit(1, "b.mp4"), &after_save));

    // The list emptied out from under it.
    assert!(!edit_survives(&edit(0, "a.mp4"), &[]));
}

#[test]
fn only_a_changed_comment_is_written() {
    // Untouched, in both directions of "no comment".
    assert!(!needs_write("", None));
    assert!(!needs_write("", Some("")));
    assert!(!needs_write("検証クリップ", Some("検証クリップ")));

    // Written: a first comment, a replacement, and the clear.
    assert!(needs_write("検証クリップ", None));
    assert!(needs_write("書き換え", Some("検証クリップ")));
    assert!(needs_write("", Some("検証クリップ")));
}

/// t260916-568d. The guess is what the tile shows while the write is in
/// flight; a read that has not seen the write does not end it.
#[test]
fn a_committed_comment_stands_until_a_read_that_saw_the_write() {
    use std::time::{Duration, SystemTime};
    let before = SystemTime::UNIX_EPOCH;
    let after = before + Duration::from_secs(1);
    let pending = PendingComment {
        text: "new".into(),
        at: before,
    };

    // The pre-write read task2990 files under the old time has missed it.
    assert!(!pending.superseded_by(before));
    // A read taken after the write moved the time is the truth from here.
    assert!(pending.superseded_by(after));

    // While it stands it wins over the cache, including when it clears the
    // comment; without one the cache is used however stale it is.
    assert_eq!(tile_comment(Some("new"), Some("old")), Some("new"));
    assert_eq!(tile_comment(Some(""), Some("old")), Some(""));
    assert_eq!(tile_comment(None, Some("old")), Some("old"));
    assert_eq!(tile_comment(None, None), None);
}

// ---------- the selection (t260916-5568) ----------

fn picked(selection: &Selection<String>) -> Vec<&str> {
    selection.selected().iter().map(String::as_str).collect()
}

/// A plain click keeps round20 §6: it plays, and the pile goes.
#[test]
fn a_plain_click_clears_the_selection_and_asks_for_the_expansion() {
    let order = strings(&["a", "b", "c"]);
    let mut selection = Selection::new(PlainClick::Ignore);
    selection.select_all(&order);
    assert_eq!(selection.len(), 3);
    assert_eq!(
        selection.click(&"b".to_owned(), false, false, &order),
        ClickOutcome::Expand
    );
    assert!(selection.is_empty());
}

#[test]
fn ctrl_click_toggles_one_tile_and_expands_nothing() {
    let order = strings(&["a", "b", "c"]);
    let mut selection = Selection::new(PlainClick::Ignore);
    let a = "a".to_owned();
    let c = "c".to_owned();
    assert_eq!(
        selection.click(&a, false, true, &order),
        ClickOutcome::Selected
    );
    assert_eq!(
        selection.click(&c, false, true, &order),
        ClickOutcome::Selected
    );
    assert_eq!(picked(&selection), ["a", "c"]);
    assert_eq!(
        selection.click(&a, false, true, &order),
        ClickOutcome::Selected
    );
    assert_eq!(picked(&selection), ["c"]);
}

/// The range runs along the grid's order from the last tile clicked, in
/// either direction, and a second Shift+click re-ranges from the same
/// anchor rather than from the end of the first range.
#[test]
fn shift_click_selects_the_grid_order_range_from_the_anchor() {
    let order = strings(&["a", "b", "c", "d", "e"]);
    let mut selection = Selection::new(PlainClick::Ignore);
    selection.click(&"b".to_owned(), false, true, &order);
    assert_eq!(
        selection.click(&"d".to_owned(), true, false, &order),
        ClickOutcome::Selected
    );
    assert_eq!(picked(&selection), ["b", "c", "d"]);
    selection.click(&"a".to_owned(), true, false, &order);
    assert_eq!(picked(&selection), ["a", "b"]);
    // No anchor yet: the tile alone, which then becomes the anchor.
    let mut fresh = Selection::new(PlainClick::Ignore);
    fresh.click(&"c".to_owned(), true, false, &order);
    assert_eq!(picked(&fresh), ["c"]);
}

/// A heading pushes the next group down by `heading-pitch` on top of the
/// line pitch -- measured through `place`, so the rectangle is the one the
/// grid actually draws.
#[test]
fn a_heading_step_moves_the_next_groups_rectangles_down() {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &strings(&["2026/09/06 20:00:00", "2026/08/20 20:00:00"]),
        NOW,
        4,
        None,
        1,
    );
    assert_eq!((placed[1].line, placed[1].heading_offset), (1, 2));
    let rects = tile_rects(&placed, 100.0, 56.25);
    assert_eq!(rects[0].y, GRID_PAD_TOP + GRID_HEADING_PITCH);
    assert_eq!(
        rects[1].y,
        GRID_PAD_TOP + (56.25 + GRID_GAP_Y) + 2.0 * GRID_HEADING_PITCH
    );
    assert_eq!(rects[1].x, GRID_PAD);
    assert_eq!((rects[1].width, rects[1].height), (100.0, 56.25));
}

/// The expanded tile is hit as the 3x3 footprint it is drawn as.
#[test]
fn an_expanded_tile_is_hit_across_its_whole_span() {
    let tile = ClipPlacement {
        column: 1,
        line: 0,
        heading_offset: 1,
        heading: String::new(),
        span: 3,
    };
    let rect = tile_rects(&[tile], 100.0, 50.0)[0];
    assert_eq!(rect.x, GRID_PAD + 100.0 + GRID_GAP_X);
    assert_eq!(rect.width, 3.0 * 100.0 + 2.0 * GRID_GAP_X);
    assert_eq!(rect.height, 3.0 * 50.0 + 2.0 * GRID_GAP_Y);
    // The rectangle really does reach the far corner of the block it is
    // drawn as -- a drag touching only that corner overlaps it.
    //
    // What this test no longer says is that such a drag *selects* the tile.
    // It used to (`picked(&selection) == ["big"]`), and the user's
    // 2026-09-19 ruling (t260920-c6eb) took that outcome away: an expanded
    // tile is out of every selection path, because it carries no selection
    // mark and a rectangle that swept it put an invisible clip into the next
    // Delete. The geometry below is unchanged and still the grid's; which
    // tiles are offered to `Selection::marquee` is decided by
    // `Clips::tile_rects` in `src/bin/liveback/clips.rs`, and tested there.
    let corner = Rect::from_corners(
        rect.x + rect.width - 1.0,
        rect.y + rect.height - 1.0,
        rect.x + rect.width + 30.0,
        rect.y + rect.height + 30.0,
    );
    assert!(rect.overlaps(&corner));
}

#[test]
fn a_marquee_adds_what_it_touches_to_its_base() {
    let a = Rect {
        x: 0.0,
        y: 0.0,
        width: 10.0,
        height: 10.0,
    };
    let b = Rect {
        x: 20.0,
        y: 0.0,
        width: 10.0,
        height: 10.0,
    };
    let c = Rect {
        x: 40.0,
        y: 0.0,
        width: 10.0,
        height: 10.0,
    };
    let rows = vec![
        ("a".to_owned(), a),
        ("b".to_owned(), b),
        ("c".to_owned(), c),
    ];
    let order = strings(&["a", "b", "c"]);
    let mut selection = Selection::new(PlainClick::Ignore);
    selection.click(&"c".to_owned(), false, true, &order);
    // Plain: the old selection goes, only what the band touches stays.
    selection.begin_marquee(false);
    selection.marquee(&rows, Rect::from_corners(5.0, 5.0, 25.0, 6.0));
    assert_eq!(picked(&selection), ["a", "b"]);
    // Shrinking the band gives back what it no longer touches.
    selection.marquee(&rows, Rect::from_corners(5.0, 5.0, 8.0, 6.0));
    assert_eq!(picked(&selection), ["a"]);
    // Ctrl: on top of what was there.
    selection.begin_marquee(true);
    selection.marquee(&rows, Rect::from_corners(45.0, 5.0, 46.0, 6.0));
    assert_eq!(picked(&selection), ["a", "c"]);
}

/// A row the search hid leaves the selection, and so does the anchor.
#[test]
fn a_row_the_search_hid_leaves_the_selection() {
    let all = strings(&["a", "b", "c"]);
    let mut selection = Selection::new(PlainClick::Ignore);
    selection.click(&"a".to_owned(), false, true, &all);
    selection.click(&"b".to_owned(), false, true, &all);
    let shown = strings(&["a", "c"]);
    selection.retain(&shown);
    assert_eq!(picked(&selection), ["a"]);
    // The anchor `b` went too: a Shift+click now starts fresh.
    selection.click(&"c".to_owned(), true, false, &shown);
    assert_eq!(picked(&selection), ["c"]);
}

#[test]
fn a_menu_acts_on_the_selection_only_from_inside_it() {
    let order = strings(&["a", "b", "c"]);
    let mut selection = Selection::new(PlainClick::Ignore);
    selection.select_all(&order[..2]);
    assert_eq!(
        selection.menu_targets(&"a".to_owned()),
        strings(&["a", "b"])
    );
    selection.select_for_menu(&"c".to_owned());
    assert_eq!(picked(&selection), ["c"]);
    assert_eq!(selection.menu_targets(&"c".to_owned()), strings(&["c"]));
}

#[test]
fn the_drag_threshold_is_crossed_past_four_pixels_on_either_axis() {
    assert!(!past_drag_threshold((10.0, 10.0), (14.0, 6.0)));
    assert!(past_drag_threshold((10.0, 10.0), (14.5, 10.0)));
    assert!(past_drag_threshold((10.0, 10.0), (10.0, 5.0)));
}

/// `groups[g]` clips on one day each, a fortnight apart, so every group is a
/// week heading of its own.
fn week_groups(groups: &[usize]) -> Vec<String> {
    const DAYS: [&str; 4] = ["2026/09/06", "2026/08/23", "2026/08/09", "2026/07/26"];
    groups
        .iter()
        .zip(DAYS)
        .flat_map(|(count, day)| (0..*count).map(move |n| format!("{day} 12:{:02}:00", 59 - n)))
        .collect()
}

fn expanded_at(groups: &[usize], expanded: usize) -> Vec<ClipPlacement> {
    let placed = place(
        Locale::Ja,
        ClipGrouping::Week,
        &week_groups(groups),
        NOW,
        4,
        Some(expanded),
        3,
    );
    // Control: the fixture really is that many groups, or the short-group
    // cases below would silently be one long group.
    assert_eq!(headings(&placed).len(), groups.len(), "{groups:?}");
    placed
}

/// For every expanded position and every top line: nothing before the
/// answer reaches the window, the answer itself does, and it is never
/// later than the product the `.slint` side sent. The three conditions pin
/// the first on-screen tile exactly, so task4100's three counted cases
/// (`[40]` line 5 -> 12, `[5, 40]` line 4 -> 5, `seed-clips.ps1`'s
/// `[16, 9, 40]` line 9 -> 25) are rows here.
#[test]
fn the_first_visible_index_never_passes_a_tile_on_screen() {
    for groups in [
        &[40][..],
        &[13],
        &[1],
        &[5, 40],
        &[16, 9, 5],
        &[16, 9, 40],
        &[1, 1, 30],
    ] {
        let count: usize = groups.iter().sum();
        for expanded in 0..count {
            let placed = expanded_at(groups, expanded);
            for first_line in 0..=line_count(&placed) as usize {
                let first = first_visible_index(&placed, first_line);
                let case = format!("{groups:?}, expanded {expanded}, line {first_line}");
                assert!(first <= first_line * 4, "{case}");
                assert!(
                    placed[..first]
                        .iter()
                        .all(|entry| (entry.line + entry.span) as usize <= first_line),
                    "{case}: a tile before {first} is still on screen"
                );
                if let Some(entry) = placed.get(first) {
                    assert!((entry.line + entry.span) as usize > first_line, "{case}");
                }
            }
        }
    }
}

/// The same three conditions as the expanded case, over plain placements.
#[test]
fn with_nothing_expanded_the_first_visible_index_never_passes_a_tile_on_screen() {
    for groups in [
        &[40][..],
        &[13],
        &[1],
        &[1, 5, 9],
        &[5, 5, 5, 5],
        &[1, 1, 30],
    ] {
        let placed = place_plain(Locale::Ja, ClipGrouping::Week, &week_groups(groups), NOW, 4);
        for first_line in 0..=line_count(&placed) as usize + 1 {
            let first = first_visible_index(&placed, first_line);
            let case = format!("{groups:?}, line {first_line}");
            assert!(first <= first_line * 4, "{case}");
            assert!(
                placed[..first]
                    .iter()
                    .all(|entry| (entry.line + entry.span) as usize <= first_line),
                "{case}: a tile before {first} is still on screen"
            );
            if let Some(entry) = placed.get(first) {
                assert!((entry.line + entry.span) as usize > first_line, "{case}");
            }
        }
    }
}

/// t260916-5568: a menu over a selection keeps only what makes sense for
/// several files, and a menu over one clip keeps all four.
#[test]
fn a_menu_over_a_selection_offers_only_reveal_and_delete() {
    assert!(menu_actions(1) == MENU_ACTIONS.to_vec());
    assert!(menu_actions(0) == MENU_ACTIONS.to_vec());
    assert!(menu_actions(2) == vec![ClipMenuAction::Reveal, ClipMenuAction::Delete]);
}

/// t261101-c8b1: what each entry says, as the bin's render wrote it before
/// the menu moved here -- one clip, a selection, and the menu with no target
/// at all (one clip's list).
#[test]
fn the_clip_menu_says_what_the_render_drew() {
    use crate::ui_state::locale::Locale;
    let summary = |targets| {
        clip_menu(Locale::Ja, targets)
            .into_iter()
            .map(|item| {
                (
                    item.label,
                    item.hint,
                    item.rule_above,
                    item.danger,
                    item.disabled,
                )
            })
            .collect::<Vec<_>>()
    };
    let one = vec![
        ("再生", "", false, false, false),
        ("コメントを編集", "F2", false, false, false),
        ("フォルダで表示", "", false, false, false),
        ("削除", "", true, true, false),
    ];
    assert_eq!(summary(1), one);
    assert_eq!(summary(0), one);
    assert_eq!(
        summary(3),
        vec![
            ("フォルダで表示", "", false, false, false),
            ("削除", "", true, true, false),
        ]
    );
    let actions: Vec<_> = clip_menu(Locale::En, 2)
        .iter()
        .map(|item| item.action)
        .collect();
    assert_eq!(actions, menu_actions(2));
}
