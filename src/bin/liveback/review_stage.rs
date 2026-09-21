//! The video stage's callback wiring (task128) plus export / screenshot
//! (task129): frame pump, first-run hint, stage tap/hold gestures, and the
//! export pipeline's slint end. Moved verbatim out of `main`; the state and
//! renderers live in `review`.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use livia::export::{ExportController, ExportRequest, ExportStatus};
use livia::playback::PlaybackCommand;
use livia::settings::AppSettings;
use livia::ui_state::export as ex;
use livia::ui_state::playback as pb;
use livia::ui_state::timeline as tl;
use slint::{ComponentHandle, VecModel};

use super::review::{
    hide_feedback_after, pump_playback, redraw_paused_stage, render_rate, render_review,
    render_stage_feedback, reveal_in_directory, save_current_frame, Review, StageGesture,
};
use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{AppWindow, GapSpan, MarkerRow, MarkerSpan, ReviewVm, TrackSpan};

/// Wires the stage and export handlers. Everything below moved verbatim out
/// of `main` (the blocks only borrow what they used to capture from it).
///
/// The argument count is the point of the function, not a smell to refactor
/// away: these are the pieces of `main`'s state the handlers close over, and
/// bundling them into a struct would only move the same list somewhere else.
#[allow(clippy::too_many_arguments)]
pub(super) fn wire(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    review_segments: &Rc<VecModel<TrackSpan>>,
    review_gaps: &Rc<VecModel<GapSpan>>,
    review_markers: &Rc<VecModel<MarkerSpan>>,
    review_marker_rows: &Rc<VecModel<MarkerRow>>,
    settings: &Arc<Mutex<AppSettings>>,
    // Both built in `main` since task130; the sink carries the tray handle.
    (exporter, export_slot): (&ExportController, &Arc<Mutex<Option<ExportStatus>>>),
    status_line: &Rc<RefCell<livia::ui_state::toast::Toast>>,
) {
    // The stage's timers. All slint timers, so they run on the event loop and
    // need no synchronization with the handlers that arm them. The mark's own
    // pair lives on `Review` instead: the keyboard raises the same mark from
    // `review_wiring` and has to be able to take it down (task1020).
    let hold_timer = Rc::new(slint::Timer::default());
    let hint_timer = Rc::new(slint::Timer::default());
    {
        // ---------- the stage's frame gate (task1580) ----------
        // Without it the pump hands slint a new frame every engine tick, and at
        // full screen x2 -- 114 ticks a second into a 60Hz display --
        // `set_stage_frame` costs 3.75ms of the 5.13ms `ui_pump_playback`, 58%
        // of the UI thread, to replace pictures nobody ever saw.
        //
        // `Window::set_rendering_notifier` would be the exact signal. Since
        // 68d0d0c0 (wgpu-30 renderer) it does fire on the Direct3D path
        // (measured: `RenderingSetup` n=1, `AfterRendering` n>=3), but it
        // still never fires under `LIVEBACK_RENDERER=software` (n=0) -- see
        // `.agents/docs/slint-ui-migration.md`. So the display is still
        // asked for its rate instead.
        let hz = super::desktop::display_refresh_hz();
        tracing::info!(
            target: "task1580_stage",
            refresh_hz = hz,
            "stage frame gate paced to the display"
        );
        review.borrow_mut().stage_gate = pb::StageGate::for_refresh_hz(hz);
    }
    {
        wire_stage_pointer(ui, review, settings, &hold_timer, &hint_timer);
        wire_stage_panels(
            ui,
            review,
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
            settings,
            (exporter, export_slot),
            status_line,
            &hold_timer,
        );
    }
}

/// The stage itself: pointer gestures, the hold-to-mark timer and the hint
/// it raises.
#[allow(clippy::too_many_arguments)]
fn wire_stage_pointer(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    settings: &Arc<Mutex<AppSettings>>,
    hold_timer: &Rc<slint::Timer>,
    hint_timer: &Rc<slint::Timer>,
) {
    // ---------- video stage (task128) ----------
    {
        // How big the picture is drawn, from the layout that already
        // computes it (task990). The engine resamples to this instead of
        // handing skia a 1440p frame to minify bilinearly.
        let weak = ui.as_weak();
        let review_rc = review.clone();
        ui.global::<ReviewVm>()
            .on_stage_size_changed(move |width, height| {
                let Some(ui) = weak.upgrade() else { return };
                let target = pb::stage_target(width, height, ui.window().scale_factor());
                let mut review = review_rc.borrow_mut();
                if review.stage_target == target {
                    return;
                }
                review.stage_target = target;
                // Task1580: the size the engine resamples to, and whether
                // it still shrinks the recording. Neither was in any log,
                // so a run's fullscreen moments had to be inferred from the
                // `ui_image_from_rgba` mean. t260914-a025 added the surface
                // the stage is really drawn on next to it.
                super::shell::log_stage_resize(ui.window(), "review", target);
                if let Some(engine) = review.engine.as_ref() {
                    engine.shared().set_display_target(target);
                }
                // Playing, the next frame arrives at the new size on its
                // own. Paused, no frame is coming and the one on screen
                // would stay at the size the stage used to be, so it is
                // resampled here -- a one-off on the UI thread, not the
                // per-frame work task205 moved off it.
                if !review.playing {
                    redraw_paused_stage(&ui, &review);
                }
            });
    }
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let settings = settings.clone();
        let hint = hint_timer.clone();
        ui.global::<ReviewVm>().on_frame_ready(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            if !pump_playback(&ui, &mut review) || review.hint_counted {
                return;
            }
            // First three sessions only, and only once playback has
            // actually started -- a hint about the transport is noise
            // until there is something to move through (task119).
            review.hint_counted = true;
            drop(review);
            let shown = settings_snapshot(&settings)
                .video_stage_hint_shown
                .unwrap_or(0);
            if shown >= i64::from(pb::STAGE_HINT_MAX_SHOWINGS) {
                return;
            }
            commit_settings(&settings, |current| {
                current.video_stage_hint_shown = Some(shown + 1);
            });
            let weak = ui.as_weak();
            let hide = hint.clone();
            hint.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(pb::STAGE_HINT_DELAY_MS),
                move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.global::<ReviewVm>().set_stage_hint_visible(true);
                    let weak = ui.as_weak();
                    hide.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(pb::STAGE_HINT_DURATION_MS),
                        move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.global::<ReviewVm>().set_stage_hint_visible(false);
                            }
                        },
                    );
                },
            );
        });
    }

    wire_stage_hold(ui, review, hold_timer);
}

/// The panels around the stage: the range/marker panel, export, screenshots
/// and the toasts they raise.
#[allow(clippy::too_many_arguments)]
fn wire_stage_panels(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    review_segments: &Rc<VecModel<TrackSpan>>,
    review_gaps: &Rc<VecModel<GapSpan>>,
    review_markers: &Rc<VecModel<MarkerSpan>>,
    review_marker_rows: &Rc<VecModel<MarkerRow>>,
    settings: &Arc<Mutex<AppSettings>>,
    (exporter, export_slot): (&ExportController, &Arc<Mutex<Option<ExportStatus>>>),
    status_line: &Rc<RefCell<livia::ui_state::toast::Toast>>,
    hold_timer: &Rc<slint::Timer>,
) {
    // ---------- export / screenshot (task129) ----------
    {
        // The sink already folded the event into `export_slot`; this only
        // copies it onto the UI thread. No polling: unlike the WebView
        // build there is no event bus that can silently drop a packet.
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let slot = export_slot.clone();
        let line = status_line.clone();
        ui.global::<ReviewVm>().on_export_changed(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            let next = slot.lock().ok().and_then(|slot| slot.clone());
            if let Some(next) = next.as_ref() {
                // Task2970: a clip's comment is written here -- the pipeline
                // knows nothing about comments, only about the file it wrote.
                // Scoped so the borrow of `clip_pending` ends before it is
                // cleared below.
                let (clip_finished, comment_failed) = {
                    let clip = ex::clip_finish(review.clip_pending.as_ref(), next);
                    let mut failed = false;
                    // On the UI thread deliberately: task2960 measured `Commit`
                    // under a millisecond for this app's own moov-last output
                    // at both 900 bytes and 0.94GB, and the property handler's
                    // one-off ~45ms load is paid once per process. Every part
                    // of a gap-spanning selection gets the same comment -- it
                    // is one clip.
                    if let Some((clip, comment)) =
                        clip.as_ref().and_then(|clip| Some((clip, clip.comment?)))
                    {
                        for path in clip.paths {
                            if let Err(error) = livia::clips::write_comment(path, comment) {
                                tracing::warn!(path = %path.display(), %error, "clip comment");
                                failed = true;
                            }
                        }
                    }
                    (clip.is_some(), failed)
                };
                // Whichever way the job ended, it is no longer pending: a
                // failure or a cancel has nothing to write into.
                if !ex::is_busy(Some(next)) {
                    review.clip_pending.take();
                }
                // The video is on disk either way, so a comment that could not
                // be written is a warning over the clip's own success line,
                // not a failure.
                let clip_title = |default: &'static str| match (comment_failed, clip_finished) {
                    (true, _) => ex::clip_comment_failed(tr_locale()),
                    (false, true) => ex::clip_saved(tr_locale()),
                    (false, false) => default,
                };

                // `notifyIfHidden`, ported (task130): a finished job is worth
                // an OS toast only when the user is looking elsewhere -- the
                // corner below already says the same thing.
                if let Some((title, body)) =
                    ex::completion_toast(tr_locale(), review.export_status.as_ref(), next)
                {
                    let focused = super::shell::hwnd_of(ui.window())
                        .is_some_and(super::desktop::is_foreground);
                    if !focused {
                        super::desktop::toast(clip_title(title), &body);
                    }
                }
                // Task169: the outcome used to be a card with no expiry in
                // the middle of the screen.
                if let Some(corner) =
                    ex::outcome_toast(tr_locale(), review.export_status.as_ref(), next)
                {
                    let variant = if comment_failed {
                        livia::ui_state::toast::ToastVariant::Warning
                    } else {
                        corner.variant
                    };
                    super::publish_toast(
                        &ui,
                        &line,
                        clip_title(corner.title),
                        &corner.detail,
                        variant,
                        corner.action,
                    );
                    // The toast carries its own reveal target now
                    // (task1040): reading the export panel's list meant
                    // every other producer's フォルダで表示 opened the last
                    // export instead of its own file.
                    if !corner.action.is_empty() {
                        if let Some(path) = next.paths.first() {
                            line.borrow_mut().set_action_path(&path.to_string_lossy());
                        }
                    }
                }
            }
            review.export_status = next;
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    wire_panel_rows(
        ui,
        review,
        review_segments,
        review_gaps,
        review_markers,
        review_marker_rows,
        settings,
        exporter,
        status_line,
        hold_timer,
    );
}

/// The hold-to-mark timer and the hint it raises.
fn wire_stage_hold(ui: &AppWindow, review: &Rc<RefCell<Review>>, hold_timer: &Rc<slint::Timer>) {
    {
        // Zone is fixed at pointerdown so sliding mid-gesture can't turn
        // a rewind into a fast-forward. The hold's repeat runs on a slint
        // timer, which is the event loop's own -- no extra thread.
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let hold = hold_timer.clone();
        ui.global::<ReviewVm>()
            .on_stage_pressed(move |x, y, width, height| {
                let Some(ui) = weak.upgrade() else { return };
                // Outside the picture is the letterbox, which is not the
                // video: no gesture starts there (task1010). The move handler
                // above still runs over it, so the overlay and the zone hints
                // behave as they always did.
                let Some(ratio) = pb::stage_hit(x, y, width, height) else {
                    return;
                };
                let zone = pb::stage_zone(ratio);
                {
                    let mut review = review_rc.borrow_mut();
                    // The second press of a double click. Full screen goes now
                    // rather than on the release slint would have reported it
                    // on, and the tap that opened the pair is undone so the
                    // whole gesture leaves the transport where it found it --
                    // the playhead included (task3270). The undo alone stopped
                    // being enough once task3150 made the opening tap jump to
                    // the selection's start, so the position saved with the
                    // tap is handed back here as well.
                    let since = review
                        .last_center_tap
                        .map(|(at, _)| at.elapsed().as_millis() as u64);
                    if pb::stage_double_press(since, zone) {
                        // Taken before the toggle: the toggle is what may move
                        // the playhead, and this is the only copy of where it
                        // was.
                        let saved = review.last_center_tap.take().map(|(_, at)| at);
                        review.gesture = None;
                        review.toggle_play();
                        if let Some(target) =
                            pb::restore_after_double_press(review.playing, saved, review.position)
                        {
                            review.seek_clamped(target, true);
                        }
                        review.stage_feedback = None;
                        drop(review);
                        hold.stop();
                        render_stage_feedback(&ui, &review_rc.borrow());
                        ui.global::<ReviewVm>().invoke_fullscreen_toggled();
                        return;
                    }
                    review.last_center_tap = None;
                    review.gesture = Some(StageGesture {
                        zone,
                        held: false,
                        hold_seconds: 0,
                        hold_steps: 0,
                        restore_rate: None,
                        repause: false,
                    });
                }
                if zone == pb::StageZone::Center {
                    // Centre hold = x2 for as long as it is held (task650).
                    // One shot, not a repeat: this is continuous playback, so
                    // there is nothing to step.
                    let weak = ui.as_weak();
                    let review_rc = review_rc.clone();
                    hold.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(pb::STAGE_HOLD_DELAY_MS),
                        move || {
                            let Some(ui) = weak.upgrade() else { return };
                            let mut review = review_rc.borrow_mut();
                            let hold = pb::stage_hold_rate(review.playing, review.rate);
                            // Held, so the release cannot fall through to the
                            // play/pause tap: a long press is a skim, not a
                            // transport command.
                            match review.gesture.as_mut() {
                                Some(gesture) => {
                                    gesture.held = true;
                                    gesture.restore_rate = Some(hold.restore);
                                    gesture.repause = hold.repause;
                                }
                                None => return,
                            }
                            review.rate = hold.applied;
                            review.send(PlaybackCommand::SetRate(hold.applied));
                            // A hold that started paused runs anyway and stops
                            // again on release (task1010).
                            if hold.repause {
                                review.playing = true;
                                review.send(PlaybackCommand::Play);
                            }
                            review.stage_feedback = Some(pb::StageFeedback::RateHold);
                            render_rate(&ui, &review);
                            render_stage_feedback(&ui, &review);
                        },
                    );
                    return;
                }
                let weak = ui.as_weak();
                let review_rc = review_rc.clone();
                let hold_repeat = hold.clone();
                hold.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(pb::STAGE_HOLD_DELAY_MS),
                    move || {
                        let weak = weak.clone();
                        let review_rc = review_rc.clone();
                        // Each repeat reads the engine's real position rather
                        // than advancing a private cursor -- a cursor walks
                        // off to somewhere the decoder never reaches (the
                        // lesson React learned at 3s/500ms).
                        let step = move || {
                            let Some(ui) = weak.upgrade() else { return };
                            let mut review = review_rc.borrow_mut();
                            // A stray settle from a released key or an
                            // earlier tap must not slip a precise decode
                            // into the scan (task2580, same guard as the
                            // key scan's).
                            review.key_settle_timer.stop();
                            let Some(gesture) = review.gesture.as_mut() else {
                                return;
                            };
                            gesture.held = true;
                            let step_no = gesture.hold_steps + 1;
                            gesture.hold_seconds += pb::hold_step_seconds(step_no);
                            gesture.hold_steps += 1;
                            let (zone, seconds, steps) =
                                (gesture.zone, gesture.hold_seconds, gesture.hold_steps);
                            let target = review.position + pb::hold_step_100ns(zone, step_no);
                            let outcome = review.seek_clamped(target, false);
                            review.stage_feedback =
                                Some(pb::fold_hold(outcome, zone.direction(), seconds, steps));
                            render_stage_feedback(&ui, &review);
                        };
                        step();
                        hold_repeat.start(
                            slint::TimerMode::Repeated,
                            Duration::from_millis(pb::STAGE_REPEAT_INTERVAL_MS),
                            step,
                        );
                    },
                );
            });
    }

    wire_stage_taps(ui, review, hold_timer);
}

/// Export and the screenshot door, and the toasts they raise.
#[allow(clippy::too_many_arguments)]
fn wire_export_doors(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    review_segments: &Rc<VecModel<TrackSpan>>,
    review_gaps: &Rc<VecModel<GapSpan>>,
    review_markers: &Rc<VecModel<MarkerSpan>>,
    review_marker_rows: &Rc<VecModel<MarkerRow>>,
    settings: &Arc<Mutex<AppSettings>>,
    exporter: &ExportController,
    status_line: &Rc<RefCell<livia::ui_state::toast::Toast>>,
    hold_timer: &Rc<slint::Timer>,
) {
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let exporter = exporter.clone();
        ui.global::<ReviewVm>().on_export_cancel(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            if let Err(error) = exporter.cancel() {
                review.status = Some(error);
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
        ui.global::<ReviewVm>().on_export_reveal(move |path| {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            if reveal_in_directory(std::path::Path::new(path.as_str())).is_err() {
                review.status = Some(ex::reveal_error(tr_locale()).to_owned());
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    {
        // Every shot gets a corner toast rather than a line on the video
        // that only a session reload could clear (task1040). Latest wins,
        // so holding `s` down leaves one toast naming the last file.
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let line = status_line.clone();
        ui.global::<ReviewVm>().on_screenshot_save(move || {
            let Some(ui) = weak.upgrade() else { return };
            let saved = save_current_frame(&review_rc.borrow());
            let corner = ex::screenshot_toast(tr_locale(), &saved);
            super::publish_toast(
                &ui,
                &line,
                &corner.title,
                &corner.detail,
                corner.variant,
                corner.action,
            );
            if let Ok(path) = saved {
                line.borrow_mut().set_action_path(&path.to_string_lossy());
            }
        });
    }

    {
        // Cancelled means the browser/compositor abandoned the gesture,
        // so its coordinate names no position the user chose: tear down
        // without acting.
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let hold = hold_timer.clone();
        ui.global::<ReviewVm>().on_stage_cancelled(move || {
            hold.stop();
            let mut review = review_rc.borrow_mut();
            let Some(gesture) = review.gesture.take() else {
                return;
            };
            // An abandoned gesture still owes back whatever it borrowed:
            // without this the stage stays stuck at x2 (task650), or
            // playing after a paused skim (task1010).
            let (Some(rate), Some(ui)) = (gesture.restore_rate, weak.upgrade()) else {
                return;
            };
            review.rate = rate;
            review.send(PlaybackCommand::SetRate(rate));
            if gesture.repause {
                review.playing = false;
                review.send(PlaybackCommand::Pause);
            }
            review.stage_feedback = None;
            render_rate(&ui, &review);
            render_stage_feedback(&ui, &review);
        });
    }
}

/// The stage's own taps and the feedback they raise.
fn wire_stage_taps(ui: &AppWindow, review: &Rc<RefCell<Review>>, hold_timer: &Rc<slint::Timer>) {
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let hold = hold_timer.clone();
        ui.global::<ReviewVm>().on_stage_released(move || {
            let Some(ui) = weak.upgrade() else { return };
            hold.stop();
            let mut review = review_rc.borrow_mut();
            let Some(gesture) = review.gesture.take() else {
                return;
            };
            // The centre hold's x2 lasts exactly as long as the button is
            // down: give the rate back and take the band down with it
            // (task650), rather than leaving it to the hide timer.
            if let Some(rate) = gesture.restore_rate {
                review.rate = rate;
                review.send(PlaybackCommand::SetRate(rate));
                review.stage_feedback = None;
                render_rate(&ui, &review);
            }
            // Started paused: hand back a paused video, at whatever
            // position the skim reached (task1010).
            if gesture.repause {
                review.playing = false;
                review.send(PlaybackCommand::Pause);
            }
            // Any side release -- tap or hold -- books the settle confirm
            // (task2580): the coarse seeks left a nearby clean frame, and
            // one precise decode lands the exact one once the flurry stops.
            if gesture.zone != pb::StageZone::Center {
                review.arm_settle(&review_rc);
            }
            // A hold already did the seeking; releasing it must not also
            // fire the tap.
            if !gesture.held {
                match gesture.zone {
                    pb::StageZone::Center => {
                        // Arms the double-click pairing: a second press
                        // inside the window undoes this and goes full
                        // screen instead (task1010). The position rides
                        // along so that undo can put the playhead back
                        // where this tap found it (task3270) -- read here,
                        // before the toggle that may jump it to the
                        // selection's start.
                        review.last_center_tap = Some((std::time::Instant::now(), review.position));
                        review.toggle_play();
                        review.stage_feedback = None;
                    }
                    zone => {
                        let delta = zone.direction() * pb::STAGE_TAP_SECONDS;
                        let target = review.position + pb::tap_delta_100ns(zone);
                        // Coarse (task2580): each tap of a flurry moves the
                        // picture in a few ms, and the settle armed above
                        // decodes the exact frame once the taps stop.
                        let outcome = review.seek_clamped(target, false);
                        let previous = review.stage_feedback;
                        review.stage_feedback = Some(pb::fold_tap(previous, outcome, delta));
                    }
                }
            }
            render_stage_feedback(&ui, &review);
            drop(review);
            hide_feedback_after(&ui, &review_rc);
        });
    }
}

/// The range panel's rows and the marker list beside them.
#[allow(clippy::too_many_arguments)]
fn wire_panel_rows(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    review_segments: &Rc<VecModel<TrackSpan>>,
    review_gaps: &Rc<VecModel<GapSpan>>,
    review_markers: &Rc<VecModel<MarkerSpan>>,
    review_marker_rows: &Rc<VecModel<MarkerRow>>,
    settings: &Arc<Mutex<AppSettings>>,
    exporter: &ExportController,
    status_line: &Rc<RefCell<livia::ui_state::toast::Toast>>,
    hold_timer: &Rc<slint::Timer>,
) {
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let exporter = exporter.clone();
        ui.global::<ReviewVm>().on_export_start(move || {
            let Some(ui) = weak.upgrade() else { return };
            let current = settings_snapshot(&settings);
            let job = {
                let review = review_rc.borrow();
                review
                    .snapshot
                    .as_ref()
                    .zip(review.range)
                    .map(|(snapshot, (start, end))| {
                        (
                            snapshot.session_id.clone(),
                            snapshot.target_title.clone().unwrap_or_default(),
                            start,
                            end,
                        )
                    })
            };
            let Some((session_id, game_name, start, end)) = job else {
                return;
            };
            // Where and what to call it is the user's answer now
            // (task1050), not a folder buried in the settings screen. The
            // dialog opens on the last place they saved, with the name the
            // automatic route would have picked -- so pressing 保存 with
            // nothing changed is exactly the old behaviour. Modal to the
            // app window and blocking on the event loop, like the folder
            // picker the settings screen used to have.
            let directory = current
                .export_output_directory
                .as_ref()
                .map(PathBuf::from)
                .or_else(|| livia::export::resolve_export_directory(None).ok());
            let mut dialog = rfd::FileDialog::new()
                .set_parent(&ui.window().window_handle())
                .set_file_name(livia::export::suggested_export_filename(&game_name))
                .add_filter("MP4", &["mp4"]);
            if let Some(directory) = directory {
                // The folder has to exist for the dialog to open in it; a
                // first-ever export has never had one created.
                let _ = std::fs::create_dir_all(&directory);
                dialog = dialog.set_directory(&directory);
            }
            // Cancelled: nothing was asked for, so nothing happens -- not
            // even a message.
            let Some(chosen) = dialog.save_file() else {
                return;
            };
            // Remembered as soon as it is chosen rather than on success:
            // an export that fails still tells us where the user works.
            if let Some(parent) = chosen.parent() {
                commit_settings(&settings, |current| {
                    current.export_output_directory = Some(parent.to_string_lossy().into_owned());
                });
            }
            let started = exporter.start(ExportRequest {
                session_id,
                start_100ns: start,
                end_100ns: end,
                game_name,
                output_file: Some(chosen),
            });
            let mut review = review_rc.borrow_mut();
            match started {
                // The pipeline publishes `Running` through the sink, so
                // there is nothing to write here on success.
                Ok(_) => review.status = None,
                Err(error) => review.status = Some(error),
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    wire_clip_save(
        ui,
        review,
        review_segments,
        review_gaps,
        review_markers,
        review_marker_rows,
        settings,
        exporter,
    );

    wire_export_doors(
        ui,
        review,
        review_segments,
        review_gaps,
        review_markers,
        review_marker_rows,
        settings,
        exporter,
        status_line,
        hold_timer,
    );
}

/// 「クリップに保存」 (task2970): the primary of the export pair.
///
/// The one thing that makes it feel unlike the row below is that it opens no
/// file dialog -- the button turns into a comment field in place, and Enter
/// both closes it and starts the job straight into the clip folder.
#[allow(clippy::too_many_arguments)]
fn wire_clip_save(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    review_segments: &Rc<VecModel<TrackSpan>>,
    review_gaps: &Rc<VecModel<GapSpan>>,
    review_markers: &Rc<VecModel<MarkerSpan>>,
    review_marker_rows: &Rc<VecModel<MarkerRow>>,
    settings: &Arc<Mutex<AppSettings>>,
    exporter: &ExportController,
) {
    // Opening and cancelling the field are the same one-flag write. Esc
    // discards by construction: the draft only ever lived in the element that
    // is about to go away.
    //
    // task3860: closing it is no longer *only* that write. The 確定ステップ is
    // the mode the range is editable in now, so ending it also ends whatever
    // band gesture was in flight -- `tl::end_confirm_step` carries both that
    // and the rule that the range itself is kept exactly where the drag left
    // it (round19 §1). The committed path below applies the same function, so
    // Enter and Esc cannot leave different state behind.
    let toggle = |open: bool| {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            review.clip_editing = open;
            if !open {
                let ended = tl::end_confirm_step(review.range);
                review.range = ended.range;
                review.range_dragging = ended.range_dragging;
                review.resume_after_range_drag = ended.resume_after_range_drag;
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        }
    };
    ui.global::<ReviewVm>().on_clip_requested(toggle(true));
    ui.global::<ReviewVm>().on_clip_cancelled(toggle(false));

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let seg = review_segments.clone();
        let gap = review_gaps.clone();
        let mark = review_markers.clone();
        let rows = review_marker_rows.clone();
        let settings = settings.clone();
        let exporter = exporter.clone();
        ui.global::<ReviewVm>().on_clip_committed(move |comment| {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            // Enter is the only exit that reaches here since task3430, but the
            // guard stays: `clip-committed` can still arrive with the field
            // already gone -- an unload, or the export that just covered the
            // panel. Without this an abandoned draft would start a job of its
            // own.
            let accepted = ex::clip_commit_accepted(
                review.clip_editing,
                review.range.is_some(),
                review.export_status.as_ref(),
            );
            review.clip_editing = false;
            // task3860: the other exit from the step, and it has to leave the
            // same state as the cancel above -- including on the `!accepted`
            // path, where the field was already gone but a band gesture could
            // still be latched.
            let ended = tl::end_confirm_step(review.range);
            review.range = ended.range;
            review.range_dragging = ended.range_dragging;
            review.resume_after_range_drag = ended.resume_after_range_drag;
            if !accepted {
                return;
            }
            let current = settings_snapshot(&settings);
            let job = review
                .snapshot
                .as_ref()
                .zip(review.range)
                .map(|(snapshot, (start, end))| {
                    (
                        snapshot.session_id.clone(),
                        snapshot.target_title.clone().unwrap_or_default(),
                        start,
                        end,
                    )
                });
            let Some((session_id, game_name, start, end)) = job else {
                return;
            };
            match livia::clips::clip_directory(&current) {
                Ok(directory) => {
                    // Created here rather than left to the pipeline: it
                    // refuses an output path whose parent does not exist, and
                    // `Videos\Liveback\Clips` has two levels that may never
                    // have been made.
                    let _ = std::fs::create_dir_all(&directory);
                    let path = livia::clips::allocate_clip_path(&directory, &game_name);
                    let started = exporter.start(ExportRequest {
                        session_id,
                        start_100ns: start,
                        end_100ns: end,
                        game_name,
                        // No dialog, so no dialog to ask about overwriting --
                        // the path above is already free.
                        output_file: Some(path),
                    });
                    match started {
                        // Held against the job id, not as a loose "last
                        // comment": the next 指定フォルダに書き出し must not
                        // inherit it. `export_output_directory` is deliberately
                        // left alone -- a clip is unrelated to where the
                        // dialog last pointed.
                        Ok(job_id) => {
                            review.status = None;
                            review.clip_pending = Some((job_id, comment.trim().to_owned()));
                        }
                        Err(error) => review.status = Some(error),
                    }
                }
                Err(error) => review.status = Some(error),
            }
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }
}
