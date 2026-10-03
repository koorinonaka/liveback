//! The history grid's placement under date headings, and the app column's
//! derived bits (t260927-9a17). Split from `sessions.rs`, which keeps the
//! selection state.

use super::{day_number, split_stamp, today, yesterday, Rect};
use crate::ui_state::clips::ClipPlacement;
use crate::ui_state::locale::Locale;

/// Days since the Monday that starts `day`'s week. The same arithmetic as
/// `ui_state::clips`' private copy (1970-01-01, day 719468, was a Thursday).
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
    let columns = columns.max(1);
    let mut placed = Vec::with_capacity(stamps.len());
    let mut key = None;
    let mut headings = 0;
    // The first line of the group being filled, and the position inside it.
    let mut group_line = 0;
    let mut in_group: usize = 0;
    for (index, stamp) in stamps.iter().enumerate() {
        let (this, heading) = group_of(locale, stamp, now, index);
        let opens = key != Some(this);
        if opens {
            if in_group > 0 {
                group_line += in_group.div_ceil(columns);
            }
            in_group = 0;
            headings += 1;
            key = Some(this);
        }
        placed.push(ClipPlacement {
            column: (in_group % columns) as i32,
            line: (group_line + in_group / columns) as i32,
            heading_offset: headings,
            heading: if opens { heading } else { String::new() },
            span: 1,
        });
        in_group += 1;
    }
    placed
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

/// Every placed session's rectangle in the grid's content space.
pub fn placed_rects(
    placed: &[ClipPlacement],
    metrics: GridMetrics,
    cell_width: f32,
    cell_height: f32,
) -> Vec<Rect> {
    placed
        .iter()
        .map(|place| Rect {
            x: metrics.pad_x + place.column as f32 * (cell_width + metrics.gap_x),
            y: metrics.pad_top
                + place.line as f32 * (cell_height + metrics.gap_y)
                + place.heading_offset as f32 * metrics.heading_pitch,
            width: cell_width,
            height: cell_height,
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
