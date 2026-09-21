//! The session-history page (task125): its UI-thread state, the session
//! worker, the row renderer, the message application, and the callback wiring
//! moved out of `main`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use crossbeam_channel::Sender;
use livia::capture::CaptureController;
use livia::ring_buffer::SessionSummary;
use livia::settings::{AppSettings, SessionViewMode};
use livia::ui_state::sessions::{self, SessionMenuAction, SessionsState, ViewMode};
use livia::ui_state::timeline as tl;
use slint::{ComponentHandle, Image, VecModel};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{image_from_rgba, AppWindow, Cmd, Msg, SessionRow};

// `#[path]` for the same reason as the crate root's module declarations: the
// parent `mod history` is itself `#[path]`-declared, so children resolve
// relative to this file's directory rather than a `history/` subdirectory.
#[path = "history/render.rs"]
mod render;
#[path = "history/worker.rs"]
mod worker;

pub(super) use render::{apply_selection, queue_visible_thumbnails, render_sessions};
pub(super) use worker::spawn_session_worker;

/// UI-thread state for the session screen.
#[derive(Default)]
pub(super) struct History {
    pub(super) state: SessionsState,
    /// ponytail: decoded thumbnails stay for the life of the process, so a
    /// scroll through 400 sessions settles at ~23MB (160x90 RGBA each). Add an
    /// LRU if that ever matters; it does not at this size.
    thumbnails: HashMap<String, Option<Image>>,
    /// Ids already handed to the worker, so a re-render does not re-request a
    /// thumbnail that is merely still in flight.
    pub(super) requested: HashSet<String>,
    /// "the listing itself failed", cleared by the next successful listing.
    list_error: Option<&'static str>,
    /// The outcome of the last discard / recover / load. Kept separate from
    /// `list_error` because every one of those actions re-lists immediately
    /// afterwards, and a shared field would wipe the result the user is meant
    /// to read (React keeps `discardError` apart from `sessionsError` for the
    /// same reason).
    action: Option<(String, bool)>,
    /// Ids waiting on the shared confirm dialog: one for a row's discard button,
    /// many for the toolbar's bulk discard -- one dialog, one flow for both.
    confirm: Vec<String>,
    /// The selection a drag extends from, captured at press time.
    marquee_base: Vec<String>,
    dragging: bool,
    loaded: Option<String>,
    /// Which row is being edited in place, and which of its two fields
    /// (round5 §6-C2 / §6-C3). Nothing reaches disk until the edit commits.
    /// The row whose note is open in the inline editor.
    editing: Option<String>,
    /// The open right-click menu: the session id of the row it belongs to.
    /// `None` closes it (round5 §6-D). Where it is drawn is the `.slint`
    /// side's own business (task1400) -- only there are the scroll offsets
    /// that turn a press in the grid's content space into pane coordinates.
    menu: Option<String>,
    /// Visible tiles as `(first index, count)`, reported by the `.slint` side
    /// (task188). `None` until the first `visible-range-changed`, which does
    /// not fire for the initial values -- the thumbnail queue substitutes a
    /// first screenful so the list is not blank before the user scrolls.
    pub(super) visible_range: Option<(usize, usize)>,
}

/// What to fetch before the viewport has said anything: a list screenful.
pub(super) const ASSUMED_VISIBLE_TILES: usize = 24;

impl History {
    /// How long the recording runs, for the row's meta column (round5 §6-A).
    /// A session whose manifest carries no span -- a directory of loose mp4s
    /// with nothing readable in it -- has no honest answer, so it says nothing.
    fn duration_text(&self, session: &SessionSummary) -> String {
        match (session.start_100ns, session.end_100ns) {
            (Some(start), Some(end)) if end > start => tl::format_duration(end - start),
            _ => "—".to_owned(),
        }
    }

    /// Drops an open in-place edit without writing anything (task3000, the same
    /// shape as `Clips::close_editor` from 62e13f0).
    ///
    /// Called from two places. Leaving セッション管理 is the original one: the
    /// pane is a conditional element, so switching away destroys
    /// `SessionsPane` outright and no blur is ever delivered, and a return
    /// would otherwise find the previous visit's editor still open.
    ///
    /// Since task3430 it is also the `session_handler!` prologue. Every one of
    /// those handlers ends in a re-render that rebuilds the row list and
    /// destroys an open editor, so the editor's fate has to be decided here
    /// rather than left to a blur that may never arrive -- and the user's
    /// ruling (2026-09-07) is that a click anywhere else discards. That
    /// reverses task194-D, which had made the same moment commit; the cost is
    /// spelled out in `ui/controls.slint`'s `changed has-focus`.
    pub(super) fn close_editor(&mut self) {
        self.editing = None;
    }

    /// The rendered row index the `.slint` side counts in, back to an id.
    fn row_id(&self, row: i32) -> Option<String> {
        self.state
            .rendered()
            .get(usize::try_from(row).ok()?)
            .map(|session| session.session_id.clone())
    }
}

/// Padding and gap are the only two numbers still written on both sides, and
/// they are the two that never move.
const SESSION_GRID_PADDING: f32 = 12.0;
const SESSION_GRID_GAP: f32 = 10.0;

/// The rendered rows paired with their rectangles in the grid's content space.
///
/// `top`, `cell_height` and `columns` arrive with the pointer event rather than
/// being recomputed here: a second copy of the layout maths drifts by a whole
/// row the moment anything above the grid changes height (the junk group
/// appearing did exactly that), and a click then lands on the wrong session.
fn session_row_rects(
    history: &History,
    pane_width: f32,
    top: f32,
    cell_height: f32,
    columns: i32,
) -> (Vec<String>, Vec<sessions::Rect>) {
    let columns = columns.max(1) as usize;
    let cell_width =
        (pane_width - 36.0 - (columns as f32 - 1.0) * SESSION_GRID_GAP) / columns as f32;
    let rendered: Vec<String> = history
        .state
        .rendered()
        .iter()
        .map(|session| session.session_id.clone())
        .collect();
    let rects = sessions::grid_rects(
        rendered.len(),
        columns,
        cell_width,
        cell_height,
        SESSION_GRID_GAP,
        SESSION_GRID_PADDING,
    )
    .into_iter()
    .map(|rect| sessions::Rect {
        y: rect.y - SESSION_GRID_PADDING + top,
        ..rect
    })
    .collect();
    (rendered, rects)
}

/// Which rendered row a point landed on, or `None` for empty space. Shared by
/// the press, the double-click and the right-click: three gestures that differ
/// only in what they do with the answer.
fn hit_row(rendered: &[String], rects: &[sessions::Rect], x: f32, y: f32) -> Option<String> {
    rects
        .iter()
        .position(|rect| {
            rect.x <= x && x <= rect.x + rect.width && rect.y <= y && y <= rect.y + rect.height
        })
        .map(|index| rendered[index].clone())
}

/// `capture-{unix_nanos:032x}` → `2026/08/11 12:00:41` in local time. Win32
/// does the timezone and DST, same as the title bar's clock, so there is still
/// no date crate in the tree.
fn session_date_text(session_id: &str) -> String {
    let Some(millis) = sessions::session_epoch_millis(session_id) else {
        return session_id.to_owned();
    };
    local_stamp(millis).unwrap_or_else(|| session_id.to_owned())
}

/// The wall clock in the same `YYYY/MM/DD HH:MM:SS` shape a session id
/// produces, so `sessions::row_date_text` can compare the two without either
/// side learning what a calendar is (task243).
pub(super) fn now_date_text() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| local_stamp(since.as_millis() as i64))
        .unwrap_or_default()
}

/// A clip's modification time in the same shape (task3020), so the grid's date
/// headings and its tiles' relative dates run on the history screen's calendar
/// rather than a second one. Empty when the clock will not convert -- which
/// `ui_state::clips::place` and `sessions::row_date_text` both pass through.
pub(super) fn clip_date_text(modified: std::time::SystemTime) -> String {
    modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|since| local_stamp(since.as_millis() as i64))
        .unwrap_or_default()
}

/// Epoch milliseconds as local wall-clock text. `None` if Win32 refuses the
/// conversion, which is the caller's cue to fall back to whatever it had.
fn local_stamp(millis: i64) -> Option<String> {
    // FILETIME counts 100ns ticks from 1601-01-01; the epoch is 11644473600s later.
    let ticks = millis * 10_000 + 116_444_736_000_000_000;
    let file_time = FILETIME {
        dwLowDateTime: (ticks as u64 & 0xFFFF_FFFF) as u32,
        dwHighDateTime: ((ticks as u64) >> 32) as u32,
    };
    let mut utc = SYSTEMTIME::default();
    let mut local = SYSTEMTIME::default();
    unsafe {
        if FileTimeToSystemTime(&file_time, &mut utc).is_err()
            || SystemTimeToTzSpecificLocalTime(None, &utc, &mut local).is_err()
        {
            return None;
        }
    }
    Some(format!(
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02}",
        local.wYear, local.wMonth, local.wDay, local.wHour, local.wMinute, local.wSecond
    ))
}

pub(super) fn apply_session(history: &mut History, message: Msg) {
    match message {
        Msg::Sessions(Ok(sessions)) => {
            let live: HashSet<&str> = sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect();
            history
                .thumbnails
                .retain(|id, _| live.contains(id.as_str()));
            livia::insight_images!(History, history.thumbnails.len());
            history.requested.retain(|id| live.contains(id.as_str()));
            history.state.set_sessions(sessions);
            history.list_error = None;
            // A discard that succeeded leaves nothing to confirm.
            history
                .confirm
                .retain(|id| history.state.find(id).is_some());
        }
        Msg::Sessions(Err(_)) => history.list_error = Some(sessions::error(tr_locale())),
        Msg::SessionThumbnail(id, pixels) => {
            let image =
                pixels.and_then(|(width, height, rgba)| image_from_rgba(width, height, &rgba));
            history.thumbnails.insert(id, image);
            livia::insight_images!(History, history.thumbnails.len());
        }
        Msg::SessionError(error) => history.action = Some((error.to_owned(), true)),
        Msg::SessionFailed(text) => history.action = Some((text, true)),
        Msg::Loaded {
            session_id, title, ..
        } => {
            history.loaded = Some(session_id);
            history.action = Some((sessions::loaded_text(tr_locale(), &title), false));
        }
        _ => unreachable!("handled by apply"),
    }
}

/// Wires the session page's callbacks. Everything below moved verbatim out
/// of `main` (the blocks only borrow what they used to capture from it).
/// What every session-grid handler needs: the window, the state it borrows and
/// the two channels it ends on.
#[derive(Clone, Copy)]
struct SessionCtx<'a> {
    ui: &'a AppWindow,
    history: &'a Rc<RefCell<History>>,
    session_rows: &'a Rc<VecModel<SessionRow>>,
    cmd_tx: &'a Sender<Cmd>,
    controller: &'a CaptureController,
}

/// Every session interaction ends in one of two places: a state change that
/// only needs a re-render, or a command for the worker.
///
/// The context is a macro *parameter* rather than something read from the
/// enclosing scope, which is what lets these handlers live in more than one
/// function: `macro_rules!` resolves a bare `ui` at the definition site.
macro_rules! session_handler {
    ($ctx:expr, $setter:ident, |$window:ident, $history:ident, $cmd:ident $(, $arg:ident)*| $body:block) => {{
        let ctx = $ctx;
        let weak = ctx.ui.as_weak();
        let history_rc = ctx.history.clone();
        let rows = ctx.session_rows.clone();
        let $cmd = ctx.cmd_tx.clone();
        ctx.ui.$setter(move |$($arg),*| {
            let Some($window) = weak.upgrade() else { return };
            let mut $history = history_rc.borrow_mut();
            // **Before the body, because the body ends in a re-render**
            // (task1340). Rebuilding the list destroys an open editor, and
            // slint delivers no blur to an element it has already torn down --
            // so the editor's fate is decided here, on the way in, rather than
            // left to a callback that may never fire.
            //
            // It used to be a commit (task194-D). Since task3430 it discards:
            // every handler in this macro is reached by a click somewhere that
            // is not the open field, and the user's ruling is that such a click
            // throws the draft away. Enter and Esc are deliberately not
            // handlers of this macro -- they run through `session_edit!` and
            // `on_sessions_edit_cancelled`, which do their own thing first.
            $history.close_editor();
            $body
            render_sessions(&$window, &$history, &rows);
            queue_visible_thumbnails(&mut $history, &$cmd);
        });
    }};
}

/// The edits that reach disk. They need the controller, which the shared macro
/// does not carry.
macro_rules! session_edit {
    ($ctx:expr, $setter:ident, |$window:ident, $history:ident, $ctl:ident, $cmd:ident $(, $arg:ident)*| $body:block) => {{
        let ctx = $ctx;
        let weak = ctx.ui.as_weak();
        let history_rc = ctx.history.clone();
        let rows = ctx.session_rows.clone();
        let $ctl = ctx.controller.clone();
        let $cmd = ctx.cmd_tx.clone();
        ctx.ui.$setter(move |$($arg),*| {
            let Some($window) = weak.upgrade() else { return };
            let mut $history = history_rc.borrow_mut();
            $body
            render_sessions(&$window, &$history, &rows);
            queue_visible_thumbnails(&mut $history, &$cmd);
        });
    }};
}

#[allow(clippy::too_many_arguments)]
pub(super) fn wire(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    review: &Rc<RefCell<super::Review>>,
    stop_requests: &super::StopRequests,
    settings: &Arc<Mutex<AppSettings>>,
) {
    // The saved view (`sessionViewMode`): settings have parsed it and task078
    // migrated it since, but nothing ever read it back into the page, so every
    // launch opened on list however the user had left it (the user,
    // 2026-09-20). Read once here, written back by `on_sessions_set_view`.
    history
        .borrow_mut()
        .state
        .set_view_mode(match settings_snapshot(settings).session_view_mode {
            SessionViewMode::Thumbnail => ViewMode::Thumbnail,
            SessionViewMode::List => ViewMode::List,
        });
    wire_grid(
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
        review,
        settings,
    );
    wire_edits(ui, history, session_rows, cmd_tx, controller, stop_requests);
}

/// The edits that reach disk (rename, note, protect, discard), the LIVE
/// row's own stop, and the context menu those all hang off.
fn wire_edits(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    let ctx = SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
    };
    // `sessions-edit-draft-changed` is deliberately left unwired (task3430).
    // The draft it reported existed so a teardown could still commit what had
    // been typed (task1340); nothing commits on a teardown any more, so the
    // only text that ever reaches disk is the one Enter hands over directly.
    // An unset slint callback is a no-op, so the `.slint` side keeps calling
    // into nothing.

    // The edits that reach disk. They need the controller, which the render
    // macro does not carry, so each opens its own block -- same shape as the
    // review screen's marker handlers.

    session_edit!(
        ctx,
        on_sessions_note_committed,
        |ui, history, controller, cmd, row, note| {
            history.editing = None;
            let Some(id) = history.row_id(row) else {
                return;
            };
            let result = controller.set_session_note(&id, &note);
            if let Some(text) = sessions::note_update_notice(tr_locale(), &result) {
                history.action = Some((text.to_owned(), true));
            }
            let _ = cmd.send(Cmd::ListSessions);
        }
    );

    session_edit!(
        ctx,
        on_sessions_protect_toggled,
        |ui, history, controller, cmd, row| {
            history.menu = None;
            let Some(id) = history.row_id(row) else {
                return;
            };
            if history.state.find(&id).is_none() {
                return;
            }
            // `state.protected`, not the summary's own field: the overlay is
            // what the row is showing, so it is what this flips (t260920-bb09).
            let next = !history.state.protected(&id);
            let result = controller.set_session_protected(&id, next);
            history.state.stage_protected(&id, next);
            if let Some(text) = sessions::protect_update_notice(tr_locale(), &result) {
                history.state.rollback_protected(&id);
                history.action = Some((text.to_owned(), true));
            }
            let _ = cmd.send(Cmd::ListSessions);
        }
    );

    wire_stop_and_menu(ui, history, session_rows, cmd_tx, controller, stop_requests);
}

/// The LIVE row's own stop and the context menu the edits hang off.
fn wire_stop_and_menu(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    let ctx = SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
    };
    // ---------- the LIVE row's 22px stop (task2030 判断5) ----------
    // Stops that recording and leaves the others running, with no confirmation
    // (判断4). Its own block rather than `session_edit!` only because it needs
    // the shared stop-request list the toast is raised from.
    {
        let weak = ui.as_weak();
        let history_rc = history.clone();
        let rows = session_rows.clone();
        let controller = controller.clone();
        let cmd_tx = cmd_tx.clone();
        let stop_requests = stop_requests.clone();
        ui.on_sessions_stop_requested(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut history = history_rc.borrow_mut();
            history.menu = None;
            // `is_active`, not merely "the row drew a stop button": the row
            // model is one render behind the poll, so a capture that has just
            // ended must not be stopped a second time.
            let Some(id) = history
                .row_id(row)
                .filter(|id| history.state.is_active(id.as_str()))
            else {
                return;
            };
            tracing::info!(
                event = "capture_stop_requested",
                session_id = %id,
                "stop asked for by a history LIVE row"
            );
            livia::ui_state::lifecycle::request_stop(&mut stop_requests.borrow_mut(), &id);
            controller.stop_session_async(&id);
            let _ = cmd_tx.send(Cmd::Reclaim);
            let _ = cmd_tx.send(Cmd::ListSessions);
            render_sessions(&ui, &history, &rows);
            queue_visible_thumbnails(&mut history, &cmd_tx);
        });
    }

    // The menu is the receptacle the row's four buttons emptied into, so its
    // entries route to exactly what those buttons did (round5 §6-D) -- and
    // since 2026-08-17 it is also where the bottom-right selection popup went, so
    // a right-click inside a multi-row selection acts on the whole pile.
    session_edit!(
        ctx,
        on_sessions_menu_selected,
        |ui, history, controller, cmd, index| {
            let Some(menu) = history.menu.take() else {
                return;
            };
            let Some(session) = history.state.find(&menu) else {
                return;
            };
            let is_active = history.state.is_active(&menu);
            // Through the overlay, not the listing (t260920-bb09): while a
            // recording's toggle is still in the index writer's queue the
            // listing answers with the old value, and reading it here is what
            // made the *next* press re-send the value the last one had already
            // written instead of flipping it.
            let protected = history.state.protected(&menu);
            let load_disabled =
                sessions::is_load_disabled(session, history.state.capturing(), is_active, false);
            // Same rule, same order as `render_sessions` builds the entries
            // with: the click arrives as an index into that list.
            let targets = history.state.menu_targets(&menu);
            // One switch for the whole pile: if any of them is unprotected the
            // entry protects everything, which is what a mixed pile most often
            // means. On a single row that is plain `!protected`.
            let next_protected = targets.iter().any(|id| !history.state.protected(id));
            let has_live = targets.iter().any(|id| history.state.is_active(id));
            let items = if targets.len() > 1 {
                sessions::selection_menu(tr_locale(), !next_protected, has_live)
            } else {
                sessions::session_menu(
                    tr_locale(),
                    history.state.view_mode(),
                    protected,
                    load_disabled,
                    has_live,
                )
            };
            let Some(item) = items.get(usize::try_from(index).unwrap_or(usize::MAX)) else {
                return;
            };
            if item.disabled {
                return;
            }
            let id = menu;
            match item.action {
                SessionMenuAction::Load => {
                    let _ = cmd.send(if is_active {
                        Cmd::LoadActive(Some(id))
                    } else {
                        Cmd::Load(id)
                    });
                }
                SessionMenuAction::EditNote => history.editing = Some(id),
                SessionMenuAction::ToggleProtect => {
                    for id in &targets {
                        let result = controller.set_session_protected(id, next_protected);
                        // Optimistic first, rolled back on the error, so the
                        // row and this menu's own label are right before the
                        // listing that will not carry it yet (t260920-bb09).
                        history.state.stage_protected(id, next_protected);
                        if let Some(text) = sessions::protect_update_notice(tr_locale(), &result) {
                            history.state.rollback_protected(id);
                            history.action = Some((text.to_owned(), true));
                            break;
                        }
                    }
                    let _ = cmd.send(Cmd::ListSessions);
                }
                SessionMenuAction::OpenDirectory => {
                    let _ = cmd.send(Cmd::OpenDirectory(id));
                }
                // The disabled check above already turned this away, but the
                // pile is re-read here and a recording must not slip through
                // whichever way it arrived (t260920-bb09).
                SessionMenuAction::Discard => {
                    if !targets.iter().any(|id| history.state.is_active(id)) {
                        history.confirm = targets;
                    }
                }
            }
        }
    );
}

/// The row menu, the inline editor and the confirm dialog those open.
fn wire_grid(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    review: &Rc<RefCell<super::Review>>,
    settings: &Arc<Mutex<AppSettings>>,
) {
    let ctx = SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
    };
    {
        // Every session interaction ends in one of two places: a state change
        // that only needs a re-render, or a command for the worker.

        session_handler!(
            ctx,
            on_sessions_pressed,
            |ui, history, cmd, x, y, top, cell, columns, shift, ctrl| {
                let (rendered, rects) =
                    session_row_rects(&history, ui.get_pane_width(), top, cell, columns);
                let hit = hit_row(&rendered, &rects, x, y);
                history.marquee_base = history.state.press(hit.as_deref(), shift, ctrl);
                history.dragging = true;
                // A press anywhere is also the way out of an open menu -- and,
                // since task3430, out of an open editor: this macro's prologue
                // has already discarded it. task194-D's second pass left the
                // editor standing here so the `keys.focus()` this press issues
                // would blur it into a commit; with blur meaning "discard" the
                // two paths now agree, and the prologue is the one that runs
                // first.
                history.menu = None;
            }
        );

        // Wired by hand rather than through `session_handler!` (task189): the
        // macro re-renders every row afterwards, and this fires on every step
        // of a drag. A drag only ever moves `selected`, so it writes back the
        // rows that changed and nothing else -- no `render_sessions`, and no
        // `queue_visible_thumbnails` either, since the visible range does not
        // move while the button is down. `marquee_ended` still goes through the
        // macro, so the release always reconciles the whole list.
        {
            let weak = ui.as_weak();
            let history_rc = history.clone();
            let rows_model = session_rows.clone();
            ui.on_sessions_marquee(move |x1, y1, x2, y2, top, cell, columns| {
                let Some(ui) = weak.upgrade() else { return };
                let mut history = history_rc.borrow_mut();
                if !history.dragging {
                    return;
                }
                let (rendered, rects) =
                    session_row_rects(&history, ui.get_pane_width(), top, cell, columns);
                let rows: Vec<(String, sessions::Rect)> = rendered.into_iter().zip(rects).collect();
                let base = history.marquee_base.clone();
                history
                    .state
                    .marquee(&base, &rows, sessions::Rect::from_corners(x1, y1, x2, y2));
                apply_selection(&ui, &history, &rows_model);
            });
        }

        session_handler!(ctx, on_sessions_marquee_ended, |ui, history, cmd| {
            history.dragging = false;
        });

        session_handler!(
            ctx,
            on_sessions_search_changed,
            |ui, history, cmd, query| {
                history.state.set_search(&query);
                // A filter that moves rows under an open editor or menu would leave
                // both pointing at whatever slid into that slot.
                history.editing = None;
                history.menu = None;
            }
        );

        let view_settings = settings.clone();
        session_handler!(ctx, on_sessions_set_view, |ui, history, cmd, thumbnail| {
            history.state.set_view_mode(if thumbnail {
                ViewMode::Thumbnail
            } else {
                ViewMode::List
            });
            // Straight to disk, like every other remembered toggle: the
            // choice is the kind that has to survive a restart, and there is
            // no 設定 row for it to be saved by.
            commit_settings(&view_settings, |current| {
                current.session_view_mode = if thumbnail {
                    SessionViewMode::Thumbnail
                } else {
                    SessionViewMode::List
                };
            });
            // A tile draws no note, so an open note editor has nowhere to be.
            history.editing = None;
            history.menu = None;
        });

        // Loading is the double-click now: the row's 読み込む button went with
        // the rest of the row's action cluster (round5 §6-A). The recording keeps
        // its own door (task162) -- the LiveReview path is unchanged, only the
        // gesture that opens it.
        session_handler!(
            ctx,
            on_sessions_row_activated,
            |ui, history, cmd, x, y, top, cell, columns| {
                let (rendered, rects) =
                    session_row_rects(&history, ui.get_pane_width(), top, cell, columns);
                let Some(id) = hit_row(&rendered, &rects, x, y) else {
                    return;
                };
                // A LIVE row names itself (task2030): this is the only way to
                // move the stage from one running capture to another.
                let _ = cmd.send(if history.state.is_active(&id) {
                    Cmd::LoadActive(Some(id))
                } else {
                    Cmd::Load(id)
                });
            }
        );

        session_handler!(
            ctx,
            on_sessions_row_menu_requested,
            |ui, history, cmd, x, y, top, cell, columns| {
                let (rendered, rects) =
                    session_row_rects(&history, ui.get_pane_width(), top, cell, columns);
                // Right-clicking a row that is not in the selection selects it
                // first (task1120), so the menu never acts on something the
                // user can see is not picked. Inside the selection, the pile
                // stands and the multi-row menu opens.
                let hit = hit_row(&rendered, &rects, x, y);
                if let Some(session_id) = hit.as_deref() {
                    history.state.select_for_menu(session_id);
                }
                history.menu = hit;
                history.editing = None;
            }
        );

        wire_row_editor(ui, history, session_rows, cmd_tx, controller, review);
    }
}

/// The row menu's own entries and the inline editor they open.
fn wire_row_editor(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    review: &Rc<RefCell<super::Review>>,
) {
    let ctx = SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
    };
    {
        session_handler!(ctx, on_sessions_menu_dismissed, |ui, history, cmd| {
            history.menu = None;
        });

        // F2. Only meaningful on a single selection: the menu that advertises
        // the key is itself single-row, and one note for several rows has no
        // meaning. And only in the list view: a tile draws no note editor.
        // It opened the title editor until titles stopped being editable
        // (the user, 2026-09-19).
        session_handler!(ctx, on_sessions_note_edit_selected, |ui, history, cmd| {
            let view = history.state.view_mode();
            if let (true, [id]) = (sessions::f2_edits_note_in(view), history.state.selected()) {
                let id = id.clone();
                history.menu = None;
                history.editing = Some(id);
            }
        });

        // Delete. Unlike F2 this takes the whole pile: the confirm screen
        // already counts the rows and their bytes, so several at once is one
        // screen, not several. It opens that screen rather than discarding --
        // the selection is a gesture, the confirmation is the decision.
        //
        // An empty selection stays silent. A confirm screen offering to
        // discard nothing is a dialog the user has to dismiss to learn it had
        // nothing to say.
        session_handler!(ctx, on_sessions_discard_selected, |ui, history, cmd| {
            let targets = history.state.selected().to_vec();
            // A pile that holds a recording opens nothing at all -- not the
            // confirmation with the rest of it, because an implicit exclusion
            // would disagree with the count the pile shows (t260920-bb09, the
            // user's 2026-09-19 ruling). The menu's 破棄 is disabled for the
            // same pile, so the two doors answer alike.
            if !targets.is_empty() && !targets.iter().any(|id| history.state.is_active(id)) {
                history.menu = None;
                history.confirm = targets;
            }
        });

        // Ctrl+A, mirroring the clip screen's `on_clips_select_all`
        // (follow-up of t260916-5568, the user's 2026-09-19 「この場で実装」).
        // `select_all` takes the rendered rows only and never a live one, so
        // under a search or a collapsed group it arms nothing the user
        // cannot see (task115, task2030).
        session_handler!(ctx, on_sessions_select_all, |ui, history, cmd| {
            history.state.select_all();
        });

        session_handler!(
            ctx,
            on_sessions_note_edit_requested,
            |ui, history, cmd, row| {
                history.menu = None;
                history.editing = history.row_id(row);
            }
        );

        // Esc, wired by hand: every `session_handler!` saves an open editor on
        // the way in, and Esc must reach *this* handler rather than being
        // answered by it (task1340). Since task3430 the prologue discards too,
        // so the two agree on the outcome -- but the macro would still fire a
        // second render before the body ran, so the hand-wiring stays.
        {
            let weak = ui.as_weak();
            let history_rc = history.clone();
            let rows = session_rows.clone();
            let cmd = cmd_tx.clone();
            ui.on_sessions_edit_cancelled(move || {
                let Some(ui) = weak.upgrade() else { return };
                let mut history = history_rc.borrow_mut();
                history.close_editor();
                render_sessions(&ui, &history, &rows);
                queue_visible_thumbnails(&mut history, &cmd);
            });
        }

        session_handler!(ctx, on_sessions_confirm_cancelled, |ui, history, cmd| {
            history.confirm.clear();
        });

        // Wired by hand rather than through `session_handler!` (task188): that
        // macro re-renders every row afterwards, and this fires on every step
        // of a scroll. Nothing here touches what is drawn -- the thumbnails it
        // asks for arrive as `Msg::SessionThumbnail` and reach the screen on
        // the next ordinary render.
        {
            let history_rc = history.clone();
            let cmd = cmd_tx.clone();
            ui.on_sessions_visible_range_changed(move |first, count| {
                let mut history = history_rc.borrow_mut();
                history.visible_range = Some((first.max(0) as usize, count.max(0) as usize));
                queue_visible_thumbnails(&mut history, &cmd);
            });
        }

        {
            // The review pane's engine holds a lease on whatever it has open,
            // and `discard_session` refuses a leased session -- so discarding
            // the recording you are watching failed every time, and (until the
            // pane's alert was drawn at all) failed in silence (task1520).
            // Unloading first is also the only honest end state: the file is
            // about to go, so the picture on the stage cannot stay.
            let weak = ui.as_weak();
            let history_rc = history.clone();
            let rows = session_rows.clone();
            let review_rc = review.clone();
            let cmd = cmd_tx.clone();
            ui.on_sessions_confirm_accepted(move || {
                let Some(ui) = weak.upgrade() else { return };
                let mut history = history_rc.borrow_mut();
                // Same as `session_handler!`'s prologue: the render below tears
                // an open editor down, and slint delivers no blur to an element
                // it has already destroyed, so the edit is ended here
                // (task1340, discarding since task3430).
                history.close_editor();
                let targets = std::mem::take(&mut history.confirm);
                if !targets.is_empty() {
                    // The last attempt's verdict is not this one's: leaving a
                    // red "破棄に失敗しました" standing over a discard that
                    // then succeeds is the same lie in the other direction.
                    history.action = None;
                    if history
                        .loaded
                        .as_ref()
                        .is_some_and(|loaded| targets.iter().any(|id| id == loaded))
                    {
                        // Synchronous: dropping the engine joins its worker,
                        // which releases the lease before the command below
                        // reaches the file.
                        super::review::unload_review(&mut review_rc.borrow_mut(), &ui);
                        history.loaded = None;
                    }
                    let _ = cmd.send(Cmd::Discard(targets));
                }
                render_sessions(&ui, &history, &rows);
                queue_visible_thumbnails(&mut history, &cmd);
            });
        }
    }
}
