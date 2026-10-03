//! Renders the session page's rows and every derived label into the UI.
//! Split from `history.rs`, which keeps the state and callback wiring.

use std::time::{Duration, Instant};

use livia::ring_buffer::SessionSummary;
use livia::ui_state::sessions::{self, SessionsState, ViewMode};
use slint::{ComponentHandle, Image, Model, VecModel};

use crossbeam_channel::Sender;

use super::super::tr_locale;
use super::super::{AppWindow, Cmd, Icons, MenuEntry, SessionLabels, SessionRow, SessionsVm};
use super::{session_date_text, History, APP_ICON_KEY};

/// Bytes to the StorageMeter's GB: the same 1024^3 the retention sweep turns
/// `retentionCapacityGb` into bytes with.
const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The glyph each context-menu entry carries (round5 §6-D, task184). The path
/// data lives in `ui/icons.slint`, so it is read back off the global rather
/// than duplicated here -- the menu model is built in Rust, which is the only
/// reason this mapping is on this side at all.
fn menu_icon(ui: &AppWindow, action: sessions::SessionMenuAction) -> slint::SharedString {
    let icons = ui.global::<Icons>();
    match action {
        sessions::SessionMenuAction::Load => icons.get_play(),
        sessions::SessionMenuAction::EditNote => icons.get_pencil(),
        sessions::SessionMenuAction::ToggleProtect => icons.get_lock(),
        sessions::SessionMenuAction::OpenDirectory => icons.get_folder(),
        sessions::SessionMenuAction::Discard => icons.get_discard(),
    }
}

/// The executable image an app icon is read from: the path the manifest
/// recorded at capture start. Recordings from before 2026-09-20 have none and
/// take the monogram.
fn app_icon_path(session: &SessionSummary) -> Option<&str> {
    session
        .target_executable
        .as_deref()
        .and(session.target_executable_path.as_deref())
        .filter(|path| !path.trim().is_empty())
}

/// How long a tile whose picture is still being fetched stays bare stage
/// before the placeholder glyph comes up (t260930-cc68). t260930-40e7's
/// picker threshold, measured there between the fast group of loads (+88 to
/// +111ms) and the slow one (+287ms on): a picture inside it replaces bare
/// stage and the glyph never flashes. A literal rather than a
/// `Tokens.duration-*`: waiting is not motion, so reduce-motion leaves it be.
pub(crate) const THUMBNAIL_FACE_DELAY: Duration = Duration::from_millis(150);

/// Whether this row's picture is still on its way: not back from the worker,
/// and there is one to fetch. A row whose manifest names no thumbnail
/// (`thumbnail_index: None`) is never fetched, so it is not pending -- it
/// draws the gap glyph at once, as does a fetch that came back without a
/// picture (`Some(None)`).
pub(super) fn thumbnail_pending(thumbnail: Option<&Option<Image>>, index: Option<u64>) -> bool {
    thumbnail.is_none() && index.is_some()
}

/// Whether a tile holds its placeholder glyph back and shows bare stage
/// (t260930-cc68, history and clips alike). Only while the picture is pending,
/// and only until `THUMBNAIL_FACE_DELAY` after it was asked for -- counted from
/// the request, not from the tile's mount: the grid's rows are all mounted
/// with the list, long before a scroll brings one into view. A pending row
/// not asked for yet is held too; asking arms the timer that re-renders it.
pub(crate) fn thumbnail_face_held(
    pending: bool,
    requested_at: Option<Instant>,
    now: Instant,
) -> bool {
    pending
        && !matches!(requested_at, Some(at) if now.saturating_duration_since(at) >= THUMBNAIL_FACE_DELAY)
}

/// How long until the first held face among `rows` is due, if any is: what
/// the face timer is set to after a render. `rows` yields `(pending,
/// requested_at)`; a row not requested yet is left to the request's own arming.
pub(crate) fn next_face_due(
    rows: impl Iterator<Item = (bool, Option<Instant>)>,
    now: Instant,
) -> Option<Duration> {
    rows.filter(|(pending, _)| *pending)
        .filter_map(|(_, at)| at)
        .map(|at| THUMBNAIL_FACE_DELAY.saturating_sub(now.saturating_duration_since(at)))
        .filter(|wait| !wait.is_zero())
        .min()
}

/// Sets the face timer (started once, callback and all, where its screen is
/// wired) to fire `wait` from now.
pub(crate) fn arm_face_timer(timer: &slint::Timer, wait: Duration) {
    timer.set_interval(wait);
    timer.restart();
}

/// Asks the worker for the thumbnails the viewport is about to need, and only
/// those (task188), plus each visible row's app icon the first time its
/// executable is seen (t260927-9a17) -- read on the worker, never here.
///
/// ponytail: a range scrolled past is still decoded once requested; the count
/// is bounded by the list, so there is no cancellation. Add one if a long
/// flick through 400 rows ever feels like it is still catching up.
pub(crate) fn queue_visible_thumbnails(history: &mut History, cmd_tx: &Sender<Cmd>) {
    livia::insight_scope!("ui_queue_thumbnails");
    let (first, count) = history
        .visible_range
        .unwrap_or((0, super::ASSUMED_VISIBLE_TILES));
    let total = history.state.rendered().len();
    let range = sessions::visible_thumbnail_range(first, count, total);
    let (wanted, icons): (Vec<(String, u64)>, Vec<String>) = {
        let rendered = history.state.rendered();
        let visible = &rendered[range.clone()];
        let wanted = visible
            .iter()
            .filter(|session| {
                let id = session.session_id.as_str();
                !history.thumbnails.contains_key(id) && !history.requested.contains_key(id)
            })
            .filter_map(|session| {
                session
                    .thumbnail_index
                    .map(|index| (session.session_id.clone(), index))
            })
            .collect();
        let mut icons: Vec<String> = visible
            .iter()
            .filter_map(|session| app_icon_path(session))
            .filter(|path| {
                !history.app_icons.contains_key(*path)
                    && !history
                        .requested
                        .contains_key(&format!("{APP_ICON_KEY}{path}"))
            })
            .map(str::to_owned)
            .collect();
        icons.sort();
        icons.dedup();
        (wanted, icons)
    };
    // Only when something is actually asked for (task192): this runs on every
    // redraw and every viewport change, so logging the empty case would put
    // hundreds of lines behind one flick of the scroll wheel.
    if !wanted.is_empty() {
        tracing::info!(
            event = "task192_thumbnail_request",
            visible_first = first,
            visible_count = count,
            range_start = range.start,
            range_end = range.end,
            requested = wanted.len(),
            total,
            "queueing thumbnails for the visible range"
        );
    }
    let now = Instant::now();
    // Once per batch, and not while already set: re-arming on every request
    // would push a row asked for at the start of a flick past its 150ms
    // until the flick ended. A later render sets it to the exact next due.
    if !wanted.is_empty() && !history.face_timer.running() {
        arm_face_timer(&history.face_timer, THUMBNAIL_FACE_DELAY);
    }
    for (id, index) in wanted {
        history.requested.insert(id.clone(), now);
        let _ = cmd_tx.send(Cmd::Thumbnail(id, index));
    }
    for path in icons {
        let key = format!("{APP_ICON_KEY}{path}");
        history.requested.insert(key.clone(), now);
        let _ = cmd_tx.send(Cmd::Thumbnail(key, 0));
    }
}

/// The footer's selection bar. Split out of `render_sessions` so a marquee drag
/// can keep it live without rebuilding the rows (task189).
fn set_selection_labels(ui: &AppWindow, state: &sessions::SessionsState) {
    let vm = ui.global::<SessionsVm>();
    let selected = state.selected();
    let locale = tr_locale();
    vm.set_selected(selected.len() as i32);
    let (count, rest) = sessions::selection_text(locale, selected.len(), state.selected_bytes());
    vm.set_selection_count(count.into());
    vm.set_selection_text(rest.into());
    // Protect everything unless everything already is (DS HistoryScreen).
    let all_protected = !selected.is_empty() && selected.iter().all(|id| state.protected(id));
    vm.set_protect_label(sessions::protect_menu_label(locale, all_protected).into());
    vm.set_discard_label(sessions::discard_selection_label(locale, selected.len()).into());
    let protected = selected.iter().filter(|id| state.protected(id)).count();
    vm.set_protected_text(
        sessions::protected_in_selection_text(locale, protected)
            .unwrap_or_default()
            .into(),
    );
    vm.set_has_live(selected.iter().any(|id| state.is_active(id)));
}

/// Writes back only the rows whose selection actually changed, plus the labels
/// that follow the selection (task189). A marquee drag moves `selected` and
/// nothing else, so a full `render_sessions` per mouse move would reproduce
/// text it already had. `set_row_data` notifies per row, so the rows that did
/// not move are not even re-evaluated.
///
/// Index and id line up because both sides walk `state.rendered()` in order,
/// the same assumption `session_row_rects` makes.
pub(crate) fn apply_selection(ui: &AppWindow, history: &History, model: &VecModel<SessionRow>) {
    let state = &history.state;
    for (index, session) in state.rendered().iter().enumerate() {
        let selected = state.is_selected(&session.session_id);
        let Some(row) = model.row_data(index) else {
            continue;
        };
        if row.selected != selected {
            model.set_row_data(index, SessionRow { selected, ..row });
        }
    }
    set_selection_labels(ui, state);
}

/// One history row's model entry.
fn session_row(
    history: &History,
    session: &SessionSummary,
    place: livia::ui_state::clips::ClipPlacement,
    now: &str,
    clock: Instant,
) -> SessionRow {
    let state = &history.state;

    let id = session.session_id.as_str();
    // Every running capture is a LIVE row (task2030), not just whichever
    // one started last.
    let is_active = state.is_active(id);
    let thumbnail = history.thumbnails.get(id);
    let app = sessions::app_name(session.target_executable.as_deref());
    let icon = app_icon_path(session).and_then(|path| history.app_icons.get(path));
    let app_name = app
        .clone()
        .unwrap_or_else(|| sessions::whole_screen(tr_locale()).to_owned());
    SessionRow {
        id: id.into(),
        title: session
            .target_title
            .clone()
            // No window name (an unreadable row): the created date in the
            // column's own shape, not the raw stamp (t260928-23aa).
            .unwrap_or_else(|| sessions::tile_date_text(&session_date_text(id), now))
            .into(),
        note: session.note.clone().unwrap_or_default().into(),
        meta_date: sessions::tile_date_text(&session_date_text(id), now).into(),
        meta_duration: history.duration_text(session).into(),
        meta_size: sessions::format_bytes(session.total_bytes).into(),
        recording: is_active,
        selected: state.is_selected(id),
        selectable: state.is_selectable(id),
        // Through the overlay (t260920-bb09): a recording's toggle is staged in
        // memory and reaches the file the listing reads only when the index
        // writer next wakes.
        protected: state.protected(id),
        unreadable: sessions::is_row_inert(session),
        load_disabled: sessions::is_load_disabled(session, state.capturing(), is_active, false),
        thumbnail: thumbnail.cloned().flatten().unwrap_or_default(),
        has_thumbnail: matches!(thumbnail, Some(Some(_))),
        thumbnail_held: thumbnail_face_held(
            thumbnail_pending(thumbnail, session.thumbnail_index),
            history.requested.get(id).copied(),
            clock,
        ),
        app_letter: sessions::monogram_letter(&app_name).into(),
        app_hue: sessions::monogram_hue(&app_name),
        app_icon: icon.cloned().flatten().unwrap_or_default(),
        has_app_icon: matches!(icon, Some(Some(_))),
        // No executable: a monitor recording, or an unreadable row whose
        // manifest cannot say -- both take the display glyph rather than a
        // monogram lettered from 「画面全体」 (t260928-23aa, DS SessionRow).
        whole_screen: app.is_none(),
        marker_count: session.marker_count as i32,
        column: place.column,
        line: place.line,
        heading_offset: place.heading_offset,
        heading: place.heading.into(),
    }
}

pub(crate) fn render_sessions(ui: &AppWindow, history: &History, model: &VecModel<SessionRow>) {
    // Rebuilds every history row's model entry; grows with the session list,
    // so it is the UI scope most likely to scale badly (task205).
    livia::insight_scope!("ui_render_sessions");
    let state = &history.state;
    let thumbnail_view = state.view_mode() == ViewMode::Thumbnail;
    let rendered = state.rendered();
    let placed = history.placement(history.columns);
    let now = super::now_date_text();
    let clock = Instant::now();
    // The first row on each grid line, for the pane's window.
    let mut line_first: Vec<i32> = Vec::new();
    for (index, place) in placed.iter().enumerate() {
        if line_first.len() as i32 <= place.line {
            line_first.push(index as i32);
        }
    }
    let rows: Vec<SessionRow> = rendered
        .iter()
        .zip(placed)
        .map(|(session, place)| session_row(history, session, place, &now, clock))
        .collect();
    // The held faces' next due time (t260930-cc68): the timer's render flips
    // them to the glyph. Only the rows whose `thumbnail_held` changed are
    // written below, so the tick costs one `set_row_data` per face that comes up.
    let due = next_face_due(
        rendered.iter().map(|session| {
            let id = session.session_id.as_str();
            (
                thumbnail_pending(history.thumbnails.get(id), session.thumbnail_index),
                history.requested.get(id).copied(),
            )
        }),
        clock,
    );
    if let Some(wait) = due {
        arm_face_timer(&history.face_timer, wait);
    }

    let shown = rows.len();
    if model.row_count() == shown {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
    } else {
        model.set_vec(rows);
    }
    // Replaced only when it changed (t260928-e029): the pane's window reads
    // it, and a fresh model on every thumbnail's arrival re-ran that window.
    let vm = ui.global::<SessionsVm>();
    let current = vm.get_line_first();
    if !current.iter().eq(line_first.iter().copied()) {
        vm.set_line_first(std::rc::Rc::new(VecModel::from(line_first)).into());
    }

    render_session_chrome(ui, history, thumbnail_view, shown);
}

/// The page's fixed words (t260927-9a17). Pushed with every render: cheap, and
/// it follows a language switch without a hook of its own.
fn set_static_labels(ui: &AppWindow) {
    let labels = ui.global::<SessionLabels>();
    let locale = tr_locale();
    labels.set_column_window(sessions::column_window(locale).into());
    labels.set_column_start(sessions::column_start(locale).into());
    labels.set_column_length(sessions::column_length(locale).into());
    labels.set_column_size(sessions::column_size(locale).into());
    labels.set_whole_screen(sessions::whole_screen(locale).into());
    labels.set_stop_recording(sessions::stop_recording(locale).into());
    labels.set_clear_selection(sessions::clear_selection(locale).into());
    labels.set_more_actions(sessions::more_actions(locale).into());
    labels.set_hold_to_discard(sessions::hold_to_discard(locale).into());
    labels.set_live_cannot_discard(sessions::live_cannot_discard(locale).into());
    labels.set_storage_label(sessions::storage_label(locale).into());
    labels.set_storage_protected(sessions::storage_protected(locale).into());
}

/// The footer's StorageMeter (決めたこと 10): used is every session, protected
/// the protected ones, the limit the capacity setting (0 = none).
fn set_storage(ui: &AppWindow, history: &History) {
    let vm = ui.global::<SessionsVm>();
    let (used, protected) = storage_bytes(&history.state);
    let limit_gb = history
        .settings
        .as_ref()
        .and_then(|settings| {
            settings
                .lock()
                .ok()
                .map(|current| current.retention_capacity_gb)
        })
        .unwrap_or(0)
        .max(0);
    vm.set_storage_used((used as f64 / GB) as f32);
    vm.set_storage_protected((protected as f64 / GB) as f32);
    vm.set_storage_limit(limit_gb as f32);
    // Protected sessions alone over the limit: said beside the meter, in
    // warning, for as long as it is true (t260928-e309 F10, DS 知らせ方 「続け
    // ると困ること → その場所の近くに warning の 1 行」). It used to be an
    // error toast raised on every start.
    let over = livia::ring_buffer::protected_over_limit(
        history.state.sessions(),
        limit_gb as u64 * 1024 * 1024 * 1024,
    );
    vm.set_storage_note(
        sessions::protected_over_limit_notice(tr_locale(), over)
            .unwrap_or_default()
            .into(),
    );
    vm.set_unlimited_text(sessions::storage_unlimited_text(tr_locale(), used, protected).into());
}

/// What the StorageMeter reads, in bytes: every session, then the protected
/// ones. Split out of `set_storage` so a test can reach it without a window.
pub(super) fn storage_bytes(state: &SessionsState) -> (u64, u64) {
    let protected = state
        .sessions()
        .iter()
        .filter(|session| state.protected(&session.session_id))
        .map(|session| session.total_bytes)
        .sum();
    (state.total_bytes(), protected)
}

/// The list's chrome: the view switch, the tallies, the footer and the error
/// line.
fn render_session_chrome(ui: &AppWindow, history: &History, thumbnail_view: bool, shown: usize) {
    let state = &history.state;
    let rendered = state.rendered();
    set_static_labels(ui);
    ui.set_sessions_thumbnail_view(thumbnail_view);
    set_storage(ui, history);
    // Three different nothings: a listing that would not read, no recordings
    // at all, or none that match what was typed. The last has to say so, or
    // the search box looks broken -- and only "none at all" takes the top row
    // down with it (task3800). A failed listing wins over the other two: 「履歴は
    // まだありません」 under it would claim what nobody could read.
    let no_sessions = state.sessions().is_empty();
    ui.set_sessions_no_sessions(no_sessions);
    ui.global::<SessionsVm>()
        .set_list_failed(history.list_error.is_some());
    ui.set_sessions_empty_text(
        if let Some(error) = history.list_error {
            error.to_owned()
        } else if no_sessions {
            sessions::empty(tr_locale()).to_owned()
        } else if shown == 0 {
            sessions::search_empty_text(tr_locale(), state.search())
        } else {
            String::new()
        }
        .into(),
    );
    ui.set_sessions_empty_sub_text(
        if history.list_error.is_some() {
            sessions::error_sub(tr_locale())
        } else if no_sessions {
            sessions::empty_sub(tr_locale())
        } else {
            ""
        }
        .into(),
    );
    set_selection_labels(ui, state);
    ui.set_sessions_editing_row(
        history
            .editing
            .as_ref()
            .and_then(|id| rendered.iter().position(|s| &s.session_id == id))
            .map(|index| index as i32)
            .unwrap_or(-1),
    );
    ui.set_sessions_editing_note(history.editing.is_some());

    // ---------- context menu (round5 §6-D) ----------
    let menu_row = history.menu.as_ref().and_then(|id| state.find(id));
    let entries: Vec<MenuEntry> = match menu_row {
        None => Vec::new(),
        Some(session) => {
            let is_active = state.is_active(&session.session_id);
            let targets = state.menu_targets(&session.session_id);
            // No menu on a row nothing could read (task1520 follow-up).
            let items = if !sessions::has_row_menu(session) {
                Vec::new()
            } else if targets.len() > 1 {
                sessions::selection_menu(
                    tr_locale(),
                    targets.iter().all(|id| state.protected(id)),
                    targets.iter().any(|id| state.is_active(id)),
                )
            } else {
                sessions::session_menu(
                    tr_locale(),
                    state.view_mode(),
                    // The overlay's value (t260920-bb09).
                    state.protected(&session.session_id),
                    sessions::is_load_disabled(session, state.capturing(), is_active, false),
                    is_active,
                )
            };
            items
                .into_iter()
                .map(|item| MenuEntry {
                    label: item.label.into(),
                    icon: menu_icon(ui, item.action),
                    hint: item.hint.into(),
                    separator_above: item.rule_above,
                    danger: item.danger,
                    // Listed and refused, so the reason is visible rather than
                    // merely absent (t260920-bb09).
                    disabled: item.disabled,
                })
                .collect()
        }
    };
    ui.set_sessions_menu_open(!entries.is_empty());
    // Only when the entries changed (t260929-7037): a new model rebuilds the
    // menu's rows, and with them the `HoldGesture` of the 破棄 being held --
    // this runs on every list refresh while another session records.
    if super::super::review::model_differs(ui.get_sessions_menu_entries(), &entries) {
        ui.set_sessions_menu_entries(std::rc::Rc::new(VecModel::from(entries)).into());
    }

    let loaded_session = history.loaded.as_deref().and_then(|id| state.find(id));
    let loaded = loaded_session.map(|session| {
        session
            .target_title
            .clone()
            .unwrap_or_else(|| session_date_text(&session.session_id))
    });
    ui.set_loaded_title(loaded.unwrap_or_default().into());
    // The title bar mark's other two rungs, the same as this session's
    // history row (`session_row`): no executable is a whole screen, else the
    // monogram of the app's name (t260928-b780 F3). The icon rung is
    // `review.rs`'s `loaded-icon`.
    let app =
        loaded_session.and_then(|session| sessions::app_name(session.target_executable.as_deref()));
    ui.set_loaded_whole_screen(loaded_session.is_some() && app.is_none());
    ui.set_loaded_app_letter(
        app.as_deref()
            .map(sessions::monogram_letter)
            .unwrap_or_default()
            .into(),
    );
    ui.set_loaded_app_hue(app.as_deref().map_or(0, sessions::monogram_hue));
}
