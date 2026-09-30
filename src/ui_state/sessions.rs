//! The session history / recovery screen's state (task125).
//!
//! Transcribed from `src/pages/RecoveryPage.tsx` and `src/lib/sessions.ts` as
//! they stand after round-2 (tasks 115 / 116). The selection model is the part
//! worth having here: single click, ctrl-additive toggle, shift range against
//! the *rendered* order, and an Explorer-style any-overlap drag marquee -- four
//! behaviours the React version could only test through the DOM.

use crate::ring_buffer::SessionSummary;
use crate::ui_state::locale::Locale;

mod layout;
mod text;
pub use layout::*;
pub use text::*;

#[cfg(test)]
mod tests;

/// Session ids embed their creation time as `capture-{unix_nanos:032x}`, so the
/// date comes from the id instead of a separate field. Returns the raw
/// milliseconds; turning those into a local timestamp is the caller's job (the
/// bin does it with `FileTimeToLocalFileTime`, no date crate).
pub fn session_epoch_millis(session_id: &str) -> Option<i64> {
    let hex = session_id.strip_prefix("capture-")?;
    if hex.len() != 32 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    u128::from_str_radix(hex, 16)
        .ok()
        .map(|nanos| (nanos / 1_000_000) as i64)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewMode {
    #[default]
    List,
    Thumbnail,
}

/// A rectangle in the list's own content space, so scrolling is just an offset
/// the caller applies before asking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl Rect {
    pub fn from_corners(ax: f32, ay: f32, bx: f32, by: f32) -> Self {
        Self {
            x: ax.min(bx),
            y: ay.min(by),
            width: (bx - ax).abs(),
            height: (by - ay).abs(),
        }
    }

    /// Any overlap, not containment -- Explorer's marquee rule (task079).
    pub fn overlaps(&self, other: &Rect) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

/// Which entries the context menu carries for one row (round5 §6-D). A pure
/// list rather than six conditionals in `.slint`: the set genuinely varies --
/// コメントを編集 is list-only, and the protect entry names the *action*, not the
/// state -- and each of those rules is worth a test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionMenuAction {
    Load,
    EditNote,
    ToggleProtect,
    OpenDirectory,
    Discard,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMenuItem {
    pub action: SessionMenuAction,
    pub label: &'static str,
    /// The right-aligned key hint, or "" for none.
    pub hint: &'static str,
    /// A separator is drawn above this entry.
    pub rule_above: bool,
    pub danger: bool,
    pub disabled: bool,
}

/// Whether F2 may open the note editor in `view`. Only the list row draws
/// one; on a tile it would raise an editing state nobody can see, which is the
/// same reason [`session_menu`] leaves the note out of the tile menu. (F2
/// renamed the row until titles stopped being editable, 2026-09-19.)
pub fn f2_edits_note_in(view: ViewMode) -> bool {
    view == ViewMode::List
}

/// `has_live` is "this menu's targets include a session that is recording"
/// (t260920-bb09). A recording cannot be discarded, and since the row became
/// selectable the entry has to say so rather than simply be absent -- disabled,
/// exactly as `load_disabled` already makes the reason for a closed door
/// visible.
pub fn session_menu(
    locale: Locale,
    view: ViewMode,
    protected: bool,
    load_disabled: bool,
    has_live: bool,
) -> Vec<SessionMenuItem> {
    let mut items = vec![SessionMenuItem {
        action: SessionMenuAction::Load,
        label: menu_load(locale),
        hint: hint_open(locale),
        rule_above: false,
        danger: false,
        // Still listed while a *different* session is recording, so the reason
        // the row will not open is visible rather than merely absent.
        disabled: load_disabled,
    }];
    // The note editor lives on the list row and nowhere else: a tile is a
    // thumbnail with a name under it, and offering an editor for a field it
    // does not draw would open something the user cannot see (round5 §6-C3).
    // No rename entry: titles are not editable (the user, 2026-09-19).
    if view == ViewMode::List {
        items.push(SessionMenuItem {
            action: SessionMenuAction::EditNote,
            label: menu_edit_note(locale),
            hint: hint_edit_note(locale),
            rule_above: false,
            danger: false,
            disabled: false,
        });
    }
    // DS SessionRow order: open / note, rule, protect / folder, rule, discard
    // (the user, 2026-09-28).
    items.push(SessionMenuItem {
        action: SessionMenuAction::ToggleProtect,
        label: protect_menu_label(locale, protected),
        hint: "",
        rule_above: true,
        danger: false,
        disabled: false,
    });
    items.push(SessionMenuItem {
        action: SessionMenuAction::OpenDirectory,
        label: open_directory(locale),
        hint: "",
        rule_above: false,
        danger: false,
        disabled: false,
    });
    items.push(SessionMenuItem {
        action: SessionMenuAction::Discard,
        label: menu_discard(locale),
        hint: hint_discard(locale),
        rule_above: true,
        danger: true,
        disabled: has_live,
    });
    items
}

/// The same menu opened over a selection of several rows (2026-08-17). Only the
/// entries that mean something for a pile: there is one confirm screen per
/// discard, one folder per session, and one name per rename. It replaced the
/// bottom-right popup, which was a second place to say the same three things.
///
/// `all_protected` matches what the toggle does to a mixed pile -- any
/// unprotected row means the entry protects everything.
/// `has_live`: see `session_menu`. A pile that holds a recording keeps its
/// 破棄 entry and shows it disabled -- never "discard the rest", because an
/// implicit exclusion would disagree with the count the pile itself shows.
pub fn selection_menu(locale: Locale, all_protected: bool, has_live: bool) -> Vec<SessionMenuItem> {
    vec![
        SessionMenuItem {
            action: SessionMenuAction::ToggleProtect,
            label: protect_menu_label(locale, all_protected),
            hint: "",
            rule_above: false,
            danger: false,
            disabled: false,
        },
        SessionMenuItem {
            action: SessionMenuAction::Discard,
            label: menu_discard(locale),
            hint: hint_discard(locale),
            rule_above: true,
            danger: true,
            disabled: has_live,
        },
    ]
}

/// `isLoadDisabled`. The session being recorded is the one exception to the
/// recording-time block (task162, LiveReview): it resolves from memory through
/// `load_active_session`, while every *other* session's manifest on disk is
/// stale until the recording stops -- which is what the block is protecting.
pub fn is_load_disabled(
    session: &SessionSummary,
    capturing: bool,
    is_active: bool,
    pending_load: bool,
) -> bool {
    (capturing && !is_active) || !session.readable || pending_load
}

/// Whether the history's right-click menu opens on this row at all (task1520
/// follow-up).
///
/// Nothing in the menu can act on a session that would not open: loading is
/// already refused by `is_load_disabled`, renaming and the note need a manifest
/// to write into, and discarding it is the operation that has to be *hardest*
/// to aim by accident. So the row takes no menu, and stays in the list saying
/// why instead.
pub fn has_row_menu(session: &SessionSummary) -> bool {
    session.readable
}

/// Whether the row answers a click, a drag or a keystroke at all. Same rule and
/// the same reason: an inert row cannot be selected into a bulk discard either.
pub fn is_row_inert(session: &SessionSummary) -> bool {
    !session.readable
}

/// The warning line beside the StorageMeter when protected sessions alone
/// are over the limit the sweep cannot free (task167; beside the meter rather
/// than a toast on every start since t260928-e309 F10). `None` is the normal
/// case.
pub fn protected_over_limit_notice(locale: Locale, over_limit: bool) -> Option<&'static str> {
    over_limit.then_some(protected_over_limit(locale))
}

crate::tr! {
    /// The capacity toast's one button (round7 §2-11).
    settings_action { ja: "設定を開く", en: "Open settings" }
}

/// The toast a completed sweep raises, if it should raise one at all (task700).
///
/// Only the capacity-driven share is reported. A session aged out by
/// `sessionLifetimeDays` went exactly when the setting said it would, and a
/// toast about it on every start would be noise -- which is how a real warning
/// gets ignored. `None` is the normal case, same shape as
/// `protected_over_limit_notice` above.
pub fn capacity_reclaim_notice(
    locale: Locale,
    sessions: u32,
    bytes: u64,
) -> Option<(String, String)> {
    (sessions > 0).then(|| {
        (
            capacity_reclaimed_text(locale, sessions),
            capacity_reclaimed_detail(locale, &format_bytes(bytes)),
        )
    })
}

/// What a note or protection edit says when it fails (task167). Success is
/// silent by design: the row already shows the new value, and the protect
/// toggle is specified as "no confirmation, no toast".
pub fn note_update_notice(locale: Locale, result: &Result<(), String>) -> Option<&'static str> {
    result.is_err().then_some(note_error(locale))
}

pub fn protect_update_notice(locale: Locale, result: &Result<(), String>) -> Option<&'static str> {
    result.is_err().then_some(protect_error(locale))
}

/// The context menu's protect entry, which names the action rather than the
/// state: W-17 on an already-protected row.
pub fn protect_menu_label(locale: Locale, protected: bool) -> &'static str {
    if protected {
        unprotect(locale)
    } else {
        protect(locale)
    }
}

/// W-18: the extra line a discard confirmation carries when the selection
/// includes protected sessions. Manual discard is never blocked by protection --
/// this only makes sure the user knows what is in the pile.
pub fn protected_in_selection_text(locale: Locale, count: usize) -> Option<String> {
    (count > 0).then(|| match locale {
        Locale::Ja => format!("保護中 {count} 件を含みます"),
        Locale::En => format!("Includes {count} protected"),
    })
}

#[derive(Debug, Default)]
pub struct SessionsState {
    /// Newest-first, exactly as `list_sessions` returns it -- and that is the
    /// order the screen shows, full stop (round5 §6-B). The four sort chips
    /// are gone: for a replay buffer the row you want is nearly always the
    /// most recent, and three other orders cost a permanent row of controls
    /// to serve a choice nobody was making.
    sessions: Vec<SessionSummary>,
    selected: Vec<String>,
    anchor: Option<String>,
    view_mode: ViewMode,
    /// Title substring, case-insensitive. Not persisted: a filter that
    /// outlives the screen hides rows nobody asked it to hide.
    search: String,
    /// Every session being recorded right now, oldest start first (task2030).
    /// It used to be a `bool` and the guess below it -- "the newest directory
    /// on disk is the live one" -- which can only ever name one row. The
    /// controller knows the real ids, so the history asks it instead of
    /// inferring, and every one of them gets its own LIVE row.
    active_ids: Vec<String>,
    /// What a protect toggle just asked for, before any listing has caught up
    /// (t260920-bb09). The listing is read from disk by `ring_buffer::
    /// list_sessions`, but a *recording* session's protect flag is staged in
    /// the ring's in-memory manifest and only reaches the file when the
    /// `livia-index` thread next wakes (500 ms `recv_timeout`) -- and the UI
    /// sends its `Cmd::ListSessions` in the same handler as the toggle, so the
    /// read is stale. Nothing relists on a timer, so without this the new value
    /// was invisible until the user's *next* action, measured at 5 of 10
    /// toggles looking like they had done nothing.
    protect_overlay: Vec<(String, bool)>,
}

impl SessionsState {
    /// Replaces the list and drops any selection that no longer refers to a
    /// selectable session, mirroring the React reconciliation that ran during
    /// render rather than in an effect.
    pub fn set_sessions(&mut self, sessions: Vec<SessionSummary>) {
        // task3800: with nothing in the list the screen stops drawing its
        // toolbar, so a query typed into the list that was there has no field
        // left to sit in -- and would go on filtering the first recording that
        // comes back, behind a search box that reads as empty. An empty list is
        // where it gets dropped.
        if sessions.is_empty() {
            self.search.clear();
        }
        self.sessions = sessions;
        self.prune_protect_overlay();
        self.reconcile();
    }

    /// An optimistic entry earns its keep only while the listing disagrees with
    /// it *and* the session is still recording. Three ways out, all here:
    /// the listing caught up (the value matches -- the common case, and the only
    /// one a stopped row ever takes, because its write is synchronous), the
    /// recording stopped (the finalized container is authoritative, whatever it
    /// says, so a queued append the writer never managed cannot leave a phantom
    /// padlock), or the row is gone from the list entirely.
    fn prune_protect_overlay(&mut self) {
        let sessions = &self.sessions;
        let active = &self.active_ids;
        self.protect_overlay.retain(|(id, staged)| {
            sessions
                .iter()
                .find(|session| &session.session_id == id)
                .is_some_and(|session| session.protected != *staged)
                && active.iter().any(|live| live == id)
        });
    }

    /// What the screen should say about a session's protect flag: the value a
    /// toggle has just asked for, if the listing has yet to show it, otherwise
    /// the listing's own (t260920-bb09).
    ///
    /// Everything that reads the flag goes through here -- the row, the menu's
    /// label, and the *next* toggle's target value. That last one is why the
    /// old behaviour looked like "the second press fixed it": the second press
    /// read the stale listing and re-staged the value the first press had
    /// already written, so what the user saw catch up was the first press.
    pub fn protected(&self, id: &str) -> bool {
        self.protect_overlay
            .iter()
            .find(|(key, _)| key == id)
            .map(|(_, staged)| *staged)
            .unwrap_or_else(|| self.find(id).is_some_and(|session| session.protected))
    }

    /// A protect toggle has been sent; show `protected` until a listing agrees.
    pub fn stage_protected(&mut self, id: &str, protected: bool) {
        match self.protect_overlay.iter_mut().find(|(key, _)| key == id) {
            Some(entry) => entry.1 = protected,
            None => self.protect_overlay.push((id.to_owned(), protected)),
        }
    }

    /// The write came back an error: forget the optimistic value and let the
    /// listing speak again.
    pub fn rollback_protected(&mut self, id: &str) {
        self.protect_overlay.retain(|(key, _)| key != id);
    }

    /// The ids the controller reports as recording (task2030).
    pub fn set_active_ids(&mut self, active_ids: Vec<String>) {
        if self.active_ids == active_ids {
            return;
        }
        self.active_ids = active_ids;
        self.reconcile();
    }

    fn reconcile(&mut self) {
        // Owned, not borrowed: `rendered()` borrows `self`, and the retain
        // below needs `self.selected` mutably.
        let live: Vec<String> = self
            .rendered()
            .iter()
            .map(|session| session.session_id.clone())
            .collect();
        // On the grid and nothing more. A LIVE row used to be dropped here as
        // well (task2030); the user's 2026-09-19 ruling (t260920-bb09) opened
        // the selection to recordings, so a row selected while it records stays
        // selected across the list updates recording produces. What keeps a
        // recording out of a bulk discard is now the discard side: the menu
        // entry is disabled, the Delete key refuses, and
        // `CaptureController::discard_session` answers `Err`.
        let keep = |id: &String| live.contains(id);
        self.selected.retain(keep);
        if !self.anchor.as_ref().is_some_and(keep) {
            self.anchor = None;
        }
    }

    /// Whether this row is one of the recordings in flight -- a LIVE row.
    pub fn is_active(&self, id: &str) -> bool {
        self.active_ids.iter().any(|active| active == id)
    }

    pub fn active_ids(&self) -> &[String] {
        &self.active_ids
    }

    /// The one gate the click path, the right click and the marquee all go
    /// through, which is why the unreadable rule belongs here and not at either
    /// call site: a row that takes no context menu must not be reachable by a
    /// click or a drag either, or it arrives in a bulk discard through the back
    /// door. An id with no session behind it stays selectable, as it always was.
    ///
    /// A **recording** row is selectable (the user's 2026-09-19 ruling,
    /// t260920-bb09). It used not to be (task2030), and that exclusion was what
    /// stood between a LIVE session and a bulk discard -- so it was replaced by
    /// guards on the discard itself rather than simply lifted: the menu's 破棄
    /// is disabled while the pile holds a recording, the Delete key refuses the
    /// same pile, and `CaptureController::discard_session` answers `Err` for a
    /// live session however it is asked. Before this, the left click and the
    /// right click also disagreed -- `select_for_menu` never asked this
    /// question, so a right click selected a LIVE row that a left click would
    /// not (measured 0/5 against 5/5 on the tree before the change).
    pub fn is_selectable(&self, id: &str) -> bool {
        !self.find(id).is_some_and(is_row_inert)
    }

    pub fn sessions(&self) -> &[SessionSummary] {
        &self.sessions
    }

    pub fn capturing(&self) -> bool {
        !self.active_ids.is_empty()
    }

    pub fn search(&self) -> &str {
        &self.search
    }

    pub fn set_search(&mut self, query: &str) {
        self.search = query.to_owned();
        // A row the search just hid must not stay armed for a bulk discard
        // whose target the user can no longer see.
        self.reconcile();
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    pub fn set_view_mode(&mut self, mode: ViewMode) {
        self.view_mode = mode;
    }

    pub fn find(&self, id: &str) -> Option<&SessionSummary> {
        self.sessions
            .iter()
            .find(|session| session.session_id == id)
    }

    /// The rows actually on screen, in on-screen order: newest first (the only
    /// order there is now), narrowed by the search box. The collapsed junk
    /// group went with it -- task164's startup sweep repairs or discards the
    /// unreadable directories it used to gather, so there is no pile left to
    /// fold away.
    pub fn rendered(&self) -> Vec<&SessionSummary> {
        let query = self.search.to_lowercase();
        self.sessions
            .iter()
            .filter(|session| {
                query.is_empty()
                    || session
                        .target_title
                        .as_deref()
                        .is_some_and(|title| title.to_lowercase().contains(&query))
                    // Unnamed sessions are shown by their start time, which is
                    // derived from the id -- so that is what a query has to
                    // match for them.
                    || session.session_id.to_lowercase().contains(&query)
                    || session
                        .note
                        .as_deref()
                        .is_some_and(|note| note.to_lowercase().contains(&query))
            })
            .collect()
    }

    /// Rendered rows only, never the whole list (task115): with the junk group
    /// collapsed, a list-wide select-all would arm the bulk discard with
    /// hundreds of sessions the user cannot even see.
    /// Ctrl+A's universe: every row on the grid a gesture may reach. Recordings
    /// are in it since t260920-bb09; `is_selectable` is the single answer, so
    /// the unreadable rule cannot drift away from the click path's copy of it.
    pub fn selectable_ids(&self) -> Vec<&str> {
        self.rendered()
            .into_iter()
            .map(|session| session.session_id.as_str())
            .filter(|id| self.is_selectable(id))
            .collect()
    }

    pub fn selected(&self) -> &[String] {
        &self.selected
    }

    pub fn is_selected(&self, id: &str) -> bool {
        self.selected.iter().any(|selected| selected == id)
    }

    /// The selection a right-click over `id` leaves behind (task1120). Inside
    /// the current pile it stands, so the menu acts on the whole thing.
    /// Outside it, the row replaces the selection before the menu opens.
    ///
    /// This reverses the 2026-08-17 rule, which left the selection alone so a
    /// stray right-click could not discard one the user had forgotten about.
    /// The cost of that was the other surprise: a menu acting on a row that is
    /// visibly not selected. Every file manager picks the row; so does this.
    pub fn select_for_menu(&mut self, id: &str) {
        // Through the same gate as the click and the marquee (t260920-bb09).
        // It used to write `selected` unconditionally, which is how a right
        // click reached rows a left click could not -- and the row it wrote
        // was then dropped again by the next `reconcile`, so the selection it
        // drew was neither reachable nor durable.
        if !self.is_selectable(id) {
            return;
        }
        if !self.is_selected(id) {
            self.selected = vec![id.to_owned()];
            self.anchor = Some(id.to_owned());
        }
    }

    /// The rows a menu opened over `id` acts on: the whole selection when that
    /// row is part of one, the row alone otherwise (2026-08-17). Both the render
    /// side and the click side ask this -- they build the entry list and then
    /// re-derive it by index, so a rule stated twice would silently run the
    /// wrong entry.
    pub fn menu_targets(&self, id: &str) -> Vec<String> {
        if self.selected.len() > 1 && self.is_selected(id) {
            self.selected.clone()
        } else {
            vec![id.to_owned()]
        }
    }

    pub fn selected_bytes(&self) -> u64 {
        self.selected
            .iter()
            .filter_map(|id| self.find(id))
            .map(|session| session.total_bytes)
            .sum()
    }

    pub fn total_bytes(&self) -> u64 {
        self.sessions
            .iter()
            .map(|session| session.total_bytes)
            .sum()
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
        self.anchor = None;
    }

    pub fn select_all(&mut self) {
        self.selected = self
            .selectable_ids()
            .into_iter()
            .map(str::to_owned)
            .collect();
    }

    fn set_selection(&mut self, ids: Vec<String>) {
        self.selected = ids;
    }

    /// The click semantics from `handleContainerPointerDown`, including the
    /// pre-toggle selection a subsequent drag extends from. `None` for `id`
    /// means the press landed on empty space.
    ///
    /// Returns the base selection the marquee should build on.
    pub fn press(&mut self, id: Option<&str>, shift: bool, additive: bool) -> Vec<String> {
        let id = id.filter(|id| self.is_selectable(id));

        if shift {
            if let (Some(id), Some(anchor)) = (id, self.anchor.clone()) {
                let range = self.range_between(&anchor, id);
                self.set_selection(range.clone());
                return range;
            }
        }

        let base: Vec<String> = if additive {
            self.selected.clone()
        } else {
            Vec::new()
        };
        match id {
            Some(id) => {
                let mut applied = base.clone();
                if additive && applied.iter().any(|selected| selected == id) {
                    applied.retain(|selected| selected != id);
                } else if !applied.iter().any(|selected| selected == id) {
                    applied.push(id.to_owned());
                }
                self.set_selection(applied);
                self.anchor = Some(id.to_owned());
            }
            // Empty-space start: nothing to toggle, so the base *is* the
            // selection until a drag extends it.
            None => self.set_selection(base.clone()),
        }
        base
    }

    /// Shift+click range against the rendered order, not the raw list.
    pub fn range_between(&self, from: &str, to: &str) -> Vec<String> {
        let ids = self.selectable_ids();
        let from_index = ids.iter().position(|id| *id == from);
        let to_index = ids.iter().position(|id| *id == to);
        let (Some(from_index), Some(to_index)) = (from_index, to_index) else {
            return if ids.contains(&to) {
                vec![to.to_owned()]
            } else {
                Vec::new()
            };
        };
        let (start, end) = if from_index < to_index {
            (from_index, to_index)
        } else {
            (to_index, from_index)
        };
        ids[start..=end].iter().map(|id| (*id).to_owned()).collect()
    }

    /// One marquee frame: the base selection plus every selectable row whose
    /// rect overlaps the drag rect. `rows` is the rendered order paired with
    /// each row's rect in the list's content space.
    pub fn marquee(&mut self, base: &[String], rows: &[(String, Rect)], drag: Rect) {
        let mut next = base.to_vec();
        for (id, rect) in rows {
            if !self.is_selectable(id) || !rect.overlaps(&drag) {
                continue;
            }
            if !next.iter().any(|selected| selected == id) {
                next.push(id.clone());
            }
        }
        self.set_selection(next);
    }
}

/// How far either side of the visible tiles a thumbnail is still worth
/// fetching (task188). Under a screenful, so the queue stays small; enough that
/// a short scroll finds pictures already there.
pub const THUMBNAIL_PREFETCH_MARGIN: usize = 8;

/// Which slice of the rendered list should have thumbnails in hand: the
/// visible tiles plus a margin, clamped to what exists. The whole list used to
/// be requested at once, which put 400 decodes in front of every list, discard
/// and load the same worker had to answer.
///
/// An empty visible count means the `.slint` side has not reported a range
/// yet, and an empty range is the honest answer -- the caller seeds the first
/// screenful from its own default rather than having this guess one.
pub fn visible_thumbnail_range(first: usize, count: usize, total: usize) -> std::ops::Range<usize> {
    if count == 0 || total == 0 {
        return 0..0;
    }
    let start = first.saturating_sub(THUMBNAIL_PREFETCH_MARGIN).min(total);
    let end = first
        .saturating_add(count)
        .saturating_add(THUMBNAIL_PREFETCH_MARGIN)
        .min(total);
    start..end.max(start)
}

/// Row rectangles for a uniform grid in content space, which is what the
/// marquee needs and what the `.slint` side lays out. `columns` is 1 for the
/// list view.
pub fn grid_rects(
    count: usize,
    columns: usize,
    cell_width: f32,
    cell_height: f32,
    gap: f32,
    padding: f32,
) -> Vec<Rect> {
    let columns = columns.max(1);
    (0..count)
        .map(|index| {
            let column = index % columns;
            let row = index / columns;
            Rect {
                x: padding + column as f32 * (cell_width + gap),
                y: padding + row as f32 * (cell_height + gap),
                width: cell_width,
                height: cell_height,
            }
        })
        .collect()
}
