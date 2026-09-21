//! Renders the session page's rows and every derived label into the UI.
//! Split from `history.rs`, which keeps the state and callback wiring.

use livia::ui_state::sessions::{self, ViewMode};
use slint::{ComponentHandle, Model, VecModel};

use crossbeam_channel::Sender;

use super::super::tr_locale;
use super::super::{AppWindow, Cmd, Icons, MenuEntry, SessionLabels, SessionRow};
use super::{session_date_text, History};

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

/// Asks the worker for the thumbnails the viewport is about to need, and only
/// those (task188). `render_sessions` used to do this while it built the rows,
/// which meant every render queued a decode for all 400 sessions -- and the
/// worker answers `ListSessions` / `Discard` / `Load` on the same channel, so
/// the queue was in front of everything the user actually asked for.
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
    let wanted: Vec<(String, u64)> = {
        let rendered = history.state.rendered();
        rendered[range.clone()]
            .iter()
            .filter(|session| {
                let id = session.session_id.as_str();
                !history.thumbnails.contains_key(id) && !history.requested.contains(id)
            })
            .filter_map(|session| {
                session
                    .thumbnail_index
                    .map(|index| (session.session_id.clone(), index))
            })
            .collect()
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
    for (id, index) in wanted {
        history.requested.insert(id.clone());
        let _ = cmd_tx.send(Cmd::Thumbnail(id, index));
    }
}

/// What the footer says about the selection. Split out of `render_sessions` so
/// a marquee drag can keep it live without rebuilding the rows (task189).
fn set_selection_labels(ui: &AppWindow, state: &sessions::SessionsState) {
    ui.set_sessions_footer_selected_text(
        sessions::footer_selected_text(tr_locale(), state.selected().len(), state.selected_bytes())
            .unwrap_or_default()
            .into(),
    );
}

/// Writes back only the rows whose selection actually changed, plus the labels
/// that follow the selection (task189). A marquee drag moves `selected` and
/// nothing else -- `title` / `meta` / `date` and friends are all fixed for the
/// length of the drag -- so a full `render_sessions` per mouse move spent 400
/// Win32 time conversions and several hundred `format!`s to reproduce text it
/// already had. `set_row_data` notifies per row, so the rows that did not move
/// are not even re-evaluated.
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
    session: &livia::ring_buffer::SessionSummary,
    now: &str,
) -> SessionRow {
    let state = &history.state;

    let id = session.session_id.as_str();
    // Every running capture is a LIVE row (task2030), not just whichever
    // one started last.
    let is_active = state.is_active(id);
    let thumbnail = history.thumbnails.get(id);
    SessionRow {
        id: id.into(),
        title: session
            .target_title
            .clone()
            .unwrap_or_else(|| session_date_text(id))
            .into(),
        note: session.note.clone().unwrap_or_default().into(),
        meta_date: sessions::row_date_text(tr_locale(), &session_date_text(id), now).into(),
        meta_duration: history.duration_text(session).into(),
        meta_size: sessions::format_bytes(session.total_bytes).into(),
        date: sessions::tile_date_text(&session_date_text(id)).into(),
        recording: is_active,
        selected: state.is_selected(id),
        selectable: state.is_selectable(id),
        // Through the overlay (t260920-bb09): a recording's toggle is staged in
        // memory and reaches the file the listing reads only when the index
        // writer next wakes, so the summary is stale by up to one 500 ms poll
        // -- and nothing relists again until the user acts.
        protected: state.protected(id),
        unreadable: sessions::is_row_inert(session),
        load_disabled: sessions::is_load_disabled(session, state.capturing(), is_active, false),
        thumbnail: thumbnail.cloned().flatten().unwrap_or_default(),
        has_thumbnail: matches!(thumbnail, Some(Some(_))),
    }
}

pub(crate) fn render_sessions(ui: &AppWindow, history: &History, model: &VecModel<SessionRow>) {
    // Rebuilds every history row's model entry; grows with the session list,
    // so it is the UI scope most likely to scale badly (task205).
    livia::insight_scope!("ui_render_sessions");
    let state = &history.state;
    let thumbnail_view = state.view_mode() == ViewMode::Thumbnail;
    let rendered = state.rendered();
    // Read once per render, not once per row: every row asks the same question
    // of it, and a clock that moved mid-list could label two rows differently.
    let now = super::now_date_text();
    let rows: Vec<SessionRow> = rendered
        .iter()
        .map(|session| session_row(history, session, &now))
        .collect();

    let shown = rows.len();
    // The filter's own subtotal, not the whole buffer: the footer reports what
    // is on screen (task242).
    let visible_bytes: u64 = state
        .rendered()
        .iter()
        .map(|session| session.total_bytes)
        .sum();
    if model.row_count() == shown {
        for (index, row) in rows.into_iter().enumerate() {
            if model.row_data(index).as_ref() != Some(&row) {
                model.set_row_data(index, row);
            }
        }
    } else {
        model.set_vec(rows);
    }

    render_session_chrome(ui, history, thumbnail_view, shown, visible_bytes);
}

/// The list's chrome: the view toggle, the footer tally and the error line.
fn render_session_chrome(
    ui: &AppWindow,
    history: &History,
    thumbnail_view: bool,
    shown: usize,
    visible_bytes: u64,
) {
    let state = &history.state;
    let rendered = state.rendered();
    ui.set_sessions_thumbnail_view(thumbnail_view);
    // The footer's own half of the tally (task242): what is on screen after the
    // filter, and what it costs.
    ui.set_sessions_footer_visible_text(
        sessions::footer_visible_text(tr_locale(), shown, visible_bytes).into(),
    );
    ui.set_sessions_error_text(
        history
            .list_error
            .map(str::to_owned)
            .or_else(|| {
                history
                    .action
                    .as_ref()
                    .filter(|(_, failed)| *failed)
                    .map(|(text, _)| text.clone())
            })
            .unwrap_or_default()
            .into(),
    );
    // Two different nothings: no recordings at all, or none that match what was
    // typed. The second one has to say so, or the search box looks broken.
    //
    // task3800 gave the first one a second caption line and takes the toolbar
    // down with it -- so the two nothings are told apart here as well, and by
    // "are there sessions" rather than by "is there empty text". A toolbar
    // hidden on empty text would vanish the moment a query matched nothing,
    // taking the field holding that query with it.
    let no_sessions = state.sessions().is_empty();
    ui.set_sessions_no_sessions(no_sessions);
    ui.set_sessions_empty_text(
        if !no_sessions && shown == 0 {
            sessions::search_empty(tr_locale())
        } else if no_sessions {
            sessions::empty(tr_locale())
        } else {
            ""
        }
        .into(),
    );
    ui.set_sessions_empty_sub_text(
        if no_sessions {
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
            // `set_sessions_menu_open` below is driven by this vec being empty,
            // so returning nothing here is what keeps the menu shut.
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
                    // The overlay's value, so the label says what the row says
                    // while a recording's toggle is still queued (t260920-bb09).
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
                    // merely absent (t260920-bb09). `history.rs` already turned
                    // the press away with this flag; it just never reached the
                    // drawing, which is why 破棄 over a recording -- and
                    // `load_disabled` before it -- looked live.
                    disabled: item.disabled,
                })
                .collect()
        }
    };
    ui.set_sessions_menu_open(!entries.is_empty());
    ui.set_sessions_menu_entries(std::rc::Rc::new(VecModel::from(entries)).into());

    // ---------- discard confirmation ----------
    // The names are gone (round5 §6-B): the selection was the gesture just
    // performed, and listing it back adds reading, not certainty.
    let targets: Vec<_> = history
        .confirm
        .iter()
        .filter_map(|id| state.find(id))
        .collect();
    let protected = targets.iter().filter(|session| session.protected).count();
    let (title, lead) = if targets.is_empty() {
        (String::new(), String::new())
    } else if targets.len() == 1 {
        (
            sessions::discard_confirm_title(tr_locale()).to_owned(),
            sessions::discard_lead(tr_locale()).to_owned(),
        )
    } else {
        let bytes: u64 = targets.iter().map(|session| session.total_bytes).sum();
        (
            sessions::bulk_discard_confirm_title(tr_locale(), targets.len()),
            sessions::bulk_discard_lead(tr_locale(), &sessions::format_bytes(bytes)),
        )
    };
    ui.set_sessions_confirm_title(title.into());
    ui.set_sessions_confirm_lead(lead.into());
    ui.set_sessions_confirm_protected(
        sessions::protected_in_selection_text(tr_locale(), protected)
            .unwrap_or_default()
            .into(),
    );
    ui.global::<SessionLabels>().set_confirm_ok(
        if targets.len() > 1 {
            sessions::bulk_discard_confirm(tr_locale(), targets.len())
        } else {
            sessions::discard_confirm(tr_locale()).to_owned()
        }
        .into(),
    );

    let loaded = history
        .loaded
        .as_deref()
        .and_then(|id| state.find(id))
        .map(|session| {
            session
                .target_title
                .clone()
                .unwrap_or_else(|| session_date_text(&session.session_id))
        });
    ui.set_loaded_title(loaded.unwrap_or_default().into());
}
