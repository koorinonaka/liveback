//! Pure state and strings for the slint settings screen (task126). Ported from
//! `src/pages/SettingsPage.tsx` and the `settings` section of `src/i18n/ja.ts`
//! as they stand after round-2 (task117/119).
//!
//! The clamping itself lives in `crate::settings` (task121); this module only
//! carries what the view needs: labels, the shortcut composer and the path
//! truncation, all testable without slint.

use crate::ring_buffer::{MAX_RETENTION_MINUTES, MIN_RETENTION_MINUTES};
use crate::settings::RecordingCodec;
use crate::settings::{
    MAX_RETENTION_CAPACITY_GB, MAX_SESSION_LIFETIME_DAYS, MIN_RETENTION_CAPACITY_GB,
    MIN_SESSION_LIFETIME_DAYS, RETENTION_NONE,
};
use crate::ui_state::locale::Locale;
use crate::ui_state::sessions::sessions_noun;

// Ranges the numeric rows display and check against. Same sources task121's
// clamps use, re-exported as i64 so the bin has one type to hand to slint.
pub const RETENTION_MINUTES_RANGE: (i64, i64) =
    (MIN_RETENTION_MINUTES as i64, MAX_RETENTION_MINUTES as i64);
pub const RETENTION_CAPACITY_GB_RANGE: (i64, i64) =
    (MIN_RETENTION_CAPACITY_GB, MAX_RETENTION_CAPACITY_GB);
pub const SESSION_LIFETIME_DAYS_RANGE: (i64, i64) =
    (MIN_SESSION_LIFETIME_DAYS, MAX_SESSION_LIFETIME_DAYS);

// ---------- the settings screen's wording ----------

crate::tr! {
    group_capture { ja: "録画", en: "Recording" }
    /// t260927-fde8: the DS SettingsScreen's groups the old form lacked.
    group_where { ja: "保存先", en: "Folders" }
    group_general { ja: "一般", en: "General" }
    /// The settings page's StorageMeter legend (the history footer is
    /// compact and draws none).
    storage_deletable { ja: "自動削除の対象", en: "Auto-delete" }
    storage_free { ja: "上限まで", en: "Left" }
    group_hotkey { ja: "ホットキー", en: "Hotkey" }
    /// The hotkey group's description (t260928-7e9a, DS `screens.jsx`).
    group_hotkey_sub {
        ja: "ゲームの画面にいても効きます。",
        en: "Works even while a game is in front."
    }
    update_check { ja: "更新を自動で確かめる", en: "Check for updates automatically" }
    /// t261009-5ebd. The URL and a taken port go on the same line instead,
    /// from `mcp_listening` / `mcp_port_taken`.
    mcp_server { ja: "MCP サーバー", en: "MCP server" }
    mcp_server_sub {
        ja: "オンにすると、このパソコンの他のアプリがセッションとマーカーを読み、範囲を書き出せます。マーカーとコメントの追加、保護、クリップの保存、録画の開始と停止もできます。",
        en: "When on, other apps on this PC can read sessions and markers and export ranges. They can also add markers and comments, protect sessions, save clips, and start and stop recordings."
    }
    update_now { ja: "更新…", en: "Update…" }
    update_check_now { ja: "更新を確認", en: "Check for updates" }
    /// Shown while the switch is off, in place of a version line that would be
    /// stale the moment it was written.
    update_off { ja: "自動では確かめません", en: "Not checked automatically" }
    /// Only after a check answered that nothing newer exists.
    update_up_to_date { ja: "最新です", en: "Up to date" }
    /// Deliberately not "最新版です": the check did not answer, so the app does
    /// not know either way, and saying it is up to date would be a guess.
    update_failed {
        ja: "更新を確かめられませんでした",
        en: "Could not check for updates"
    }
    update_checking { ja: "確かめています…", en: "Checking…" }
    update_downloading { ja: "ダウンロードしています…", en: "Downloading…" }

    group_retention { ja: "保存領域", en: "Storage" }
    retention_minutes { ja: "保持時間", en: "Buffer length" }
    /// t260928-7e9a: the DS SettingsScreen's row descriptions (F16).
    retention_minutes_sub { ja: "リングバッファに残す長さ", en: "How much the ring buffer keeps" }
    frame_rate { ja: "フレームレート", en: "Frame rate" }
    /// The user, 2026-10-20: the stage's side taps and J / L jump this far.
    seek_seconds { ja: "左右の送り幅", en: "Skip length" }
    seek_seconds_sub { ja: "動画の左右を押したときと J / L で移動する秒数", en: "How far a side press or J / L jumps" }
    /// Task1490: the two retention rows are switches now, and the label is what
    /// the switch does rather than what the number under it means. The Japanese
    /// is the mock's verbatim; the English keeps the two terms the rest of the
    /// screen uses -- "disk limit" and "keep-for".
    retention_capacity_gb { ja: "容量で自動削除", en: "Auto-delete by size" }
    session_lifetime_days { ja: "期間で自動削除", en: "Auto-delete by age" }
    unit_minutes { ja: "分", en: "min" }
    unit_gigabytes { ja: "GB", en: "GB" }
    unit_days { ja: "日", en: "days" }
    /// The frame-rate row's trailing unit (task185). Its chips carry bare
    /// numbers like every other `PresetRow`, so the `fps` has to be stated
    /// once at the end of the line instead of five times on the chips.
    unit_fps { ja: "fps", en: "fps" }
    /// W-9 (round5 §11). The old line spelled out *when* the sweep runs ("at
    /// startup and after a recording stops"), which is scheduling detail the
    /// user cannot act on. What they can act on is that protection exempts a
    /// session. Task1080 split it across three lines; task1490 dropped the
    /// 「なし」 sentence with the chip it described -- two switches explain
    /// themselves -- and took the rest from the 2026-08-23 mock. The
    /// both-off line went the same way for the same reason.
    retention_hint {
        ja: "上限を超えると、古いセッションから順に自動で削除します。保護したセッションは削除しません。",
        en: "Past a limit, the oldest sessions are deleted first. Protected sessions are never deleted."
    }
    /// round8 §3-1's label, kept verbatim -- renaming a spec-quoted string is
    /// the design side's call.
    ring_buffer { ja: "リングバッファ録画", en: "Continuous recording" }
    /// The sub line is *not* the mock's any more (task1410): the switch stopped
    /// gating recording and now decides only whether footage older than the
    /// buffer length is dropped, so the mock's 「手動キャプチャのみ」 would
    /// describe something the app does not do. Reported back to design.
    ///
    /// t260928-7e9a (DS Switch: a setting that reaches the next recording says
    /// so): the switch is read when a session starts.
    ring_buffer_sub {
        ja: "オンのあいだは保持時間より古い映像を消し、オフにすると止めるまで録り続けます。次の録画から適用されます。",
        en: "On drops footage older than the buffer length; off keeps everything. Applies from the next recording."
    }
    include_cursor { ja: "マウスカーソルも録画する", en: "Record the mouse cursor" }
    /// The cursor is read once, when the capture session is built.
    include_cursor_sub { ja: "次の録画から適用されます。", en: "Applies from the next recording." }
    /// Task1430, replacing the picker's per-application track chooser. The
    /// sub line says it is still one track and, like every capture setting,
    /// reaches the next recording rather than the running one. t260928-7e9a:
    /// the DS SettingsScreen's sentence, which dropped the aside about
    /// Liveback's own playback.
    capture_all_audio { ja: "他のアプリの音も録音する", en: "Record other applications too" }
    capture_all_audio_sub {
        ja: "オンにすると、対象以外の音も 1 つのトラックに混ぜて録ります。次の録画から適用されます。",
        en: "On mixes the sound of everything else into the one track too. Applies from the next recording."
    }
    /// Task1760. The row is only shown at all on a machine that can both
    /// encode and decode AV1, so the wording never has to explain an option
    /// the user cannot take.
    codec_av1 { ja: "AV1 で録画する", en: "Record with AV1" }
    codec_av1_sub {
        ja: "同じ画質でファイルが小さくなります。次の録画から適用されます。",
        en: "Smaller files at the same quality. Applies from the next recording."
    }
    auto_start { ja: "Windows にサインインしたら起動する", en: "Start when Windows signs in" }
    auto_start_sub {
        ja: "トレイに常駐し、自動録画の対象を待ちます。",
        en: "Stays in the tray and waits for auto-record targets."
    }
    /// Task1090: 「録画確認ホットキー」 named three things at once -- raise the
    /// window, mark the moment, open the live session. Only the marker
    /// survived, so the label says that and nothing else.
    hotkey { ja: "マーカーを追加", en: "Add a marker" }
    /// What the press reports. There is deliberately no window to look at, so
    /// the OS notification is the whole of the feedback.
    marker_hotkey_added { ja: "マーカーを追加しました", en: "Marker added" }
    marker_hotkey_refused { ja: "マーカーを追加できませんでした", en: "The marker could not be added" }
    marker_hotkey_not_recording { ja: "録画していません。", en: "Nothing is being recorded." }
    /// The second hotkey row (t261025-3669): the window in front starts or
    /// stops recording.
    record_hotkey { ja: "録画の開始/停止", en: "Start/stop recording" }
    /// The third hotkey row (t261025-034f): raise the window, and review the
    /// recording when there is one.
    show_hotkey { ja: "ウィンドウを前面に出す", en: "Bring window to front" }
    /// The record row's face while no chord is set, which is its default.
    hotkey_unset { ja: "未設定", en: "Not set" }
    /// What a record press reports. "Starting" rather than "started": the
    /// start runs off the UI thread and can still fail, which the picker's
    /// own failure toast then says.
    record_hotkey_starting { ja: "録画を開始します", en: "Starting the recording" }
    record_hotkey_stopped { ja: "録画を停止しました", en: "Recording stopped" }
    record_hotkey_refused { ja: "録画を開始できませんでした", en: "The recording could not be started" }
    record_hotkey_refused_self {
        ja: "Liveback の窓は録画できません。録画する窓を前面にしてから押してください。",
        en: "Liveback cannot record its own window. Bring the window to record to the front first."
    }
    record_hotkey_refused_target {
        ja: "前面の窓は録画できません。",
        en: "The window in front cannot be recorded."
    }
    hotkey_capture_armed { ja: "キーを入力…", en: "Press a key…" }
    /// After an `Esc` keycap (DS HotkeyField).
    hotkey_capture_abort { ja: "でやめる", en: "to cancel" }
    /// Task195: the failure used to be a banner inside the settings page,
    /// which is invisible when the page is closed -- and registration also
    /// runs at startup, where the page never is open. A toast reaches the user
    /// on any screen. t260928-7e9a: only for a chord that is already dead
    /// (startup, a resume); a chord refused as it is typed is said on the
    /// field instead, never on both.
    hotkey_failure_title {
        ja: "ホットキーを登録できませんでした",
        en: "The shortcut could not be registered"
    }

    export_directory_change { ja: "変更…", en: "Change…" }
    directory_open { ja: "開く", en: "Open" }
    buffer_directory_label { ja: "録画バッファ", en: "Recording buffer" }
    buffer_directory_sub {
        ja: "録画中のセッションが入るフォルダ。高速なドライブを選んでください。",
        en: "Where recording sessions are written. Pick a fast drive."
    }
    /// Task2970. No confirmation dialog behind this one, unlike the buffer
    /// row: changing it moves nothing, it only points the next save somewhere
    /// else.
    clip_directory_label { ja: "クリップ", en: "Clips" }
    /// Task3030 (round18 design §4-4): the default is a folder under the
    /// buffer root, and both the save and the list side now create it, so
    /// the row can say so instead of silently having a fixed default the
    /// user could hit as missing.
    clip_directory_sub {
        ja: "「クリップに保存」の保存先。なければ作ります。",
        en: "Where \"Save clip\" writes. Created if missing."
    }
    /// Round5 §7-C: the buffer folder stopped being read-only, so it needs the
    /// export row's vocabulary -- and one word the export row does not,
    /// because this is the only path that can be refused.
    buffer_directory_change_blocked {
        ja: "録画中は変更できません",
        en: "Cannot be changed while recording"
    }
    /// Shared by both path rows, like `directory_open`.
    directory_reset { ja: "既定に戻す", en: "Reset to default" }
    buffer_directory_reset_done {
        ja: "録画バッファフォルダを既定に戻しました。以前のフォルダにあるセッションは移動しません。",
        en: "The recording buffer folder is back to its default. Sessions in the old folder stay where they are."
    }
    buffer_directory_confirm_title {
        ja: "録画バッファフォルダを変更しますか？",
        en: "Change the recording buffer folder?"
    }
    /// The consequence worth naming: the history lists what is under the
    /// buffer root, so moving the root moves what the history can see. Nothing
    /// is copied.
    buffer_directory_confirm_lead {
        ja: "これ以降の録画は新しい場所に保存され、履歴はその場所を読みます。",
        en: "New recordings go to the new folder and the history scans it."
    }
    /// The dialog's note (t260928-5f3b, DS Dialog: what happens in the body,
    /// what to watch for in the note).
    buffer_directory_confirm_note {
        ja: "今あるセッションは移動せず、元の場所に残ります。",
        en: "Existing sessions are not moved and stay where they are."
    }
    buffer_directory_confirm_ok { ja: "変更する", en: "Change" }
    /// t261018-34fe: the held-back retention change's two buttons. 取り消す,
    /// not 元に戻す: the band's own sentence says the deletion cannot be undone
    /// (元に戻せません), and one band must not use one word for two things.
    retention_confirm { ja: "長押しで削除して反映", en: "Hold to delete and apply" }
    retention_revert { ja: "取り消す", en: "Cancel" }
    /// Said on the toast when the user leaves the settings screen with the
    /// change still held back: it was dropped, not applied.
    retention_reverted {
        ja: "自動削除の変更を取り消しました",
        en: "The auto-delete change was cancelled"
    }
    buffer_directory_change_error {
        ja: "録画バッファフォルダを変更できませんでした",
        en: "The recording buffer folder could not be changed"
    }
    open_directory_error { ja: "フォルダを開けませんでした", en: "The folder could not be opened" }

    /// A machine where `GlobalHotKeyManager` itself refuses to start.
    hotkeys_unavailable {
        ja: "グローバルホットキーを利用できません",
        en: "Global hotkeys are not available on this machine"
    }

    // ---------- インストール (task4140) ----------
    //
    // One row at the end of 一般 (no title of its own since 344c7896), present
    // only on a machine where something the installer registers is missing.
    // Nothing in it is pressable: 「今すぐ登録する」 is a separate task, and this side of
    // it only reads the registry.

    /// The two features that die with no signal at all when the installer's
    /// registration is missing, named by what the user would notice rather
    /// than by what is actually absent -- 「AUMID」 and 「livia_thumb.dll」 name
    /// causes the user has no way to check.
    install_feature_notifications { ja: "OS の通知", en: "OS notifications" }
    install_feature_thumbnails {
        ja: "エクスプローラーのサムネイル",
        en: "Explorer thumbnails"
    }
    /// Why, and what fixes it. Deliberately about the *registration* and not
    /// about whether the feature works: the check reads one key per feature,
    /// so an install whose keys survive but whose files do not (task1250's
    /// `InprocServer32` pointing at an old DLL) reads as registered and says
    /// nothing here. Claiming 「登録済みなら動きます」 would be the one
    /// sentence this row cannot support. t260928-7e9a cut it to two sentences
    /// (DS: a state is one line), and neither of them makes that claim.
    install_unregistered_reason {
        ja: "インストーラが登録するキーが見つかりません。インストーラで入れ直すと有効になります。",
        en: "The keys the installer writes are missing. Running the installer again turns these back on."
    }
}

/// The failure toast's second line (t260928-7e9a, DS 言葉: a failure says why
/// and what to do). The why is the field's own sentence naming the chord; the
/// what is the toast's 「設定を開く」, which the caller attaches.
pub fn hotkey_failure_detail(locale: Locale, chord: &str) -> String {
    hotkey_taken(locale, hotkey_display(chord))
}

/// A hotkey chord as the settings hold it (`Ctrl+Shift+R`), one Keycap per
/// key: the DS Keycap never draws the `+` (t260928-ac18).
pub fn split_hotkey(chord: &str) -> Vec<String> {
    chord
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One key per element, as the field's caps draw them: the stored chord's
/// `Super` is shown as `Win` (DS 言葉: the Windows names). The stored spelling
/// stays global-hotkey's, so this is for display only.
pub fn hotkey_keys(chord: &str) -> Vec<String> {
    split_hotkey(chord)
        .into_iter()
        .map(|key| {
            if key == "Super" {
                "Win".to_owned()
            } else {
                key
            }
        })
        .collect()
}

/// The chord in running text: `Ctrl + Shift + R` (DS 言葉, spaces around `+`).
pub fn hotkey_display(chord: &str) -> String {
    hotkey_keys(chord).join(" + ")
}

// ---------- インストール登録の状態行 (task4140) ----------

/// The 「インストール」 row's line, or an empty string when there is
/// nothing to say.
///
/// Both arguments carry the same polarity as the `installer_registration` log
/// line the bin writes: `true` is 「the registry holds this registration」. So
/// both `true` -- the installed machine -- returns `""`, and that empty string
/// is the whole of 「登録済みの機械では出ない」: the `.slint` side wraps the
/// row in an `if` on it, so it is not built. There is no
/// separate "everything is fine" line, because a form does not need a row per
/// thing that is not wrong.
///
/// Composed here rather than in `.slint`, which has no string concatenation,
/// and here rather than in the bin, where `desktop.rs` can hold no tests.
///
/// Only the two features that die *silently* are named. The `.lvb`
/// association is missing just as often and is deliberately left out: its
/// symptom is Explorer drawing a different icon and type name, which the user
/// has already seen, so listing it would spend the one row on the one failure
/// that announces itself.
pub fn unregistered_features_line(locale: Locale, aumid: bool, thumbnail_handler: bool) -> String {
    let mut missing: Vec<&str> = Vec::new();
    if !aumid {
        missing.push(install_feature_notifications(locale));
    }
    if !thumbnail_handler {
        missing.push(install_feature_thumbnails(locale));
    }
    if missing.is_empty() {
        return String::new();
    }
    // The join has to read for one name as well as two, which is why the
    // state is part of each locale's sentence instead of a shared suffix
    // bolted onto a list: 「と」 and " and " are the natural joins in each,
    // and neither needs a plural form of the state to go with it.
    not_registered_line(locale, missing.join(feature_join(locale)))
}

crate::tr! {
    /// What joins the names in `unregistered_features_line`.
    feature_join { ja: "と", en: " and " }
}

crate::trf! {
    /// `unregistered_features_line`'s sentence around the joined names.
    not_registered_line(names) {
        ja: "{names}は未登録のため無効",
        en: "{names}: not registered, so disabled"
    }
}

// ---------- preset rows (round5 §7-B) ----------
//
// Four numeric settings became chips: the values people actually pick, one
// press each, instead of a text field that has to be selected, cleared, typed
// into and committed. Three of them keep a free field for the values in
// between; the frame-rate row does not, because the engine accepts three rates
// and they are all on the list.

/// 20 / 60 / 120. The clamp is 5..1440, and the row carries a free field for
/// everything off the chips (a 5, a 90, a whole day).
///
/// 2026-09-13 (the user): 40 is gone and the free field is back, overriding
/// task1080's chips-only row -- 40 分 was the chip nobody pressed, and the
/// minutes people do mean between the chips have to be typable somewhere.
///
/// 2026-09-13 (the user, task t260913-3127): the ceiling is one day, so long
/// sessions can be kept whole and cut afterwards. 5 and 10 left the row for the
/// free field, and 1440 is deliberately not a chip -- the chips no longer
/// bracket the clamp; both bounds are typed.
pub const RETENTION_MINUTES_PRESETS: [i64; 3] = [20, 60, 120];
/// The three rates `crate::settings::FRAME_RATES` accepts, widened for the
/// preset row.
pub const FRAME_RATE_PRESETS: [i64; 3] = {
    let rates = crate::settings::FRAME_RATES;
    [rates[0] as i64, rates[1] as i64, rates[2] as i64]
};
/// t260927-fde8: four, the DS PresetField's most. 5 GB and 3 days left the
/// row; a stored 5 or 3 is still valid and simply shows in the free field.
pub const RETENTION_CAPACITY_PRESETS: [i64; 4] = [10, 20, 50, 100];
pub const SESSION_LIFETIME_PRESETS: [i64; 4] = [1, 7, 30, 90];

// ---------- the codec switch (task1760) ----------
//
// A switch rather than a chip row: there are exactly two codecs, one of them is
// the default everywhere, and the row is hidden outright on a machine that
// cannot do the other one. The mapping lives here rather than in the bin so a
// test can see it -- the same rule that keeps every other decision out of
// `.slint` and out of `settings_page.rs`.

/// What flipping the AV1 switch means.
pub fn codec_from_switch(av1: bool) -> RecordingCodec {
    if av1 {
        RecordingCodec::Av1
    } else {
        RecordingCodec::H264
    }
}

/// Whether the switch is on for the codec the settings actually hold.
///
/// Takes the stored codec *and* whether this machine can do AV1 at all: a
/// hand-edited `settings.json` can say `av1` on a machine whose row is hidden,
/// and drawing that as "on" in a row nobody can see would be a lie the user has
/// no way to correct. Recording refuses it separately
/// (`encoder::ensure_recording_codec`), which is where they find out.
pub fn codec_switch_on(codec: RecordingCodec, av1_supported: bool) -> bool {
    av1_supported && matches!(codec, RecordingCodec::Av1)
}

/// The free fields' placeholders (t260928-7e9a, DS TextField: a placeholder is
/// an example, not the range -- the range is said when a draft misses it).
/// Each is a value between the chips, which is what the free field is for.
pub const RETENTION_MINUTES_EXAMPLE: i64 = 90;
pub const RETENTION_CAPACITY_EXAMPLE: i64 = 200;
pub const SESSION_LIFETIME_EXAMPLE: i64 = 14;

crate::trf! {
    numeric_out_of_range(min, max) {
        ja: "{min}〜{max} の範囲で入力してください",
        en: "Enter a value from {min} to {max}"
    }
}

/// Which chip of a `PresetRow` is lit for the value the setting actually holds
/// (task168, design §7-B). `-1` means none of them is: the user typed something
/// off the list, and the free field carries it instead. Returned as an `i32`
/// because that is the index type the `.slint` side compares against.
pub fn preset_index(presets: &[i64], value: i64) -> i32 {
    presets
        .iter()
        .position(|preset| *preset == value)
        .map_or(-1, |index| index as i32)
}

/// What typing a number into a `PresetRow` and pressing Enter means.
///
/// Round2 7-2 and round6 3: an out-of-range draft shows an error and is never
/// committed. Until the mock's `.errrow` was drawn, nothing in the pane could
/// say so, so the typed draft was silently clamped instead -- 9999 became
/// 2000 and the row looked like it had agreed with the user. The clamp stays
/// where it is (a value arriving from the settings file still has to land in
/// range); this is only about the draft the user typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetDraft {
    /// A number inside the range: commit it.
    Commit(i64),
    /// A number outside it: refuse it and show the range.
    OutOfRange,
    /// Not a number at all. The field keeps what was typed and the setting is
    /// left alone, with no error -- the contract the numeric rows have always
    /// had, and the state an empty field lands in.
    Ignore,
}

pub fn preset_draft(text: &str, min: i64, max: i64) -> PresetDraft {
    match text.trim().parse::<i64>() {
        Ok(value) if (min..=max).contains(&value) => PresetDraft::Commit(value),
        Ok(_) => PresetDraft::OutOfRange,
        Err(_) => PresetDraft::Ignore,
    }
}

// ---------- the retention switches (task1490) ----------

/// What a retention row shows -- chips and free field alike -- for a stored
/// value. `0` is the off sentinel (`crate::settings::RETENTION_NONE`) and has
/// no number to show, so the row shows the one it goes back to: what it was
/// holding before the switch went off, or the shipped default.
pub fn retention_shown(stored: i64, remembered: i64) -> i64 {
    if stored == RETENTION_NONE {
        remembered
    } else {
        stored
    }
}

/// Flipping a retention row's switch: `(what to store, what to remember)`.
///
/// Off writes the sentinel and keeps the number that was in force, which is
/// what the dimmed row goes on showing and what turning it back on restores.
/// Off twice must not forget it, hence the `stored == RETENTION_NONE` arm.
pub fn retention_toggle(on: bool, stored: i64, remembered: i64) -> (i64, i64) {
    if on {
        (remembered, remembered)
    } else {
        (RETENTION_NONE, retention_shown(stored, remembered))
    }
}

// ---------- the held-back retention change (t261018-34fe) ----------

/// The band's headline: everything the new limits delete, and that it is for
/// good -- the sweep skips the recycle bin.
pub fn retention_pending_text(locale: Locale, count: usize, size: &str) -> String {
    match locale {
        Locale::Ja => format!(
            "この設定にすると、古いセッション {count} 件（{size}）を削除します。ごみ箱には移さず、元に戻せません。"
        ),
        Locale::En => format!(
            "With this setting, {count} old {} ({size}) will be deleted. They skip the recycle bin and cannot be restored.",
            sessions_noun(count)
        ),
    }
}

/// Which limit takes how many, only when both take some.
pub fn retention_pending_breakdown(locale: Locale, by_capacity: usize, by_age: usize) -> String {
    if by_capacity == 0 || by_age == 0 {
        return String::new();
    }
    retention_breakdown_counts(locale, by_capacity, by_age)
}

crate::trf! {
    /// `retention_pending_breakdown` once both limits take some.
    retention_breakdown_counts(by_capacity, by_age) {
        ja: "（容量で {by_capacity} 件・期間で {by_age} 件）",
        en: "({by_capacity} by size, {by_age} by age)"
    }
}

/// The in-use share: the sweep skips what is recording or open on the review
/// screen, and only the next stop or launch runs it again.
pub fn retention_pending_in_use(locale: Locale, count: usize) -> String {
    if count == 0 {
        return String::new();
    }
    match locale {
        Locale::Ja => format!(
            "うち使用中の {count} 件は、次に録画を停止するか、次回起動したときに削除します。"
        ),
        Locale::En => format!(
            "{count} of them {} in use and will be deleted when a recording next stops or on the next launch.",
            if count == 1 { "is" } else { "are" }
        ),
    }
}

/// What the confirmed sweep actually removed.
pub fn retention_applied_text(locale: Locale, count: u32) -> String {
    match locale {
        Locale::Ja => format!("{count} 件を削除しました"),
        Locale::En => format!("Deleted {count} {}", sessions_noun(count as usize)),
    }
}

crate::trf! {
    /// The hotkey row's line when another app holds the chord (DS HotkeyField).
    hotkey_taken(chord) {
        ja: "{chord} は他のアプリが使っています。",
        en: "{chord} is in use by another app."
    }
    /// The hotkey row's line when the other Liveback hotkey already uses the
    /// chord (t261025-3669).
    hotkey_clash(chord) {
        ja: "{chord} は別のホットキーで使っています。",
        en: "{chord} is already used by the other shortcut."
    }
    /// The MCP row's line while the server listens (t261009-5ebd): where to
    /// point a client.
    mcp_listening(port) {
        ja: "http://127.0.0.1:{port}/mcp で待ち受けています。",
        en: "Listening at http://127.0.0.1:{port}/mcp."
    }
    /// The MCP row's line when the port is taken -- usually a second Liveback.
    /// The switch stays on; turning it off and on again retries.
    mcp_port_taken(port) {
        ja: "ポート {port} は他のアプリが使っているため、開始できませんでした。オフにしてからオンにすると、もう一度試します。",
        en: "Could not start: port {port} is in use by another app. Turn this off and on to try again."
    }
    /// The update row's first line. It names the number as a version: the old
    /// "最新版です（0.9.0）" left the user asking what 0.9.0 was and what was
    /// up to date. The check's answer goes on the line under it.
    update_version(version) {
        ja: "バージョン {version}",
        en: "Version {version}"
    }
    /// The line under the version when a newer release exists. The running
    /// version is already on the line above, so only the new one is named.
    update_available(latest) {
        ja: "{latest} が利用できます",
        en: "{latest} is available"
    }
    /// `update_notice`'s title.
    update_notice_title(latest) {
        ja: "Liveback {latest} が出ました",
        en: "Liveback {latest} is available"
    }
    hotkey_saved(shortcuts) {
        ja: "ホットキーを保存しました（{shortcuts}）",
        en: "Shortcut saved ({shortcuts})"
    }
}

crate::tr! {
    /// `update_notice`'s body.
    update_notice_body { ja: "設定の「更新」から入れられます", en: "Install it from Settings → Updates" }
}

/// The OS notification a startup check raises when a newer release exists
/// (t260930-4822): title, then body.
pub fn update_notice(locale: Locale, latest: &str) -> (String, String) {
    (
        update_notice_title(locale, latest),
        update_notice_body(locale).to_string(),
    )
}

// ---------- path truncation ----------

/// Mirrors `PATH_TAIL_MAX_CHARS` / `splitPathTail` in SettingsPage.tsx: the
/// head gets the ellipsis, the tail is pinned, and the tail keeps two segments
/// only when they fit -- one is always kept, however long it is.
const PATH_TAIL_MAX_CHARS: usize = 22;

pub fn split_path_tail(path: &str) -> (String, String) {
    let separator = if path.contains('\\') { '\\' } else { '/' };
    let segments: Vec<&str> = path.split(separator).collect();
    if segments.len() <= 1 {
        return (String::new(), path.to_owned());
    }
    let mut tail = format!("{separator}{}", segments[segments.len() - 1]);
    if segments.len() > 2 {
        let two = format!(
            "{separator}{}",
            segments[segments.len() - 2..].join(&separator.to_string())
        );
        if two.chars().count() <= PATH_TAIL_MAX_CHARS {
            tail = two;
        }
    }
    let head_chars = path.chars().count() - tail.chars().count();
    (path.chars().take(head_chars).collect(), tail)
}

// ---------- immediate apply (task146) ----------

/// What a freshly captured chord should cause. There is no save button since
/// task146: capture *is* the commit, so the decision that used to live behind
/// one now has to be made the moment the key lands.
///
/// `Conflict` went with the clip hotkey (task1090): with one row there is
/// nothing for it to collide with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotkeyCommit {
    /// The chord that was already on the row. Nothing to register or save.
    Unchanged,
    /// Register this chord, and save it only if the OS accepts.
    Apply(String),
}

pub fn commit_capture(marker: &str, captured: &str) -> HotkeyCommit {
    if captured == marker {
        return HotkeyCommit::Unchanged;
    }
    HotkeyCommit::Apply(captured.to_owned())
}

/// What the row shows once the registrar has answered. A refusal rolls the
/// registration back to the previous chord, so the display has to roll back
/// too -- otherwise the screen would advertise a chord the OS never accepted.
pub fn displayed_hotkey(previous: &str, attempted: &str, registered: bool) -> String {
    if registered { attempted } else { previous }.to_owned()
}

// ---------- shortcut composition ----------

// slint delivers non-character keys as single chars; the values below are
// transcribed from i-slint-common-1.17.1/key_codes.rs (cargo registry), the
// same table `slint::platform::Key` is generated from. The names on the right
// are what the React version's `composeShortcut` produced from the browser's
// `event.key`, so both builds write the same accelerator strings.
const MODIFIER_KEYS: std::ops::RangeInclusive<char> = '\u{0010}'..='\u{0018}'; // Shift..MetaR (incl. AltGr, CapsLock)

/// Windows hands slint the ASCII control code for Ctrl+&lt;letter&gt; -- Ctrl+A
/// arrives as 0x01, Ctrl+Z as 0x1A -- so with Ctrl held the letter has to be
/// reconstructed or no Ctrl chord can ever be captured (task145; the defaults
/// Ctrl+Shift+R = 0x12 and Ctrl+Shift+S = 0x13 were among the casualties).
///
/// The catch is that 0x10..=0x18 are *also* the modifier keys' own codes
/// (verified against i-slint-common-1.17.1/key_codes.rs). Converting greedily
/// would confirm `Ctrl+Shift+P` the instant Shift went down while Ctrl was
/// held. So a code in that band only becomes a letter when the modifier it
/// would otherwise be is *not* pressed -- which leaves four chords permanently
/// unreachable, since their code always coincides with a held modifier:
/// Ctrl+Q (0x11 = Control), Ctrl+V (0x16 = ControlR), Ctrl+Shift+P (0x10 =
/// Shift) and Ctrl+Shift+U (0x15 = ShiftR). They stay armed rather than saving
/// the wrong chord.
///
/// CapsLock (0x14) carries no modifier flag at all, so a bare CapsLock press
/// with Ctrl held is indistinguishable from Ctrl+T and is read as the latter --
/// the useful reading of the two.
fn ctrl_letter(key: char, modifiers: Modifiers) -> Option<char> {
    let Modifiers {
        ctrl,
        alt,
        shift,
        meta,
    } = modifiers;
    if !ctrl {
        return None;
    }
    let code = key as u32;
    if !(0x01..=0x1a).contains(&code) {
        return None;
    }
    let is_a_held_modifier = match code {
        0x10 | 0x15 => shift, // Shift / ShiftR
        0x11 | 0x16 => true,  // Control / ControlR -- ctrl is held by definition
        0x12 | 0x13 => alt,   // Alt / AltGr
        0x17 | 0x18 => meta,  // Meta / MetaR
        _ => false,
    };
    (!is_a_held_modifier).then(|| char::from(b'A' + (code as u8 - 1)))
}

fn named_key(key: char) -> Option<&'static str> {
    Some(match key {
        ' ' => "Space",
        '\u{0008}' => "Backspace",
        '\u{0009}' => "Tab",
        '\u{000a}' => "Enter",
        '\u{007f}' => "Delete",
        '\u{F700}' => "Up",
        '\u{F701}' => "Down",
        '\u{F702}' => "Left",
        '\u{F703}' => "Right",
        '\u{F727}' => "Insert",
        '\u{F729}' => "Home",
        '\u{F72B}' => "End",
        '\u{F72C}' => "PageUp",
        '\u{F72D}' => "PageDown",
        _ => return None,
    })
}

/// The modifier keys held with a slint key event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
}

/// The printable non-alphanumeric keys the registration side can read back:
/// global-hotkey 0.8.0's `parse_key` (`hotkey.rs`) names exactly these (plus
/// ASCII letters and digits). Anything else is refused as `UnsupportedKey`.
const HOTKEY_SYMBOLS: [char; 11] = ['`', '\\', '[', ']', ',', '=', '-', '.', '\'', ';', '/'];

/// Builds a Tauri-accelerator-style string (`Ctrl+Shift+R`) from a slint key
/// event. `None` means "keep waiting": a modifier-only press, or a key that has
/// no stable name on both sides (unmapped function-row and media keys, IME
/// strings, control codes, symbols outside [`HOTKEY_SYMBOLS`]). Escape never
/// reaches this -- the caller disarms on it first, same as the React handler.
pub fn compose_shortcut(key_text: &str, modifiers: Modifiers) -> Option<String> {
    let Modifiers {
        ctrl,
        alt,
        shift,
        meta,
    } = modifiers;
    let mut chars = key_text.chars();
    let key = chars.next()?;
    // A multi-char text is an IME commit, not a chord.
    if chars.next().is_some() {
        return None;
    }
    // `named_key` first, so Ctrl+Tab / Ctrl+Enter / Ctrl+Backspace keep working:
    // their codes (0x09 / 0x0a / 0x08) are the same ones Ctrl+I / Ctrl+J /
    // Ctrl+H produce, and the named reading is the one that already worked.
    // Those three letters join the unreachable list in `ctrl_letter`.
    let name = if let Some(named) = named_key(key) {
        named.to_owned()
    } else if let Some(letter) = ctrl_letter(key, modifiers) {
        letter.to_string()
    } else if MODIFIER_KEYS.contains(&key) {
        return None;
    } else if ('\u{F704}'..='\u{F71B}').contains(&key) {
        // F1..F24, contiguous in the table.
        format!("F{}", key as u32 - 0xF703)
    } else if key.is_control() || ('\u{F700}'..='\u{F8FF}').contains(&key) {
        // Anything else non-printable has no name the registration side
        // (task130) could parse back; stay armed instead of saving garbage.
        return None;
    } else if key.is_ascii_alphanumeric() || HOTKEY_SYMBOLS.contains(&key) {
        key.to_ascii_uppercase().to_string()
    } else {
        // `@` `:` `^` `!` and every non-ASCII letter: registration would
        // refuse them as `UnsupportedKey`, which the field reports as "taken
        // by another app" (t261101-23dd). Stay armed instead.
        return None;
    };
    let mut parts: Vec<&str> = Vec::new();
    if ctrl {
        parts.push("Ctrl");
    }
    if alt {
        parts.push("Alt");
    }
    if shift {
        parts.push("Shift");
    }
    if meta {
        parts.push("Super");
    }
    parts.push(&name);
    Some(parts.join("+"))
}

// ---------- round16: the theme row ----------

pub use crate::settings::THEME_SYSTEM;
pub const THEME_LIGHT: &str = "light";
pub const THEME_DARK: &str = "dark";

/// The chip row's values, in the order the chips are drawn. Kept beside the
/// resolver below so the row and the stored value cannot disagree about which
/// chip means what -- the same arrangement `LANGUAGE_SETTINGS` uses.
pub const THEME_SETTINGS: [&str; 3] = [THEME_SYSTEM, THEME_LIGHT, THEME_DARK];

pub fn theme_index(setting: &str) -> i32 {
    THEME_SETTINGS
        .iter()
        .position(|candidate| *candidate == setting)
        .unwrap_or(0) as i32
}

pub fn theme_setting(index: i32) -> &'static str {
    THEME_SETTINGS
        .get(usize::try_from(index).unwrap_or(usize::MAX))
        .copied()
        .unwrap_or(THEME_SYSTEM)
}

/// Whether the window paints light, from the stored setting and what the OS
/// says. An unrecognised setting follows the OS rather than picking a theme,
/// for the reason [`crate::ui_state::locale::resolve`] gives: a record written
/// by a later build that knows a third theme should degrade to "whatever this
/// machine is".
pub fn theme_is_light(setting: &str, system_is_light: bool) -> bool {
    match setting {
        THEME_LIGHT => true,
        THEME_DARK => false,
        _ => system_is_light,
    }
}

crate::tr! {
    /// The settings row itself (round16).
    theme_label { ja: "テーマ", en: "Theme" }
    theme_system { ja: "システム", en: "System" }
    theme_light { ja: "ライト", en: "Light" }
    theme_dark { ja: "ダーク", en: "Dark" }
}

/// The chips' captions, in `THEME_SETTINGS` order.
pub fn theme_choices(locale: Locale) -> [&'static str; THEME_SETTINGS.len()] {
    [
        theme_system(locale),
        theme_light(locale),
        theme_dark(locale),
    ]
}

#[cfg(test)]
mod tests;
