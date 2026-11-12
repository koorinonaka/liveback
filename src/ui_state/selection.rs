//! The one selection model the history and the clip grid share (t261025-67ce).
//! It was the clip screen's `ClipSelection<T>`, itself written to the history
//! screen's rules; the two copies drifted only where round20 §6 forced them
//! apart, and that one difference is now the [`PlainClick`] policy.
//!
//! Generic over the id so the rules can be tested with plain strings. The clip
//! screen keys it by *path*, because the folder is a re-scan with no index
//! behind it and a delete resolves its files from here (`CLAUDE.md`'s
//! wrong-target rule); the history keys it by session id.
//!
//! Which rows a gesture may reach at all is the caller's: the history passes
//! only its selectable rows in, so an unreadable row cannot arrive through any
//! door, and the clip grid has no such rule.

use super::sessions::Rect;

/// What a click with neither Shift nor Ctrl does to the selection -- and,
/// carried over from the two screens' own copies, what Shift does with no
/// anchor to range from (`Selection::click`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlainClick {
    /// The row replaces the selection (the history list and grid).
    #[default]
    Select,
    /// The selection is cleared and the click goes back to the caller as
    /// [`ClickOutcome::Expand`]: on the clip grid a plain click expands and
    /// plays, and round20 §6 gives it no second meaning.
    Ignore,
}

/// What a click that did not become a marquee asks the caller to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClickOutcome {
    /// A plain click under [`PlainClick::Ignore`]: the selection is cleared and
    /// the tile expands (plays) exactly as it did before there was a selection
    /// (round20 §6, kept).
    Expand,
    /// The selection changed, and nothing expands.
    Selected,
}

#[derive(Clone, Debug)]
pub struct Selection<T> {
    plain: PlainClick,
    selected: Vec<T>,
    /// Where a Shift+click range starts: the last row clicked.
    anchor: Option<T>,
    /// What a marquee in progress adds to. Set by `begin_marquee`, and by
    /// `click` to what a drag growing out of that press extends: the range a
    /// Shift press made, the selection before a Ctrl press toggled it, nothing
    /// after a plain press.
    base: Vec<T>,
}

/// [`PlainClick::Select`], the history's policy.
impl<T> Default for Selection<T> {
    fn default() -> Self {
        Self::new(PlainClick::default())
    }
}

impl<T> Selection<T> {
    pub fn new(plain: PlainClick) -> Self {
        Self {
            plain,
            selected: Vec::new(),
            anchor: None,
            base: Vec::new(),
        }
    }
}

impl<T: Clone + PartialEq> Selection<T> {
    pub fn selected(&self) -> &[T] {
        &self.selected
    }

    pub fn is_selected(&self, id: &T) -> bool {
        self.selected.contains(id)
    }

    pub fn len(&self) -> usize {
        self.selected.len()
    }

    pub fn is_empty(&self) -> bool {
        self.selected.is_empty()
    }

    pub fn clear(&mut self) {
        self.selected.clear();
        self.anchor = None;
        self.base.clear();
    }

    /// A click on `id`. `order` is the rows a gesture may reach, in on-screen
    /// order, which is what a Shift range runs along.
    ///
    /// Shift wins over Ctrl when there is an anchor on `order` to range from.
    /// Without one the two screens kept the answers they had before they
    /// shared this (t261025-67ce: no gesture changes meaning), so the policy
    /// settles it too. Under `Ignore` (the clip grid) Shift still wins: the
    /// row alone, anchored there. Under `Select` (the history) an anchor that
    /// left `order` still takes Shift, to the row alone with the anchor kept,
    /// and no anchor at all leaves the click to Ctrl or the plain rule.
    pub fn click(&mut self, id: &T, shift: bool, ctrl: bool, order: &[T]) -> ClickOutcome {
        if shift {
            if let Some(range) = self
                .anchor
                .as_ref()
                .and_then(|anchor| range(order, anchor, id))
            {
                self.selected = range;
                self.base = self.selected.clone();
                return ClickOutcome::Selected;
            }
            if self.plain == PlainClick::Ignore {
                self.selected = vec![id.clone()];
                self.anchor = Some(id.clone());
                self.base.clear();
                return ClickOutcome::Selected;
            }
            if self.anchor.is_some() {
                self.selected = vec![id.clone()];
                self.base = self.selected.clone();
                return ClickOutcome::Selected;
            }
        }
        self.anchor = Some(id.clone());
        if ctrl {
            self.base = self.selected.clone();
            if let Some(position) = self.selected.iter().position(|selected| selected == id) {
                self.selected.remove(position);
            } else {
                self.selected.push(id.clone());
            }
            return ClickOutcome::Selected;
        }
        self.base.clear();
        if self.plain == PlainClick::Select {
            self.selected = vec![id.clone()];
            ClickOutcome::Selected
        } else {
            self.selected.clear();
            ClickOutcome::Expand
        }
    }

    /// A marquee begins: from bare ground on the press, from a tile once the
    /// press crosses the threshold. `additive` (Ctrl) keeps what was selected.
    pub fn begin_marquee(&mut self, additive: bool) {
        self.base = if additive {
            self.selected.clone()
        } else {
            Vec::new()
        };
        self.selected = self.base.clone();
    }

    /// One marquee step: the base plus every row the drag rectangle touches.
    pub fn marquee(&mut self, rows: &[(T, Rect)], drag: Rect) {
        let mut next = self.base.clone();
        for (id, rect) in rows {
            if rect.overlaps(&drag) && !next.contains(id) {
                next.push(id.clone());
            }
        }
        self.selected = next;
    }

    /// Ctrl+A: everything in `order`, which under a search is what it matched.
    pub fn select_all(&mut self, order: &[T]) {
        self.selected = order.to_vec();
    }

    /// Drops whatever is no longer on screen (clips design 7; the history's
    /// reconcile).
    pub fn retain(&mut self, order: &[T]) {
        self.selected.retain(|id| order.contains(id));
        self.base.retain(|id| order.contains(id));
        if self
            .anchor
            .as_ref()
            .is_some_and(|anchor| !order.contains(anchor))
        {
            self.anchor = None;
        }
    }

    /// A right-click on `id` (the history's task1120 rule, clips design 4):
    /// inside the selection it stands, outside it the row replaces it.
    pub fn select_for_menu(&mut self, id: &T) {
        if !self.is_selected(id) {
            self.selected = vec![id.clone()];
            self.anchor = Some(id.clone());
        }
    }

    /// What a menu opened over `id` acts on: the whole selection when `id` is
    /// part of one of two or more, `id` alone otherwise. Both the render side
    /// and the click side ask this -- they build the entry list and then
    /// re-derive it by index, so a rule stated twice would silently run the
    /// wrong entry.
    pub fn menu_targets(&self, id: &T) -> Vec<T> {
        if self.selected.len() > 1 && self.is_selected(id) {
            self.selected.clone()
        } else {
            vec![id.clone()]
        }
    }
}

/// `from..=to` along `order`, whichever way round they are; `None` unless both
/// are on it.
fn range<T: Clone + PartialEq>(order: &[T], from: &T, to: &T) -> Option<Vec<T>> {
    let from = order.iter().position(|id| id == from)?;
    let to = order.iter().position(|id| id == to)?;
    Some(order[from.min(to)..=from.max(to)].to_vec())
}

#[cfg(test)]
mod tests;
