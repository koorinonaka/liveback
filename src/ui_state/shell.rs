//! Where the shell is: which pane the rail has selected, and whether the target
//! picker is open on top of it (task169).
//!
//! Round5 puts the rail back (task142 had collapsed it into title-bar buttons
//! over a single review page), so "where am I" is a pane again. The one thing
//! that is *not* a pane is 対象: picking a target is a modal that can open over
//! any of them, which is why it is a separate bool rather than a fourth variant.

/// The panes the rail switches between. 確認 is the default -- the app is
/// for looking at what was recorded, and everything else supports that.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pane {
    #[default]
    Review,
    Sessions,
    Settings,
    /// クリップ (task2980). Appended rather than slotted in beside 履歴, which
    /// is where it sits in the rail: `Pane::Settings` has to stay index 2. Two
    /// places in `ui/app.slint` write that 2 as a literal -- the hotkey blur on
    /// leaving 設定 and the pane's own `if` -- and renumbering would break both
    /// silently. Only the *rail* positions move.
    Clips,
}

impl Pane {
    /// The `.slint` side carries this as an int: slint has no enums shared with
    /// Rust, and an int keeps the view a pure mapping.
    pub fn index(self) -> i32 {
        match self {
            Pane::Review => 0,
            Pane::Sessions => 1,
            Pane::Settings => 2,
            Pane::Clips => 3,
        }
    }

    pub fn from_index(index: i32) -> Pane {
        match index {
            1 => Pane::Sessions,
            2 => Pane::Settings,
            3 => Pane::Clips,
            _ => Pane::Review,
        }
    }
}

/// The width of the window's resize band, in **physical** pixels, for a window
/// at `dpi` (task4120).
///
/// This is the band the user *gets*, not the number handed to slint:
/// `resize_band_slint_px` below is the supply, and since slint 1.18 scales that
/// supply itself the two are only equal at 100%. This one is what the drag
/// measurements and the `resize band set` log line speak in -- task4120's
/// acceptance criteria are stated in physical inset px. The old flat `6px` was
/// 4 logical px at 150% -- half the frame every other window on the desktop
/// has, which is why the edges were hard to grab.
///
/// 8 logical px is `SM_CXSIZEFRAME` + `SM_CXPADDEDBORDER`, i.e. Windows' own
/// frame. Computed rather than read back with `GetSystemMetricsForDpi`, which
/// returns the same 4+4 at 96 and scales the same way: those are syscalls, and
/// the convention here is that testable UI logic lives in `ui_state` as a pure
/// function rather than in the bin (KNOWLEDGE「テストの配置と書き方の規約」).
pub fn resize_band_physical_px(dpi: u32) -> f32 {
    RESIZE_BAND_LOGICAL_PX * dpi as f32 / 96.0
}

/// The number to hand `AppWindow::resize-band-width`, in **logical** px.
///
/// slint 1.18's winit backend scales `resize-border-width` itself before
/// comparing it against physical cursor coordinates
/// (`i-slint-backend-winit-1.18.0/drag_resize_window.rs:15`:
/// `f64::from(border_width.get()) * window.scale_factor()`), so the supply is
/// logical and the same number at every scale. 1.17.1 took the raw value
/// against physical coordinates, which is why task4120 pre-scaled it here; the
/// 1.18 upgrade (486ac99f) left that pre-scaling in place and the band came out
/// `8 * 1.5 * 1.5` = 18 physical px on a 150% desk.
///
/// `dpi` is taken and ignored on purpose. Both callers have one, the band the
/// user *gets* does depend on it (`resize_band_physical_px` above is that
/// number, and it is what the drag measurements read), and a signature without
/// the dpi is an invitation to scale at the call site again -- which is the bug
/// this function exists to name.
pub fn resize_band_slint_px(_dpi: u32) -> f32 {
    RESIZE_BAND_LOGICAL_PX
}

/// The band's width in **logical** px -- the one number both sides of the
/// window edge are allowed to know (task4320).
///
/// Two things need it, and since slint 1.18 they need it in the *same* unit:
/// the window's `resize-border-width` (through `resize_band_slint_px`, which
/// 1.18 scales for winit itself) and the layout's `Tokens.resize-band-inset`,
/// which pulls a flush-right `AppScroll`'s lane this far in from the window edge
/// so the thumb is not sitting in the band. A `length` in a `.slint` layout is
/// logical by definition, so feeding the inset the physical number would move
/// the lane 12 logical px at dpi=144 -- which is why `resize_band_physical_px`
/// must never be the one that reaches either property. Before this const the 8
/// was written twice; the scrollbar overlapped the band because only one of them
/// had been looked at.
pub const RESIZE_BAND_LOGICAL_PX: f32 = 8.0;

/// What pressing a rail button does. 対象 sits between 確認 and 履歴 in the
/// rail but is an action, not a destination: it opens the picker over whatever
/// is showing and leaves the pane alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RailAction {
    Show(Pane),
    OpenPicker,
}

/// Rail order, top to bottom: 確認 / 対象 / 履歴 / クリップ, then 設定 at the
/// foot. The rail position and `Pane::index` are deliberately different numbers
/// (task2980) -- see `Pane::Clips`.
pub fn rail_action(index: i32) -> RailAction {
    match index {
        1 => RailAction::OpenPicker,
        2 => RailAction::Show(Pane::Sessions),
        3 => RailAction::Show(Pane::Clips),
        4 => RailAction::Show(Pane::Settings),
        _ => RailAction::Show(Pane::Review),
    }
}

/// Which rail item draws as `current`. The picker owns the mark while it is
/// open -- it is what the user is looking at -- and hands it straight back to
/// the pane underneath on close, which is why the pane is never disturbed.
pub fn rail_current(pane: Pane, picker_open: bool) -> i32 {
    if picker_open {
        return 1;
    }
    match pane {
        Pane::Review => 0,
        Pane::Sessions => 2,
        Pane::Clips => 3,
        Pane::Settings => 4,
    }
}

/// Escape, the close button and a scrim click all mean the same thing, and all
/// of them mean it only while the picker is open. `false` lets the key fall
/// through to whatever is underneath instead of being swallowed.
pub fn closes_picker(picker_open: bool) -> bool {
    picker_open
}

/// The history pane asks for a fresh session list every time it opens, the way
/// the React overlay's mount effect did -- but only on the transition into it,
/// never on a re-render while it is already showing.
pub fn opens_sessions(previous: Pane, next: Pane) -> bool {
    next == Pane::Sessions && previous != Pane::Sessions
}

/// The clip list is a directory scan with no index behind it (task2980), so it
/// is re-taken on the way into the pane the same way the session list is --
/// which is also what makes a file added or deleted in Explorer show up without
/// restarting the app.
pub fn opens_clips(previous: Pane, next: Pane) -> bool {
    next == Pane::Clips && previous != Pane::Clips
}

/// The session count belongs to the screen showing the sessions. It moved from
/// the (now deleted) status bar to the history toolbar, but the rule is the
/// same one: anything else takes the tally down with it.
pub fn keeps_session_count(next: Pane) -> bool {
    next == Pane::Sessions
}

/// What the window draws while a file is held over it (task870, round7 §2-13).
/// Dropping already works (task600); until this, nothing said so while the file
/// was still in the air.
///
/// The name rides along only when the file is one we can open: the refusal is
/// about the extension, not about that particular file, so it says which
/// extension instead of repeating a name the pointer is already holding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DropHint {
    pub accepted: bool,
    pub name: String,
}

crate::tr! {
    drop_accept_title { ja: "ドロップして開く", en: "Drop to open" }
    drop_reject_title { ja: "このファイルは開けません", en: "This file cannot be opened" }
    drop_reject_detail {
        ja: ".lvb ファイルのみ読み込めます",
        en: "Only .lvb files can be opened"
    }
}

/// Same test the drop itself applies (`is_container_path`), so what the overlay
/// promises and what the drop accepts can never drift apart.
pub fn drop_hint(path: &std::path::Path) -> DropHint {
    if !crate::ring_buffer::is_container_path(path) {
        return DropHint::default();
    }
    DropHint {
        accepted: true,
        name: path
            .file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned(),
    }
}

/// Which of the five inline editors is open, as the shell sees them (task3640).
///
/// The pane-local flags are ints because that is how the panes carry "which
/// row", with -1 for none; the caller passes the raw values so the -1 rule is
/// pinned here rather than at each site.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InlineEdits {
    /// The history pane's row note (`sessions-editing-row`).
    pub sessions_row: i32,
    /// The clip grid's comment (`clips-editing-row`).
    pub clips_row: i32,
    /// The review panel's marker rename (`ReviewVm.marker-editing`).
    pub marker: bool,
    /// The review panel's クリップに保存 comment (`ReviewVm.clip-editing`).
    pub clip_comment: bool,
    /// Whether 確認 is the pane on screen *and* has a session in it -- i.e.
    /// whether `ReviewPane` exists at all.
    pub review_showing: bool,
}

/// Whether the full-window overlay that ends an edit on the next press should
/// exist (task3640).
///
/// **`ui/app.slint`'s `inline-edit-open` binding is the live copy** -- two of
/// these five states never reach Rust, so the view cannot call this. What it
/// fixes is the truth table, and above all the one row that is not a plain OR:
/// the two review states live on a *global*, and nothing resets a global when
/// the pane that filled it is destroyed, so leaving 確認 mid-rename would leave
/// the overlay standing over a pane with no editor in it. They only count while
/// the review pane is showing.
pub fn any_inline_edit_open(edits: InlineEdits) -> bool {
    edits.sessions_row >= 0
        || edits.clips_row >= 0
        || (edits.review_showing && (edits.marker || edits.clip_comment))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// One column per editor, plus the guard that keeps a stale global from
    /// raising the overlay over a pane that no longer exists.
    #[test]
    fn the_overlay_stands_for_any_open_editor_and_for_nothing_else() {
        let none = InlineEdits {
            sessions_row: -1,
            clips_row: -1,
            ..InlineEdits::default()
        };
        assert!(!any_inline_edit_open(none));

        assert!(any_inline_edit_open(InlineEdits {
            sessions_row: 0,
            ..none
        }));
        assert!(any_inline_edit_open(InlineEdits {
            clips_row: 7,
            ..none
        }));
        assert!(any_inline_edit_open(InlineEdits {
            marker: true,
            review_showing: true,
            ..none
        }));
        assert!(any_inline_edit_open(InlineEdits {
            clip_comment: true,
            review_showing: true,
            ..none
        }));

        // The row the guard exists for: 確認 was left while the marker rename
        // was open, so `ReviewVm` still says true and the pane is gone.
        assert!(!any_inline_edit_open(InlineEdits {
            marker: true,
            clip_comment: true,
            review_showing: false,
            ..none
        }));
        // ... and the guard covers only the review pair. A history edit is
        // still an edit while 確認 is showing.
        assert!(any_inline_edit_open(InlineEdits {
            sessions_row: 3,
            review_showing: true,
            ..none
        }));
    }

    #[test]
    fn the_rail_switches_four_panes_and_opens_the_picker() {
        assert_eq!(rail_action(0), RailAction::Show(Pane::Review));
        assert_eq!(rail_action(1), RailAction::OpenPicker);
        assert_eq!(rail_action(2), RailAction::Show(Pane::Sessions));
        assert_eq!(rail_action(3), RailAction::Show(Pane::Clips));
        assert_eq!(rail_action(4), RailAction::Show(Pane::Settings));
        // Anything the view could not have produced lands on the default pane
        // rather than panicking.
        assert_eq!(rail_action(9), RailAction::Show(Pane::Review));
        assert_eq!(rail_action(-1), RailAction::Show(Pane::Review));
    }

    /// The pin task2980 asks for: クリップ moved 設定 down the rail, and the two
    /// numbers it must not have moved are the settings rail slot and the
    /// settings *pane* index -- `ui/app.slint` writes that 2 out as a literal
    /// twice (the hotkey blur on leaving the pane, and the pane's own `if`), so
    /// a renumbering here would unregister the global hotkeys with nothing
    /// failing.
    #[test]
    fn adding_clips_moved_the_settings_rail_slot_but_not_its_pane_index() {
        assert_eq!(rail_action(4), RailAction::Show(Pane::Settings));
        assert_eq!(rail_current(Pane::Settings, false), 4);
        assert_eq!(Pane::Settings.index(), 2, "ui/app.slint hard-codes this 2");
        assert_eq!(Pane::from_index(2), Pane::Settings);
        // And the new pane is the one that took the free index.
        assert_eq!(Pane::Clips.index(), 3);
        assert_eq!(rail_current(Pane::Clips, false), 3);
    }

    /// The whole reason 対象 is not a pane: opening and closing the picker has
    /// to leave the rail exactly where it was.
    #[test]
    fn the_picker_borrows_the_current_mark_and_gives_it_back() {
        for (pane, index) in [
            (Pane::Review, 0),
            (Pane::Sessions, 2),
            (Pane::Clips, 3),
            (Pane::Settings, 4),
        ] {
            assert_eq!(rail_current(pane, false), index);
            assert_eq!(rail_current(pane, true), 1, "the picker holds the mark");
            // Closing it restores the pane's own mark, because the pane never
            // changed in the first place.
            assert_eq!(rail_current(pane, false), index);
        }
    }

    #[test]
    fn escape_only_reports_a_close_when_the_picker_is_open() {
        assert!(closes_picker(true));
        assert!(!closes_picker(false));
    }

    #[test]
    fn the_session_list_is_requested_once_per_open() {
        assert!(opens_sessions(Pane::Review, Pane::Sessions));
        assert!(opens_sessions(Pane::Settings, Pane::Sessions));
        // Re-rendering while it is already up must not re-request.
        assert!(!opens_sessions(Pane::Sessions, Pane::Sessions));
        assert!(!opens_sessions(Pane::Sessions, Pane::Review));
    }

    /// Same rule for the clip list, and for the same reason: it is a directory
    /// scan, so it has to be re-taken on the way in or Explorer's edits never
    /// show up (task2980).
    #[test]
    fn the_clip_list_is_requested_once_per_open() {
        assert!(opens_clips(Pane::Review, Pane::Clips));
        assert!(opens_clips(Pane::Sessions, Pane::Clips));
        assert!(!opens_clips(Pane::Clips, Pane::Clips));
        assert!(!opens_clips(Pane::Clips, Pane::Sessions));
        // The two lists never answer for each other.
        assert!(!opens_sessions(Pane::Review, Pane::Clips));
        assert!(!opens_clips(Pane::Review, Pane::Sessions));
    }

    #[test]
    fn only_the_history_pane_keeps_the_session_count() {
        assert!(keeps_session_count(Pane::Sessions));
        assert!(!keeps_session_count(Pane::Review));
        assert!(!keeps_session_count(Pane::Settings));
        assert!(!keeps_session_count(Pane::Clips));
    }

    /// The overlay may only promise what the drop can keep: a container is
    /// named, anything else is refused whatever it is called.
    #[test]
    fn only_a_container_is_named_as_droppable() {
        let hint = drop_hint(Path::new(r"C:\Videos\Liveback\ランク戦 最終ラウンド.lvb"));
        assert!(hint.accepted);
        assert_eq!(hint.name, "ランク戦 最終ラウンド.lvb");
        // Explorer's own casing is not the app's business.
        assert!(drop_hint(Path::new("clip.LVB")).accepted);

        for rejected in ["clip.mp4", "clip.lvb.txt", "clip", r"C:\Videos\Liveback"] {
            let hint = drop_hint(Path::new(rejected));
            assert!(!hint.accepted, "{rejected} is not a container");
            assert!(hint.name.is_empty(), "a refusal names no file");
        }
    }

    /// The three scale factors Windows offers on this machine's monitors. The
    /// numbers are Windows' own frame width at those dpi
    /// (`SM_CXSIZEFRAME` + `SM_CXPADDEDBORDER`), which is the point: the band
    /// is meant to be as wide as every other window's, not a constant.
    #[test]
    fn the_resize_band_is_eight_logical_pixels_at_every_dpi() {
        assert_eq!(resize_band_physical_px(96), 8.0);
        assert_eq!(resize_band_physical_px(120), 10.0);
        assert_eq!(resize_band_physical_px(144), 12.0);
        // 200%: the band scales past the 8 the .slint default carries.
        assert_eq!(resize_band_physical_px(192), 16.0);
    }

    /// The scrollbar's inset and the band's width are the same number (task4320):
    /// the scrollbar thumb overlapped the band because the two were written
    /// separately and only one of them was ever widened. `dpi=96` is the one
    /// scale where the logical and physical answers coincide, which is exactly
    /// why the overlap survived the eye -- so this pins the identity itself
    /// rather than the 96 row.
    #[test]
    fn the_scrollbar_inset_is_the_band_at_one_hundred_percent() {
        assert_eq!(resize_band_physical_px(96), RESIZE_BAND_LOGICAL_PX);
        // And it is *not* the physical number anywhere else -- the inset must
        // stay this constant rather than follow `resize_band_physical_px`.
        assert_ne!(resize_band_physical_px(144), RESIZE_BAND_LOGICAL_PX);
    }

    /// The band the user actually gets is the number handed to
    /// `resize-border-width` **times the window's scale factor**: slint 1.18's
    /// winit backend does that multiply itself
    /// (`i-slint-backend-winit-1.18.0/drag_resize_window.rs:15`,
    /// `f64::from(border_width.get()) * window.scale_factor()`).
    ///
    /// Pinning the product rather than the supply is the whole point. dpi=96
    /// cannot tell a logical supply from a physical one -- the two coincide
    /// there -- so a 100%-only test stays green through exactly the regression
    /// this pins: after the 1.18 upgrade (486ac99f) the physical supply task4120
    /// had built for 1.17.1 was scaled a second time and the band came out
    /// `8 * 1.5 * 1.5` = 18 physical px on a 150% desk.
    #[test]
    fn the_band_slint_gets_lands_on_the_os_frame_width_at_every_scale() {
        for (dpi, want_physical) in [(96, 8.0), (120, 10.0), (144, 12.0), (192, 16.0)] {
            let scale = dpi as f32 / 96.0;
            assert_eq!(
                resize_band_slint_px(dpi) * scale,
                want_physical,
                "dpi={dpi}: slint scales the supply by {scale}"
            );
        }
    }

    #[test]
    fn the_view_index_round_trips() {
        for pane in [Pane::Review, Pane::Sessions, Pane::Settings, Pane::Clips] {
            assert_eq!(Pane::from_index(pane.index()), pane);
        }
        assert_eq!(Pane::from_index(9), Pane::Review);
        assert_eq!(Pane::from_index(-1), Pane::Review);
    }
}
