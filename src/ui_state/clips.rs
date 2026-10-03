//! What the クリップ screen says (task2980), and which row it is editing
//! (task2990).
//!
//! Thin on purpose: the list itself is a directory scan (`crate::clips::list`)
//! and every number on it is already formatted by the history screen's own
//! helpers -- `sessions::format_bytes` for a size, `sessions::selection_text`
//! for the selection bar's tally, `timeline::format_duration` for a length. A second copy of any
//! of those would be a second set of rounding rules to keep in step. What is
//! here is only the wording that has no counterpart on the history screen.
//!
//! Task3020 added the one piece of real logic: the grid's date headings. The
//! calendar behind them is `sessions::day_number` -- the same six lines the
//! history row's 「昨日」 runs on -- borrowed rather than copied.

use crate::ui_state::locale::Locale;

crate::tr! {
    /// The rail item, and the pane's own name.
    rail_clips { ja: "クリップ", en: "Clips" }
    /// No `.mp4` in the folder -- which, since task3030, is the only nothing
    /// this screen ever shows: `livia::clips::list` creates the folder itself
    /// when it is missing, so "the folder does not exist" never reaches here
    /// to need its own sentence (round18 design §4-4 dropped it).
    ///
    /// One sentence, and it is this one rather than round18 §4-4's 「0件」:
    /// round20 §8-(6) settled the wording the other way round, on the mock's
    /// `.clipempty` (`mock/liveback-mock.html:1108`) once task3790's refresh had
    /// brought it down to a single line. No trailing 「。」 -- the mock has none
    /// and neither does `filter_empty`; the full stop in the design answer's
    /// prose is punctuation around the quote, not part of it. The English is the
    /// repo's own, as usual: design specifies ja only.
    ///
    /// t260927-bb89 took the DS ClipsScreen's own pair instead: a title that
    /// says the folder is merely new, and a second line naming where clips
    /// come from.
    empty { ja: "クリップはまだありません", en: "No clips yet" }
    empty_sub {
        ja: "確認画面の「クリップに保存」で、ここに並びます。",
        en: "Clips you save from the review screen land here."
    }
    /// The row's own hint, since the gesture is not visible on it. Task3890
    /// made that gesture a single click (round20 design response §6, which
    /// specifies this wording), and the hint follows it: the tile expands in
    /// place and the expanded tile is the playback surface. The history list
    /// keeps its double-click, and §6 accepted the two screens reading
    /// differently.
    open_hint { ja: "クリックで再生", en: "Click to play" }
    /// Context-menu entries.
    /// task3020 shortened both: the menu sits over a tile now, and the two
    /// entries read as a pair of verbs rather than a pair of sentences.
    menu_open { ja: "再生", en: "Play" }
    menu_reveal { ja: "フォルダで表示", en: "Show in folder" }
    /// The bottom overlay of a tile with no comment written yet (task3020).
    /// A field, not a label: the pencil beside it opens the editor.
    add_comment { ja: "コメントを追加", en: "Add a comment" }
    /// The 週 / 月 switch over the grid's date headings (task3020).
    group_week { ja: "週", en: "Week" }
    group_month { ja: "月", en: "Month" }
    /// The toolbar's search box (task3820, round20 §9). Named after what it
    /// looks for rather than what it searches through -- the file name matches
    /// too, but nobody opens this screen to type a file name.
    search_placeholder { ja: "コメントで検索", en: "Search comments" }
    /// task2990's two additions to the same context menu.
    menu_edit_comment { ja: "コメントを編集", en: "Edit comment" }
    menu_delete { ja: "削除", en: "Delete" }
    /// Deliberately not 「ごみ箱に移動しました」. The delete goes through
    /// `FOF_ALLOWUNDO`, but Windows permanently deletes anything larger than
    /// the bin's own cap and reports success either way
    /// (`ring_buffer::sessions::recycle`'s own doc comment). A clip is a
    /// finished mp4 and can be large, and nothing here checks the bin
    /// afterwards -- so the toast says what is certain and no more.
    deleted { ja: "クリップを削除しました", en: "The clip was deleted" }
    /// The shell refused, or the name is still in the folder afterwards. The
    /// reason goes on the toast's second line (t260928-6080 F13).
    delete_failed { ja: "クリップを削除できませんでした", en: "The clip could not be deleted" }
    /// Its second line (t260928-e309 F9): the likely reason and the next step,
    /// in place of the OS's own text, which goes to the log.
    delete_failed_hint {
        ja: "他のアプリが使っているかもしれません。閉じてからもう一度削除してください。",
        en: "Another app may be using it. Close it and delete again."
    }
    /// The selection bar's hint when its delete is tapped rather than held
    /// (t260927-bb89: every clip delete is a hold now, no dialog).
    hold_to_delete { ja: "長押しで削除", en: "Hold to delete" }
    /// The top row's button: the clip folder in Explorer (DS ClipsScreen).
    open_folder { ja: "フォルダを開く", en: "Open folder" }
    /// The expanded player's close, as its tooltip (DS ClipCard, t260928-6080 F5).
    close { ja: "閉じる", en: "Close" }
    /// The card's 「…」, which draws no word: its accessible name (t260928-13e9).
    more_actions { ja: "その他の操作", en: "More actions" }
    /// The folder could not be read (t260928-6080 F9, DS: 知らせ方). The OS's
    /// reason is the empty state's second line. Overrides task3030's silence,
    /// which showed the same 「まだありません」 as an empty folder.
    list_failed { ja: "一覧を取得できませんでした", en: "The list could not be read" }
    /// `write_comment` (task2960/task2990): the property store would not open
    /// or would not take the new value. The reason is appended by the caller.
    ///
    /// Task3360 added the parenthetical. Measured by
    /// `clips::tests::a_file_another_process_holds_open_refuses_the_comment`:
    /// a file held with a share mode that denies writers -- what a player
    /// leaves behind -- makes `GPS_READWRITE` fail, and this is the message
    /// the user gets, previously followed only by an HRESULT that says
    /// nothing about the player. The hint is hedged (「多くは」/
    /// "usually") on purpose: this one key also prefixes `file_missing` and
    /// `path_unresolvable`, and naming the player as *the* cause would be a
    /// lie on those. A dedicated key for the sharing violation would mean
    /// changing `write_comment` itself, which task3360 puts out of scope.
    ///
    /// Still right after task3530: Liveback itself no longer holds a clip open
    /// -- the external-player route is gone and the in-place player reads --
    /// but another app that does is exactly what the parenthetical describes.
    ///
    /// t260928-6080 F13: the heading is what failed, 「〜できませんでした」,
    /// and the hint moved to the toast's second line with the reason.
    comment_write_failed { ja: "コメントを書き込めませんでした", en: "The comment could not be written" }
    comment_write_hint {
        ja: "多くは、再生中など他のアプリがこのファイルを開いているためです。",
        en: "Usually another app -- a player, say -- has the file open."
    }
    /// Same call, once the value is set but `Commit` itself refuses.
    comment_save_failed { ja: "コメントを保存できませんでした", en: "The comment could not be saved" }
    /// `open_store` rejecting a path that is not a file. The path is appended
    /// by the caller.
    file_missing { ja: "ファイルがありません", en: "The file does not exist" }
    /// `open_store`'s `std::path::absolute` call failing.
    path_unresolvable { ja: "パスを解決できません", en: "The path could not be resolved" }
    /// task3520: the in-place player refused to open because the shell reports
    /// no `System.Media.Duration` for the file.
    ///
    /// The player is not opened at all in that case rather than opened at
    /// length 0: `ClipPlayer::open` clamps a missing length to zero, and a
    /// zero-length clip is a transport whose every seek lands at 0 and whose
    /// bar can never move -- a broken player rather than a refused one.
    no_duration { ja: "クリップの長さを読めませんでした", en: "The clip's length could not be read" }
}

/// Which calendar span one date heading covers (task3020, design §1-3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClipGrouping {
    #[default]
    Week,
    Month,
}

/// Where one clip sits in the grid, and the heading above it if it opens a
/// group (task3020).
///
/// The whole layout is decided here rather than in `.slint` because a heading
/// spans the grid's full width and pushes everything below it down: a tile's y
/// is `line * row-pitch + heading_offset * heading-pitch`, and neither running
/// count is something a slint `for` can carry. The `.slint` side reads these
/// four numbers and places a rectangle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipPlacement {
    /// 0..columns.
    pub column: i32,
    /// Which grid line, counting headings as no line of their own.
    pub line: i32,
    /// How many headings stand above this tile, its own group's included --
    /// so it is 1 on the first group and never 0.
    pub heading_offset: i32,
    /// The group's heading, on the group's first clip only. Empty elsewhere,
    /// which is also how the `.slint` side knows where to draw one.
    pub heading: String,
    /// How many columns *and* lines this tile's footprint covers: 1 for every
    /// ordinary tile, `span` for the one that is expanded (round20 §2-1 B案).
    /// The `.slint` side turns it into a width and a height.
    pub span: i32,
}

/// How many grid lines the placement occupies (task3880, from task3490's
/// prototype).
///
/// Not `placed.last().line + 1`, which is what `clips.slint` derived before:
/// with a footprint in the grid the last tile is usually *not* on the bottom
/// line -- it fills a hole beside the expanded tile -- and the pane's
/// `content-height` would then undercount and refuse to scroll to the bottom.
pub fn line_count(placed: &[ClipPlacement]) -> i32 {
    placed
        .iter()
        .map(|entry| entry.line + entry.span)
        .max()
        .unwrap_or(0)
}

/// The first grid index the detail queue has to treat as on screen, given the
/// first line the `.slint` side reported (task4100).
///
/// `clips.slint` sends `first-line * columns`, which assumes every line above
/// the window holds `columns` tiles. Two things break that. An expanded tile's
/// `span x span` footprint is one tile in `span * span` cells, so below it
/// every line starts `span * span - 1` indices earlier than the product says
/// -- 8 at the shipping 3 against 4 columns, which is all of
/// `THUMBNAIL_PREFETCH_MARGIN`. And a heading starts a fresh line, so a short
/// group leaves up to `columns - 1` cells empty; with the margin already spent
/// on the footprint, one such group above the window is enough for the queue
/// to skip tiles on screen.
///
/// The answer is therefore read off the placement itself: the first tile whose
/// footprint reaches `first_line` or below. Every tile on screen does, because
/// `first_line` is at or above the window's top line (the `.slint` side takes
/// the whole heading budget off to make it so).
///
/// That holds with nothing expanded too (t260925-4e00): short groups alone
/// drift past the margin once enough of them sit above the window -- four
/// groups of 5 at 4 columns put the product 9 past the first tile on screen.
pub fn first_visible_index(placed: &[ClipPlacement], first_line: usize, _columns: usize) -> usize {
    placed
        .iter()
        .position(|entry| {
            usize::try_from(entry.line + entry.span).is_ok_and(|end| end > first_line)
        })
        .unwrap_or(placed.len())
}

/// Which tile is expanded after a press on `pressed`, given `current`
/// (round20 §2-1 B案).
///
/// The state transition lives here rather than in the `.slint` gesture that
/// will call it (task3890): a gesture layer cannot be exercised by an automated
/// test, and this rule can. `pressed: None` is a press that resolved to no
/// entry at all -- an index the grid moved out from under -- and collapses,
/// because leaving a tile drawn large that nothing points at any more is the
/// stale-index mistake `CLAUDE.md` was written for.
pub fn toggle_expanded(current: Option<usize>, pressed: Option<usize>) -> Option<usize> {
    match pressed {
        None => None,
        Some(row) if current == Some(row) => None,
        Some(row) => Some(row),
    }
}

/// Greedy first-fit in reading order: the first free cell, marked taken.
///
/// `occupied` is one `columns`-wide row of flags per grid line the group has
/// reached so far, grown on demand. Confined to one group by the caller, which
/// is what keeps a tile from landing above its own date heading: a group's
/// occupancy starts empty, so nothing it places can reach the lines above it.
///
/// One cell, not a `footprint × footprint` block: since 2026-09-13 the expanded
/// tile is reserved by `place` at the line it already had, so no footprint ever
/// goes looking for a hole to fall into.
fn take_slot(occupied: &mut Vec<Vec<bool>>, columns: usize) -> (usize, usize) {
    let mut row = 0;
    loop {
        let line = &mut grow(occupied, columns, row + 1)[row];
        if let Some(column) = line.iter().position(|taken| !taken) {
            line[column] = true;
            return (row, column);
        }
        row += 1;
    }
}

/// Days since the Monday that starts `n`'s week.
///
/// `day_number` counts from 0000-03-01, whose phase we pin with a known date:
/// `day_number(1970, 1, 1)` is 719468 and 1970-01-01 was a Thursday, so
/// `n % 7 == 1` is Thursday and Monday is `(n + 2) % 7 == 0`.
fn days_into_week(day: i64) -> i64 {
    (day + 2).rem_euclid(7)
}

fn week_heading(locale: Locale, weeks_ago: i64) -> String {
    match (locale, weeks_ago) {
        (Locale::Ja, 0) => "今週".to_owned(),
        (Locale::Ja, 1) => "先週".to_owned(),
        (Locale::Ja, weeks) => format!("{weeks} 週間前"),
        (Locale::En, 0) => "This week".to_owned(),
        (Locale::En, 1) => "Last week".to_owned(),
        (Locale::En, weeks) => format!("{weeks} weeks ago"),
    }
}

fn month_heading(locale: Locale, year: i64, month: i64) -> String {
    const EN: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    match locale {
        Locale::Ja => format!("{year}年{month}月"),
        Locale::En => match EN.get((month - 1).clamp(0, 11) as usize) {
            Some(name) => format!("{name} {year}"),
            None => format!("{year}"),
        },
    }
}

/// Lays the clip list out as a grid of `columns` with a date heading over each
/// group (task3020).
///
/// `stamps` are the clips' modification times in the `YYYY/MM/DD HH:MM:SS`
/// shape the history screen already formats, newest first (which is the order
/// `clips::list` returns). `now` is the same shape, so "which week is this
/// week" stays a pure function with a test rather than something only a clock
/// can answer -- the rule `sessions::row_date_text` set.
///
/// A stamp that will not parse is its own group headed by the raw text: the
/// caller falls back to something unformatted when the Win32 clock conversion
/// refuses, and half-reading that is worse than showing it. An unparseable
/// `now` costs only the relative wording -- week groups fall back to naming
/// their month, which is still true.
///
/// `expanded` is the *grid position* of the one tile drawn large (round20 §2-1
/// B案) -- an index into `stamps`, not into the folder -- and `span` how many
/// columns and lines its footprint covers. `expanded: None` (or `span: 1`) is
/// the layout this function had before, cell for cell.
///
/// **The expanded tile keeps the line it had in the list** (user ruling,
/// 2026-09-13: 「リストに並んでるときと拡大したときで上のラインを変えない」).
/// Its footprint is reserved *before* the group is filled, at the row it
/// occupied unexpanded and at its own column pulled left only as far as it must
/// be for the footprint to fit. The rest of the group then fills in reading
/// order around it -- which is still B案's 「押しのける」, only now the tile
/// that was pressed stays put and its neighbours move instead of the other way
/// round. Filling first and letting the footprint take the first hole that fits
/// (what this did until 2026-09-13) dropped a tile a whole line the moment it
/// was not near enough to the left edge: 1,2,3 with 3 pressed laid out as
/// 1,2 / 3 when the user was pointing at 3.
pub fn place(
    locale: Locale,
    grouping: ClipGrouping,
    stamps: &[String],
    now: &str,
    columns: usize,
    expanded: Option<usize>,
    span: usize,
) -> Vec<ClipPlacement> {
    let columns = columns.max(1);
    // A footprint wider than the grid has no slot to land in, and `take_slot`
    // would spin looking for one.
    let span = span.clamp(1, columns);
    let now_week_start = super::sessions::split_stamp(now).map(|(year, month, day, _)| {
        let today = super::sessions::day_number(year, month, day);
        today - days_into_week(today)
    });

    // Which group every entry belongs to, up front: reserving the footprint
    // before the group is filled means knowing where the group *ends* while
    // placing its first tile, and a single forward pass cannot answer that.
    //
    // `(kind, value)`: two groupings must not collide on a shared number, and
    // an unparseable stamp must not join the group above it.
    let keys: Vec<(i64, i64)> = stamps
        .iter()
        .enumerate()
        .map(
            |(index, stamp)| match (super::sessions::split_stamp(stamp), grouping) {
                (Some((year, month, day, _)), ClipGrouping::Week) => {
                    let day_number = super::sessions::day_number(year, month, day);
                    (0, day_number - days_into_week(day_number))
                }
                (Some((year, month, _, _)), ClipGrouping::Month) => (1, year * 12 + month),
                (None, _) => (2, index as i64),
            },
        )
        .collect();

    let mut placed: Vec<ClipPlacement> = Vec::with_capacity(stamps.len());
    // The first line of the group being filled, and how many headings stand
    // above its tiles -- its own included.
    let mut group_line = 0;
    let mut headings = 0;
    let mut start = 0;

    while start < stamps.len() {
        let key = keys[start];
        let end = keys[start..]
            .iter()
            .position(|other| *other != key)
            .map_or(stamps.len(), |offset| start + offset);
        headings += 1;

        // One `columns`-wide row of flags per grid line the group has reached,
        // grown on demand. Confined to this group, which is what keeps a
        // footprint from straddling a date heading: it starts empty, so nothing
        // placed in it can reach the lines above.
        let mut occupied: Vec<Vec<bool>> = Vec::new();
        // Reserved first, so the rest of the group fills what is left rather
        // than the other way round.
        let anchor = expanded
            .filter(|index| (start..end).contains(index))
            .map(|index| {
                let plain = index - start;
                // Its line in the unexpanded list, and its own column pulled
                // left only as far as the footprint needs to fit.
                let (row, column) = (plain / columns, (plain % columns).min(columns - span));
                for line in grow(&mut occupied, columns, row + span)
                    .iter_mut()
                    .skip(row)
                    .take(span)
                {
                    for cell in line.iter_mut().skip(column).take(span) {
                        *cell = true;
                    }
                }
                (index, row, column)
            });

        // How many lines the group actually used -- the footprint's own height
        // included, which is why the next group cannot be started from the last
        // tile's line.
        let mut rows_used = 0;

        for (index, stamp) in stamps.iter().enumerate().take(end).skip(start) {
            let (footprint, row, column) = match anchor {
                Some((expanded_index, row, column)) if expanded_index == index => {
                    (span, row, column)
                }
                _ => {
                    let (row, column) = take_slot(&mut occupied, columns);
                    (1, row, column)
                }
            };
            rows_used = rows_used.max(row + footprint);

            let heading = if index == start {
                group_heading(locale, grouping, stamp, now_week_start)
            } else {
                String::new()
            };

            placed.push(ClipPlacement {
                column: column as i32,
                line: group_line as i32 + row as i32,
                heading_offset: headings,
                heading,
                span: footprint as i32,
            });
        }

        // A heading owns the full width, so the next group starts a fresh line
        // below every line this one filled, footprint and all.
        group_line += rows_used;
        start = end;
    }
    placed
}

/// `occupied`, grown to at least `rows` lines of `columns` flags.
fn grow(occupied: &mut Vec<Vec<bool>>, columns: usize, rows: usize) -> &mut Vec<Vec<bool>> {
    while occupied.len() < rows {
        occupied.push(vec![false; columns]);
    }
    occupied
}

/// The full-width heading a group is introduced by, off its first clip's stamp.
///
/// `now_week_start` is the Monday of the week `now` falls in, or `None` when
/// `now` itself would not parse -- which costs only the relative wording: week
/// groups fall back to naming their month, which is still true.
fn group_heading(
    locale: Locale,
    grouping: ClipGrouping,
    stamp: &str,
    now_week_start: Option<i64>,
) -> String {
    match (super::sessions::split_stamp(stamp), grouping) {
        (Some((year, month, day, _)), ClipGrouping::Week) => match now_week_start {
            Some(week_start) => {
                let day_number = super::sessions::day_number(year, month, day);
                // Clamped: a file copied in with a future modification time (or
                // a machine whose clock moved back) would otherwise be headed
                // 「-1週間前」.
                week_heading(
                    locale,
                    ((week_start - (day_number - days_into_week(day_number))) / 7).max(0),
                )
            }
            None => month_heading(locale, year, month),
        },
        (Some((year, month, _, _)), ClipGrouping::Month) => month_heading(locale, year, month),
        // Never empty: empty is how the `.slint` side spells "no heading here".
        (None, _) if stamp.is_empty() => "—".to_owned(),
        (None, _) => stamp.to_owned(),
    }
}

/// Which clips the search box leaves on the grid, as indices into the list it
/// was handed (task3820, round20 §9).
///
/// Indices rather than a `bool` per clip, because every caller needs the same
/// two things out of it: the surviving clips *in order* (to lay out and to
/// count), and the way back from a grid position to the entry behind it. A
/// predicate would leave that second mapping to be rebuilt at each of the four
/// call sites that resolve a tile index -- one of which sends the delete.
///
/// `clips` is `(file name, comment)`. The comment is an `Option` because it is
/// read out of the file itself, one round trip per clip, and a clip whose read
/// has not landed yet has no comment *known* rather than no comment written --
/// it simply matches on its name until it does. (The caller's answer to that is
/// to ask for every clip's details while a query is up, not to guess here.)
///
/// Case-insensitive both ways, and the query's surrounding whitespace is
/// dropped -- a query of nothing but spaces is the same as no query, which is
/// every clip. `to_lowercase` is identity on Japanese, which is what makes one
/// rule enough for both locales.
pub fn filter<'a>(
    query: &str,
    clips: impl IntoIterator<Item = (&'a str, Option<&'a str>)>,
) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    clips
        .into_iter()
        .enumerate()
        .filter(|(_, (name, comment))| {
            needle.is_empty()
                || name.to_lowercase().contains(&needle)
                || comment.is_some_and(|text| text.to_lowercase().contains(&needle))
        })
        .map(|(index, _)| index)
        .collect()
}

/// The clip list's open comment editor (task2990).
///
/// Holds the *path*, not only the row. The list is a directory re-scan with no
/// index behind it, so a row number means nothing across a scan: between
/// opening the editor and pressing Enter the folder can gain or lose a file and
/// slide every index. The commit and the delete both resolve their target from
/// `path` for the same reason CLAUDE.md makes a destructive click re-check its
/// row first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipEdit {
    /// Which row the `.slint` side draws the editor on.
    pub row: usize,
    /// The file that row pointed at when the editor opened.
    pub path: std::path::PathBuf,
    /// The text the field was *seeded* with, which is what "unchanged" has to
    /// be measured against.
    ///
    /// Not the row's current comment: a row whose details have not landed yet
    /// opens its editor on an empty field, and the comment can arrive from the
    /// worker while that field is open. Measured against the live value, an
    /// editor opened on a cold row and closed without a keystroke would then
    /// read as "emptied" and write `""` over a comment the user never saw.
    pub original: String,
}

/// Whether an open editor still belongs to the row it was opened on.
///
/// `false` once the file was deleted, renamed, or merely shuffled by a newer
/// clip landing above it -- in every one of those the editor is now drawn over
/// a different file, and the only safe answer is to close it.
pub fn edit_survives(edit: &ClipEdit, paths: &[std::path::PathBuf]) -> bool {
    paths.get(edit.row) == Some(&edit.path)
}

/// Whether committing this draft has anything to write.
///
/// `write_comment` opens the file read-write and commits the property store,
/// which moves its modification time -- and that is the key the row's detail
/// cache is filed under. An editor opened and closed unchanged must therefore
/// write nothing. Clearing a comment *is* a change: an empty draft over an
/// existing comment writes `""`, which is how the property is removed
/// (measured, task2960).
pub fn needs_write(draft: &str, current: Option<&str>) -> bool {
    draft != current.unwrap_or_default()
}

/// A comment that has been committed but not yet read back off the file
/// (t260916-568d).
///
/// The clip screen's detail cache is filed under the file's modification time,
/// and writing a comment moves that time -- so between the commit and the
/// re-read the row has no details it is allowed to use, and the tile drew an
/// empty comment over a discarded thumbnail for as long as the round trip took
/// (measured before this task: 4 frames of bare ground on the worker route,
/// ~550ms of the *old* comment on the playing route). The guess closes that
/// gap: the text the user just typed is what the tile shows until the file
/// itself answers.
///
/// `at` is the modification time the file had **when the commit was sent**,
/// which is what says whether an arriving read has seen the write yet. It must
/// not be confused with the time the read was filed under: `Clips::detailed`
/// deliberately files an in-flight answer under the *pre*-write time
/// (task2990), and such an answer has not seen the write either -- so it leaves
/// the guess standing rather than replacing it with the comment it just missed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingComment {
    /// The text as committed. Empty means the comment was cleared, which is a
    /// value and not an absence (`needs_write` treats it as a change).
    pub text: String,
    /// The file's modification time at the moment of the commit.
    pub at: std::time::SystemTime,
}

impl PendingComment {
    /// Whether a detail read filed under `read_at` replaces this guess.
    ///
    /// Only a read filed under a *different* modification time can have seen
    /// the write, because the write is what moved that time.
    pub fn superseded_by(&self, read_at: std::time::SystemTime) -> bool {
        read_at != self.at
    }
}

/// The comment a tile draws: the guess while one stands, otherwise whatever the
/// cache holds for that path -- however old, because "the cache is behind the
/// file" is an update in flight, not an absence.
pub fn tile_comment<'a>(pending: Option<&'a str>, cached: Option<&'a str>) -> Option<&'a str> {
    pending.or(cached)
}

// ---------- the selection (t260916-5568) ----------

/// The marquee's rectangle type, shared with the history screen rather than
/// copied: `overlaps` is Explorer's any-overlap rule (task079), and a second
/// copy would be a second place to get it wrong.
pub use super::sessions::Rect;

/// `ClipsPane`'s grid constants in `clips.slint` -- `pad`, `pad-top`, `gap-x`,
/// `gap-y` and `heading-pitch` -- mirrored for the marquee's hit test, the way
/// `COLUMNS` mirrors `columns`. The cell size is *not* mirrored: it is derived
/// from the pane's width, so the `.slint` side sends it with every press.
pub const GRID_PAD: f32 = 32.0;
pub const GRID_PAD_TOP: f32 = 0.0;
pub const GRID_GAP_X: f32 = 16.0;
pub const GRID_GAP_Y: f32 = 16.0;
pub const GRID_HEADING_PITCH: f32 = 36.0;

/// How far a press on a tile has to travel, on either axis, before it is a
/// marquee rather than a click (design 1). No shared value existed to borrow:
/// the history grid starts its marquee on the press itself, because a click
/// there means "select" and costs nothing to start early.
pub const DRAG_THRESHOLD: f32 = 4.0;

pub fn past_drag_threshold(from: (f32, f32), to: (f32, f32)) -> bool {
    (to.0 - from.0).abs() > DRAG_THRESHOLD || (to.1 - from.1).abs() > DRAG_THRESHOLD
}

/// Every placed tile's rectangle in the grid's content space: the same
/// arithmetic `clips.slint` lays the cells out with -- `x = pad + column *
/// (cell-width + gap-x)`, `y = pad-top + line * row-pitch + heading-offset *
/// heading-pitch`, and a footprint of `span` cells plus the gaps it swallows.
/// The heading step is why `sessions::grid_rects` cannot be used here, and
/// `span` is why an expanded tile is hit as the 3x3 block it is drawn as.
pub fn tile_rects(placed: &[ClipPlacement], cell_width: f32, cell_height: f32) -> Vec<Rect> {
    let row_pitch = cell_height + GRID_GAP_Y;
    placed
        .iter()
        .map(|place| {
            let span = place.span.max(1) as f32;
            Rect {
                x: GRID_PAD + place.column as f32 * (cell_width + GRID_GAP_X),
                y: GRID_PAD_TOP
                    + place.line as f32 * row_pitch
                    + place.heading_offset as f32 * GRID_HEADING_PITCH,
                width: span * cell_width + (span - 1.0) * GRID_GAP_X,
                height: span * cell_height + (span - 1.0) * GRID_GAP_Y,
            }
        })
        .collect()
}

/// What a click that did not become a marquee asks the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickOutcome {
    /// A plain click: the selection is cleared and the tile expands (plays)
    /// exactly as it did before there was a selection (round20 §6, kept).
    Expand,
    /// Ctrl or Shift: the selection changed, and nothing expands.
    Selected,
}

/// The clip screen's selection (t260916-5568). Generic over the id so the
/// rules can be tested with plain strings; the app keys it by *path*, because
/// the folder is a re-scan with no index behind it and a delete resolves its
/// files from here (`CLAUDE.md`'s wrong-target rule).
///
/// The rules are the history screen's (`SessionsState::press` / `marquee`)
/// with one difference forced by round20 §6: a plain click does not select.
#[derive(Clone, Debug)]
pub struct ClipSelection<T> {
    selected: Vec<T>,
    /// Where a Shift+click range starts: the last tile clicked.
    anchor: Option<T>,
    /// What a marquee in progress adds to -- the selection before it began
    /// when Ctrl was held, nothing otherwise.
    base: Vec<T>,
}

impl<T> Default for ClipSelection<T> {
    fn default() -> Self {
        Self {
            selected: Vec::new(),
            anchor: None,
            base: Vec::new(),
        }
    }
}

impl<T: Clone + PartialEq> ClipSelection<T> {
    pub fn selected(&self) -> &[T] {
        &self.selected
    }

    pub fn is_selected(&self, id: &T) -> bool {
        self.selected.contains(id)
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.base.clear();
    }

    /// A click on `id` that stayed under the drag threshold. `order` is the
    /// grid's own order, which is what a Shift range runs along.
    ///
    /// Shift wins over Ctrl when both are held, as in the history screen.
    pub fn click(&mut self, id: &T, shift: bool, ctrl: bool, order: &[T]) -> ClickOutcome {
        if shift {
            let anchor = self.anchor.clone().filter(|anchor| order.contains(anchor));
            match anchor {
                Some(anchor) if order.contains(id) => {
                    self.selected = range(order, &anchor, id);
                }
                _ => {
                    self.selected = vec![id.clone()];
                    self.anchor = Some(id.clone());
                }
            }
            return ClickOutcome::Selected;
        }
        if ctrl {
            if let Some(position) = self.selected.iter().position(|selected| selected == id) {
                self.selected.remove(position);
            } else {
                self.selected.push(id.clone());
            }
            self.anchor = Some(id.clone());
            return ClickOutcome::Selected;
        }
        self.selected.clear();
        self.anchor = Some(id.clone());
        ClickOutcome::Expand
    }

    /// A marquee begins: from bare ground on the press, from a tile once the
    /// press crosses the threshold. `additive` (Ctrl) keeps what was selected.
    pub fn begin_marquee(&mut self, additive: bool) {
        self.base = if additive {
            self.selected.clone()
        } else {
            Vec::new()
        };
        self.selected = self.base.clone();
    }

    /// One marquee step: the base plus every tile the drag rectangle touches.
    pub fn marquee(&mut self, rows: &[(T, Rect)], drag: Rect) {
        let mut next = self.base.clone();
        for (id, rect) in rows {
            if rect.overlaps(&drag) && !next.contains(id) {
                next.push(id.clone());
            }
        }
        self.selected = next;
    }

    /// Ctrl+A: everything on the grid, which under a search is what it matched.
    pub fn select_all(&mut self, order: &[T]) {
        self.selected = order.to_vec();
    }

    /// Drops whatever is no longer on the grid (design 7).
    pub fn retain(&mut self, order: &[T]) {
        self.selected.retain(|id| order.contains(id));
        self.base.retain(|id| order.contains(id));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !order.contains(anchor))
        {
            self.anchor = None;
        }
    }

    /// A right-click on `id` (design 4, the history screen's task1120 rule):
    /// inside the selection it stands, outside it the tile replaces it.
    pub fn select_for_menu(&mut self, id: &T) {
        if !self.is_selected(id) {
            self.selected = vec![id.clone()];
            self.anchor = Some(id.clone());
        }
    }

    /// What a menu opened over `id` acts on: the whole selection when `id` is
    /// part of one of two or more, `id` alone otherwise.
    pub fn menu_targets(&self, id: &T) -> Vec<T> {
        if self.selected.len() > 1 && self.is_selected(id) {
            self.selected.clone()
        } else {
            vec![id.clone()]
        }
    }
}

/// `from..=to` along `order`, whichever way round they are.
fn range<T: Clone + PartialEq>(order: &[T], from: &T, to: &T) -> Vec<T> {
    let from = order.iter().position(|id| id == from);
    let to = order.iter().position(|id| id == to);
    match (from, to) {
        (Some(from), Some(to)) => order[from.min(to)..=from.max(to)].to_vec(),
        (_, Some(to)) => vec![order[to].clone()],
        _ => Vec::new(),
    }
}

/// The search found nothing (task3820). Kept apart from `empty` because the
/// toolbar is drawn on only one of them -- on one property, the search field
/// would vanish the moment a query stopped matching. t260927-bb89 names the
/// query back, as the DS ClipsScreen and the history's `search_empty_text` do.
pub fn filter_empty(locale: Locale, query: &str) -> String {
    let query = query.trim();
    match locale {
        Locale::Ja => format!("「{query}」に合うクリップはありません"),
        Locale::En => format!("No clips match \u{201c}{query}\u{201d}"),
    }
}

/// The selection bar's danger-quiet hold button (DS ClipsScreen).
pub fn delete_selection_label(locale: Locale, count: usize) -> String {
    match locale {
        Locale::Ja => format!("{count} 件を削除"),
        Locale::En => format!("Delete {count}"),
    }
}

/// The toast after deleting more than one clip; one clip keeps `deleted`.
/// A title and a detail line, the history's `discarded_text` shape
/// (t260928-6080 F8, DS: 「2 件のクリップを削除しました / ごみ箱に移しました（84 MB）。」).
pub fn bulk_deleted(locale: Locale, count: usize, formatted: &str) -> (String, String) {
    match locale {
        Locale::Ja => (
            format!("{count} 件のクリップを削除しました"),
            format!("ごみ箱に移しました（{formatted}）。"),
        ),
        Locale::En => (
            format!("Deleted {count} clips"),
            format!("Moved to the recycle bin ({formatted})."),
        ),
    }
}

/// The selection bar's count (t260928-6080 F7, DS ClipsScreen): 「N 件を選択」
/// and no size -- a clip is a picture to look back at, not bytes to budget
/// (t260927-bb89's decision 7). The history's `selection_text` carries one.
/// The bar's two halves, as the history's (t260928-13e9).
pub fn selection_text(locale: Locale, count: usize) -> (String, String) {
    match locale {
        Locale::Ja => (format!("{count} 件"), "を選択".to_owned()),
        Locale::En => (
            format!("{count} {}", if count == 1 { "clip" } else { "clips" }),
            " selected".to_owned(),
        ),
    }
}

/// The app a clip was saved from, read back off its file name (t260927-bb89):
/// the export names it `{game}_{YYYYMMDD-HHMMSS}.mp4`, with `_NN` / `_partNN`
/// after the stamp when a name was taken (`export::plan`). The clip itself
/// records no executable, so this is all the card's monogram has. `None` for
/// a name that does not follow the rule (a file dropped in by hand).
pub fn app_from_file_name(name: &str) -> Option<&str> {
    let stem = name.len().checked_sub(4).and_then(|cut| {
        name.get(cut..)
            .filter(|ext| ext.eq_ignore_ascii_case(".mp4"))
            .map(|_| &name[..cut])
    })?;
    // The last `_` that opens a stamp: a game name may carry `_` itself.
    stem.match_indices('_').rev().find_map(|(at, _)| {
        let rest = &stem.as_bytes()[at + 1..];
        let stamp = rest.get(..15)?;
        let is_stamp = stamp[8] == b'-'
            && stamp[..8].iter().chain(&stamp[9..]).all(u8::is_ascii_digit)
            && rest.get(15).is_none_or(|next| *next == b'_');
        (is_stamp && at > 0).then(|| &stem[..at])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
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
            filter_empty(Locale::Ja, " 戦車 "),
            "「戦車」に合うクリップはありません"
        );
        assert_eq!(
            filter_empty(Locale::En, "tank"),
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

    fn stamps(list: &[&str]) -> Vec<String> {
        list.iter().map(|text| (*text).to_owned()).collect()
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

    fn headings(placed: &[ClipPlacement]) -> Vec<&str> {
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
            &stamps(&["2026/08/31 00:10:00", "2026/08/30 23:50:00"]),
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
            &stamps(&["2026/08/17 12:00:00", "2026/07/06 12:00:00"]),
            NOW,
            4,
        );
        assert_eq!(headings(&placed), vec!["2 週間前", "8 週間前"]);

        let english = place_plain(
            Locale::En,
            ClipGrouping::Week,
            &stamps(&[
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
            &stamps(&[
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
            &stamps(&["2026/09/01 00:05:00"]),
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
            &stamps(&["capture-0000", "2026/09/06 12:00:00", "capture-0001"]),
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
            &stamps(&["2026/09/06 12:00:00", "2026/08/30 12:00:00"]),
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
            &stamps(&ONE_WEEK),
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
            place_plain(Locale::Ja, ClipGrouping::Week, &stamps(&ONE_WEEK), NOW, 4)
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
            &stamps(&ONE_WEEK),
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
            &stamps(&ONE_WEEK[..4]),
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
            &stamps(&ONE_WEEK),
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
            &stamps(&ONE_WEEK),
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
            &stamps(&ONE_WEEK[..3]),
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
            &stamps(&[
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

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    fn picked(selection: &ClipSelection<String>) -> Vec<&str> {
        selection.selected().iter().map(String::as_str).collect()
    }

    /// A plain click keeps round20 §6: it plays, and the pile goes.
    #[test]
    fn a_plain_click_clears_the_selection_and_asks_for_the_expansion() {
        let order = ids(&["a", "b", "c"]);
        let mut selection = ClipSelection::default();
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
        let order = ids(&["a", "b", "c"]);
        let mut selection = ClipSelection::default();
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
        let order = ids(&["a", "b", "c", "d", "e"]);
        let mut selection = ClipSelection::default();
        selection.click(&"b".to_owned(), false, true, &order);
        assert_eq!(
            selection.click(&"d".to_owned(), true, false, &order),
            ClickOutcome::Selected
        );
        assert_eq!(picked(&selection), ["b", "c", "d"]);
        selection.click(&"a".to_owned(), true, false, &order);
        assert_eq!(picked(&selection), ["a", "b"]);
        // No anchor yet: the tile alone, which then becomes the anchor.
        let mut fresh = ClipSelection::default();
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
            &stamps(&["2026/09/06 20:00:00", "2026/08/20 20:00:00"]),
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
        // tiles are offered to `ClipSelection::marquee` is decided by
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
        let order = ids(&["a", "b", "c"]);
        let mut selection = ClipSelection::default();
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
        let all = ids(&["a", "b", "c"]);
        let mut selection = ClipSelection::default();
        selection.click(&"a".to_owned(), false, true, &all);
        selection.click(&"b".to_owned(), false, true, &all);
        let shown = ids(&["a", "c"]);
        selection.retain(&shown);
        assert_eq!(picked(&selection), ["a"]);
        // The anchor `b` went too: a Shift+click now starts fresh.
        selection.click(&"c".to_owned(), true, false, &shown);
        assert_eq!(picked(&selection), ["c"]);
    }

    #[test]
    fn a_menu_acts_on_the_selection_only_from_inside_it() {
        let order = ids(&["a", "b", "c"]);
        let mut selection = ClipSelection::default();
        selection.select_all(&order[..2]);
        assert_eq!(selection.menu_targets(&"a".to_owned()), ids(&["a", "b"]));
        selection.select_for_menu(&"c".to_owned());
        assert_eq!(picked(&selection), ["c"]);
        assert_eq!(selection.menu_targets(&"c".to_owned()), ids(&["c"]));
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
                    let first = first_visible_index(&placed, first_line, 4);
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
                let first = first_visible_index(&placed, first_line, 4);
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
}
