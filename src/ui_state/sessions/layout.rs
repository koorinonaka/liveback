//! The history grid's placement under date headings, and the app column's
//! derived bits (t260927-9a17). Split from `sessions.rs`, which keeps the
//! selection state.

use super::{
    day_number, days_into_week, split_stamp, today, week_heading, yesterday, Rect, ViewMode,
};
use crate::ui_state::clips::{place_groups, ClipPlacement};
use crate::ui_state::locale::Locale;

/// Which date group a start stamp falls in, and that group's heading.
///
/// 今日 / 昨日 first, then the clip screen's weeks (今週 / 先週 / N 週間前).
/// A stamp in the future (a clock moved back) counts as today. An unparseable
/// `now` costs only the relative wording: the group is the calendar day. An
/// unparseable stamp is its own group headed by the raw text -- the caller falls
/// back to the raw session id when the clock conversion fails.
fn group_of(locale: Locale, stamp: &str, now: &str, index: usize) -> ((i64, i64), String) {
    let Some((year, month, date, _)) = split_stamp(stamp) else {
        let heading = if stamp.is_empty() { "—" } else { stamp };
        return ((3, index as i64), heading.to_owned());
    };
    let day = day_number(year, month, date);
    let Some((now_year, now_month, now_day, _)) = split_stamp(now) else {
        return ((2, day), format!("{year}/{month}/{date}"));
    };
    let today_number = day_number(now_year, now_month, now_day);
    match today_number - day {
        diff if diff <= 0 => ((0, 0), today(locale).to_owned()),
        1 => ((0, 1), yesterday(locale).to_owned()),
        _ => {
            let weeks =
                ((today_number - days_into_week(today_number)) - (day - days_into_week(day))) / 7;
            ((1, weeks), week_heading(locale, weeks.max(0)))
        }
    }
}

/// Lays the history out under its date headings (t260927-9a17): `columns`
/// wide (1 for the list), each group starting a fresh line. `stamps` are the
/// sessions' start times in the `YYYY/MM/DD HH:MM:SS` shape, newest first.
///
/// The same four numbers the clip grid uses (`ClipPlacement`, `span` always
/// 1): a tile's y is `line * row-pitch + heading_offset * heading-pitch`.
pub fn place(locale: Locale, stamps: &[String], now: &str, columns: usize) -> Vec<ClipPlacement> {
    place_groups(
        stamps.len(),
        columns,
        None,
        1,
        |index| group_of(locale, &stamps[index], now, index).0,
        |index| group_of(locale, &stamps[index], now, index).1,
    )
}

/// The grid's pixel constants, the same ones `sessions.slint` lays out with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridMetrics {
    pub pad_x: f32,
    pub pad_top: f32,
    pub gap_x: f32,
    pub gap_y: f32,
    pub heading_pitch: f32,
}

/// The history grid's pixel constants, the ones `sessions.slint` lays out
/// with: 24 each side for the list and 28 for the tiles, rows 2 apart and
/// tiles 8 / 12, and a date heading 36 tall *including* the row gap before it
/// (t260928-23aa): the grid starts one gap down, so every heading -- the first
/// one too, in either view -- sits 16 below the row above it and 4 above its
/// own first row.
pub fn grid_metrics(view: ViewMode) -> GridMetrics {
    let tiles = view == ViewMode::Thumbnail;
    let gap_y = if tiles { 8.0 } else { 2.0 };
    GridMetrics {
        pad_x: if tiles { 28.0 } else { 24.0 },
        pad_top: gap_y,
        gap_x: 12.0,
        gap_y,
        heading_pitch: 36.0 - gap_y,
    }
}

/// Every placed tile's rectangle in the grid's content space: the same
/// arithmetic the `.slint` grids lay the cells out with -- `x = pad + column *
/// (cell-width + gap-x)`, `y = pad-top + line * row-pitch + heading-offset *
/// heading-pitch`, and a footprint of `span` cells plus the gaps it swallows
/// (the clip grid's expanded tile is hit as the 3x3 block it is drawn as; a
/// history session always spans 1).
pub fn placed_rects(
    placed: &[ClipPlacement],
    metrics: GridMetrics,
    cell_width: f32,
    cell_height: f32,
) -> Vec<Rect> {
    placed
        .iter()
        .map(|place| {
            let span = place.span.max(1) as f32;
            Rect {
                x: metrics.pad_x + place.column as f32 * (cell_width + metrics.gap_x),
                y: metrics.pad_top
                    + place.line as f32 * (cell_height + metrics.gap_y)
                    + place.heading_offset as f32 * metrics.heading_pitch,
                width: span * cell_width + (span - 1.0) * metrics.gap_x,
                height: span * cell_height + (span - 1.0) * metrics.gap_y,
            }
        })
        .collect()
}

/// The app column's name: the executable without its extension
/// (`obs64.exe` -> `obs64`). `None` for a monitor recording.
pub fn app_name(executable: Option<&str>) -> Option<String> {
    let executable = executable?.trim();
    let file = executable.rsplit(['\\', '/']).next().unwrap_or(executable);
    let stem = match file.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => file,
    };
    (!stem.is_empty()).then(|| stem.to_owned())
}

/// Which of the DS `AppIcon`'s eight hues a monogram takes: the DS hashes the
/// name (`hash * 31 + code unit`, wrapping at 32 bits) and takes it mod 8.
pub fn monogram_hue(name: &str) -> i32 {
    // `for (const ch of name)` walks code points and `charCodeAt(0)` reads each
    // one's first UTF-16 unit.
    let hash = name.chars().fold(0u32, |hash, ch| {
        let mut units = [0u16; 2];
        hash.wrapping_mul(31)
            .wrapping_add(u32::from(ch.encode_utf16(&mut units)[0]))
    });
    (hash % 8) as i32
}

/// The monogram's letter: the first character, upper-cased.
pub fn monogram_letter(name: &str) -> String {
    name.trim()
        .chars()
        .next()
        .map_or_else(|| "?".to_owned(), |first| first.to_uppercase().collect())
}
