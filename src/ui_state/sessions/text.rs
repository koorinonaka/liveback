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
    ///
    /// t260927-9a17 took the DS HistoryScreen's wording for both lines.
    empty { ja: "履歴はまだありません", en: "No history yet" }
    empty_sub {
        ja: "録画を止めると、ここに並びます。",
        en: "Recordings appear here once you stop them."
    }
    /// A listing that would not read (t260928-23aa, DS HistoryScreen 知らせ方):
    /// the screen's own EmptyState, a title with no full stop and the reason
    /// under it -- not an Alert above a list that may be empty.
    error { ja: "一覧を取得できませんでした", en: "The session list could not be read" }
    error_sub {
        ja: "録画バッファのフォルダを読めません。設定で場所を確かめてください。",
        en: "The recording buffer folder can't be read. Check its location in Settings."
    }
    /// The length column of a session still being written (t260927-9a17, the
    /// DS SessionRow: 「長さの列に「録画中」」). The picture carries REC.
    status_recording { ja: "録画中", en: "Recording" }
    /// W-15: the empty comment's hover text (round5 §6). The UI word is
    /// 「コメント」 since t260927-9a17 (the clip screen already used it); the
    /// identifiers keep `note`.
    note_placeholder { ja: "コメントを追加", en: "Add a comment" }
    note_error { ja: "コメントを保存できませんでした", en: "The comment could not be saved" }
    /// The sub-line of a row nothing could read (task1520 follow-up). It used
    /// to be no row at all, which moved every row below it up one -- under a
    /// right-click menu, that aims the destructive entries at whatever slid
    /// into its place.
    unreadable_row {
        ja: "読み込めません（他のアプリが使っているかもしれません）",
        en: "Cannot be read (another app may be using it)"
    }
    protect { ja: "保護する", en: "Protect" }
    /// W-17: the same menu entry on a row that is already protected.
    unprotect { ja: "保護を解除", en: "Unprotect" }
    protect_error {
        ja: "保護の設定を変更できませんでした",
        en: "The protection setting could not be changed"
    }
    /// The StorageMeter's warning line while protected sessions alone are over
    /// the limit, because the sweep cannot free any (task167, design §6-C).
    /// t260928-e309 F10: names the setting as the settings screen does
    /// (「容量で自動削除」) and says 「セッション」, the DS term.
    protected_over_limit {
        ja: "保護中のセッションだけで上限を超えているため、古いセッションを自動削除できません。保護を解除するか、「容量で自動削除」の上限を上げてください。",
        en: "Protected sessions alone exceed the limit, so old sessions can't be deleted automatically. Unprotect some, or raise \"Auto-delete by size\"."
    }
    load_error { ja: "セッションを読み込めませんでした", en: "The session could not be opened" }
    load_disabled_while_capturing {
        ja: "録画中に読み込めるのは録画中のセッションだけです。破棄と復旧は行えます。",
        en: "While recording, only the session being recorded can be opened. Discarding and recovery still work."
    }
    discard_confirm { ja: "破棄する", en: "Discard" }
    discard_cancel { ja: "キャンセル", en: "Cancel" }
    open_directory { ja: "フォルダで表示", en: "Show in folder" }
    open_directory_error {
        ja: "セッションのフォルダを開けませんでした",
        en: "That session's folder could not be opened"
    }
    view_mode_list { ja: "リスト", en: "List" }
    view_mode_thumbnail { ja: "サムネイル", en: "Thumbnails" }
    thumbnail_placeholder { ja: "サムネイルなし", en: "No thumbnail" }
    /// Round5 §6-B: the history gained a search box of its own. It matches
    /// the window name and the comment (`SessionsState::rendered`).
    search_placeholder { ja: "ウィンドウ名やコメントで検索", en: "Search window names and comments" }
    /// Context-menu entries (round5 §6-D). The load entry names the
    /// destination rather than the verb alone: from a list of recordings,
    /// "読み込む" leaves open where it goes.
    menu_load { ja: "確認で開く", en: "Open in review" }
    menu_edit_note { ja: "コメントを編集", en: "Edit comment" }
    menu_discard { ja: "破棄", en: "Discard" }
    /// The right-hand key hints on those entries. Opening advertises Enter,
    /// the list's own key for it (t260928-23aa); the double-click stays.
    hint_open { ja: "Enter", en: "Enter" }
    hint_edit_note { ja: "F2", en: "F2" }
    hint_discard { ja: "Delete", en: "Delete" }
    /// `row_date_text`'s middle case.
    yesterday { ja: "昨日", en: "Yesterday" }
    /// The history's 今日 heading (`layout.rs`). `row_date_text` left it in
    /// t260928-6080: the clip card prints today as a bare `21:04` (DS).
    today { ja: "今日", en: "Today" }
    /// The list's column headings (DS SessionHeader).
    column_window { ja: "ウィンドウ", en: "Window" }
    column_start { ja: "作成日時", en: "Created" }
    column_length { ja: "長さ", en: "Length" }
    column_size { ja: "サイズ", en: "Size" }
    /// The app column of a recording with no executable: a monitor.
    whole_screen { ja: "画面全体", en: "Entire screen" }
    /// The LIVE row's stop button's tooltip.
    stop_recording { ja: "録画を止める", en: "Stop recording" }
    /// The selection bar (DS HistoryScreen).
    clear_selection { ja: "選択を解除", en: "Clear selection" }
    /// The 「…」 on a tile or row, which draws no word: its accessible name
    /// (t260928-13e9).
    more_actions { ja: "その他の操作", en: "More actions" }
    hold_to_discard { ja: "長押しで破棄", en: "Hold to discard" }
    live_cannot_discard {
        ja: "録画中のセッションは破棄できません",
        en: "Sessions being recorded can't be discarded"
    }
    /// The footer's StorageMeter.
    storage_label { ja: "録画バッファ", en: "Recording buffer" }
    storage_protected { ja: "保護", en: "Protected" }
}

/// The English noun a count of sessions takes (t260928-23aa): "1 session",
/// "2 sessions". Japanese counts with 件 and has no plural.
fn sessions_noun(count: usize) -> &'static str {
    if count == 1 {
        "session"
    } else {
        "sessions"
    }
}

/// The selection bar's own count (t260927-9a17, DS HistoryScreen):
/// 「2 件を選択 · 8.3 GB」.
///
/// Returned as the bar's two halves (t260928-13e9, DS `.lb-selbar`): the
/// count, drawn 600 `ink`, and the rest, drawn flush after it 400
/// `ink-muted` -- so the rest carries its own leading space where the
/// language wants one.
pub fn selection_text(locale: Locale, count: usize, bytes: u64) -> (String, String) {
    let size = format_bytes(bytes);
    match locale {
        Locale::Ja => (format!("{count} 件"), format!("を選択 · {size}")),
        Locale::En => (
            format!("{count} {}", sessions_noun(count)),
            format!(" selected · {size}"),
        ),
    }
}

/// The selection bar's hold-to-discard button.
pub fn discard_selection_label(locale: Locale, count: usize) -> String {
    match locale {
        Locale::Ja => format!("{count} 件を破棄"),
        Locale::En => format!("Discard {count} {}", sessions_noun(count)),
    }
}

/// The footer with no limit set: no track to draw, so one line of numbers.
pub fn storage_unlimited_text(locale: Locale, used: u64, protected: u64) -> String {
    let (used, protected) = (format_bytes(used), format_bytes(protected));
    match locale {
        Locale::Ja => format!("{used} · 保護 {protected} · 上限なし"),
        Locale::En => format!("{used} · {protected} protected · no limit"),
    }
}

/// The search's own nothing, naming the query (DS HistoryScreen).
pub fn search_empty_text(locale: Locale, query: &str) -> String {
    let query = query.trim();
    match locale {
        Locale::Ja => format!("「{query}」に合うセッションはありません"),
        Locale::En => format!("No sessions match \u{201c}{query}\u{201d}"),
    }
}

/// The toast after a discard (t260927-9a17, DS HistoryScreen): nothing was
/// freed yet, it was moved (task1520 follow-up). The DS splits it into a title
/// and a detail line, returned in that order (t260927-73fd).
pub fn discarded_text(locale: Locale, count: usize, formatted: &str) -> (String, String) {
    match locale {
        Locale::Ja => (
            format!("{count} 件のセッションを破棄しました"),
            format!("ごみ箱に移しました（{formatted}）。"),
        ),
        Locale::En => (
            format!("Discarded {count} {}", sessions_noun(count)),
            format!("Moved to the recycle bin ({formatted})."),
        ),
    }
}

/// What the user is told when the capacity budget deleted recordings behind
/// their back (task700): what happened, as the DS word table has it
/// (t260928-23aa). The cause is the second line's.
pub fn capacity_reclaimed_text(locale: Locale, count: u32) -> String {
    match locale {
        Locale::Ja => format!("古いセッションを {count} 件削除しました"),
        Locale::En => format!("Deleted {count} old {}", sessions_noun(count as usize)),
    }
}

/// The second line: why, and how much room it made (DS word table,
/// t260928-23aa; it replaced round7 §2-11's three qualifiers). Separate from
/// the headline because the toast puts the two in different colours -- and
/// because the headline has to stay readable when the detail elides.
pub fn capacity_reclaimed_detail(locale: Locale, formatted: &str) -> String {
    match locale {
        Locale::Ja => format!("保存領域の上限に達したため。{formatted} を空けました。"),
        Locale::En => format!("The storage limit was reached. {formatted} freed."),
    }
}

/// A partial run says what failed, the likely reason, and where the rest went
/// (t260928-23aa, DS: 「5 件中 2 件を破棄できませんでした（理由）。3 件をごみ箱に
/// 移しました（X）。」). Moved, not freed: the recycle bin still holds it
/// (task1520 follow-up).
///
/// The title is what failed, the detail the likely reason and where the rest
/// went (t260928-e309: the error toast's two lines, the split 23aa left for it).
pub fn bulk_discard_partial_error(
    locale: Locale,
    total: usize,
    failed: usize,
    formatted: &str,
) -> (String, String) {
    let moved = total.saturating_sub(failed);
    match locale {
        Locale::Ja => (
            format!("{total} 件中 {failed} 件を破棄できませんでした"),
            format!(
                "他のアプリが使っているかもしれません。{moved} 件をごみ箱に移しました（{formatted}）。"
            ),
        ),
        Locale::En => (
            format!(
                "{failed} of {total} {} could not be discarded",
                sessions_noun(total)
            ),
            format!(
                "Another app may be using them. Moved {moved} to the recycle bin ({formatted})."
            ),
        ),
    }
}

/// A `.lvb` opened from Explorer or a drop that the catalog refused
/// (t260928-e309 F9): the title and the next step, in place of the catalog's
/// English reason, which goes to the log. The second line is the DS
/// DropOverlay's own sentence for a file it will not take.
pub fn file_open_failed(locale: Locale) -> (String, String) {
    match locale {
        Locale::Ja => (
            "ファイルを開けませんでした".to_owned(),
            "Liveback のセッション（.lvb）だけを開けます。".to_owned(),
        ),
        Locale::En => (
            "The file could not be opened".to_owned(),
            "Only Liveback sessions (.lvb) can be opened.".to_owned(),
        ),
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

/// The clip card's time (task243, t260928-6080 F1, DS ClipCard): a bare
/// `21:04` for today, `昨日 23:41` for yesterday, `9/13 22:05` for anything
/// older this year, and `2025/9/13 22:05` once the year is not this one --
/// `tile_date_text`'s year rule. t260926-97dd's `今日` prefix was the history
/// list's, and this function's only caller is the clip grid now.
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
        _ if year != now_year => format!("{year}/{month}/{day} {clock}"),
        _ => format!("{month}/{day} {clock}"),
    }
}

/// The created date, on the list's column and the tile's second line alike
/// (round5 §6-A, task184; t260928-e029): `8/13 22:05`, today and yesterday
/// included, so the column keeps one width and one shape. The seconds come
/// off; the year comes off only while it is this year's (t260928-23aa):
/// last year's August is `2025/8/13 22:05`. An unreadable `now` costs only
/// the year. Anything that is not the expected `YYYY/MM/DD HH:MM:SS` shape is
/// passed through untouched, because the caller falls back to the raw session
/// id when the clock conversion fails and a half-parsed id would be worse than
/// the id.
pub fn tile_date_text(formatted: &str, now: &str) -> String {
    let Some((year, month, day, clock)) = split_stamp(formatted) else {
        return formatted.to_owned();
    };
    match split_stamp(now) {
        Some((now_year, ..)) if now_year != year => format!("{year}/{month}/{day} {clock}"),
        _ => format!("{month}/{day} {clock}"),
    }
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
