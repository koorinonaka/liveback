//! The shell's transient messages (task169), which replaced the status bar
//! and the diagnostics drawer round4 put at the bottom of every screen.
//!
//! A stack of at most `MAX_STACK`, bottom-right, newest at the bottom
//! (t260927-73fd). It used to be one at a time, latest wins; the DS Toast
//! (`project/components/Toast/README.md`) says otherwise: 「右下に縦に積む。
//! 3件を超えたら古いものから消す」. One slot also meant a success landing
//! right after a warning pushed the warning off before anyone read it. A
//! result fades after `HOLD_MS`; a warning or an error stays until the user
//! closes it or three newer ones push it out, because an error nobody saw is
//! the one that mattered.

/// Four seconds (t260928-983d, the user's call: round4 A3 and the DS's
/// 「成功は6秒で消える」 said six, and six read as long). Still long enough to
/// see a フォルダで表示 button and press it; the path itself is in the folder it
/// opens. A card can ask for a different hold with `Toast::set_hold_ms`.
pub const HOLD_MS: u64 = 4_000;

/// DS: 「3件を超えたら古いものから消す」.
pub const MAX_STACK: usize = 3;

/// Elides `text` from the head down to `keep_chars`, so the tail -- the half
/// that names the file or the folder -- survives. Shared by
/// `ui_state::export`'s toast detail and `ui_state::auto_capture`'s folder
/// rule row (identical bodies, verified 2026-09-07 follow-up of task3670);
/// each keeps its own width constant since the two rows are unrelated UI
/// elements that only happen to agree on 44 today.
pub(crate) fn keep_the_tail(text: &str, keep_chars: usize) -> String {
    let chars = text.chars().count();
    if chars <= keep_chars {
        return text.to_owned();
    }
    format!(
        "…{}",
        text.chars().skip(chars - keep_chars).collect::<String>()
    )
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToastVariant {
    #[default]
    Info,
    Success,
    Error,
    /// round7 §2-11: the sweep deleted sessions on its own. Nothing failed, so
    /// it is not an error -- but it is not a result the user asked for either,
    /// and it stays until they have seen it.
    Warning,
}

impl ToastVariant {
    /// The `.slint` side takes an int, like every other enum crossing that
    /// boundary.
    pub fn index(self) -> i32 {
        match self {
            ToastVariant::Info => 0,
            ToastVariant::Success => 1,
            ToastVariant::Error => 2,
            ToastVariant::Warning => 3,
        }
    }
}

/// One card in the stack. `id` is what the `.slint` side hands back when its
/// close or action button is pressed; ids only grow, so a later card always
/// has the larger one.
#[derive(Debug)]
pub struct Entry {
    id: i32,
    title: String,
    detail: String,
    variant: ToastVariant,
    shown_at_ms: u64,
    action: String,
    action_path: String,
    /// Set by `Toast::set_hold_ms`: this card fades after it whatever its
    /// variant.
    hold_ms: Option<u64>,
    /// Set by `Toast::set_detail_mono`: the detail is a path, drawn as one
    /// mono line (t260928-5f3b, DS Toast 「パスなら `mono`」).
    detail_mono: bool,
}

impl Entry {
    pub fn id(&self) -> i32 {
        self.id
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn detail(&self) -> &str {
        &self.detail
    }

    pub fn variant(&self) -> ToastVariant {
        self.variant
    }

    /// The button label, empty for the usual toast that only reports.
    pub fn action(&self) -> &str {
        &self.action
    }

    /// What this toast's action button opens (task1040).
    pub fn action_path(&self) -> &str {
        &self.action_path
    }

    pub fn detail_mono(&self) -> bool {
        self.detail_mono
    }

    /// How long this card stays, for the draining line along its foot: its
    /// set hold, else `HOLD_MS` for a result, and 0 for a card that waits to
    /// be closed (a warning or an error) -- no clock to draw.
    pub fn fade_ms(&self) -> u64 {
        match self.hold_ms {
            Some(hold) => hold,
            None if matches!(self.variant, ToastVariant::Error | ToastVariant::Warning) => 0,
            None => HOLD_MS,
        }
    }

    pub fn visible(&self, now_ms: u64) -> bool {
        let shown_for = now_ms.saturating_sub(self.shown_at_ms);
        if let Some(hold) = self.hold_ms {
            return shown_for < hold;
        }
        // A warning waits for the user the same way an error does: what it
        // reports (sessions deleted, disk freed) cannot be undone, so fading it
        // out after a few seconds would be the app hiding it.
        matches!(self.variant, ToastVariant::Error | ToastVariant::Warning) || shown_for < HOLD_MS
    }
}

/// The stack. Named `Toast` still because every producer holds it as the one
/// corner it writes to.
#[derive(Debug, Default)]
pub struct Toast {
    entries: Vec<Entry>,
    last_id: i32,
    /// The card the last `publish` added or matched: where `set_action_path`
    /// lands.
    current: i32,
}

impl Toast {
    /// Reports whether this actually added a card. A poll that re-sends a
    /// message already in the stack must not restart its hold or stack
    /// a copy, or a status the app repeats every second would never fade.
    pub fn publish(
        &mut self,
        title: &str,
        detail: &str,
        variant: ToastVariant,
        action: &str,
        now_ms: u64,
    ) -> bool {
        if let Some(same) = self
            .entries
            .iter()
            .find(|e| e.title == title && e.detail == detail && e.variant == variant)
        {
            self.current = same.id;
            return false;
        }
        if self.entries.len() >= MAX_STACK {
            self.entries.remove(0);
        }
        self.last_id += 1;
        self.current = self.last_id;
        self.entries.push(Entry {
            id: self.last_id,
            title: title.to_owned(),
            detail: detail.to_owned(),
            variant,
            shown_at_ms: now_ms,
            action: action.to_owned(),
            // Never carried over: each toast has its own フォルダで表示
            // target, or none at all (task1040).
            action_path: String::new(),
            hold_ms: None,
            detail_mono: false,
        });
        true
    }

    /// Set after `publish`, like `set_action_path`, by a producer whose card
    /// should not keep its variant's hold: the marker hotkey's acknowledgement
    /// (two seconds), and the auto-stop `Warning`, which would otherwise stay
    /// until closed (t260928-983d).
    pub fn set_hold_ms(&mut self, hold_ms: u64) {
        let current = self.current;
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == current) {
            entry.hold_ms = Some(hold_ms);
        }
    }

    /// Set after `publish`, by the one producer that has somewhere to point --
    /// an export's file, or a saved screenshot. The dispatcher used to read the
    /// export panel's own list, which meant every フォルダで表示 opened the last
    /// export.
    pub fn set_action_path(&mut self, path: &str) {
        let current = self.current;
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == current) {
            entry.action_path = path.to_owned();
        }
    }

    /// Set after `publish`, like `set_action_path`, by a producer whose detail
    /// is a path: an export's file, a saved screenshot's name.
    pub fn set_detail_mono(&mut self) {
        let current = self.current;
        if let Some(entry) = self.entries.iter_mut().find(|e| e.id == current) {
            entry.detail_mono = true;
        }
    }

    pub fn get(&self, id: i32) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn dismiss(&mut self, id: i32) {
        self.entries.retain(|e| e.id != id);
    }

    /// Drops the results whose hold is up; reports whether any went.
    pub fn expire(&mut self, now_ms: u64) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.visible(now_ms));
        self.entries.len() != before
    }

    /// Oldest first, so the newest paints at the bottom.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn titles(toast: &Toast) -> Vec<&str> {
        toast.entries().iter().map(Entry::title).collect()
    }

    #[test]
    fn a_result_fades_after_the_hold_and_a_repeat_does_not_restart_it() {
        let mut toast = Toast::default();
        assert!(toast.publish("保存しました", "", ToastVariant::Success, "", 0));
        // The poll re-sends the same message three seconds in.
        assert!(!toast.publish("保存しました", "", ToastVariant::Success, "", 3_000));
        assert_eq!(toast.entries().len(), 1, "a repeat is not a second card");
        // Still keyed to the original instant, so it goes at `HOLD_MS`, not three seconds later.
        assert!(!toast.expire(HOLD_MS - 1));
        assert!(toast.expire(HOLD_MS));
        assert!(toast.entries().is_empty());
    }

    /// The DS stack (t260927-73fd) replaced "latest wins": the error is not
    /// displaced by the result that follows it, and the result's own hold
    /// takes only the result away.
    #[test]
    fn an_error_stays_beside_the_result_that_follows_it() {
        let mut toast = Toast::default();
        toast.publish("保存できません", "readonly", ToastVariant::Error, "", 0);
        assert!(toast.publish("保存しました", "", ToastVariant::Success, "", 60_000));
        assert_eq!(titles(&toast), ["保存できません", "保存しました"]);
        toast.expire(60_000 + HOLD_MS - 1);
        assert_eq!(titles(&toast), ["保存できません", "保存しました"]);
        toast.expire(60_000 + HOLD_MS);
        assert_eq!(titles(&toast), ["保存できません"]);
    }

    /// Per card, not per stack: the success goes at `HOLD_MS` while the
    /// warning and the error published beside it stay (DS 「注意と失敗は閉じるまで残る」;
    /// the six seconds the DS gives a success are four since t260928-983d).
    #[test]
    fn each_card_keeps_its_own_hold() {
        let mut toast = Toast::default();
        toast.publish(
            "2 件のセッションを破棄しました",
            "",
            ToastVariant::Success,
            "",
            0,
        );
        toast.publish(
            "古いセッションを削除しました",
            "",
            ToastVariant::Warning,
            "",
            1_000,
        );
        toast.publish("破棄に失敗しました", "", ToastVariant::Error, "", 2_000);
        toast.publish("情報", "", ToastVariant::Info, "", 3_000);
        // The fourth pushed the oldest out; the rest are all still there.
        assert_eq!(
            titles(&toast),
            ["古いセッションを削除しました", "破棄に失敗しました", "情報"]
        );
        toast.expire(600_000);
        assert_eq!(
            titles(&toast),
            ["古いセッションを削除しました", "破棄に失敗しました"]
        );
    }

    /// t260928-983d: the user asked for four seconds, down from six.
    #[test]
    fn a_result_holds_for_four_seconds() {
        assert_eq!(HOLD_MS, 4_000);
        let mut toast = Toast::default();
        toast.publish("保存しました", "", ToastVariant::Success, "", 0);
        toast.publish("情報", "", ToastVariant::Info, "", 0);
        assert!(toast.entries().iter().all(|e| e.visible(3_999)));
        assert!(toast.entries().iter().all(|e| !e.visible(4_000)));
    }

    /// t260928-983d: a set hold overrides the variant -- the marker's two
    /// seconds on a success, the auto-stop's four on a warning -- and only on
    /// the card it was set on. The warning and the error beside them, with no
    /// hold of their own, still stay until closed.
    #[test]
    fn a_set_hold_fades_only_its_own_card() {
        let mut toast = Toast::default();
        toast.publish(
            "古いセッションを削除しました",
            "",
            ToastVariant::Warning,
            "",
            0,
        );
        toast.publish(
            "マーカーを追加しました",
            "0:12",
            ToastVariant::Success,
            "",
            0,
        );
        toast.set_hold_ms(2_000);
        toast.publish("録画が停止しました", "", ToastVariant::Warning, "", 0);
        toast.set_hold_ms(4_000);
        assert_eq!(toast.entries().len(), 3);
        toast.expire(1_999);
        assert_eq!(toast.entries().len(), 3);
        toast.expire(2_000);
        assert_eq!(
            titles(&toast),
            ["古いセッションを削除しました", "録画が停止しました"]
        );
        toast.expire(3_999);
        assert_eq!(toast.entries().len(), 2);
        toast.expire(4_000);
        assert_eq!(titles(&toast), ["古いセッションを削除しました"]);
        toast.publish("保存できません", "", ToastVariant::Error, "", 0);
        toast.expire(60_000);
        assert_eq!(
            titles(&toast),
            ["古いセッションを削除しました", "保存できません"]
        );
    }

    #[test]
    fn a_fourth_card_pushes_out_the_oldest() {
        let mut toast = Toast::default();
        for (i, title) in ["1", "2", "3", "4"].into_iter().enumerate() {
            toast.publish(title, "", ToastVariant::Error, "", i as u64);
        }
        assert_eq!(titles(&toast), ["2", "3", "4"]);
        let ids: Vec<i32> = toast.entries().iter().map(Entry::id).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]), "{ids:?}");
    }

    #[test]
    fn closing_one_card_leaves_the_others() {
        let mut toast = Toast::default();
        toast.publish("a", "", ToastVariant::Error, "", 0);
        toast.publish("b", "", ToastVariant::Error, "", 0);
        let first = toast.entries()[0].id();
        toast.dismiss(first);
        assert_eq!(titles(&toast), ["b"]);
        assert!(toast.get(first).is_none());
    }

    #[test]
    fn an_action_target_belongs_to_the_toast_that_set_it() {
        let mut toast = Toast::default();
        toast.publish(
            "書き出しました",
            "clip.mp4",
            ToastVariant::Success,
            "開く",
            0,
        );
        toast.set_action_path(r"D:\clips\clip.mp4");
        let export = toast.entries()[0].id();
        // The next toast along must not inherit somewhere to go.
        toast.publish(
            "静止画を保存しました",
            "shot.png",
            ToastVariant::Success,
            "開く",
            1,
        );
        let shot = toast.entries()[1].id();
        assert_eq!(toast.get(shot).unwrap().action_path(), "");
        assert_eq!(
            toast.get(export).unwrap().action_path(),
            r"D:\clips\clip.mp4"
        );
        // A repeat of the *older* card points the next set at that card.
        assert!(!toast.publish(
            "書き出しました",
            "clip.mp4",
            ToastVariant::Success,
            "開く",
            2
        ));
        toast.set_action_path(r"D:\clips\clip2.mp4");
        assert_eq!(
            toast.get(export).unwrap().action_path(),
            r"D:\clips\clip2.mp4"
        );
        assert_eq!(toast.get(shot).unwrap().action_path(), "");
        assert_eq!(toast.get(export).unwrap().action(), "開く");
    }

    /// t260928-5f3b: the draining line follows the card's own hold -- the
    /// 4 s result, a set 2 s -- and a card that waits to be closed has none.
    #[test]
    fn the_fade_line_follows_the_cards_own_hold() {
        let mut toast = Toast::default();
        toast.publish("保存しました", "", ToastVariant::Success, "", 0);
        toast.publish(
            "マーカーを追加しました",
            "0:12",
            ToastVariant::Success,
            "",
            0,
        );
        toast.set_hold_ms(2_000);
        toast.publish("書き出せませんでした", "", ToastVariant::Error, "", 0);
        let fades: Vec<u64> = toast.entries().iter().map(Entry::fade_ms).collect();
        assert_eq!(fades, [HOLD_MS, 2_000, 0]);
        assert_eq!(HOLD_MS, 4_000);
        toast.set_hold_ms(4_000);
        assert_eq!(toast.entries()[2].fade_ms(), 4_000);
    }

    /// t260928-5f3b: mono belongs to the card it was set on, never carried to
    /// the next one.
    #[test]
    fn a_path_detail_is_mono_only_on_its_own_card() {
        let mut toast = Toast::default();
        toast.publish(
            "書き出しました",
            r"…\clip.mp4",
            ToastVariant::Success,
            "開く",
            0,
        );
        toast.set_detail_mono();
        toast.publish(
            "書き出せませんでした",
            "ディスクの空きが足りません",
            ToastVariant::Error,
            "",
            1,
        );
        let mono: Vec<bool> = toast.entries().iter().map(Entry::detail_mono).collect();
        assert_eq!(mono, [true, false]);
    }

    /// Same text, different severity, is still news: an operation that started
    /// as a notice and ended as a failure must not be swallowed as a repeat.
    #[test]
    fn a_variant_change_alone_is_a_new_toast() {
        let mut toast = Toast::default();
        toast.publish("書き出し", "", ToastVariant::Info, "", 0);
        assert!(toast.publish("書き出し", "", ToastVariant::Error, "", 1_000));
        toast.expire(30_000);
        assert_eq!(titles(&toast), ["書き出し"]);
        assert_eq!(toast.entries()[0].variant(), ToastVariant::Error);
    }
}
