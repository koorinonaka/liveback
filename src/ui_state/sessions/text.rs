//! The session screen's UI wording and pure formatting helpers, split from
//! `sessions.rs` (which keeps the selection/sort/group state).

use crate::ui_state::locale::Locale;

crate::tr! {
    // ja.recovery.*
    /// The history's own nothing, as two lines (task3800, 2026-09-08 ruling):
    /// the mock splits the sentence into a title with no full stop and a
    /// second line that says what will fill the screen. `empty_sub` is that
    /// second line; the search's nothing (`search_empty`) has none, because
    /// what fills the screen there is a shorter query, not time passing.
    empty { ja: "履歴がありません", en: "No history yet" }
    empty_sub {
        ja: "録画が終わるとここに並びます。",
        en: "Recordings appear here once they finish."
    }
    error { ja: "セッション一覧を取得できません。", en: "The session list could not be read." }
    /// round7 §2-15: one word for "this is being written right now",
    /// everywhere. The row used to keep its own 録画中 on the grounds that a
    /// file still growing is not the same as a picture on screen -- the round
    /// overruled that: the user sees one state, so it gets one name.
    status_recording { ja: "LIVE", en: "LIVE" }
    /// W-15: the empty note's placeholder, which doubles as the button that
    /// starts editing it (round5 §6).
    note_placeholder { ja: "メモを追加", en: "Add a note" }
    note_error { ja: "メモを保存できませんでした。", en: "The note could not be saved." }
    /// The sub-line of a row nothing could read (task1520 follow-up). It used
    /// to be no row at all, which moved every row below it up one -- under a
    /// right-click menu, that aims the destructive entries at whatever slid
    /// into its place.
    unreadable_row {
        ja: "読み込めません（他のプロセスが使用中かもしれません）",
        en: "Cannot be read (another process may be using it)"
    }
    protect { ja: "保護する", en: "Protect" }
    /// W-17: the same menu entry on a row that is already protected.
    unprotect { ja: "保護を解除", en: "Unprotect" }
    protect_error {
        ja: "保護の設定を変更できませんでした。",
        en: "The protection setting could not be changed."
    }
    /// Warned at the moments that need room -- starting a recording, writing
    /// an export -- because the sweep cannot free any (task167, design §6-C).
    protected_over_limit {
        ja: "保護したセッションだけで保持の上限に達しています。保護を解除するか上限を上げてください。",
        en: "Protected sessions alone have reached the retention limit. Unprotect some, or raise the limit."
    }
    load_error { ja: "読み込みに失敗しました。", en: "Loading failed." }
    load_disabled_while_capturing {
        ja: "録画中に読み込めるのは録画中のセッションだけです。破棄と復旧は行えます。",
        en: "While recording, only the session being recorded can be opened. Discarding and recovery still work."
    }
    discard_confirm_title { ja: "このセッションを破棄しますか？", en: "Discard this session?" }
    discard_confirm { ja: "破棄する", en: "Discard" }
    discard_cancel { ja: "キャンセル", en: "Cancel" }
    /// Says where it goes, because that changed (task1520 follow-up): a hand
    /// discard is recoverable now, and a confirmation that still said "deleted
    /// from disk" would be talking the user out of the safe thing.
    discard_lead {
        ja: "録画ファイルはごみ箱に移動します。",
        en: "The recording will be moved to the recycle bin."
    }
    open_directory { ja: "フォルダで表示", en: "Show in folder" }
    open_directory_error {
        ja: "セッションのフォルダを開けませんでした。",
        en: "That session's folder could not be opened."
    }
    view_mode_list { ja: "リスト", en: "List" }
    view_mode_thumbnail { ja: "サムネイル", en: "Thumbnails" }
    thumbnail_placeholder { ja: "サムネイルなし", en: "No thumbnail" }
    /// Round5 §6-B: the history gained a search box of its own, matching the
    /// picker's. Title substring only -- there is nothing else on a row to
    /// match.
    search_placeholder { ja: "セッションを検索", en: "Search sessions" }
    search_empty { ja: "一致するセッションはありません。", en: "No sessions match." }
    /// Context-menu entries (round5 §6-D). The load entry names the
    /// destination rather than the verb alone: from a list of recordings,
    /// "読み込む" leaves open where it goes.
    menu_load { ja: "確認で読み込む", en: "Open in review" }
    menu_edit_note { ja: "メモを編集", en: "Edit note" }
    menu_discard { ja: "破棄", en: "Discard" }
    /// The right-hand key hints on those entries.
    hint_double_click { ja: "Dbl", en: "Dbl" }
    hint_edit_note { ja: "F2", en: "F2" }
    hint_discard { ja: "Delete", en: "Delete" }
    /// `row_date_text`'s middle case.
    yesterday { ja: "昨日", en: "Yesterday" }
}

/// The history footer's right-hand tally (task242): how many rows are on
/// screen and what they cost on disk. It moved out of the toolbar, where the
/// sentence competed with the search field for width and pushed the view
/// toggle around whenever the number grew a digit.
pub fn footer_visible_text(locale: Locale, shown: usize, bytes: u64) -> String {
    let size = format_bytes(bytes);
    match locale {
        Locale::Ja => format!("{shown} 件を表示（{size}）"),
        Locale::En => format!("{shown} shown ({size})"),
    }
}

/// The footer's left-hand half, or `None` with nothing selected -- an empty
/// selection has no size worth a line of its own.
pub fn footer_selected_text(locale: Locale, count: usize, bytes: u64) -> Option<String> {
    (count > 0).then(|| {
        let size = format_bytes(bytes);
        match locale {
            Locale::Ja => format!("{count} 件を選択（{size}）"),
            Locale::En => format!("{count} selected ({size})"),
        }
    })
}

pub fn bulk_discard_confirm_title(locale: Locale, count: usize) -> String {
    match locale {
        Locale::Ja => format!("{count} 件のセッションを破棄しますか？"),
        Locale::En => format!("Discard {count} sessions?"),
    }
}

pub fn bulk_discard_confirm(locale: Locale, count: usize) -> String {
    match locale {
        Locale::Ja => format!("{count} 件を破棄する"),
        Locale::En => format!("Discard {count}"),
    }
}

/// One muted sentence, and the only one (task184). The separate red
/// 「元に戻せません。」 line it used to sit above is gone: the same warning said
/// twice, once in error red, made a routine discard look like a failure.
///
/// It no longer claims either half of what it used to (task1520 follow-up): a
/// hand discard goes to the recycle bin, so it *can* be undone -- and the disk
/// does not come back until the bin is emptied, which is the part a user
/// discarding to make room has to know before they walk away satisfied.
pub fn bulk_discard_lead(locale: Locale, formatted: &str) -> String {
    match locale {
        Locale::Ja => {
            format!("合計 {formatted} をごみ箱に移動します。ごみ箱を空にするまでディスクは解放されません。")
        }
        Locale::En => {
            format!("{formatted} will be moved to the recycle bin. The space comes back when the bin is emptied.")
        }
    }
}

/// Same correction as `bulk_discard_lead`: nothing was freed yet, it was moved
/// (task1520 follow-up).
pub fn discarded_text(locale: Locale, count: usize, formatted: &str) -> String {
    match locale {
        Locale::Ja => format!("{count} 件をごみ箱に移動しました（{formatted}）。"),
        Locale::En => format!("Moved {count} to the recycle bin ({formatted})."),
    }
}

/// What the user is told when the capacity budget deleted recordings behind
/// their back (task700). Names the cause and the setting, because the whole
/// complaint was that two sessions vanished with nothing anywhere saying why.
pub fn capacity_reclaimed_text(locale: Locale, count: u32) -> String {
    match locale {
        Locale::Ja => {
            format!("保存領域の上限に達したため、古いセッション {count} 件を自動的に削除しました")
        }
        Locale::En => {
            format!("The storage limit was reached, so {count} old sessions were deleted")
        }
    }
}

/// The second line (round7 §2-11): what it freed, what it spared, and that it
/// cannot be taken back. Separate from the headline because the toast puts the
/// two in different colours -- and because the headline has to stay readable
/// when the detail elides.
pub fn capacity_reclaimed_detail(locale: Locale, formatted: &str) -> String {
    match locale {
        Locale::Ja => {
            format!("{formatted} を解放 · 保護したセッションは対象外 · この操作は取り消せません")
        }
        Locale::En => {
            format!("{formatted} freed · protected sessions spared · cannot be undone")
        }
    }
}

/// A partial run reports what it freed as well as what failed: the point of the
/// whole screen is reclaiming disk (task116).
pub fn bulk_discard_partial_error(
    locale: Locale,
    total: usize,
    failed: usize,
    formatted: &str,
) -> String {
    match locale {
        Locale::Ja => format!(
            "{total}件中{failed}件の破棄に失敗しました。失敗したセッションは選択に残っています。{formatted} を解放しました。"
        ),
        Locale::En => format!(
            "{failed} of {total} could not be discarded and are still selected. {formatted} freed."
        ),
    }
}

pub fn loaded_text(locale: Locale, label: &str) -> String {
    match locale {
        Locale::Ja => format!("{label} を読み込みました。"),
        Locale::En => format!("Opened {label}."),
    }
}

pub fn total_size_text(locale: Locale, formatted: &str) -> String {
    match locale {
        Locale::Ja => format!("合計{formatted}"),
        Locale::En => format!("{formatted} total"),
    }
}

/// Days since 1970-03-01 for a proleptic Gregorian date. Howard Hinnant's
/// `days_from_civil`, which is the whole calendar in six lines -- month lengths
/// and leap years included -- and is why "was that yesterday?" needs no date
/// crate (task243).
pub(crate) fn day_number(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era
}

/// `YYYY/MM/DD HH:MM:SS` -> (year, month, day, `HH:MM`).
pub(crate) fn split_stamp(formatted: &str) -> Option<(i64, i64, i64, String)> {
    let (date, time) = formatted.split_once(' ')?;
    let date: Vec<&str> = date.split('/').collect();
    let clock: Vec<&str> = time.split(':').collect();
    if date.len() != 3 || clock.len() < 2 {
        return None;
    }
    Some((
        date[0].parse().ok()?,
        date[1].parse().ok()?,
        date[2].parse().ok()?,
        format!("{}:{}", clock[0], clock[1]),
    ))
}

/// The list row's timestamp column (task243): `21:04` for today, `昨日 23:41`
/// for yesterday, `8/13 22:05` for anything older. No year -- two recordings
/// are never told apart by it at this size.
///
/// `now` arrives in the same `YYYY/MM/DD HH:MM:SS` shape the caller already
/// has rather than being read here, so the rule is a pure function with a test
/// instead of something only a clock can answer. Anything unparseable is
/// passed through, the same contract `tile_date_text` keeps: the caller falls
/// back to a raw session id when the clock conversion fails, and half-parsing
/// that is worse than showing it.
pub fn row_date_text(locale: Locale, formatted: &str, now: &str) -> String {
    let (Some((year, month, day, clock)), Some((now_year, now_month, now_day, _))) =
        (split_stamp(formatted), split_stamp(now))
    else {
        return formatted.to_owned();
    };
    match day_number(now_year, now_month, now_day) - day_number(year, month, day) {
        0 => clock,
        1 => format!("{} {clock}", yesterday(locale)),
        _ => format!("{month}/{day} {clock}"),
    }
}

/// The thumbnail tile's caption date (round5 §6-A, task184): the same
/// timestamp the list row shows, cut to `8/13 22:05`. The tile has one short
/// line under a picture, so the year and the seconds -- neither of which tells
/// two recordings apart at a glance -- come off. Anything that is not the
/// expected `YYYY/MM/DD HH:MM:SS` shape is passed through untouched, because
/// the caller falls back to the raw session id when the clock conversion
/// fails and a half-parsed id would be worse than the id.
pub fn tile_date_text(formatted: &str) -> String {
    let Some((date, time)) = formatted.split_once(' ') else {
        return formatted.to_owned();
    };
    let parts: Vec<&str> = date.split('/').collect();
    let clock: Vec<&str> = time.split(':').collect();
    if parts.len() != 3 || clock.len() < 2 {
        return formatted.to_owned();
    }
    let month = parts[1].trim_start_matches('0');
    let day = parts[2].trim_start_matches('0');
    if month.is_empty() || day.is_empty() {
        return formatted.to_owned();
    }
    format!("{month}/{day} {}:{}", clock[0], clock[1])
}

/// `formatBytes` from `src/lib/format.ts`: binary units, one decimal above KB,
/// and no unit past GB -- a buffer that large is not a size the screen has to
/// abbreviate further.
pub fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}
