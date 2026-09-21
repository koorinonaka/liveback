//! The shell's one transient message (task169), which replaced the status bar
//! and the diagnostics drawer round4 put at the bottom of every screen.
//!
//! One at a time, latest wins: two stacked toasts would be a status bar with
//! extra steps, and the thing they replaced was removed for taking permanent
//! room. A result fades after `HOLD_MS`; an error stays until something else
//! displaces it or the user closes it, because an error nobody saw is the one
//! that mattered.

/// Round4 A3's six seconds, kept: long enough to read a path, short enough that
/// the corner is empty again before the user wonders whether it is stuck.
pub const HOLD_MS: u64 = 6_000;

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

#[derive(Debug, Default)]
pub struct Toast {
    title: String,
    detail: String,
    variant: ToastVariant,
    shown_at_ms: u64,
    action_path: String,
}

impl Toast {
    /// Reports whether this actually started a new hold. A poll that re-sends
    /// the message already showing must not restart its six seconds, or a
    /// status the app repeats every second would never fade.
    pub fn publish(
        &mut self,
        title: &str,
        detail: &str,
        variant: ToastVariant,
        now_ms: u64,
    ) -> bool {
        if self.title == title && self.detail == detail && self.variant == variant {
            return false;
        }
        self.title = title.to_owned();
        self.detail = detail.to_owned();
        self.variant = variant;
        self.shown_at_ms = now_ms;
        // Cleared, never carried over: the toast that replaces this one has
        // its own フォルダで表示 target, or none at all (task1040).
        self.action_path.clear();
        true
    }

    /// What this toast's action button opens. Set after `publish`, by the one
    /// producer that has somewhere to point -- an export's file, or a saved
    /// screenshot. The dispatcher used to read the export panel's own list,
    /// which meant every フォルダで表示 opened the last export.
    pub fn set_action_path(&mut self, path: &str) {
        self.action_path = path.to_owned();
    }

    pub fn action_path(&self) -> &str {
        &self.action_path
    }

    pub fn dismiss(&mut self) {
        self.title.clear();
        self.detail.clear();
        self.variant = ToastVariant::Info;
        self.shown_at_ms = 0;
        self.action_path.clear();
    }

    pub fn visible(&self, now_ms: u64) -> bool {
        if self.title.is_empty() {
            return false;
        }
        // A warning waits for the user the same way an error does: what it
        // reports (sessions deleted, disk freed) cannot be undone, so fading it
        // out after six seconds would be the app hiding it.
        matches!(self.variant, ToastVariant::Error | ToastVariant::Warning)
            || now_ms.saturating_sub(self.shown_at_ms) < HOLD_MS
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_fades_after_the_hold_and_a_repeat_does_not_restart_it() {
        let mut toast = Toast::default();
        assert!(toast.publish("保存しました", "", ToastVariant::Success, 0));
        // The poll re-sends the same message five seconds in.
        assert!(!toast.publish("保存しました", "", ToastVariant::Success, 5_000));
        // Still keyed to the original instant, so it goes at 6s, not at 11s.
        assert!(toast.visible(5_999));
        assert!(!toast.visible(6_000));
    }

    #[test]
    fn an_error_stays_until_something_replaces_it() {
        let mut toast = Toast::default();
        toast.publish("保存できません", "readonly", ToastVariant::Error, 0);
        assert!(toast.visible(60_000));
        // Latest wins, and the replacement gets its own hold.
        assert!(toast.publish("保存しました", "", ToastVariant::Success, 60_000));
        assert!(toast.visible(60_000));
        assert!(!toast.visible(66_000));
    }

    #[test]
    fn dismissing_empties_the_corner() {
        let mut toast = Toast::default();
        toast.publish("保存できません", "", ToastVariant::Error, 0);
        toast.dismiss();
        assert!(!toast.visible(0));
        assert_eq!(toast.title(), "");
    }

    #[test]
    fn an_action_target_belongs_to_the_toast_that_set_it() {
        let mut toast = Toast::default();
        toast.publish("書き出しました", "clip.mp4", ToastVariant::Success, 0);
        toast.set_action_path(r"D:\clips\clip.mp4");
        assert_eq!(toast.action_path(), r"D:\clips\clip.mp4");
        // The next toast along must not inherit somewhere to go.
        toast.publish("静止画を保存しました", "shot.png", ToastVariant::Success, 1);
        assert_eq!(toast.action_path(), "");
        // A repeat is the same toast, so its target survives.
        toast.set_action_path(r"D:\clips\shot.png");
        assert!(!toast.publish("静止画を保存しました", "shot.png", ToastVariant::Success, 2));
        assert_eq!(toast.action_path(), r"D:\clips\shot.png");
        toast.dismiss();
        assert_eq!(toast.action_path(), "");
    }

    /// Same text, different severity, is still news: an operation that started
    /// as a notice and ended as a failure must not be swallowed as a repeat.
    #[test]
    fn a_variant_change_alone_is_a_new_toast() {
        let mut toast = Toast::default();
        toast.publish("書き出し", "", ToastVariant::Info, 0);
        assert!(toast.publish("書き出し", "", ToastVariant::Error, 1_000));
        assert!(toast.visible(30_000), "it is an error now, so it stays");
    }
}
