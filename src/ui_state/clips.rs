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
use crate::ui_state::sessions::{days_into_week, no_match_text, selection_count, week_heading};

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
    /// and neither does `search_empty_text`; the full stop in the design answer's
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
    /// Context-menu entries.
    /// task3020 shortened both: the menu sits over a tile now, and the two
    /// entries read as a pair of verbs rather than a pair of sentences.
    menu_open { ja: "再生", en: "Play" }

    /// The bottom overlay of a tile with no comment written yet (task3020).
    /// A field, not a label: the pencil beside it opens the editor.
    add_comment { ja: "コメントを追加", en: "Add a comment" }
    /// The expanded player's comment editor: its ✓ button (2026-10-10).
    save_comment { ja: "保存", en: "Save" }
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
        ja: "多くの場合、再生中など他のアプリがこのファイルを開いているためです。",
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
pub fn first_visible_index(placed: &[ClipPlacement], first_line: usize) -> usize {
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
    let now_week_start = super::sessions::split_stamp(now).map(|(year, month, day, _)| {
        let today = super::sessions::day_number(year, month, day);
        today - days_into_week(today)
    });
    // `(kind, value)`: two groupings must not collide on a shared number, and
    // an unparseable stamp must not join the group above it.
    let key = |index: usize| match (super::sessions::split_stamp(&stamps[index]), grouping) {
        (Some((year, month, day, _)), ClipGrouping::Week) => {
            let day_number = super::sessions::day_number(year, month, day);
            (0, day_number - days_into_week(day_number))
        }
        (Some((year, month, _, _)), ClipGrouping::Month) => (1, year * 12 + month),
        (None, _) => (2, index as i64),
    };
    let heading = |index: usize| group_heading(locale, grouping, &stamps[index], now_week_start);
    place_groups(stamps.len(), columns, expanded, span, key, heading)
}

/// The placement arithmetic both grids share (t261025-67ce): the clip grid
/// through [`place`], the history through `sessions::place`. `count` entries
/// in reading order; `key(index)` is the group an entry belongs to (equal keys
/// next to each other share one heading) and `heading(index)` the text over
/// the group it opens, asked only of each group's first entry. `expanded` and
/// `span` are [`place`]'s; the history passes `None` and 1.
pub fn place_groups(
    count: usize,
    columns: usize,
    expanded: Option<usize>,
    span: usize,
    key: impl Fn(usize) -> (i64, i64),
    heading: impl Fn(usize) -> String,
) -> Vec<ClipPlacement> {
    let columns = columns.max(1);
    // A footprint wider than the grid has no slot to land in, and `take_slot`
    // would spin looking for one.
    let span = span.clamp(1, columns);

    // Which group every entry belongs to, up front: reserving the footprint
    // before the group is filled means knowing where the group *ends* while
    // placing its first tile, and a single forward pass cannot answer that.
    let keys: Vec<(i64, i64)> = (0..count).map(key).collect();

    let mut placed: Vec<ClipPlacement> = Vec::with_capacity(count);
    // The first line of the group being filled, and how many headings stand
    // above its tiles -- its own included.
    let mut group_line = 0;
    let mut headings = 0;
    let mut start = 0;

    while start < count {
        let key = keys[start];
        let end = keys[start..]
            .iter()
            .position(|other| *other != key)
            .map_or(count, |offset| start + offset);
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

        for index in start..end {
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
                heading(index)
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

/// Every placed tile's rectangle in the grid's content space, laid out by
/// `sessions::placed_rects` with `clips.slint`'s own constants.
pub fn tile_rects(placed: &[ClipPlacement], cell_width: f32, cell_height: f32) -> Vec<Rect> {
    let metrics = super::sessions::GridMetrics {
        pad_x: GRID_PAD,
        pad_top: GRID_PAD_TOP,
        gap_x: GRID_GAP_X,
        gap_y: GRID_GAP_Y,
        heading_pitch: GRID_HEADING_PITCH,
    };
    super::sessions::placed_rects(placed, metrics, cell_width, cell_height)
}

/// The search found nothing (task3820). Kept apart from `empty` because the
/// toolbar is drawn on only one of them -- on one property, the search field
/// would vanish the moment a query stopped matching. t260927-bb89 names the
/// query back, as the DS ClipsScreen and the history do (`no_match_text`).
pub fn search_empty_text(locale: Locale, query: &str) -> String {
    no_match_text(locale, "クリップ", "clips", query.trim())
}

crate::trf! {
    /// The selection bar's danger-quiet hold button (DS ClipsScreen).
    delete_selection_label(count) {
        ja: "{count} 件を削除",
        en: "Delete {count}"
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
    let noun = if count == 1 { "clip" } else { "clips" };
    let count_text = selection_count(locale, count, noun);
    match locale {
        Locale::Ja => (count_text, "を選択".to_owned()),
        Locale::En => (count_text, " selected".to_owned()),
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

/// One entry of a clip's context menu.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClipMenuAction {
    Open,
    Reveal,
    EditComment,
    Delete,
}

/// The context menu, in the order it is drawn (task3020 §1-4). The edit and the
/// delete are here rather than on the tile itself (task2990): a clip's plain
/// click already means something -- since task3890 it expands the tile, which
/// is what plays it (round20 design response §6) -- and the history screen
/// removed click-to-edit over the same collision (task184). The overlay's
/// pencil is the one thing on the tile that takes a click of its own, and it
/// sits above `touch` so the press never reaches the expand.
///
/// Four entries, unchanged: round20's 追補便 fixes the list, and since
/// task3530 retired the external-player route 「再生」 means the in-place
/// player -- the same expansion a click on the tile makes.
pub const MENU_ACTIONS: [ClipMenuAction; 4] = [
    ClipMenuAction::Open,
    ClipMenuAction::EditComment,
    ClipMenuAction::Reveal,
    ClipMenuAction::Delete,
];

/// The menu's entries when it acts on `targets` clips (t260916-5568):
/// 「再生」 and 「コメントを編集」 are about one clip and go when the menu acts
/// on a selection (design 4). The render and the click both ask this, so the
/// index the `.slint` side sends back maps through the list it was drawn from.
pub fn menu_actions(targets: usize) -> Vec<ClipMenuAction> {
    MENU_ACTIONS
        .iter()
        .copied()
        .filter(|action| {
            targets <= 1 || matches!(action, ClipMenuAction::Reveal | ClipMenuAction::Delete)
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests;
