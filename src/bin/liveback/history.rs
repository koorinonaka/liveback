//! The session-history page (task125): its UI-thread state, the session
//! worker, the row renderer, the message application, and the callback wiring
//! moved out of `main`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Sender;
use livia::capture::CaptureController;
use livia::settings::{AppSettings, SessionViewMode};
use livia::ui_state::sessions::{self, SessionMenuAction, SessionsState, ViewMode};
use slint::{ComponentHandle, Image, Model, VecModel};
use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};

use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{image_from_rgba, AppWindow, Cmd, HistoryMsg, Msg, SessionRow, SessionsVm};

// `#[path]` for the same reason as the crate root's module declarations: the
// parent `mod history` is itself `#[path]`-declared, so children resolve
// relative to this file's directory rather than a `history/` subdirectory.
#[path = "history/preview.rs"]
mod preview;
#[path = "history/render.rs"]
mod render;
#[path = "history/worker.rs"]
mod worker;

pub(super) use render::{
    apply_selection, arm_face_timer, next_face_due, queue_visible_thumbnails, render_sessions,
    thumbnail_face_held, THUMBNAIL_FACE_DELAY,
};
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
    /// thumbnail that is merely still in flight -- with when, which is what a
    /// tile's placeholder glyph waits from (t260930-cc68).
    pub(super) requested: HashMap<String, Instant>,
    /// Re-renders when a held placeholder face comes due (t260930-cc68).
    /// Started once in `wire` with its render, then only re-armed.
    pub(super) face_timer: slint::Timer,
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
    /// The running hover preview's cancel flag (`preview`), if one is.
    preview: Option<Arc<std::sync::atomic::AtomicBool>>,
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

/// Where an app-icon request's key in `History::requested` starts
/// (t260927-9a17), so it cannot collide with a session id there. The round
/// trip itself has its own variants since t261025-0e3d (`Cmd::AppIcon`,
/// `HistoryMsg::AppIcon`).
pub(super) const APP_ICON_KEY: &str = "app-icon:";

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
        sessions::grid_metrics(history.state.view_mode()),
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
        .position(|rect| rect.contains(x, y))
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

pub(super) fn apply_session(history: &mut History, message: HistoryMsg) {
    match message {
        HistoryMsg::Sessions(Ok(sessions)) => {
            let live: HashSet<&str> = sessions
                .iter()
                .map(|session| session.session_id.as_str())
                .collect();
            history
                .thumbnails
                .retain(|id, _| live.contains(id.as_str()));
            livia::insight_images!(History, history.thumbnails.len());
            history.requested.retain(|id, _| live.contains(id.as_str()));
            history.state.set_sessions(sessions);
            history.list_error = None;
        }
        HistoryMsg::Sessions(Err(_)) => history.list_error = Some(sessions::error(tr_locale())),
        HistoryMsg::SessionThumbnail(id, pixels) => {
            let image =
                pixels.and_then(|(width, height, rgba)| image_from_rgba(width, height, &rgba));
            // Request to arrival, per row (t260930-cc68): whether its tile's
            // placeholder glyph ever came up is this against 150ms.
            if let Some(at) = history.requested.get(&id) {
                tracing::info!(
                    event = "t260930_cc68_thumbnail_arrived",
                    pane = "history",
                    id = %id,
                    elapsed_ms = at.elapsed().as_secs_f64() * 1000.0,
                    has_image = image.is_some(),
                    "thumbnail back from the worker"
                );
            }
            history.thumbnails.insert(id, image);
            livia::insight_images!(History, history.thumbnails.len());
        }
        // The screen that opens is the report; nothing is said about it
        // (t260928-e309 F4: 「読み込んだ結果は画面が見せている」).
        HistoryMsg::AppIcon(path, pixels) => {
            let image =
                pixels.and_then(|(width, height, rgba)| image_from_rgba(width, height, &rgba));
            history.app_icons.insert(path, image);
        }
        HistoryMsg::Loaded { session_id, .. } => history.loaded = Some(session_id),
    }
}

/// The load a row asks for (t261025-0e3d: three doors spelled it out): a LIVE
/// row names its recording (task2030), anything else is a closed session.
fn load_cmd(is_active: bool, id: String) -> Cmd {
    if is_active {
        Cmd::LoadActive(Some(id))
    } else {
        Cmd::Load(id)
    }
}

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
///
/// The callbacks are `SessionsVm`'s (t261025-1aec); `window.` names one of
/// the few the shell raises too, which stayed on `AppWindow`.
macro_rules! session_handler {
    ($ctx:expr, window.$setter:ident, $($rest:tt)*) => {
        session_handler!(@on $ctx, (), $setter, $($rest)*)
    };
    ($ctx:expr, $setter:ident, $($rest:tt)*) => {
        session_handler!(@on $ctx, (.global::<SessionsVm>()), $setter, $($rest)*)
    };
    (@on $ctx:expr, ($($vm:tt)*), $setter:ident, |$window:ident, $history:ident, $cmd:ident $(, $arg:ident)*| $body:block) => {{
        let ctx = $ctx;
        let weak = ctx.ui.as_weak();
        let history_rc = ctx.history.clone();
        let rows = ctx.session_rows.clone();
        let $cmd = ctx.cmd_tx.clone();
        ctx.ui $($vm)* .$setter(move |$($arg),*| {
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
        ctx.ui.global::<SessionsVm>().$setter(move |$($arg),*| {
            let Some($window) = weak.upgrade() else { return };
            let mut $history = history_rc.borrow_mut();
            $body
            render_sessions(&$window, &$history, &rows);
            queue_visible_thumbnails(&mut $history, &$cmd);
        });
    }};
}

/// Wires the session page's callbacks. Everything below moved verbatim out
/// of `main` (the blocks only borrow what they used to capture from it).
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
    // 2026-09-20). Read once here, written back by `on_set_view`.
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
    let ctx = SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
    };
    wire_grid(ctx, settings);
    preview::wire(ui, history, controller);
    wire_edits(ctx, (stop_requests, review));
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
        let _ = open_cmd.send(load_cmd(is_active, id));
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
    ctx: SessionCtx<'_>,
    (stop_requests, review): (&super::StopRequests, &Rc<RefCell<super::Review>>),
) {
    // The edits that reach disk. They need the controller, which the render
    // macro does not carry, so each opens its own block -- same shape as the
    // review screen's marker handlers.

    session_edit!(
        ctx,
        on_note_committed,
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

    wire_stop_and_menu(ctx, (stop_requests, review));
}

/// The LIVE row's own stop and the context menu the edits hang off.
fn wire_stop_and_menu(
    ctx: SessionCtx<'_>,
    (stop_requests, review): (&super::StopRequests, &Rc<RefCell<super::Review>>),
) {
    let SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
    } = ctx;
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
        ui.global::<SessionsVm>().on_stop_requested(move |row| {
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
            super::shell::stop_one(
                &controller,
                &stop_requests,
                super::shell::StopWhich::One {
                    session_id: &id,
                    cmd_tx: &cmd_tx,
                    review: &review_rc,
                },
                "history_live_row",
            );
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
        on_menu_selected,
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
            let targets = history.state.selection().menu_targets(&menu);
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
                    let _ = cmd.send(load_cmd(is_active, id));
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
fn wire_grid(ctx: SessionCtx<'_>, settings: &Arc<Mutex<AppSettings>>) {
    let SessionCtx {
        ui,
        history,
        session_rows,
        ..
    } = ctx;
    {
        // Every session interaction ends in one of two places: a state change
        // that only needs a re-render, or a command for the worker.

        session_handler!(
            ctx,
            on_pressed,
            |ui, history, cmd, x, y, cell_width, cell, columns, shift, ctrl| {
                let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
                let hit = hit_row(&rendered, &rects, x, y);
                history.state.press(hit.as_deref(), shift, ctrl);
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
            ui.global::<SessionsVm>().on_marquee(
                move |x1, y1, x2, y2, cell_width, cell, columns| {
                    let Some(ui) = weak.upgrade() else { return };
                    let mut history = history_rc.borrow_mut();
                    if !history.dragging {
                        return;
                    }
                    let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
                    let rows: Vec<(String, sessions::Rect)> =
                        rendered.into_iter().zip(rects).collect();
                    history
                        .state
                        .marquee(&rows, sessions::Rect::from_corners(x1, y1, x2, y2));
                    apply_selection(&ui, &history, &rows_model);
                },
            );
        }

        session_handler!(ctx, on_marquee_ended, |ui, history, cmd| {
            history.dragging = false;
        });

        session_handler!(ctx, on_search_changed, |ui, history, cmd, query| {
            history.state.set_search(&query);
            // A filter that moves rows under an open editor or menu would leave
            // both pointing at whatever slid into that slot.
            history.editing = None;
            history.menu = None;
        });

        let view_settings = settings.clone();
        session_handler!(ctx, on_set_view, |ui, history, cmd, thumbnail| {
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
            on_row_activated,
            |ui, history, cmd, x, y, cell_width, cell, columns| {
                let (rendered, rects) = session_row_rects(&history, cell_width, cell, columns);
                let Some(id) = hit_row(&rendered, &rects, x, y) else {
                    return;
                };
                // A LIVE row names itself (task2030): this is the only way to
                // move the stage from one running capture to another.
                let _ = cmd.send(load_cmd(history.state.is_active(&id), id));
            }
        );

        session_handler!(
            ctx,
            on_row_menu_requested,
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

        wire_row_editor(ctx);
    }
}

/// The row menu's own entries and the inline editor they open.
fn wire_row_editor(ctx: SessionCtx<'_>) {
    let SessionCtx {
        ui,
        history,
        session_rows,
        cmd_tx,
        ..
    } = ctx;
    {
        session_handler!(
            ctx,
            window.on_sessions_menu_dismissed,
            |ui, history, cmd| {
                history.menu = None;
            }
        );

        // F2. Only meaningful on a single selection: the menu that advertises
        // the key is itself single-row, and one note for several rows has no
        // meaning. And only in the list view: a tile draws no note editor.
        // It opened the title editor until titles stopped being editable
        // (the user, 2026-09-19).
        session_handler!(ctx, on_note_edit_selected, |ui, history, cmd| {
            let view = history.state.view_mode();
            if let (true, [id]) = (sessions::f2_edits_note_in(view), history.state.selected()) {
                let id = id.clone();
                history.menu = None;
                history.editing = Some(id);
            }
        });

        // Ctrl+A, mirroring the clip screen's `ClipVm` `on_select_all`
        // (follow-up of t260916-5568, the user's 2026-09-19 「この場で実装」).
        // `select_all` takes the rendered rows only and never a live one, so
        // under a search or a collapsed group it arms nothing the user
        // cannot see (task115, task2030).
        session_handler!(ctx, on_select_all, |ui, history, cmd| {
            history.state.select_all();
        });

        session_handler!(ctx, on_note_edit_requested, |ui, history, cmd, row| {
            history.menu = None;
            history.editing = history.row_id(row);
        });

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
        // asks for arrive as `HistoryMsg::SessionThumbnail` and reach the screen on
        // the next ordinary render.
        //
        // It also grows the grid's cell pool (t260928-e029): one model, set
        // here once and pushed to when the window needs more cells than it
        // has. A push adds cells without touching the ones already built, so
        // no tile re-runs its entrance. Never shrunk and never replaced -- a
        // new model would rebuild every cell.
        // The held placeholder faces' timer (t260930-cc68): started here once,
        // with the render it runs, and stopped at once -- `queue_visible_thumbnails`
        // and `render_sessions` only re-arm it. Weak, because `History` owns it.
        {
            let weak = ui.as_weak();
            let history_weak = Rc::downgrade(history);
            let rows = session_rows.clone();
            let owner = history.borrow();
            let timer = &owner.face_timer;
            timer.start(
                slint::TimerMode::SingleShot,
                render::THUMBNAIL_FACE_DELAY,
                move || {
                    let (Some(ui), Some(history)) = (weak.upgrade(), history_weak.upgrade()) else {
                        return;
                    };
                    render_sessions(&ui, &history.borrow(), &rows);
                },
            );
            timer.stop();
        }

        {
            let history_rc = history.clone();
            let cmd = cmd_tx.clone();
            let slots = Rc::new(VecModel::<i32>::default());
            ui.global::<SessionsVm>().set_slots(slots.clone().into());
            ui.global::<SessionsVm>()
                .on_visible_range_changed(move |first, count| {
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

#[cfg(test)]
mod tests {
    use super::*;
    use livia::ring_buffer::SessionSummary;

    const GB: u64 = 1024 * 1024 * 1024;

    fn session(id: &str, bytes: u64, protected: bool) -> SessionSummary {
        SessionSummary {
            session_id: id.into(),
            start_100ns: None,
            end_100ns: None,
            segment_count: 1,
            total_bytes: bytes,
            closed: id != "capture-live",
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

    /// t260930-cc68. Pending is "fetch still out and there is a picture to
    /// fetch": a row not back yet is, a row back without a picture is not, and
    /// a row whose manifest names no thumbnail never is -- it is never asked
    /// for, so a hold on it would be a gap glyph lost for good.
    #[test]
    fn a_row_is_pending_only_while_a_picture_it_has_is_being_fetched() {
        let mut history = History::default();
        let mut with = session("capture-a", 1, false);
        with.thumbnail_index = Some(3);
        let without = session("capture-b", 1, false);
        apply_session(
            &mut history,
            HistoryMsg::Sessions(Ok(vec![with.clone(), without.clone()])),
        );
        let pending = |history: &History, row: &SessionSummary| {
            render::thumbnail_pending(history.thumbnails.get(&row.session_id), row.thumbnail_index)
        };
        assert!(pending(&history, &with), "not fetched yet");
        assert!(!pending(&history, &without), "thumbnail_index: None");
        // Back from the worker with no picture: `Some(None)`.
        apply_session(
            &mut history,
            HistoryMsg::SessionThumbnail("capture-a".into(), None),
        );
        assert!(!pending(&history, &with), "fetched, no picture");
    }

    /// t260930-cc68. The glyph is held from the request for 150ms and no
    /// longer; a row not pending is never held; a pending row not asked for
    /// yet is held (the request arms the timer that lets it through).
    #[test]
    fn the_placeholder_face_is_held_for_150ms_from_the_request() {
        use std::time::Duration;
        let asked = Instant::now();
        let at = |ms: u64| asked + Duration::from_millis(ms);
        assert!(thumbnail_face_held(true, Some(asked), at(0)));
        assert!(thumbnail_face_held(true, Some(asked), at(149)));
        assert!(!thumbnail_face_held(true, Some(asked), at(150)));
        assert!(!thumbnail_face_held(true, Some(asked), at(5000)));
        assert!(thumbnail_face_held(true, None, at(5000)));
        assert!(!thumbnail_face_held(false, Some(asked), at(0)));
        assert!(!thumbnail_face_held(false, None, at(0)));
        // The timer is set to the first held face's remaining time; a row not
        // pending, not asked for, or already due does not set it.
        let rows = [
            (true, Some(at(40))),
            (true, Some(asked)),
            (false, Some(at(100))),
            (true, None),
        ];
        assert_eq!(
            next_face_due(rows.into_iter(), at(100)),
            Some(Duration::from_millis(50))
        );
        assert_eq!(next_face_due(rows.into_iter(), at(190)), None);
    }

    #[test]
    fn the_storage_figures_follow_a_relisted_recording() {
        let mut history = History::default();
        let listed = |live: u64| {
            HistoryMsg::Sessions(Ok(vec![
                session("capture-live", live, false),
                session("capture-kept", GB, true),
            ]))
        };
        apply_session(&mut history, listed(10 << 20));
        history.state.set_active_ids(vec!["capture-live".into()]);
        assert_eq!(history.state.storage_bytes(), (GB + (10 << 20), GB));
        apply_session(&mut history, listed(500 << 20));
        assert_eq!(history.state.storage_bytes(), (GB + (500 << 20), GB));
    }
}
