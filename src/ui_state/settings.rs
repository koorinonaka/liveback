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
    group_capture { ja: "キャプチャ", en: "Capture" }
    group_hotkey { ja: "ホットキー", en: "Hotkey" }
    /// task t260920-2be2. The update rows sit under their own heading rather
    /// than in 保存領域: what they do has nothing to do with the buffer, and
    /// the switch is the only thing on this screen that reaches the network.
    group_update { ja: "更新", en: "Updates" }
    update_check { ja: "自動で最新版を確認する", en: "Check for updates automatically" }
    update_now { ja: "更新", en: "Update" }
    /// Shown while the switch is off, in place of a version line that would be
    /// stale the moment it was written.
    update_off { ja: "自動確認はオフです", en: "Automatic checks are off" }
    /// Deliberately not "最新版です": the check did not answer, so the app does
    /// not know either way, and saying it is up to date would be a guess.
    update_failed {
        ja: "更新を確認できませんでした",
        en: "Could not check for updates"
    }
    update_checking { ja: "確認しています…", en: "Checking…" }
    update_downloading { ja: "ダウンロードしています…", en: "Downloading…" }

    group_retention { ja: "保存領域", en: "Storage" }
    retention_minutes { ja: "保持時間", en: "Buffer length" }
    frame_rate { ja: "フレームレート", en: "Frame rate" }
    /// Task1490: the two retention rows are switches now, and the label is what
    /// the switch does rather than what the number under it means. The Japanese
    /// is the mock's verbatim; the English keeps the two terms the rest of the
    /// screen uses -- "disk limit" and "keep-for".
    retention_capacity_gb { ja: "保持容量で自動削除", en: "Auto-delete by disk limit" }
    session_lifetime_days { ja: "保持期間で自動削除", en: "Auto-delete by keep-for limit" }
    unit_minutes { ja: "分", en: "min" }
    unit_gigabytes { ja: "GB", en: "GB" }
    unit_days { ja: "日", en: "days" }
    unit_seconds { ja: "秒", en: "s" }
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
        ja: concat!(
            "古いセッションは保持容量と保持期間の上限で自動的に削除されます。\n",
            "保護したセッションは削除されません。",
        ),
        en: concat!(
            "Old sessions are deleted automatically once the disk limit or the keep-for limit is reached.\n",
            "Protected sessions are never deleted.",
        )
    }
    /// round8 §3-3: what the pencil on the hotkey box means.
    hotkey_edit_hint { ja: "クリックして変更", en: "Click to change" }
    /// round8 §3-1's label, kept verbatim -- renaming a spec-quoted string is
    /// the design side's call.
    ring_buffer { ja: "リングバッファ録画", en: "Continuous recording" }
    /// The sub line is *not* the mock's any more (task1410): the switch stopped
    /// gating recording and now decides only whether footage older than the
    /// buffer length is dropped, so the mock's 「手動キャプチャのみ」 would
    /// describe something the app does not do. Reported back to design.
    ring_buffer_sub {
        ja: "オンのあいだは保持時間を超えた古い映像から消えます。オフにすると消さずに録り続けます",
        en: "On drops footage older than the buffer length; off keeps everything"
    }
    include_cursor { ja: "カーソルを含める", en: "Include the cursor" }
    /// Task1430, replacing the picker's per-application track chooser. The
    /// sub line carries the three things the label cannot: it is still one
    /// track, Liveback's own review playback is never in it, and like every
    /// capture setting it reaches the next recording rather than the running
    /// one.
    capture_all_audio { ja: "他のアプリの音も録音", en: "Record other applications too" }
    capture_all_audio_sub {
        ja: "オンにすると対象以外のアプリの音も1トラックに混ぜて録ります(Liveback 自身の再生音は入りません)。次の録画から適用されます",
        en: "On mixes every other application's sound into the one track (never Liveback's own playback). Applies from the next recording"
    }
    /// Task1760. The row is only shown at all on a machine that can both
    /// encode and decode AV1, so the wording never has to explain an option
    /// the user cannot take.
    codec_av1 { ja: "AV1 で録画する", en: "Record with AV1" }
    codec_av1_sub {
        ja: "同じ画質でファイルが小さくなります。既存の録画はそのまま再生できます。次の録画から適用されます",
        en: "Smaller files at the same quality. Existing recordings still play. Applies from the next recording"
    }
    auto_start { ja: "Windowsログイン時に起動する", en: "Start when Windows signs in" }
    /// Task1090: 「録画確認ホットキー」 named three things at once -- raise the
    /// window, mark the moment, open the live session. Only the marker
    /// survived, so the label says that and nothing else.
    hotkey { ja: "マーカーを追加", en: "Add a marker" }
    /// What the press reports. There is deliberately no window to look at, so
    /// the OS notification is the whole of the feedback.
    marker_hotkey_added { ja: "マーカーを追加しました", en: "Marker added" }
    marker_hotkey_refused { ja: "マーカーを追加できません", en: "No marker added" }
    marker_hotkey_not_recording { ja: "録画していません", en: "Nothing is being recorded" }
    hotkey_capture_armed { ja: "キーを入力してください…", en: "Press a key…" }
    hotkey_capture_abort { ja: "Esc でキャンセル", en: "Esc to cancel" }
    hotkey_state_failed { ja: "登録失敗", en: "Not registered" }
    /// round7 §2-10: the row's line when the OS refused the chord.
    hotkey_state_taken {
        ja: "他のアプリが使用中のため登録できません",
        en: "Another app is holding this shortcut"
    }
    /// `ja.settings.hotkeyError`: shown when the OS refuses the chord. Since
    /// task146 registration happens *before* the save, so a refusal leaves
    /// both the file and the row on the previous chord -- nothing is
    /// saved-but-dead.
    hotkey_register_error {
        ja: "ホットキーを登録できません。他のアプリケーションとの競合を確認してください。",
        en: "The shortcut could not be registered. Check for a conflict with another app."
    }
    /// Task195: the failure used to be a banner inside the settings page,
    /// which is invisible when the page is closed -- and registration also
    /// runs at startup, where the page never is open. A toast reaches the user
    /// on any screen.
    hotkey_failure_title {
        ja: "ホットキーを登録できませんでした",
        en: "The shortcut could not be registered"
    }

    export_directory_change { ja: "変更", en: "Change" }
    directory_open { ja: "開く", en: "Open" }
    buffer_directory_label { ja: "録画バッファフォルダ", en: "Recording buffer folder" }
    /// Task2970. No confirmation dialog behind this one, unlike the buffer
    /// row: changing it moves nothing, it only points the next save somewhere
    /// else.
    clip_directory_label { ja: "クリップの保存先", en: "Clip folder" }
    /// Task3030 (round18 design §4-4): the default is a folder under the
    /// buffer root, and both the save and the list side now create it, so
    /// the row can say so instead of silently having a fixed default the
    /// user could hit as missing.
    clip_directory_sub {
        ja: "無ければ自動で作られます",
        en: "Created automatically if it does not exist"
    }
    /// Round5 §7-C: the buffer folder stopped being read-only, so it needs the
    /// export row's vocabulary -- and one word the export row does not,
    /// because this is the only path that can be refused.
    buffer_directory_change_blocked {
        ja: "録画中は変更できません。キャプチャを停止してから変更してください。",
        en: "Cannot be changed while recording. Stop the capture first."
    }
    /// Shared by both path rows, like `directory_open`.
    directory_reset { ja: "規定値に戻す", en: "Reset to default" }
    buffer_directory_reset_done {
        ja: "録画バッファフォルダを規定値に戻しました。以前のフォルダにある録画は移動しません。",
        en: "The recording buffer folder is back to its default. Recordings in the old folder stay where they are."
    }
    buffer_directory_confirm_title {
        ja: "録画バッファフォルダを変更しますか？",
        en: "Change the recording buffer folder?"
    }
    /// The consequence worth naming: the history lists what is under the
    /// buffer root, so moving the root moves what the history can see. Nothing
    /// is copied.
    buffer_directory_confirm_lead {
        ja: "これ以降の録画は新しい場所に保存され、履歴はその場所を走査します。既存の録画は移動せず、元の場所に残ります。",
        en: "New recordings go to the new folder and the history scans it. Existing recordings are not moved and stay where they are."
    }
    buffer_directory_confirm_ok { ja: "変更する", en: "Change" }
    buffer_directory_change_error {
        ja: "録画バッファフォルダを変更できませんでした。",
        en: "The recording buffer folder could not be changed."
    }
    open_directory_error { ja: "フォルダを開けませんでした。", en: "The folder could not be opened." }
    /// Reported by the two low-level reveal paths (task1150). Shorter than
    /// `open_directory_error`, which is the settings row's own sentence.
    open_directory_failed { ja: "フォルダを開けません", en: "The folder cannot be opened" }
    /// A machine where `GlobalHotKeyManager` itself refuses to start.
    hotkeys_unavailable {
        ja: "グローバルホットキーを利用できません",
        en: "Global hotkeys are not available on this machine"
    }

    // ---------- インストール (task4140) ----------
    //
    // The group exists only on a machine where something the installer
    // registers is missing, and it holds one row that states that. Nothing in
    // it is pressable: 「今すぐ登録する」 is a separate task, and this side of
    // it only reads the registry.

    group_install { ja: "インストール", en: "Installation" }
    /// The two features that die with no signal at all when the installer's
    /// registration is missing, named by what the user would notice rather
    /// than by what is actually absent -- 「AUMID」 and 「lvb_thumb.dll」 name
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
    /// sentence this row cannot support, so the last clause says so outright.
    install_unregistered_reason {
        ja: concat!(
            "インストーラが登録するキーが見つかりません。一度もインストールしていない可能性があります。",
            "インストーラで入れ直すと有効になります。",
            "ここで分かるのは登録の有無だけで、登録済みでも動かない場合は分かりません。",
        ),
        en: concat!(
            "The keys the installer writes are missing, so this machine may never have been installed. ",
            "Running the installer again turns these back on. ",
            "This only tells you whether the registration is there -- not whether one that is there actually works.",
        )
    }
}

/// The default buffer root, shown when the setting is unset. A path, not
/// wording -- it is the same on every machine in every language.
pub const BUFFER_DIRECTORY_PATH: &str = "%LOCALAPPDATA%\\Liveback\\buffer";

/// `ja.settings.hotkeyRegistrationFailed`, for a default that was already taken
/// when the app started.
pub fn hotkey_registration_failed(locale: Locale, shortcut: &str) -> String {
    match locale {
        Locale::Ja => format!(
            "ホットキー「{shortcut}」を登録できません。他のアプリケーションとの競合を確認してください。"
        ),
        Locale::En => format!(
            "The shortcut {shortcut} could not be registered. Check for a conflict with another app."
        ),
    }
}

/// Why a hotkey did not take (task195). Only one cause is left since task1090
/// removed the second chord: with nothing to collide with, the user's own pair
/// conflicting is not a state that exists any more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotkeyFailure {
    /// The OS refused the chord, or there is no hotkey manager at all.
    Rejected,
}

pub fn hotkey_failure_detail(locale: Locale, failure: HotkeyFailure) -> &'static str {
    match failure {
        HotkeyFailure::Rejected => hotkey_register_error(locale),
    }
}

// ---------- インストール登録の状態行 (task4140) ----------

/// The 「インストール」 group's one line, or an empty string when there is
/// nothing to say.
///
/// Both arguments carry the same polarity as the `installer_registration` log
/// line the bin writes: `true` is 「the registry holds this registration」. So
/// both `true` -- the installed machine -- returns `""`, and that empty string
/// is the whole of 「登録済みの機械では出ない」: the `.slint` side wraps the
/// group title and the row in one `if` on it, so neither is built. There is no
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
    match locale {
        Locale::Ja => format!("{}は未登録のため無効", missing.join("と")),
        Locale::En => format!("{}: not registered, so disabled", missing.join(" and ")),
    }
}

// ---------- preset rows (round5 §7-B) ----------
//
// Four numeric settings became chips: the values people actually pick, one
// press each, instead of a text field that has to be selected, cleared, typed
// into and committed. Three of them keep a free field for the values in
// between; the frame-rate row does not, because the engine accepts three rates
// and they are all on the list.

/// 5 / 10 / 20 / 60 / 120. The clamp is 5..120, and the row carries a free
/// field for the values between the chips (a 15 or a 90).
///
/// 2026-09-13 (the user): 40 is gone and the free field is back, overriding
/// task1080's chips-only row -- 40 分 was the chip nobody pressed, and the
/// minutes people do mean between the chips have to be typable somewhere.
pub const RETENTION_MINUTES_PRESETS: [i64; 5] = [5, 10, 20, 60, 120];
/// The three rates `crate::settings::FRAME_RATES` accepts.
pub const FRAME_RATE_PRESETS: [i64; 3] = [30, 60, 120];
pub const RETENTION_CAPACITY_PRESETS: [i64; 5] = [5, 10, 20, 50, 100];
pub const SESSION_LIFETIME_PRESETS: [i64; 5] = [1, 3, 7, 30, 90];

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

/// The free field's placeholder: the range it will clamp into, so the bound is
/// visible before the user finds it by being corrected.
pub fn preset_placeholder(min: i64, max: i64) -> String {
    format!("{min}-{max}")
}

pub fn numeric_range(min: i64, max: i64) -> String {
    format!("{min}–{max}")
}

pub fn numeric_out_of_range(locale: Locale, min: i64, max: i64) -> String {
    match locale {
        Locale::Ja => format!("有効範囲は {min}〜{max} です。この値は保存されません。"),
        Locale::En => format!("The range is {min}-{max}. This value will not be saved."),
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

/// What a `PresetRow`'s free field should end up holding after the user types
/// into it: the parsed number clamped into range, or `None` when the text is
/// not a number at all (the field then keeps showing what was typed and the
/// setting is left alone -- the same contract as the existing numeric fields,
/// whose out-of-range message is `numeric_out_of_range`).
pub fn preset_value_from_text(text: &str, min: i64, max: i64) -> Option<i64> {
    text.trim()
        .parse::<i64>()
        .ok()
        .map(|value| value.clamp(min, max))
}

/// What typing a number into a `PresetRow` and pressing Enter means.
///
/// Round2 7-2 and round6 3: an out-of-range draft shows an error and is never
/// committed. Until the mock's `.errrow` was drawn, nothing in the pane could
/// say so, so `preset_value_from_text` silently clamped instead -- 9999 became
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

/// The version line while the app is the newest release there is.
pub fn update_up_to_date(locale: Locale, version: &str) -> String {
    match locale {
        Locale::Ja => format!("最新版です（{version}）"),
        Locale::En => format!("Up to date ({version})"),
    }
}

/// The version line when a newer release exists. Names both numbers: "a new
/// version is available" alone leaves the user unable to tell whether the one
/// they are running is two behind or one.
pub fn update_available(locale: Locale, latest: &str, current: &str) -> String {
    match locale {
        Locale::Ja => format!("{latest} が利用できます（現在 {current}）"),
        Locale::En => format!("{latest} is available (you have {current})"),
    }
}

pub fn hotkey_saved(locale: Locale, shortcuts: &str) -> String {
    match locale {
        Locale::Ja => format!("ホットキーを保存しました（{shortcuts}）"),
        Locale::En => format!("Shortcut saved ({shortcuts})"),
    }
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
fn ctrl_letter(key: char, ctrl: bool, alt: bool, shift: bool, meta: bool) -> Option<char> {
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

/// Builds a Tauri-accelerator-style string (`Ctrl+Shift+R`) from a slint key
/// event. `None` means "keep waiting": a modifier-only press, or a key that has
/// no stable name on both sides (unmapped function-row and media keys, IME
/// strings, control codes). Escape never reaches this -- the caller disarms on
/// it first, same as the React handler.
pub fn compose_shortcut(
    key_text: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> Option<String> {
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
    } else if let Some(letter) = ctrl_letter(key, ctrl, alt, shift, meta) {
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
    } else {
        key.to_uppercase().to_string()
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

/// What the settings file stores under `theme`. `system` is the default and is
/// the only value whose meaning can change between two launches of the same
/// build -- it asks Windows, the same way `language`'s `system` does.
pub const THEME_SYSTEM: &str = "system";
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
mod tests {
    use super::*;

    /// The row is three chips over one string, and the only branch worth a test
    /// is the sentinel: `system` has to follow the OS in *both* directions, and
    /// an unknown value has to behave like the sentinel rather than like dark.
    #[test]
    fn the_theme_row_round_trips_and_system_follows_the_os() {
        for (index, setting) in THEME_SETTINGS.iter().enumerate() {
            assert_eq!(theme_setting(index as i32), *setting);
            assert_eq!(theme_index(setting), index as i32);
        }
        assert_eq!(theme_setting(-1), THEME_SYSTEM);
        assert_eq!(theme_setting(3), THEME_SYSTEM);
        assert_eq!(theme_index("nonesuch"), 0);

        assert!(theme_is_light(THEME_LIGHT, false));
        assert!(!theme_is_light(THEME_DARK, true));
        assert!(theme_is_light(THEME_SYSTEM, true));
        assert!(!theme_is_light(THEME_SYSTEM, false));
        assert!(theme_is_light("nonesuch", true));
        assert!(!theme_is_light("nonesuch", false));
    }

    #[test]
    fn the_hotkey_failure_says_what_to_do_about_it() {
        let rejected = hotkey_failure_detail(Locale::Ja, HotkeyFailure::Rejected);
        // Somebody else holds the chord: the fix is elsewhere.
        assert!(rejected.contains("他のアプリケーション"), "{rejected}");
        // The title carries the "what", the detail the "why" -- a repeat of
        // the title in the second line wastes the toast's other row.
        assert!(
            !rejected.starts_with(hotkey_failure_title(Locale::Ja)),
            "{rejected}"
        );
    }

    /// Task1760: a machine that cannot decode AV1 must not be shown a lit AV1
    /// switch, however the settings file got that way.
    #[test]
    fn the_av1_switch_follows_both_the_setting_and_the_machine() {
        assert_eq!(codec_from_switch(true), RecordingCodec::Av1);
        assert_eq!(codec_from_switch(false), RecordingCodec::H264);
        assert!(codec_switch_on(RecordingCodec::Av1, true));
        assert!(!codec_switch_on(RecordingCodec::H264, true));
        assert!(!codec_switch_on(RecordingCodec::Av1, false));
    }

    #[test]
    fn composes_the_default_marker_hotkey() {
        assert_eq!(
            compose_shortcut("r", true, false, true, false).as_deref(),
            Some("Ctrl+Shift+R")
        );
    }

    #[test]
    fn modifier_order_matches_the_react_version() {
        // React pushes Ctrl, Alt, Shift, Super in that fixed order regardless
        // of press order.
        assert_eq!(
            compose_shortcut("a", true, true, true, true).as_deref(),
            Some("Ctrl+Alt+Shift+Super+A")
        );
    }

    #[test]
    fn single_characters_are_uppercased() {
        assert_eq!(
            compose_shortcut("k", false, true, false, false).as_deref(),
            Some("Alt+K")
        );
        assert_eq!(
            compose_shortcut("5", true, false, false, false).as_deref(),
            Some("Ctrl+5")
        );
    }

    #[test]
    fn named_keys_use_the_react_names() {
        for (text, name) in [
            (" ", "Space"),
            ("\u{0008}", "Backspace"),
            ("\u{0009}", "Tab"),
            ("\u{000a}", "Enter"),
            ("\u{007f}", "Delete"),
            ("\u{F700}", "Up"),
            ("\u{F701}", "Down"),
            ("\u{F702}", "Left"),
            ("\u{F703}", "Right"),
            ("\u{F727}", "Insert"),
            ("\u{F729}", "Home"),
            ("\u{F72B}", "End"),
            ("\u{F72C}", "PageUp"),
            ("\u{F72D}", "PageDown"),
        ] {
            assert_eq!(
                compose_shortcut(text, true, false, false, false).as_deref(),
                Some(format!("Ctrl+{name}").as_str()),
                "key {text:?}"
            );
        }
    }

    #[test]
    fn function_keys_are_numbered() {
        assert_eq!(
            compose_shortcut("\u{F704}", false, false, false, false).as_deref(),
            Some("F1")
        );
        assert_eq!(
            compose_shortcut("\u{F70F}", true, false, false, false).as_deref(),
            Some("Ctrl+F12")
        );
        assert_eq!(
            compose_shortcut("\u{F71B}", false, false, false, false).as_deref(),
            Some("F24")
        );
    }

    #[test]
    fn modifier_only_presses_compose_to_nothing() {
        // Shift, Control, Alt, AltGr, CapsLock, ShiftR, ControlR, Meta, MetaR.
        for code in 0x10u32..=0x18 {
            let text = char::from_u32(code).unwrap().to_string();
            assert_eq!(
                compose_shortcut(&text, code == 0x11, false, code == 0x10, false),
                None,
                "modifier {code:#x}"
            );
        }
    }

    #[test]
    fn capturing_the_chord_the_row_already_has_changes_nothing() {
        assert_eq!(
            commit_capture("Ctrl+Shift+R", "Ctrl+Shift+R"),
            HotkeyCommit::Unchanged
        );
    }

    #[test]
    fn a_fresh_chord_is_applied() {
        assert_eq!(
            commit_capture("Ctrl+Shift+R", "Ctrl+Alt+C"),
            HotkeyCommit::Apply("Ctrl+Alt+C".into())
        );
    }

    #[test]
    fn a_refused_registration_rolls_the_display_back_to_the_previous_chord() {
        assert_eq!(
            displayed_hotkey("Ctrl+Shift+R", "Ctrl+Alt+C", true),
            "Ctrl+Alt+C"
        );
        assert_eq!(
            displayed_hotkey("Ctrl+Shift+R", "Ctrl+Alt+C", false),
            "Ctrl+Shift+R"
        );
    }

    #[test]
    fn ctrl_letters_arrive_as_control_codes_and_are_read_back() {
        // The two defaults, which windows delivers as 0x12 and 0x13 -- the same
        // codes as Alt and AltGr. Neither modifier is held, so they are letters.
        assert_eq!(
            compose_shortcut("\u{0012}", true, false, true, false).as_deref(),
            Some("Ctrl+Shift+R")
        );
        assert_eq!(
            compose_shortcut("\u{0013}", true, false, true, false).as_deref(),
            Some("Ctrl+Shift+S")
        );
        // Both ends of the band, and a code outside the modifier range.
        assert_eq!(
            compose_shortcut("\u{0001}", true, false, false, false).as_deref(),
            Some("Ctrl+A")
        );
        assert_eq!(
            compose_shortcut("\u{001a}", true, false, false, false).as_deref(),
            Some("Ctrl+Z")
        );
        // CapsLock's code has no modifier flag to check, so it reads as T.
        assert_eq!(
            compose_shortcut("\u{0014}", true, false, false, false).as_deref(),
            Some("Ctrl+T")
        );
        // Shift's code is only ambiguous *while Shift is held*.
        assert_eq!(
            compose_shortcut("\u{0010}", true, false, false, false).as_deref(),
            Some("Ctrl+P")
        );
    }

    #[test]
    fn pressing_a_modifier_while_ctrl_is_held_stays_armed() {
        // Greedy conversion would confirm Ctrl+Shift+P the moment Shift went
        // down during a Ctrl+Shift+... capture, which is the whole reason the
        // band is guarded.
        for (code, alt, shift, meta) in [
            (0x10u32, false, true, false), // Shift
            (0x11, false, false, false),   // Control (ctrl is held by definition)
            (0x12, true, false, false),    // Alt
            (0x13, true, false, false),    // AltGr
            (0x15, false, true, false),    // ShiftR
            (0x16, false, false, false),   // ControlR
            (0x17, false, false, true),    // Meta
            (0x18, false, false, true),    // MetaR
        ] {
            let text = char::from_u32(code).unwrap().to_string();
            assert_eq!(
                compose_shortcut(&text, true, alt, shift, meta),
                None,
                "modifier {code:#x} held during a ctrl chord"
            );
        }
    }

    #[test]
    fn the_chords_that_cannot_be_told_apart_stay_armed() {
        // Each of these is indistinguishable from a modifier that is provably
        // down, so it must not be saved as a chord.
        //
        // Dormant on current builds (task145 verification, 2026-08-16): slint
        // 1.17.1 on Windows hands Ctrl+letter through as the *letter*, so these
        // control codes never actually arrive and all four chords are
        // capturable. The guard stays for the layouts, IMEs and future
        // versions that do send them, and this test is what keeps it honest.
        for (text, ctrl, shift, name) in [
            ("\u{0011}", true, false, "Ctrl+Q"),
            ("\u{0016}", true, false, "Ctrl+V"),
            ("\u{0010}", true, true, "Ctrl+Shift+P"),
            ("\u{0015}", true, true, "Ctrl+Shift+U"),
        ] {
            assert_eq!(
                compose_shortcut(text, ctrl, false, shift, false),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn ctrl_keeps_the_named_keys_that_share_a_control_code() {
        // Ctrl+H / Ctrl+I / Ctrl+J collide with Backspace / Tab / Enter. The
        // named reading already worked, so it wins and the letters join the
        // unreachable list.
        assert_eq!(
            compose_shortcut("\u{0008}", true, false, false, false).as_deref(),
            Some("Ctrl+Backspace")
        );
        assert_eq!(
            compose_shortcut("\u{0009}", true, false, false, false).as_deref(),
            Some("Ctrl+Tab")
        );
        assert_eq!(
            compose_shortcut("\u{000a}", true, false, false, false).as_deref(),
            Some("Ctrl+Enter")
        );
    }

    #[test]
    fn invalid_keys_compose_to_nothing() {
        // Unmapped PUA (media keys etc.), raw control codes, IME strings, and
        // an empty text all keep the capture armed.
        assert_eq!(
            compose_shortcut("\u{F7FF}", true, false, false, false),
            None
        );
        assert_eq!(
            compose_shortcut("\u{001b}", false, false, false, false),
            None
        );
        assert_eq!(compose_shortcut("かな", false, false, false, false), None);
        assert_eq!(compose_shortcut("", true, false, false, false), None);
    }

    #[test]
    fn path_tail_keeps_two_segments_when_they_fit() {
        let (head, tail) = split_path_tail("%USERPROFILE%\\Videos\\Liveback");
        assert_eq!(head, "%USERPROFILE%");
        assert_eq!(tail, "\\Videos\\Liveback");
    }

    #[test]
    fn path_tail_falls_back_to_one_segment() {
        let (head, tail) = split_path_tail("C:\\Very\\Long\\FolderNameThatIsLong\\Liveback");
        assert_eq!(tail, "\\Liveback");
        assert_eq!(head, "C:\\Very\\Long\\FolderNameThatIsLong");
    }

    #[test]
    fn path_without_separators_is_all_tail() {
        assert_eq!(split_path_tail("plain"), (String::new(), "plain".into()));
    }

    #[test]
    fn forward_slash_paths_split_too() {
        let (head, tail) = split_path_tail("home/example/videos");
        assert_eq!(head, "home");
        assert_eq!(tail, "/example/videos");
    }

    #[test]
    fn ranges_track_the_validation_module() {
        assert_eq!(RETENTION_MINUTES_RANGE, (5, 120));
        assert_eq!(RETENTION_CAPACITY_GB_RANGE, (1, 2000));
        assert_eq!(SESSION_LIFETIME_DAYS_RANGE, (1, 365));
    }

    /// `PresetRow` value resolution (task168). The chips are the answer for
    /// almost every value; `-1` is how the row says "none of these, look at the
    /// field instead".
    #[test]
    fn a_value_off_the_preset_list_lights_no_chip() {
        let presets = [15, 30, 60];
        assert_eq!(preset_index(&presets, 15), 0);
        assert_eq!(preset_index(&presets, 60), 2);
        assert_eq!(preset_index(&presets, 45), -1);
        assert_eq!(preset_index(&[], 15), -1);
    }

    /// Round6 3's `.errrow`: the row refuses an out-of-range draft rather
    /// than clamping it, so the number the user sees is always the number the
    /// setting holds.
    #[test]
    fn an_out_of_range_draft_is_refused_rather_than_clamped() {
        let (min, max) = RETENTION_CAPACITY_GB_RANGE;
        assert_eq!(preset_draft("20", min, max), PresetDraft::Commit(20));
        assert_eq!(preset_draft(" 20 ", min, max), PresetDraft::Commit(20));
        // Both bounds are reachable, the same way the chips are.
        assert_eq!(
            preset_draft(&min.to_string(), min, max),
            PresetDraft::Commit(min)
        );
        assert_eq!(
            preset_draft(&max.to_string(), min, max),
            PresetDraft::Commit(max)
        );
        // One past either end is refused, not pulled back in.
        assert_eq!(
            preset_draft(&(max + 1).to_string(), min, max),
            PresetDraft::OutOfRange
        );
        assert_eq!(
            preset_draft(&(min - 1).to_string(), min, max),
            PresetDraft::OutOfRange
        );
        assert_eq!(preset_draft("9999", min, max), PresetDraft::OutOfRange);
        // A non-number and an empty field are neither: nothing happens and no
        // error appears.
        assert_eq!(preset_draft("", min, max), PresetDraft::Ignore);
        assert_eq!(preset_draft("abc", min, max), PresetDraft::Ignore);
    }

    /// The error text under the row has to name the same bounds the refusal
    /// used, or it sends the user back to a number that gets refused again.
    #[test]
    fn the_refusal_message_names_the_range_that_refused_it() {
        let (min, max) = RETENTION_CAPACITY_GB_RANGE;
        let message = numeric_out_of_range(Locale::Ja, min, max);
        assert!(message.contains(&min.to_string()), "{message}");
        assert!(message.contains(&max.to_string()), "{message}");
        assert_eq!(
            preset_draft(&max.to_string(), min, max),
            PresetDraft::Commit(max)
        );
    }

    #[test]
    fn a_typed_preset_value_is_clamped_and_a_non_number_is_refused() {
        assert_eq!(preset_value_from_text("45", 5, 120), Some(45));
        assert_eq!(preset_value_from_text("  45  ", 5, 120), Some(45));
        assert_eq!(preset_value_from_text("900", 5, 120), Some(120));
        assert_eq!(preset_value_from_text("0", 5, 120), Some(5));
        // Not a number: the setting is left alone rather than reset to a bound.
        assert_eq!(preset_value_from_text("", 5, 120), None);
        assert_eq!(preset_value_from_text("60fps", 5, 120), None);
    }

    /// Round5 §7-B. Every preset has to be reachable: a chip the clamp would
    /// bounce is a chip that silently does something else when pressed.
    #[test]
    fn every_preset_lands_inside_its_own_clamp() {
        let rows: [(&[i64], (i64, i64)); 4] = [
            (&RETENTION_MINUTES_PRESETS, RETENTION_MINUTES_RANGE),
            (&RETENTION_CAPACITY_PRESETS, RETENTION_CAPACITY_GB_RANGE),
            (&SESSION_LIFETIME_PRESETS, SESSION_LIFETIME_DAYS_RANGE),
            (&FRAME_RATE_PRESETS, (30, 120)),
        ];
        for (presets, (min, max)) in rows {
            for value in presets {
                assert!(
                    (min..=max).contains(value),
                    "preset {value} is outside {min}..={max}"
                );
                // And pressing it lights itself, not some neighbour.
                assert_eq!(
                    preset_index(presets, *value),
                    presets.iter().position(|p| p == value).unwrap() as i32
                );
            }
        }
        // The retention row's chips still bracket its whole clamp, so both
        // bounds stay one press away even though a free field now exists.
        assert_eq!(RETENTION_MINUTES_PRESETS[0], RETENTION_MINUTES_RANGE.0);
        assert_eq!(
            *RETENTION_MINUTES_PRESETS.last().unwrap(),
            RETENTION_MINUTES_RANGE.1
        );
        // 120 fps is on the list (task164 made the engine accept it).
        assert!(FRAME_RATE_PRESETS.contains(&120));
    }

    /// A value written into the settings file by hand lights no chip and shows
    /// up in the free field instead.
    #[test]
    fn a_hand_edited_value_lights_no_chip() {
        assert_eq!(preset_index(&RETENTION_CAPACITY_PRESETS, 37), -1);
        assert_eq!(preset_index(&SESSION_LIFETIME_PRESETS, 45), -1);
        // The two clamp bounds are the interesting off-list values: reachable
        // by typing, not by pressing.
        assert_eq!(preset_index(&RETENTION_CAPACITY_PRESETS, 1), -1);
        assert_eq!(preset_index(&RETENTION_CAPACITY_PRESETS, 2000), -1);
        assert_eq!(preset_index(&SESSION_LIFETIME_PRESETS, 365), -1);
        assert_eq!(
            preset_value_from_text(
                "2500",
                RETENTION_CAPACITY_GB_RANGE.0,
                RETENTION_CAPACITY_GB_RANGE.1
            ),
            Some(2000)
        );
        assert_eq!(
            preset_value_from_text(
                "0",
                SESSION_LIFETIME_DAYS_RANGE.0,
                SESSION_LIFETIME_DAYS_RANGE.1
            ),
            Some(1)
        );
    }

    #[test]
    fn the_free_fields_placeholder_is_the_range_it_clamps_into() {
        assert_eq!(preset_placeholder(1, 2000), "1-2000");
        assert_eq!(preset_placeholder(1, 365), "1-365");
    }

    /// Task1490: the switch is the whole of the 0 sentinel's plumbing, so it
    /// has to survive a round trip -- off writes 0 and keeps the number, on
    /// puts that number back. The regression it guards is off twice: the
    /// second flip must not remember the 0 it just wrote.
    #[test]
    fn a_retention_switch_stores_zero_off_and_restores_the_last_value_on() {
        let (stored, remembered) = retention_toggle(false, 50, 20);
        assert_eq!((stored, remembered), (0, 50));
        // The dimmed row goes on showing what it went off holding.
        assert_eq!(retention_shown(stored, remembered), 50);

        let (stored, remembered) = retention_toggle(false, stored, remembered);
        assert_eq!((stored, remembered), (0, 50));

        assert_eq!(retention_toggle(true, stored, remembered), (50, 50));

        // Nothing ever set: the row comes up on the shipped default.
        assert_eq!(retention_shown(0, 30), 30);
    }

    /// W-9 (round5 §11): the hint says what protection does, and stops
    /// reciting when the sweep runs.
    #[test]
    fn the_retention_hint_names_protection_and_not_the_schedule() {
        let hint = retention_hint(Locale::Ja);
        assert!(hint.contains("保護"), "{hint}");
        assert!(!hint.contains("起動時"), "{hint}");
        assert!(!hint.contains("録画停止後"), "{hint}");
    }
}

#[cfg(test)]
mod task1360_tests {
    use super::*;

    /// Task1360: the English storage hint had `「なし」` left in the middle of
    /// it, quoting a chip that the same screen renders as "None". A test rather
    /// than a fix alone: this is the kind of thing that comes back the next
    /// time someone edits the Japanese and copies it across. The chip itself is
    /// gone (task1490) -- the check that no Japanese rode across with the
    /// rewrite is the part worth keeping.
    #[test]
    fn the_english_storage_hint_has_no_japanese_left_in_it() {
        let english = retention_hint(Locale::En);
        assert!(
            !english.chars().any(is_japanese),
            "English hint still contains Japanese: {english}"
        );
    }

    /// The rest of the settings screen, for the same reason.
    #[test]
    fn no_english_settings_label_contains_japanese() {
        for (name, text) in [
            ("retention_hint", retention_hint(Locale::En)),
            ("retention_capacity_gb", retention_capacity_gb(Locale::En)),
            ("session_lifetime_days", session_lifetime_days(Locale::En)),
            ("group_retention", group_retention(Locale::En)),
            // task4140's group. The line composed *out of* two of these is
            // checked in `task4140_tests` instead, since it is a `String`.
            ("group_install", group_install(Locale::En)),
            (
                "install_feature_notifications",
                install_feature_notifications(Locale::En),
            ),
            (
                "install_feature_thumbnails",
                install_feature_thumbnails(Locale::En),
            ),
            (
                "install_unregistered_reason",
                install_unregistered_reason(Locale::En),
            ),
        ] {
            assert!(
                !text.chars().any(is_japanese),
                "{name} still contains Japanese: {text}"
            );
        }
    }

    /// Kana and the CJK ideographs. Deliberately not "any non-ASCII": an
    /// English string may hold a curly quote or an en dash.
    fn is_japanese(c: char) -> bool {
        matches!(c,
            '\u{3040}'..='\u{30ff}'   // hiragana, katakana
            | '\u{3000}'..='\u{303f}' // CJK punctuation, including 「」
            | '\u{4e00}'..='\u{9fff}' // ideographs
            | '\u{ff00}'..='\u{ffef}' // fullwidth forms
        )
    }
}

#[cfg(test)]
mod task4140_tests {
    use super::*;

    /// The whole of 「登録済みの機械では見出しごと出ない」. The `.slint` side
    /// wraps the group title and the row in one `if` on this string, so an
    /// empty return is what keeps both out of the form -- there is no separate
    /// "hide the group" flag to get out of step with the text.
    #[test]
    fn a_fully_registered_machine_gets_no_line_at_all() {
        for locale in [Locale::Ja, Locale::En] {
            assert_eq!(
                unregistered_features_line(locale, true, true),
                "",
                "{locale:?} produced a line for a machine with both registrations"
            );
        }
    }

    /// Each single-missing case names its own feature and, the half that
    /// actually needs asserting, *not* the other one: a line built by
    /// concatenating both names and then filtering would pass a test that only
    /// checked the name it wanted was present.
    #[test]
    fn one_missing_registration_names_only_that_feature() {
        for locale in [Locale::Ja, Locale::En] {
            let toasts = install_feature_notifications(locale);
            let thumbnails = install_feature_thumbnails(locale);

            let aumid_missing = unregistered_features_line(locale, false, true);
            assert!(
                aumid_missing.contains(toasts),
                "{locale:?} AUMID-only line is missing the notifications name: {aumid_missing}"
            );
            assert!(
                !aumid_missing.contains(thumbnails),
                "{locale:?} AUMID-only line named thumbnails too: {aumid_missing}"
            );

            let thumbnail_missing = unregistered_features_line(locale, true, false);
            assert!(
                thumbnail_missing.contains(thumbnails),
                "{locale:?} handler-only line is missing the thumbnail name: {thumbnail_missing}"
            );
            assert!(
                !thumbnail_missing.contains(toasts),
                "{locale:?} handler-only line named notifications too: {thumbnail_missing}"
            );
        }
    }

    /// Both missing lists both, and the two names are joined rather than
    /// running together -- the join is the one part of this that reads
    /// differently for one name and for two.
    #[test]
    fn both_missing_lists_both_features_joined() {
        for (locale, join) in [(Locale::Ja, "と"), (Locale::En, " and ")] {
            let line = unregistered_features_line(locale, false, false);
            let expected = format!(
                "{}{join}{}",
                install_feature_notifications(locale),
                install_feature_thumbnails(locale)
            );
            assert!(
                line.contains(&expected),
                "{locale:?} did not join the two names with {join:?}: {line}"
            );
        }
    }

    /// AC 3: the row has to carry the state, not only the feature names. The
    /// Japanese is the user's own wording from the 2026-09-09 ruling.
    #[test]
    fn every_line_states_that_the_feature_is_off_because_it_is_unregistered() {
        for (locale, state) in [
            (Locale::Ja, "未登録のため無効"),
            (Locale::En, "not registered"),
        ] {
            for (aumid, handler) in [(false, true), (true, false), (false, false)] {
                let line = unregistered_features_line(locale, aumid, handler);
                assert!(
                    line.contains(state),
                    "{locale:?} ({aumid}, {handler}) does not say {state:?}: {line}"
                );
            }
        }
    }

    /// The composed English line, for the reason `task1360_tests` checks the
    /// static ones: it is assembled from two other strings, so a Japanese name
    /// left in either arrives here.
    #[test]
    fn the_english_line_has_no_japanese_left_in_it() {
        for (aumid, handler) in [(false, true), (true, false), (false, false)] {
            let line = unregistered_features_line(Locale::En, aumid, handler);
            assert!(
                !line.chars().any(|c| matches!(c,
                    '\u{3040}'..='\u{30ff}'
                    | '\u{3000}'..='\u{303f}'
                    | '\u{4e00}'..='\u{9fff}'
                    | '\u{ff00}'..='\u{ffef}')),
                "English line ({aumid}, {handler}) still contains Japanese: {line}"
            );
        }
    }

    /// The reason line must not promise that a *present* registration works:
    /// the check reads one key per feature and cannot see a registration
    /// pointing at a file that is gone (task1250). Asserted as the presence of
    /// the sentence that admits it, in both languages.
    #[test]
    fn the_reason_admits_it_cannot_detect_a_broken_install() {
        assert!(
            install_unregistered_reason(Locale::Ja)
                .contains("登録済みでも動かない場合は分かりません"),
            "the Japanese reason dropped the clause about a registered-but-dead install"
        );
        assert!(
            install_unregistered_reason(Locale::En)
                .contains("not whether one that is there actually works"),
            "the English reason dropped the clause about a registered-but-dead install"
        );
    }
}
