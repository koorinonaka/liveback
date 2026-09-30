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
use slint::{ComponentHandle, Image, Model, VecModel};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{image_from_rgba, AppWindow, Cmd, Msg, SessionRow, SessionsVm};

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
    /// Where a failed action is said (t260928-e309 F4): the main loop's own
    /// channel, whose `Msg::SessionError` arm raises the error toast. It used
    /// to be an `action` field drawn as the pane's Alert above the list. Set
    /// once by `wire`.
    failures: Option<Sender<Msg>>,
    /// The executables' icons for the app column (t260927-9a17), by image
    /// path, read once each on the session worker. `None` is a path that gave
    /// no icon, cached so it is not asked for again.
    app_icons: HashMap<String, Option<Image>>,
    /// The tile grid's column count as the pane last reported it; the list is
    /// always one. The date groups are placed with it (`sessions::place`).
    columns: usize,
    /// The settings, for the footer's StorageMeter limit.
    settings: Option<Arc<Mutex<AppSettings>>>,
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
    /// Says a failed action on the error toast (t260928-e309 F4).
    fn fail(&self, title: &'static str) {
        if let Some(tx) = &self.failures {
            let _ = tx.send(Msg::SessionError(title));
        }
    }

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

/// Where the tag an app-icon request travels under starts (t260927-9a17).
///
/// ponytail: the icon rides the session thumbnail's own round trip --
/// `Cmd::Thumbnail` out, `Msg::SessionThumbnail` back -- keyed by this prefix
/// and the executable's path, because the two enums live in the bin root.
/// Give them their own variants when anything else needs the same trip.
pub(super) const APP_ICON_KEY: &str = "app-icon:";

/// The grid's pixel constants, the ones `sessions.slint` lays out with: 24 each
/// side for the list and 28 for the tiles, rows 2 apart and tiles 8 / 12, and a
/// date heading 36 tall *including* the row gap before it (t260928-23aa): the
/// grid starts one gap down, so every heading -- the first one too, in either
/// view -- sits 16 below the row above it and 4 above its own first row.
fn grid_metrics(view: ViewMode) -> sessions::GridMetrics {
    let tiles = view == ViewMode::Thumbnail;
    let gap_y = if tiles { 8.0 } else { 2.0 };
    sessions::GridMetrics {
        pad_x: if tiles { 28.0 } else { 24.0 },
        pad_top: gap_y,
        gap_x: 12.0,
        gap_y,
        heading_pitch: 36.0 - gap_y,
    }
}

impl History {
    /// The rendered rows under their date headings, `columns` wide.
    fn placement(&self, columns: usize) -> Vec<livia::ui_state::clips::ClipPlacement> {
        let columns = if self.state.view_mode() == ViewMode::Thumbnail {
            columns.max(1)
        } else {
            1
        };
        let stamps: Vec<String> = self
            .state
            .rendered()
            .iter()
            .map(|session| session_date_text(&session.session_id))
            .collect();
        sessions::place(tr_locale(), &stamps, &now_date_text(), columns)
    }
}

/// The rendered rows paired with their rectangles in the grid's content space.
///
/// The cell size and `columns` arrive with the pointer event rather than being
/// recomputed here: a second copy of the layout maths drifts the moment the
/// pane's width does, and a click then lands on the wrong session.
fn session_row_rects(
    history: &History,
    cell_width: f32,
    cell_height: f32,
    columns: i32,
) -> (Vec<String>, Vec<sessions::Rect>) {
    let rendered: Vec<String> = history
        .state
        .rendered()
        .iter()
        .map(|session| session.session_id.clone())
        .collect();
    let placed = history.placement(columns.max(1) as usize);
    let rects = sessions::placed_rects(
        &placed,
        grid_metrics(history.state.view_mode()),
        cell_width,
        cell_height,
    );
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
        }
        Msg::Sessions(Err(_)) => history.list_error = Some(sessions::error(tr_locale())),
        Msg::SessionThumbnail(id, pixels) => {
            let image =
                pixels.and_then(|(width, height, rgba)| image_from_rgba(width, height, &rgba));
            if let Some(path) = id.strip_prefix(APP_ICON_KEY) {
                history.app_icons.insert(path.to_owned(), image);
                return;
            }
            history.thumbnails.insert(id, image);
            livia::insight_images!(History, history.thumbnails.len());
        }
        // The screen that opens is the report; nothing is said about it
        // (t260928-e309 F4: 「読み込んだ結果は画面が見せている」).
        Msg::Loaded { session_id, .. } => history.loaded = Some(session_id),
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
    failures: &Sender<Msg>,
) {
    // The saved view (`sessionViewMode`): settings have parsed it and task078
    // migrated it since, but nothing ever read it back into the page, so every
    // launch opened on list however the user had left it (the user,
    // 2026-09-20). Read once here, written back by `on_sessions_set_view`.
    {
        let mut history = history.borrow_mut();
        history
            .state
            .set_view_mode(match settings_snapshot(settings).session_view_mode {
                SessionViewMode::Thumbnail => ViewMode::Thumbnail,
                SessionViewMode::List => ViewMode::List,
            });
        history.settings = Some(settings.clone());
        history.failures = Some(failures.clone());
    }
    wire_grid(ui, history, session_rows, cmd_tx, controller, settings);
    wire_edits(
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
        (stop_requests, review),
    );
    wire_bar(ui, history, session_rows, cmd_tx, controller, review);
}

/// Sends a pile to the recycle bin (t260927-9a17): the hold -- on the selection
/// bar, on the menu's 破棄, or on the Delete key -- is the decision, so there
/// is no confirm dialog in between.
///
/// The review pane's engine holds a lease on whatever it has open, and
/// `discard_session` refuses a leased session -- so the recording on the stage
/// is unloaded first (task1520). A pile holding a recording discards nothing:
/// the bar and the menu both refuse it already, and an implicit exclusion would
/// disagree with the count the pile shows (t260920-bb09).
fn discard_now(
    ui: &AppWindow,
    history: &mut History,
    review: &Rc<RefCell<super::Review>>,
    cmd: &Sender<Cmd>,
    targets: Vec<String>,
) {
    if targets.is_empty() || targets.iter().any(|id| history.state.is_active(id)) {
        return;
    }
    if history
        .loaded
        .as_ref()
        .is_some_and(|loaded| targets.iter().any(|id| id == loaded))
    {
        // Synchronous: dropping the engine joins its worker, which releases
        // the lease before the command below reaches the file.
        super::review::unload_review(&mut review.borrow_mut(), ui);
        history.loaded = None;
    }
    let _ = cmd.send(Cmd::Discard(targets));
}

/// Flips protection on every id in `targets`: if any of them is unprotected the
/// pile is protected, which is what a mixed pile most often means.
fn toggle_protect(history: &mut History, controller: &CaptureController, targets: &[String]) {
    let next = targets.iter().any(|id| !history.state.protected(id));
    for id in targets {
        let result = controller.set_session_protected(id, next);
        // Optimistic first, rolled back on the error, so the row and the
        // labels are right before the listing that will not carry it yet
        // (t260920-bb09).
        history.state.stage_protected(id, next);
        if let Some(text) = sessions::protect_update_notice(tr_locale(), &result) {
            history.state.rollback_protected(id);
            history.fail(text);
            break;
        }
    }
}

/// What one of the selection bar's buttons does to the page state.
type BarAction = Box<dyn Fn(&AppWindow, &mut History)>;

/// The page's own global (`SessionsVm`): the selection bar, the row checkbox,
/// and the grid's column count.
fn wire_bar(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    review: &Rc<RefCell<super::Review>>,
) {
    let vm = ui.global::<SessionsVm>();
    // Every handler: close an open editor (a press elsewhere discards it,
    // task3430), act, re-render.
    let handler = |act: BarAction| {
        let weak = ui.as_weak();
        let history_rc = history.clone();
        let rows = session_rows.clone();
        let cmd = cmd_tx.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut history = history_rc.borrow_mut();
            history.close_editor();
            history.menu = None;
            act(&ui, &mut history);
            render_sessions(&ui, &history, &rows);
            queue_visible_thumbnails(&mut history, &cmd);
        }
    };
    vm.on_clear_selection(handler(Box::new(|_, history| {
        history.state.clear_selection();
    })));
    let protect_controller = controller.clone();
    let protect_cmd = cmd_tx.clone();
    vm.on_protect_selection(handler(Box::new(move |_, history| {
        let targets = history.state.selected().to_vec();
        toggle_protect(history, &protect_controller, &targets);
        let _ = protect_cmd.send(Cmd::ListSessions);
    })));
    let discard_review = review.clone();
    let discard_cmd = cmd_tx.clone();
    vm.on_discard_selection(handler(Box::new(move |ui, history| {
        let targets = history.state.selected().to_vec();
        discard_now(ui, history, &discard_review, &discard_cmd, targets);
    })));
    // Enter on the list (t260928-23aa): the menu's first entry, on the one
    // selected row, refused exactly where that entry is disabled.
    let open_cmd = cmd_tx.clone();
    vm.on_open_selected(handler(Box::new(move |_, history| {
        let [id] = history.state.selected() else {
            return;
        };
        let id = id.clone();
        let Some(session) = history.state.find(&id) else {
            return;
        };
        let is_active = history.state.is_active(&id);
        if sessions::is_load_disabled(session, history.state.capturing(), is_active, false) {
            return;
        }
        let _ = open_cmd.send(if is_active {
            Cmd::LoadActive(Some(id))
        } else {
            Cmd::Load(id)
        });
    })));

    // The checkbox is a Ctrl-press on its row (決めたこと 7).
    {
        let weak = ui.as_weak();
        let history_rc = history.clone();
        let rows = session_rows.clone();
        let cmd = cmd_tx.clone();
        vm.on_check_toggled(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut history = history_rc.borrow_mut();
            history.close_editor();
            history.menu = None;
            if let Some(id) = history.row_id(row) {
                history.state.press(Some(id.as_str()), false, true);
            }
            render_sessions(&ui, &history, &rows);
            queue_visible_thumbnails(&mut history, &cmd);
        });
    }

    // The pane reports its tile columns from `init` as well as on a change,
    // which lands mid-instantiation of the pane -- so the re-render it asks
    // for is deferred to the event loop rather than run inside the report.
    {
        let weak = ui.as_weak();
        let history_rc = history.clone();
        let rows = session_rows.clone();
        let cmd = cmd_tx.clone();
        vm.on_columns_changed(move |columns| {
            let columns = columns.max(1) as usize;
            if history_rc.borrow().columns == columns {
                return;
            }
            history_rc.borrow_mut().columns = columns;
            let weak = weak.clone();
            let history_rc = history_rc.clone();
            let rows = rows.clone();
            let cmd = cmd.clone();
            slint::Timer::single_shot(std::time::Duration::ZERO, move || {
                let Some(ui) = weak.upgrade() else { return };
                let mut history = history_rc.borrow_mut();
                render_sessions(&ui, &history, &rows);
                queue_visible_thumbnails(&mut history, &cmd);
            });
        });
    }
}

/// The edits that reach disk (rename, note, protect, discard), the LIVE
/// row's own stop, and the context menu those all hang off.
fn wire_edits(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    (stop_requests, review): (&super::StopRequests, &Rc<RefCell<super::Review>>),
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
                history.fail(text);
            }
            let _ = cmd.send(Cmd::ListSessions);
        }
    );

    wire_stop_and_menu(
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
        (stop_requests, review),
    );
}

/// The LIVE row's own stop and the context menu the edits hang off.
fn wire_stop_and_menu(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    (stop_requests, review): (&super::StopRequests, &Rc<RefCell<super::Review>>),
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
        let review_rc = review.clone();
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
            let loaded = super::review::loaded_session_id(&review_rc.borrow());
            let _ = cmd_tx.send(Cmd::Reclaim(loaded));
            let _ = cmd_tx.send(Cmd::ListSessions);
            render_sessions(&ui, &history, &rows);
            queue_visible_thumbnails(&mut history, &cmd_tx);
        });
    }

    // The menu is the receptacle the row's four buttons emptied into, so its
    // entries route to exactly what those buttons did (round5 §6-D) -- and
    // since 2026-08-17 it is also where the bottom-right selection popup went, so
    // a right-click inside a multi-row selection acts on the whole pile.
    let menu_review = review.clone();
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
            // One switch for the whole pile (`toggle_protect`); on a single row
            // that is plain `!protected`.
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
                    toggle_protect(&mut history, &controller, &targets);
                    let _ = cmd.send(Cmd::ListSessions);
                }
                SessionMenuAction::OpenDirectory => {
                    let _ = cmd.send(Cmd::OpenDirectory(id));
                }
                // A hold entry: the hold was the decision (t260927-9a17).
                // `discard_now` re-reads the pile, so a recording cannot slip
                // through whichever way it arrived (t260920-bb09).
                SessionMenuAction::Discard => {
                    discard_now(&ui, &mut history, &menu_review, &cmd, targets);
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
            |ui, history, cmd, x, y, cell_width, cell, columns, shift, ctrl| {
                let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
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
            ui.on_sessions_marquee(move |x1, y1, x2, y2, cell_width, cell, columns| {
                let Some(ui) = weak.upgrade() else { return };
                let mut history = history_rc.borrow_mut();
                if !history.dragging {
                    return;
                }
                let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
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
            |ui, history, cmd, x, y, cell_width, cell, columns| {
                let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
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
            |ui, history, cmd, x, y, cell_width, cell, columns| {
                let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
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

        wire_row_editor(ui, history, session_rows, cmd_tx, controller);
    }
}

/// The row menu's own entries and the inline editor they open.
fn wire_row_editor(
    ui: &AppWindow,
    history: &Rc<RefCell<History>>,
    session_rows: &Rc<VecModel<SessionRow>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
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

        // Wired by hand rather than through `session_handler!` (task188): that
        // macro re-renders every row afterwards, and this fires on every step
        // of a scroll. Nothing here touches what is drawn -- the thumbnails it
        // asks for arrive as `Msg::SessionThumbnail` and reach the screen on
        // the next ordinary render.
        //
        // It also grows the grid's cell pool (t260928-e029): one model, set
        // here once and pushed to when the window needs more cells than it
        // has. A push adds cells without touching the ones already built, so
        // no tile re-runs its entrance. Never shrunk and never replaced -- a
        // new model would rebuild every cell.
        {
            let history_rc = history.clone();
            let cmd = cmd_tx.clone();
            let slots = Rc::new(VecModel::<i32>::default());
            ui.global::<SessionsVm>().set_slots(slots.clone().into());
            ui.on_sessions_visible_range_changed(move |first, count| {
                while slots.row_count() < count.max(0) as usize {
                    slots.push(slots.row_count() as i32);
                }
                let mut history = history_rc.borrow_mut();
                history.visible_range = Some((first.max(0) as usize, count.max(0) as usize));
                queue_visible_thumbnails(&mut history, &cmd);
            });
        }
    }
}

/// Polls between re-listings while something records (t260927-2aa4). A
/// recording's own size moves only on a listing, and nothing else sends one
/// while it runs, so the StorageMeter (履歴 footer and 設定 alike) sat at the
/// size the session had when it was first listed. `list_sessions` opens each
/// container at its checkpoint rather than scanning it, so this is cheap.
const LIVE_RELIST_POLLS: u32 = 5;

/// Whether this 1s capture poll should re-list the sessions: every
/// [`LIVE_RELIST_POLLS`]th poll while something records, and once more on the
/// first poll that finds nothing recording -- a capture that ends on its own
/// sends no listing, so its final size would otherwise lag a cadence behind.
pub(super) fn live_relist_due(polls: &mut u32, recording: bool) -> bool {
    if !recording {
        return std::mem::take(polls) > 0;
    }
    *polls += 1;
    (*polls).is_multiple_of(LIVE_RELIST_POLLS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn session(id: &str, bytes: u64, protected: bool) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            start_100ns: None,
            end_100ns: None,
            segment_count: 1,
            total_bytes: bytes,
            closed: id != "capture-live",
            has_partial: false,
            recoverable: false,
            manifest_version: 2,
            readable: true,
            target_title: None,
            thumbnail_index: None,
            note: None,
            protected,
            target_executable: None,
            target_executable_path: None,
            marker_count: 0,
        }
    }

    #[test]
    fn a_recording_relists_every_few_polls_and_once_when_it_stops() {
        let mut polls = 0;
        let due: Vec<usize> = (1..=10)
            .filter(|_| live_relist_due(&mut polls, true))
            .collect();
        assert_eq!(due, [5, 10]);
        // The stop edge lists once for the final size, then idle stays quiet.
        assert!(live_relist_due(&mut polls, false));
        assert!(!live_relist_due(&mut polls, false));
        // Idle from the start never lists: nothing was recording to catch up on.
        let mut idle = 0;
        assert!(!(0..10).any(|_| live_relist_due(&mut idle, false)));
        // A short recording still gets its final listing.
        let mut short = 0;
        assert!(!live_relist_due(&mut short, true));
        assert!(live_relist_due(&mut short, false));
    }

    #[test]
    fn the_storage_figures_follow_a_relisted_recording() {
        let mut history = History::default();
        let listed = |live: u64| {
            Msg::Sessions(Ok(vec![
                session("capture-live", live, false),
                session("capture-kept", GB, true),
            ]))
        };
        apply_session(&mut history, listed(10 << 20));
        history.state.set_active_ids(vec!["capture-live".into()]);
        assert_eq!(render::storage_bytes(&history.state), (GB + (10 << 20), GB));
        apply_session(&mut history, listed(500 << 20));
        assert_eq!(
            render::storage_bytes(&history.state),
            (GB + (500 << 20), GB)
        );
    }
}
