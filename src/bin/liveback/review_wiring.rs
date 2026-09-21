//! The review timeline's callback wiring (task127): track scrub/zoom/pan,
//! range handles, markers, transport, volume, and the keyboard map. Moved
//! verbatim out of `main`; the state and renderers live in `review`.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crossbeam_channel::Sender;
use livia::capture::CaptureController;
use livia::playback::PlaybackCommand;
use livia::ring_buffer::MarkerRecord;
use livia::settings::AppSettings;
use livia::ui_state::auto_capture;
use livia::ui_state::export as ex;
use livia::ui_state::lifecycle;
use livia::ui_state::playback as pb;
use livia::ui_state::timeline::{self as tl, Viewport};
use slint::{ComponentHandle, ModelRc, VecModel};

use super::review::{
    hide_feedback_after, init_review_labels, render_review, render_stage_feedback,
    set_track_volume, Review,
};
use super::settings_page::{commit_settings, push_auto_capture_list, settings_snapshot};
use super::tr_locale;
use super::{AppWindow, Cmd, GapSpan, MarkerRow, MarkerSpan, ReviewVm, TrackSpan};

/// Borderless full screen (task152). Four routes reach this -- the `F` key, the
/// overlay button, Escape, and a stage double-click -- so the window call and
/// the VM flag that drives the chrome guards are paired in one place. The
/// `.slint` `Window.full-screen` property is deliberately not used: the Rust
/// API is the one the winit backend answers to.
pub(super) fn set_fullscreen(ui: &AppWindow, on: bool, settings: &Mutex<AppSettings>) {
    // Task1580: the one funnel all four routes pass through, so this is where a
    // log can say when the stage stopped being downscaled.
    tracing::info!(target: "task1580_stage", fullscreen = on, "fullscreen toggled");
    ui.window().set_fullscreen(on);
    ui.global::<ReviewVm>().set_fullscreen(on);
    // round7 section 2-8: the right panel folds away in full screen, and comes
    // back to whatever the stored preference says on the way out. The same
    // derivation runs in `render_review`, but nothing necessarily renders on a
    // full-screen toggle -- which left a panel opened beforehand sitting there
    // with its toggle now disabled, and no way to close it (task1030).
    ui.global::<ReviewVm>()
        .set_panel_open(!on && settings_snapshot(settings).review_panel_open);
}

/// The panel hands the edge across as an int, because a slint callback carries
/// no enum: 0 is 開始, anything else 終了 (round8 §2).
fn range_edge(edge: i32) -> tl::RangeEdge {
    if edge == 0 {
        tl::RangeEdge::Start
    } else {
        tl::RangeEdge::End
    }
}

/// The band drag's `sel.mode`, as `tl::range_follow` wants it: `Some(edge)`
/// for a boundary, `None` for the whole band being panned (task3160). A mode
/// slint never assigned -- the `-1` a release sends when the gesture never
/// moved -- has no follow at all, which is the outer `None`.
#[allow(clippy::option_option)]
fn range_drag_follow_edge(mode: i32) -> Option<Option<tl::RangeEdge>> {
    match mode {
        0 => Some(Some(tl::RangeEdge::Start)),
        1 => Some(Some(tl::RangeEdge::End)),
        2 => Some(None),
        _ => None,
    }
}

/// Moves one boundary and carries the playhead along when the move left it
/// outside (task3160; it used to seek unconditionally, which yanked a playhead
/// that was sitting happily inside the range). A move the timeline refuses
/// (past the other edge, off the recording, into a pruned stretch) leaves the
/// range alone, and the row redraws from the value that never changed.
fn apply_range_edit(
    review: &mut Review,
    snapshot: &tl::TimelineSnapshot,
    edge: tl::RangeEdge,
    to_100ns: i64,
) {
    let Some(range) = review.range else {
        return;
    };
    let Some(moved) = tl::move_range_edge(snapshot, range, edge, to_100ns, tl::min_frame_100ns())
    else {
        return;
    };
    review.range = Some(moved);
    // A hand-moved boundary stops following the live edge, exactly as dragging
    // a handle on the track already did.
    review.quick = -1;
    if let Some(target) = tl::range_follow(moved, review.position, Some(edge)) {
        review.seek_clamped(target, true);
    }
}

/// 「開始（終了）を再生位置に合わせる」 (task3050): the same clamp
/// `apply_range_edit` uses, minus the seek -- the playhead is already standing
/// where the boundary is going, and re-seeking to it would break a playback
/// that is running. Deliberately not gated on the editable-range mode
/// (`range_editing` in task1060, `clip_editing` since task3860): the mode
/// exists to keep a stray drag off the track band, which is not a hazard a
/// button or a key has. That is also what keeps 再生位置に合わせる and `i`/`o`
/// available outside the 確定ステップ, which round19 §1 counts as two of the
/// entrances that make deleting the toggle safe.
fn range_edge_to_playhead(review: &mut Review, edge: tl::RangeEdge) {
    let Some(snapshot) = review.snapshot.clone() else {
        return;
    };
    let Some(range) = review.range else {
        return;
    };
    let Some(moved) = tl::move_range_edge(
        &snapshot,
        range,
        edge,
        review.position,
        tl::min_frame_100ns(),
    ) else {
        return;
    };
    review.range = Some(moved);
    review.quick = -1;
}

/// The track's three cached models, passed as one tuple so `wire` stays under
/// clippy's argument limit.
type TrackModels<'a> = (
    &'a Rc<VecModel<TrackSpan>>,
    &'a Rc<VecModel<GapSpan>>,
    &'a Rc<VecModel<MarkerSpan>>,
    &'a Rc<VecModel<MarkerRow>>,
);

/// Every timeline handler has the same shape: borrow the review state, do the
/// thing, then re-render the pane.
///
/// The context comes in as a macro *parameter* rather than being read from the
/// enclosing scope, which is what lets the handlers live in more than one
/// function: `macro_rules!` resolves a bare `ui` at the definition site, so a
/// macro that read one could never be moved out of the function that had it.
macro_rules! review_handler {
    ($ctx:expr, $setter:ident, |$ui:ident, $review:ident $(, $arg:ident)*| $body:block) => {{
        let ctx = $ctx;
        let weak = ctx.ui.as_weak();
        let review_rc = ctx.review.clone();
        let (seg, gap, mark, rows) = (
            ctx.models.0.clone(),
            ctx.models.1.clone(),
            ctx.models.2.clone(),
            ctx.models.3.clone(),
        );
        let settings = ctx.settings.clone();
        ctx.ui.global::<ReviewVm>().$setter(move |$($arg),*| {
            let Some($ui) = weak.upgrade() else { return Default::default() };
            let mut $review = review_rc.borrow_mut();
            let result = $body;
            let current = settings_snapshot(&settings);
            render_review(&$ui, &mut $review, &seg, &gap, &mark, &rows, &current);
            result
        });
    }};
}

/// task3400: `review_handler!` with the scrub's stopwatch around it.
///
/// The point of the measurement is to compare a range drag against a scrub
/// with **one ruler**, and `on_track_moved` times from handler entry through
/// `render_review`. Timing inside a `review_handler!` body cannot reach that
/// far -- the macro runs `settings_snapshot` and `render_review` *after*
/// `$body` -- so the two moving handlers get this variant instead of being
/// unrolled by hand the way `on_track_moved` is.
///
/// Counts only while `range_dragging`, i.e. inside the press/release bracket
/// `on_range_drag_changed` owns (task3160). That bracket is what resets and
/// reports the `drag_*` fields, exactly as `on_track_pressed` /
/// `on_track_released` do for the scrub -- and the two gestures are mutually
/// exclusive, so they can share the fields.
///
/// A body that bails early (no snapshot, no range, mode off) skips the render
/// and the count, which is the same shape `on_track_moved` has: a move that
/// did nothing is not a move that cost something.
macro_rules! review_drag_handler {
    ($ctx:expr, $setter:ident, |$ui:ident, $review:ident $(, $arg:ident)*| $body:block) => {{
        let ctx = $ctx;
        let weak = ctx.ui.as_weak();
        let review_rc = ctx.review.clone();
        let (seg, gap, mark, rows) = (
            ctx.models.0.clone(),
            ctx.models.1.clone(),
            ctx.models.2.clone(),
            ctx.models.3.clone(),
        );
        let settings = ctx.settings.clone();
        ctx.ui.global::<ReviewVm>().$setter(move |$($arg),*| {
            let started = Instant::now();
            let Some($ui) = weak.upgrade() else { return Default::default() };
            let mut $review = review_rc.borrow_mut();
            let result = $body;
            let current = settings_snapshot(&settings);
            render_review(&$ui, &mut $review, &seg, &gap, &mark, &rows, &current);
            if $review.range_dragging {
                $review.drag_moves += 1;
                $review.drag_handler_nanos += started.elapsed().as_nanos();
            }
            result
        });
    }};
}

/// What every timeline handler needs to rebuild the pane after it runs.
#[derive(Clone, Copy)]
struct HandlerCtx<'a> {
    ui: &'a AppWindow,
    review: &'a Rc<RefCell<Review>>,
    models: TrackModels<'a>,
    settings: &'a Arc<Mutex<AppSettings>>,
}

/// Wires the timeline handlers. Everything below moved verbatim out of
/// `main` (the blocks only borrow what they used to capture from it).
pub(super) fn wire(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    (review_segments, review_gaps, review_markers, review_marker_rows): TrackModels<'_>,
    settings: &Arc<Mutex<AppSettings>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    let ctx = HandlerCtx {
        ui,
        review,
        models: (
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
        ),
        settings,
    };
    {
        init_review_labels(ui);
        let vm = ui.global::<ReviewVm>();
        vm.set_segments(ModelRc::from(review_segments.clone()));
        vm.set_gaps(ModelRc::from(review_gaps.clone()));
        vm.set_markers(ModelRc::from(review_markers.clone()));
        vm.set_marker_rows(ModelRc::from(review_marker_rows.clone()));

        // Scrub: seek on every move, deliberately without React's 150ms
        // throttle -- measuring that this holds is the task's main goal. The
        // per-drag counters are logged at release.
        wire_track_gestures(ctx, cmd_tx, controller, stop_requests);
        wire_transport(ctx);
    }
}

/// The marker list, its add/remove buttons, the export and screenshot doors, and the live capsule's own stop.
#[allow(clippy::too_many_arguments)]
fn wire_markers_and_stop(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    (review_segments, review_gaps, review_markers, review_marker_rows): TrackModels<'_>,
    settings: &Arc<Mutex<AppSettings>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let controller = controller.clone();
        ui.global::<ReviewVm>()
            .on_marker_renamed(move |row, label| {
                let Some(ui) = weak.upgrade() else { return };
                let mut review = review_rc.borrow_mut();
                let Some(time) = review.visible_markers.get(row as usize).copied() else {
                    return;
                };
                let Some(session_id) = review
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.session_id.clone())
                else {
                    return;
                };
                // Mirrors the change locally instead of re-fetching, same as
                // React: a refetch would answer for the recording session.
                match controller.update_marker(&session_id, time, label.to_string()) {
                    Ok(()) => {
                        if let Some(marker) = review
                            .markers
                            .iter_mut()
                            .find(|marker| marker.time_100ns == time)
                        {
                            marker.label = label.to_string();
                        }
                        review.status = None;
                    }
                    // The popover had a line of its own for this; the panel
                    // has none, so it goes where every other review failure
                    // goes -- the notice card.
                    Err(_) => review.status = Some(tl::marker_save_error(tr_locale()).to_owned()),
                }
                let current = settings_snapshot(&settings);
                render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
            });
    }

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let controller = controller.clone();
        ui.global::<ReviewVm>().on_marker_removed(move |row| {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            let Some(time) = review.visible_markers.get(row as usize).copied() else {
                return;
            };
            let Some(session_id) = review
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.session_id.clone())
            else {
                return;
            };
            match controller.delete_marker(&session_id, time) {
                Ok(()) => {
                    review.markers.retain(|marker| marker.time_100ns != time);
                    review.status = None;
                }
                Err(_) => review.status = Some(tl::marker_delete_error(tr_locale()).to_owned()),
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let controller = controller.clone();
        ui.global::<ReviewVm>().on_add_marker(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            let Some(session_id) = review
                .snapshot
                .as_ref()
                .map(|snapshot| snapshot.session_id.clone())
            else {
                return;
            };
            let time = review.position;
            match controller.add_marker(&session_id, time) {
                Ok(()) => {
                    if !review
                        .markers
                        .iter()
                        .any(|marker| marker.time_100ns == time)
                    {
                        review.markers.push(MarkerRecord {
                            time_100ns: time,
                            label: String::new(),
                            // The colour is the manifest's to assign; the
                            // next reload picks up what it chose (task171).
                            color_index: None,
                        });
                        review.markers.sort_by_key(|marker| marker.time_100ns);
                    }
                    review.status = None;
                }
                Err(_) => review.status = Some(tl::marker_add_error(tr_locale()).to_owned()),
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    wire_capsule_stop(ui, review, cmd_tx, controller, stop_requests);
}

/// The per-track mixer (task1300) and the review screen's keyboard.
#[allow(clippy::too_many_arguments)]
fn wire_mixer_and_keys(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    (review_segments, review_gaps, review_markers, review_marker_rows): TrackModels<'_>,
    settings: &Arc<Mutex<AppSettings>>,
) {
    // ---------- the track mixer (task1300) ----------
    // Per-track, and *only* per-track: the master volume above still
    // applies to the mix, and nothing here is written to disk -- opening a
    // recording again starts every track at full volume.
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        ui.global::<ReviewVm>()
            .on_track_volume_changed(move |track, ratio| {
                let Some(ui) = weak.upgrade() else { return };
                let Ok(track) = usize::try_from(track) else {
                    return;
                };
                let mut review = review_rc.borrow_mut();
                let percent = (f64::from(ratio) * 100.0).round() as i64;
                set_track_volume(&mut review, track, |slot| slot.0 = percent);
                let current = settings_snapshot(&settings);
                render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
            });
    }

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        ui.global::<ReviewVm>().on_track_mute_toggled(move |track| {
            let Some(ui) = weak.upgrade() else { return };
            let Ok(track) = usize::try_from(track) else {
                return;
            };
            let mut review = review_rc.borrow_mut();
            set_track_volume(&mut review, track, |slot| slot.1 = !slot.1);
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        ui.global::<ReviewVm>().on_volume_changed(move |ratio| {
            let Some(ui) = weak.upgrade() else { return };
            let next = commit_settings(&settings, |current| {
                current.volume_percent = (f64::from(ratio) * 100.0).round() as i64;
            });
            let mut review = review_rc.borrow_mut();
            // Touching the volume at all is the user taking the audio over
            // from the start-of-recording mute, and what they set here has
            // to survive it -- so drop the override without replaying it.
            review.live_mute = None;
            review.send(PlaybackCommand::SetVolume {
                percent: next.volume_percent,
                muted: next.muted,
            });
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &next);
        });
    }

    wire_keys(
        ui,
        review,
        (
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
        ),
        settings,
    );
}

/// Pointer work on the track: press, drag, release, wheel, the range handles
/// and the markers those gestures land on.
fn wire_track_gestures(
    ctx: HandlerCtx<'_>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    let HandlerCtx {
        ui,
        review,
        models: (review_segments, review_gaps, review_markers, review_marker_rows),
        settings,
    } = ctx;
    review_handler!(ctx, on_track_pressed, |ui, review, ratio| {
        let Some(viewport) = review.viewport() else {
            return;
        };
        review.dragging = true;
        review.drag_started = Some(Instant::now());
        review.drag_moves = 0;
        review.drag_seeks = 0;
        review.drag_handler_nanos = 0;
        let raw = viewport.time_from_ratio(ratio as f64);
        review.seek_clamped(raw, false);
        review.drag_seeks += 1;
        review.hover = Some(raw);
    });

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let cmd_tx = cmd_tx.clone();
        ui.global::<ReviewVm>()
            .on_track_moved(move |ratio, inside| {
                let started = Instant::now();
                let Some(ui) = weak.upgrade() else { return };
                let mut review = review_rc.borrow_mut();
                let Some(viewport) = review.viewport() else {
                    return;
                };
                if !review.dragging && !inside {
                    return;
                }
                let raw = viewport.time_from_ratio(ratio as f64);
                if review.dragging {
                    review.seek_clamped(raw, false);
                    review.drag_moves += 1;
                    review.drag_seeks += 1;
                }
                review.hover = Some(raw);
                // Request the covering segment's preview frame once.
                if let Some((session_id, index)) = review.snapshot.as_ref().and_then(|snapshot| {
                    snapshot
                        .find_covering_segment(raw)
                        .map(|segment| (snapshot.session_id.clone(), segment.index))
                }) {
                    if !review.thumbnails.contains_key(&index)
                        && review.thumbnails_requested.insert(index)
                    {
                        let _ = cmd_tx.send(Cmd::ReviewThumbnail(session_id, index));
                    }
                }
                let current = settings_snapshot(&settings);
                render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
                if review.dragging {
                    review.drag_handler_nanos += started.elapsed().as_nanos();
                }
            });
    }

    review_handler!(ctx, on_track_released, |ui, review, ratio| {
        if !review.dragging {
            return;
        }
        let Some(viewport) = review.viewport() else {
            return;
        };
        // The order is load-bearing since task3650, do not swap it: the release
        // seek is issued while `dragging` is still true, so it is caught by the
        // same gate as the moves before it and stops where the user let go.
        // Clearing the flag first would wrap on release -- which is 案(b), the
        // option the user rejected on 2026-09-08.
        review.seek_clamped(viewport.time_from_ratio(ratio as f64), true);
        review.dragging = false;
        if let Some(started) = review.drag_started.take() {
            let elapsed = started.elapsed().as_secs_f64();
            if review.drag_moves > 0 && elapsed > 0.0 {
                // The numbers AC2 asks for: seeks/s vs React's 150ms
                // throttle ceiling (~6.7/s), and the handler cost itself.
                tracing::info!(
                    target: "task127_scrub",
                    moves = review.drag_moves,
                    seeks = review.drag_seeks,
                    duration_ms = (elapsed * 1000.0) as u64,
                    seeks_per_sec = format!("{:.1}", review.drag_seeks as f64 / elapsed).as_str(),
                    avg_handler_us =
                        review.drag_handler_nanos / u128::from(review.drag_moves) / 1000,
                    "scrub drag finished"
                );
            }
        }
    });

    review_handler!(ctx, on_track_exited, |ui, review| {
        if !review.dragging {
            review.hover = None;
        }
    });

    review_handler!(ctx, on_track_cancelled, |ui, review| {
        review.dragging = false;
        review.hover = None;
    });

    review_handler!(
        ctx,
        on_track_wheel,
        |ui, review, ratio, delta_y, ctrl, shift| {
            if !ctrl && !shift {
                return false;
            }
            let Some(snapshot) = review.snapshot.clone() else {
                return false;
            };
            let current = review.zoom.unwrap_or_else(|| Viewport::full(&snapshot));
            if shift && !ctrl {
                // slint's wheel delta is positive scrolling up -- inverted
                // from the browser's deltaY, so forward pan is negative here.
                let direction = if delta_y < 0.0 { 1 } else { -1 };
                review.zoom = Some(tl::pan(&snapshot, current, direction));
            } else {
                // Wheel up (positive) zooms in, matching ctrl+wheel page zoom.
                review.zoom = tl::zoom_at(&snapshot, current, ratio as f64, delta_y > 0.0);
            }
            true
        }
    );

    wire_range_handles(ctx, cmd_tx, controller, stop_requests);
}

/// The transport and the range panel: jump, play, step, edit, repeat, rate.
fn wire_transport(ctx: HandlerCtx<'_>) {
    let HandlerCtx {
        ui,
        review,
        models: (review_segments, review_gaps, review_markers, review_marker_rows),
        settings,
    } = ctx;
    // The playhead only: `range` is untouched, which is the whole point of
    // having these beside the transport rather than on the handles.
    review_handler!(ctx, on_jump_range, |ui, review, to_start| {
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        let (start, end) = tl::range_jump_targets(&snapshot, review.range);
        review.seek_clamped(if to_start { start } else { end }, true);
    });

    review_handler!(ctx, on_play_toggled, |ui, review| {
        review.toggle_play();
    });

    // The track reports its own width so `tl::gap_mark` can decide, in
    // Rust, whether a seam gets one rule or two (task171).
    review_handler!(ctx, on_track_resized, |ui, review, width| {
        review.track_width_px = width;
    });

    // round8 §2. The three ways to move a boundary from the panel -- a
    // stepper, a typed time, and the way back -- all land here. Since
    // task3160 they scrub the playhead to the end they moved only when the
    // move left it outside the range: the same 通り過ぎたとき rule the band's
    // own drag follows, so a boundary nudged around a playhead that is still
    // inside the selection no longer throws playback to the edge.
    review_handler!(ctx, on_range_stepped, |ui, review, edge, seconds| {
        // No 確定ステップ gate (2026-09-11, task4340). task3860 carried the old
        // range-edit mode into the 確定ステップ in *two* layers -- `editable` on
        // the .slint side and an `if !review.clip_editing { return; }` here --
        // so the « » steppers vanished outside the step and the user reported
        // it as 「UXが変」 (2026-09-10). The rule that governs them is
        // task3050's: the mode guards the *track* against a stray drag, and a
        // button carries no such risk. `on_range_set_to_playhead` and
        // `on_range_reset` below never had this gate for exactly that reason;
        // the stepper was the one left behind. The track's own handlers
        // (`on_handle_moved`, `on_range_drag_changed`, `on_range_panned`) keep
        // theirs -- the mis-drag risk there is real.
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        let Some(range) = review.range else {
            return;
        };
        let edge = range_edge(edge);
        let from = if edge == tl::RangeEdge::Start {
            range.0
        } else {
            range.1
        };
        apply_range_edit(
            &mut review,
            &snapshot,
            edge,
            from + i64::from(seconds) * tl::HNS_PER_SECOND,
        );
    });

    review_handler!(ctx, on_range_edited, |ui, review, edge, text| {
        if !review.clip_editing {
            return;
        }
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        // No parse, no move: the row rebuilds from the range that never
        // changed, which is the restore the design asks for.
        let Some(offset) = tl::parse_range_time(text.as_str()) else {
            return;
        };
        apply_range_edit(
            &mut review,
            &snapshot,
            range_edge(edge),
            snapshot.start_100ns() + offset,
        );
    });

    review_handler!(ctx, on_range_set_to_playhead, |ui, review, edge| {
        range_edge_to_playhead(&mut review, range_edge(edge));
    });

    // task3870 (design round19 §2-3): 直近15秒 / 30秒 / 60秒. The end is where
    // the playhead is standing and the start is N seconds before it, so this
    // is the 「再生位置に合わせる」 family rather than `on_marker_ranged`'s --
    // and like that family it does not seek. A marker's range starts somewhere
    // the user is not, which is why that one moves the playhead; here the
    // playhead is already on the end the chip just named, and seeking would
    // interrupt a playback the user is watching.
    //
    // The chip hands over its own `seconds`, never its row (task3820).
    // `clamp_range` is what refuses the range that cannot exist -- the playhead
    // at the very head of the recording, or an end that has been pruned away --
    // and a refusal leaves the selection exactly as it was. Nothing is saved:
    // the chip only decides what the range is.
    review_handler!(ctx, on_recent_chip_clicked, |ui, review, seconds| {
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        // Capped at `live_edge - 1` like every other range this file builds
        // (`next_range`'s live range, `quick_range`, `marker_range`'s `high`):
        // `snap_to_recorded_time` tests `time < segment.end`, so the live edge
        // itself is not a recorded time and `clamp_range` would answer `None`.
        // `review.position` is not guaranteed to be under it -- UI seeks go
        // through `classify_seek`, which caps, but the engine's own status is
        // what fills this field while playback runs on -- and the chip pressed
        // while parked at the end of the recording is the most natural press
        // there is. `recent_range` stays unclamped: the recording's ends are
        // the caller's business, which is what makes the pure function's
        // boundary cases statable.
        let end = review.position.min(snapshot.live_edge_100ns - 1);
        let (start, end) = tl::recent_range(end, seconds as i64);
        let Some(range) = snapshot.clamp_range(start, end, tl::min_frame_100ns()) else {
            return;
        };
        review.range = Some(range);
        // A hand-picked range stops following the live edge, as every other
        // deliberate range change does.
        review.quick = -1;
    });

    // task1060's `on_range_edit_toggled` stood here, stepping in and out of
    // range-edit mode. task3860 deleted the toggle: the way in is pressing
    // クリップに保存 and the way out is Esc or Enter, both of which end the
    // 確定ステップ. The mid-gesture latch it cleared on the way out moved with
    // it, into `tl::end_confirm_step` -- which `review_stage.rs` applies at
    // both of those exits, so a drag abandoned by either leaves one state.
    // (task3160's reasoning for clearing it is carried there in full.)

    review_handler!(ctx, on_range_reset, |ui, review| {
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        // Reset is 全選択, not 直近5分 (task1030): the button next to the
        // range readouts is the way back to "all of it", and a five-minute
        // window is a selection of its own, not the absence of one.
        if let Some(range) = snapshot.quick_range(None, tl::min_frame_100ns()) {
            review.range = Some(range);
            review.quick = 0;
            // Where it lands depends on the transport (task3390). Reset while
            // *playing* means "widen what I am watching", so it goes to the
            // new range's start and plays on; `range.1` is the live edge, and
            // landing there while playing only ever meant "stop at the far
            // end" -- either the next frame's `RangeGuard::PauseAtEnd`, or
            // since task3380 the `Pause` `seek_clamped` sends ahead of the
            // seek. (With repeat on that last sentence stopped being true in
            // task3570: `seek_clamped` wraps such a landing to `range.0`
            // instead of stopping on it. The branch below does not care --
            // it asks for `range.0` outright -- but "only ever meant stop"
            // now reads as "with repeat off".) Reset while *paused* keeps the old landing: that gesture
            // reads as 全選択してライブへ戻る, and the user did not ask for it
            // to change. (An assumption, stated when the task was approved.)
            let target = if review.playing { range.0 } else { range.1 };
            review.seek_clamped(target, true);
        }
    });

    // The panel's open/closed state is a setting, not a view flag: it
    // survives a restart (round5 §4-D), so it is written where every other
    // remembered preference is.
    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.global::<ReviewVm>().on_panel_toggled(move |open| {
            let Some(ui) = weak.upgrade() else { return };
            ui.global::<ReviewVm>().set_panel_open(open);
            commit_settings(&settings, |current| current.review_panel_open = open);
        });
    }

    {
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.global::<ReviewVm>().on_fullscreen_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            let on = !ui.window().is_fullscreen();
            set_fullscreen(&ui, on, &settings);
        });
    }

    // Its own clone: `review_handler!` keeps the context's settings behind
    // macro hygiene, so the body cannot borrow the one the macro made.
    let settings_for_autorec = settings.clone();
    // 起動時に自動録画 (round13 §1): the one door into
    // `autoCaptureExecutables`, moved here from the picker tile (task2340).
    // The macro's trailing `render_review` re-derives the switch from the list
    // that was just written, so nothing here sets it.
    review_handler!(ctx, on_autorec_toggled, |ui, review| {
        let Some(executable) = review
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.target_executable.clone())
        else {
            return;
        };
        // Registering resolves path and display name from the running process
        // (task2560); unregistering needs no metadata, so the snapshot walk is
        // skipped for it. An app already gone registers bare -- the settings
        // row then draws the way it always did.
        let entry = if auto_capture::is_registered(
            &settings_snapshot(&settings_for_autorec).auto_capture_executables,
            &executable,
        ) {
            livia::settings::AutoCaptureApp::named(&*executable)
        } else {
            super::app_meta::resolve_entry(&executable)
        };
        let next = commit_settings(&settings_for_autorec, |current| {
            current.auto_capture_executables =
                auto_capture::toggle(&current.auto_capture_executables, entry);
        });
        let registered = auto_capture::is_registered(&next.auto_capture_executables, &executable);
        tracing::info!(
            target: "task1970_auto_capture",
            event = "auto_capture_registration",
            %executable,
            registered,
            "the review panel's toggle changed the auto-capture list"
        );
        // The settings pane is not re-rendered by anything here, so its list is
        // written directly (see `push_auto_capture_list`).
        push_auto_capture_list(&ui, &settings_for_autorec);
    });

    review_handler!(ctx, on_repeat_toggled, |ui, review| {
        review.repeat = !review.repeat;
    });

    review_handler!(ctx, on_time_display_toggled, |ui, review| {
        review.show_remaining = !review.show_remaining;
    });

    review_handler!(ctx, on_rate_selected, |ui, review, index| {
        let index = (index.max(0) as usize).min(tl::PLAYBACK_RATES.len() - 1);
        let rate = tl::PLAYBACK_RATES[index];
        review.rate = rate;
        review.send(PlaybackCommand::SetRate(rate));
    });
    wire_mixer_and_keys(
        ui,
        review,
        (
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
        ),
        settings,
    );
}

/// The review screen's keyboard: the shortcuts the stage answers directly.
fn wire_keys(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    (review_segments, review_gaps, review_markers, review_marker_rows): TrackModels<'_>,
    settings: &Arc<Mutex<AppSettings>>,
) {
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        ui.global::<ReviewVm>().on_mute_toggled(move || {
            let Some(ui) = weak.upgrade() else { return };
            // The button reads muted while the override is up, so the press
            // that takes it down is a request to *hear* -- flipping the
            // stored setting as well would mute what the user just unmuted.
            let armed = review_rc.borrow().live_mute.is_some();
            let next = commit_settings(&settings, |current| {
                if !armed {
                    current.muted = !current.muted;
                }
            });
            let mut review = review_rc.borrow_mut();
            review.live_mute = None;
            review.send(PlaybackCommand::SetVolume {
                percent: next.volume_percent,
                muted: next.muted,
            });
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &next);
        });
    }

    wire_stage_keys(
        ui,
        review,
        (
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
        ),
        settings,
    );
}

/// The stage's own key handling: the shortcuts that reach the transport.
fn wire_stage_keys(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    (review_segments, review_gaps, review_markers, review_marker_rows): TrackModels<'_>,
    settings: &Arc<Mutex<AppSettings>>,
) {
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        ui.global::<ReviewVm>()
            .on_key_pressed_cb(move |text, shift, repeat| {
                let Some(ui) = weak.upgrade() else {
                    return false;
                };
                let mut review = review_rc.borrow_mut();
                // task2540: slint's own `repeat` never arrives on the desktop
                // backend (see `Review::key_down`), so the hold is recognised
                // here -- a press of the key that is already down. Booked
                // before every guard below: a press swallowed by a busy export
                // must not leave a stale key on the state.
                let repeat = repeat || review.key_down.as_deref() == Some(text.as_str());
                review.key_down = Some(text.clone());
                if review.snapshot.is_none() {
                    return false;
                }
                // task2130: the block-out covers the pointer, not the
                // keyboard. Space / f / m / arrows would otherwise walk
                // straight under the cover -- and `f` would fold the panel
                // away with the cancel button in it.
                if ex::is_busy(review.export_status.as_ref()) {
                    return false;
                }
                let current = settings_snapshot(&settings);
                let mut hide_mark = false;
                let handled = match text.as_str() {
                    " " | "k" | "K" => {
                        review.toggle_play();
                        true
                    }
                    "m" | "M" => {
                        // Same rule as the mute button: while the
                        // start-of-recording override is up, this key is
                        // the way *out* of silence, not into it.
                        let armed = review.live_mute.is_some();
                        drop(review);
                        let next = commit_settings(&settings, |current| {
                            if !armed {
                                current.muted = !current.muted;
                            }
                        });
                        let mut review = review_rc.borrow_mut();
                        review.live_mute = None;
                        review.send(PlaybackCommand::SetVolume {
                            percent: next.volume_percent,
                            muted: next.muted,
                        });
                        render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &next);
                        return true;
                    }
                    // Volume is a persisted setting, so this takes the same
                    // drop -> commit -> re-borrow route the mute key does.
                    "\u{F700}" | "\u{F701}" => {
                        let step = if text.as_str() == "\u{F700}" { 5 } else { -5 };
                        drop(review);
                        let next = commit_settings(&settings, |current| {
                            current.volume_percent = (current.volume_percent + step).clamp(0, 100);
                        });
                        let mut review = review_rc.borrow_mut();
                        review.live_mute = None;
                        review.send(PlaybackCommand::SetVolume {
                            percent: next.volume_percent,
                            muted: next.muted,
                        });
                        render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &next);
                        return true;
                    }
                    "s" | "S" => {
                        // Through the same callback the overlay button
                        // uses, so both raise the one corner toast
                        // (task1040) instead of two copies of the save.
                        drop(review);
                        ui.global::<ReviewVm>().invoke_screenshot_save();
                        return true;
                    }
                    other => transport_key(
                        &ui,
                        &mut review,
                        &settings,
                        other,
                        shift,
                        repeat,
                        &mut hide_mark,
                    ),
                };
                if handled {
                    render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
                }
                drop(review);
                if hide_mark {
                    hide_feedback_after(&ui, &review_rc);
                }
                handled
            });
    }

    // The release side of the transport keys (task2540): the tap and scan
    // seeks above are coarse, and this is where the precise confirm gets
    // booked once the key comes up. Losing a release to a focus change just
    // parks the stage on the coarse frame -- the next press re-arms.
    {
        let review_rc = review.clone();
        ui.global::<ReviewVm>().on_key_released_cb(move |text| {
            let mut review = review_rc.borrow_mut();
            // Unconditional, and not keyed on `text`: shift coming up during a
            // hold turns `L` back into `l`, so a keyed clear would never match
            // and the next tap would read as a scan step.
            review.key_down = None;
            match text.as_str() {
                "\u{F702}" | "\u{F703}" | "j" | "J" | "l" | "L" => {}
                _ => return,
            }
            if review.snapshot.is_none() || ex::is_busy(review.export_status.as_ref()) {
                return;
            }
            review.key_scan_steps = 0;
            review.key_scan_last = None;
            review.arm_settle(&review_rc);
        });
    }
}

/// The range handles, the band's own drag, and the markers a click lands on.
fn wire_range_handles(
    ctx: HandlerCtx<'_>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    let HandlerCtx {
        ui,
        review,
        models: (review_segments, review_gaps, review_markers, review_marker_rows),
        settings,
    } = ctx;
    review_handler!(ctx, on_track_double_clicked, |ui, review| {
        review.zoom = None;
    });

    // React's `updateHandle`: replace one endpoint, keep the result only
    // when `clamp_range` accepts it, and drop the quick-range chip echo --
    // a manual drag yields a range no chip owns.
    review_drag_handler!(ctx, on_handle_moved, |ui, review, kind, ratio| {
        // Only while the boundaries are editable -- range-edit mode in
        // task1060, the 確定ステップ since task3860. The .slint side already
        // refuses the drag; this is the same rule stated where the state
        // actually changes, so no other caller can move a boundary either.
        if !review.clip_editing {
            return;
        }
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        let Some(viewport) = review.viewport() else {
            return;
        };
        let Some((start, end)) = review.range else {
            return;
        };
        // Through `move_range_edge` since task3050, so the drag, the panel's
        // buttons and `i`/`o` all clamp by one rule: a handle pushed past its
        // partner stops one frame short of it instead of freezing where it
        // last was legal.
        if let Some(next) = tl::move_range_edge(
            &snapshot,
            (start, end),
            range_edge(kind),
            viewport.time_from_ratio(ratio as f64),
            tl::min_frame_100ns(),
        ) {
            review.range = Some(next);
            review.quick = -1;
            // task3160: coarse, like every other scrub -- the exact frame is
            // decoded once by the release below.
            if let Some(target) = tl::range_follow(next, review.position, Some(range_edge(kind))) {
                review.seek_clamped(target, false);
                // Counted at the call site, like the scrub's own seeks: it is
                // the seeks-per-move rate that says whether a range drag asks
                // the engine for more than a scrub does (task3400).
                review.drag_seeks += 1;
            }
        }
    });

    // task3160: the band's drag, bracketed. Pressing pauses so the playhead
    // can be scrubbed along with the boundary instead of racing it, and the
    // release pays that pause back -- now (task3390) whenever the follow left
    // the playhead short of the selection's end, whichever of the three
    // gestures did it, rather than only for 開始端 and 移動.
    //
    // The one gesture that still stays stopped is 終了端 dragged past the
    // playhead: the follow parks it *on* the new end, and resuming there would
    // only flicker -- the next drained frame's `range_guard` says `PauseAtEnd`
    // and stops it again. That, not the edge it came from, is what task3160's
    // 「終了位置で止まったまま」 was protecting; an end edit that leaves the
    // playhead inside the selection now keeps playing (user, 2026-09-07).
    review_handler!(ctx, on_range_drag_changed, |ui, review, on, mode| {
        if on {
            // Guarded like `on_handle_moved`: the .slint side already refuses
            // the drag outside the 確定ステップ, and this says it again where
            // the state changes. The release below is deliberately *not*
            // guarded -- a step ended mid-gesture must still be able to unpin
            // the bar and clear the latch.
            if !review.clip_editing {
                return;
            }
            review.range_dragging = true;
            // task3400: the same four counters the scrub arms in
            // `on_track_pressed`, so the two gestures are read off one ruler.
            review.drag_started = Some(Instant::now());
            review.drag_moves = 0;
            review.drag_seeks = 0;
            review.drag_handler_nanos = 0;
            review.resume_after_range_drag = review.playing;
            if review.playing {
                review.playing = false;
                review.send(PlaybackCommand::Pause);
            }
            return;
        }
        // The flag stays up until after the follow seek below (user, extending
        // task3650's ruling in person on 2026-09-09). It used to be cleared
        // right here, which is the opposite order to `on_track_released` --
        // 3650's own sweep recorded the asymmetry and what it costs: the
        // release's follow seek ran *outside* task3650's gate, so with repeat
        // on a landing past the end wrapped and the playhead jumped to the
        // selection's head. Both gestures now read the same set at the same
        // moment. What the swap does *not* do is change today's observable
        // behaviour: the press above put `playing` at false, and `seek_plan`
        // answers `Go` on `!playing` before it ever looks at the gate, so on
        // this path the gate is defence in depth (3650's sweep item 4 says the
        // same). It bites the day a release seek runs with playback live.
        // task3400: reported before the follow seek below, and on every
        // release path including `-1`, so the line describes the moves the
        // gesture actually made rather than the release's own precise decode.
        // Same fields and same shape as `task127_scrub`, plus `mode`
        // (0=開始端 / 1=終了端 / 2=移動 / -1=動かなかった) because the three
        // gestures do measurably different work per move.
        if let Some(started) = review.drag_started.take() {
            let elapsed = started.elapsed().as_secs_f64();
            if review.drag_moves > 0 && elapsed > 0.0 {
                tracing::info!(
                    target: "task3400_range_drag",
                    mode = mode,
                    moves = review.drag_moves,
                    seeks = review.drag_seeks,
                    duration_ms = (elapsed * 1000.0) as u64,
                    seeks_per_sec = format!("{:.1}", review.drag_seeks as f64 / elapsed).as_str(),
                    avg_handler_us = review.drag_handler_nanos / u128::from(review.drag_moves) / 1000,
                    "range drag finished"
                );
            }
        }
        // The latch is spent either way, so a 終了端 drag cannot leave one
        // armed for the next gesture to cash in.
        let resume = std::mem::take(&mut review.resume_after_range_drag);
        let Some(follow_edge) = range_drag_follow_edge(mode) else {
            // `-1`: the gesture never moved (a press the band passes through
            // as a plain seek) or the pointer was cancelled. Nothing was
            // edited, so nothing is followed and the transport goes back to
            // exactly what it was. No follow seek runs on this path, so the
            // flag comes down here instead -- and it must, or the render the
            // macro runs after this body would still draw the band as grabbed.
            review.range_dragging = false;
            if resume {
                review.playing = true;
                review.send(PlaybackCommand::Play);
            }
            return;
        };
        // Unconditional, unlike the moves above: `seek_clamped` moved
        // `review.position` to the classified target while the engine only
        // ever landed on a coarse keyframe, so a release that skipped the
        // precise decode would leave the picture behind the clock. Same
        // reason `on_track_released` always seeks.
        let landed_short_of_the_end = if let Some(range) = review.range {
            let position = review.position;
            let target = tl::range_follow(range, position, follow_edge).unwrap_or(position);
            review.seek_clamped(target, true);
            // Asked about `review.position` -- where the seek actually landed
            // -- rather than `target`, for the same reason task3380 asks
            // `pb::seek_plan` about its own landing: `classify_seek`
            // clamps and snaps over gaps, so the request and the result are
            // not always the same tick.
            tl::may_resume_after_range_edit(range, review.position)
        } else {
            // Unreachable while `follow_edge` is `Some` (a handle drag needs a
            // range to have moved), and harmless: with no selection there is
            // no guard to stop playback again.
            true
        };
        // After the seek, exactly as `on_track_released` clears `dragging`
        // after its own.
        review.range_dragging = false;
        if resume && landed_short_of_the_end {
            review.playing = true;
            review.send(PlaybackCommand::Play);
        }
    });

    // round8 §2: the band's middle, dragged. The length is fixed for the
    // whole gesture -- the pointer carries the range along and the ends of
    // the recording stop it, rather than squashing it.
    review_drag_handler!(ctx, on_range_panned, |ui, review, ratio| {
        if !review.clip_editing {
            return;
        }
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        let Some(viewport) = review.viewport() else {
            return;
        };
        let Some(range) = review.range else {
            return;
        };
        if let Some(next) = tl::pan_range(
            &snapshot,
            range,
            viewport.time_from_ratio(ratio as f64),
            tl::min_frame_100ns(),
        ) {
            review.range = Some(next);
            review.quick = -1;
            // task3160: a move always shows its start, wherever the playhead
            // stood -- the selection has been carried somewhere else, so its
            // first frame is the one worth looking at.
            if let Some(target) = tl::range_follow(next, review.position, None) {
                review.seek_clamped(target, false);
                review.drag_seeks += 1;
            }
        }
    });

    // A click is now only a jump. It used to open a popover on the bar;
    // naming and deleting moved to the right panel's list (round5 §4-B),
    // where there is room for them and where the colours line up.
    review_handler!(ctx, on_marker_clicked, |ui, review, row| {
        let Some(time) = review.visible_markers.get(row as usize).copied() else {
            return;
        };
        review.seek_clamped(time, true);
    });

    // task3050: a marker is where something happened, so the useful thing to
    // do with one is to select around it. Unlike the boundary buttons this
    // does move the playhead -- the new range starts somewhere the user is
    // not, and the point of picking it is to watch it.
    review_handler!(ctx, on_marker_ranged, |ui, review, row| {
        let Some(time) = review.visible_markers.get(row as usize).copied() else {
            return;
        };
        let Some(snapshot) = review.snapshot.clone() else {
            return;
        };
        if let Some(range) = tl::marker_range(&snapshot, time, tl::min_frame_100ns()) {
            review.range = Some(range);
            review.quick = -1;
            review.seek_clamped(range.0, true);
        }
    });
    wire_markers_and_stop(
        ui,
        review,
        (
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
        ),
        settings,
        cmd_tx,
        controller,
        stop_requests,
    );
}

/// The keys that only move the transport: no settings write and no re-borrow
/// of the review state, which is what keeps them out of the handler's own
/// `drop`/re-borrow dance above.
fn transport_key(
    ui: &AppWindow,
    review: &mut Review,
    settings: &Arc<Mutex<AppSettings>>,
    key: &str,
    shift: bool,
    repeat: bool,
    hide_mark: &mut bool,
) -> bool {
    match key {
        "<" => {
            review.rate = tl::shift_playback_rate(review.rate, -1);
            review.send(PlaybackCommand::SetRate(review.rate));
            true
        }
        ">" => {
            review.rate = tl::shift_playback_rate(review.rate, 1);
            review.send(PlaybackCommand::SetRate(review.rate));
            true
        }
        "," | "." => {
            // Frame step by the encoder's nominal quantum;
            // pausing first is what makes the step visible.
            review.playing = false;
            review.send(PlaybackCommand::Pause);
            let step =
                tl::HNS_PER_SECOND / i64::from(settings_snapshot(settings).frame_rate.max(1));
            let direction = if key == "," { -1 } else { 1 };
            let target = review.position + direction * step;
            review.seek_clamped(target, true);
            true
        }
        // Home / End: the two ends of the recording.
        "\u{F729}" | "\u{F72B}" => {
            let Some(snapshot) = review.snapshot.as_ref() else {
                return false;
            };
            // Read the target out first: `seek_clamped` borrows
            // `review` mutably.
            let target = if key == "\u{F729}" {
                snapshot.start_100ns()
            } else {
                snapshot.live_edge_100ns - 1
            };
            review.seek_clamped(target, true);
            true
        }
        "\u{F702}" | "\u{F703}" | "j" | "J" | "l" | "L" => {
            let direction = match key {
                "\u{F702}" | "j" | "J" => -1,
                _ => 1,
            };
            let delta = if repeat {
                // Held key (task2540): a 5Hz coarse scan with the stage
                // hold's ramp. The OS repeat arrives at ~30Hz; steps inside
                // the cadence are swallowed (handled, no seek). A stray
                // settle from a released sibling key must not fire mid-hold.
                review.key_settle_timer.stop();
                let now = Instant::now();
                let since = review
                    .key_scan_last
                    .map(|last| now.duration_since(last).as_millis() as u64);
                if !pb::key_scan_due(since) {
                    return true;
                }
                review.key_scan_steps += 1;
                review.key_scan_last = Some(now);
                // Arrows and j/l walk the same 5s base while scanning, and
                // shift is ignored: the ramp is the accelerator here.
                direction * pb::key_scan_step_seconds(review.key_scan_steps)
            } else {
                // A fresh tap supersedes both the pending settle confirm
                // and whatever scan the previous hold accumulated
                // (task2540); the release re-arms the settle.
                review.key_settle_timer.stop();
                review.key_scan_steps = 0;
                review.key_scan_last = None;
                let seconds = match key {
                    // J/L are YouTube's ±10s, and are not modified
                    // by shift the way the arrows are.
                    "j" | "J" | "l" | "L" => 10,
                    _ if shift => 30,
                    _ => 5,
                };
                direction * seconds
            };
            let target = review.position + delta * tl::HNS_PER_SECOND;
            // Coarse (task2540): the nearest clean point shows in ~15ms, so
            // every tap of a flurry moves the picture; the exact frame is
            // decoded by the settle once the key is released.
            let outcome = review.seek_clamped(target, false);
            // Same feedback layer as the stage taps: the
            // keyboard must not be the one path that says
            // nothing when a seek ran off an end (round2 §10).
            let previous = review.stage_feedback;
            review.stage_feedback = Some(pb::fold_tap(previous, outcome, delta));
            render_stage_feedback(ui, review);
            // ...and it comes down on the same schedule too
            // (task1020). Without this the keyboard was the
            // one path that raised a mark and left it up until
            // some other gesture happened to replace it.
            *hide_mark = true;
            true
        }
        // task3050: the keyboard half of the two playhead buttons. The
        // caller has already turned away a missing snapshot and a running
        // export, so there is no guard of its own to write.
        "i" | "I" => {
            range_edge_to_playhead(review, tl::RangeEdge::Start);
            true
        }
        "o" | "O" => {
            range_edge_to_playhead(review, tl::RangeEdge::End);
            true
        }
        "f" | "F" => {
            let on = !ui.window().is_fullscreen();
            set_fullscreen(ui, on, settings);
            true
        }
        // One Escape does one thing. The rate menu comes before
        // any of these -- the .slint side takes it first, so it
        // never reaches here (task148).
        "\u{001b}" => match tl::escape_action(
            review.clip_editing,
            ui.window().is_fullscreen(),
            review.zoom.is_some(),
        ) {
            // task3860: a fallback only. While the step is open its comment
            // field holds key focus and slint's `TextInput` accepts Escape
            // before this scope ever sees it, so the key reaches here only
            // with the field already gone. The range is kept either way --
            // `end_confirm_step` hands it straight back.
            tl::EscapeAction::CloseConfirmStep => {
                let ended = tl::end_confirm_step(review.range);
                review.clip_editing = false;
                review.range = ended.range;
                review.range_dragging = ended.range_dragging;
                review.resume_after_range_drag = ended.resume_after_range_drag;
                true
            }
            tl::EscapeAction::LeaveFullscreen => {
                set_fullscreen(ui, false, settings);
                true
            }
            tl::EscapeAction::ClearZoom => {
                review.zoom = None;
                true
            }
            tl::EscapeAction::Nothing => false,
        },
        // 0-9 jump to that tenth of the recording, like
        // YouTube's digit keys.
        digit if digit.len() == 1 && digit.as_bytes()[0].is_ascii_digit() => {
            let Some(snapshot) = review.snapshot.as_ref() else {
                return false;
            };
            let target = tl::digit_jump_100ns(snapshot, u32::from(digit.as_bytes()[0] - b'0'));
            review.seek_clamped(target, true);
            true
        }
        _ => false,
    }
}

/// The live capsule's own stop (task237, task2030 judgement 4/5).
fn wire_capsule_stop(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    cmd_tx: &Sender<Cmd>,
    controller: &CaptureController,
    stop_requests: &super::StopRequests,
) {
    // ---------- the live capsule's stop (task237, task2030 判断4/5) ----------
    // It stops **the session on the stage** and no other: the review screen
    // only ever shows one recording, and stopping everything from here
    // would take down captures the user cannot even see (round9 §1-3). The
    // one "stop everything" door left is the tray.
    //
    // No confirmation any more (判断4): what has already been recorded is
    // kept, so this destroys nothing, and clicking the target's tile again
    // starts a new capture. The stop toast is the feedback.
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let controller = controller.clone();
        let cmd_tx = cmd_tx.clone();
        let stop_requests = stop_requests.clone();
        ui.global::<ReviewVm>().on_stop_requested(move || {
            let Some(ui) = weak.upgrade() else { return };
            let session_id = {
                let mut review = review_rc.borrow_mut();
                if review.stopping {
                    return;
                }
                // The stage's own session, not `stop_async`'s "whatever is
                // running": with two captures those are different answers.
                let Some(session_id) = review
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.session_id.clone())
                else {
                    return;
                };
                // The 1s poll will confirm it; saying so now is what keeps
                // the chip from offering the same stop twice in between.
                review.stopping = true;
                session_id
            };
            ui.global::<ReviewVm>()
                .set_live_chip_text(lifecycle::live_chip_stopping(tr_locale()).into());
            tracing::info!(
                event = "capture_stop_requested",
                %session_id,
                "stop asked for by the live capsule"
            );
            lifecycle::request_stop(&mut stop_requests.borrow_mut(), &session_id);
            controller.stop_session_async(&session_id);
            let _ = cmd_tx.send(Cmd::Reclaim);
            let _ = cmd_tx.send(Cmd::ListSessions);
        });
    }
}
