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
use livia::playback::{PlaybackCommand, SharedPicture, StageDevice};
use livia::settings::AppSettings;
use livia::ui_state::export as ex;
use livia::ui_state::playback as pb;
use slint::{ComponentHandle, VecModel};

use super::review::{
    hide_feedback_after, pump_playback, redraw_paused_stage, render_rate, render_review,
    render_stage_feedback, reveal_in_directory, save_current_frame, Review, StageGesture,
};
use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{AppWindow, GapSpan, MarkerRow, MarkerSpan, ReviewVm, TrackSpan};

/// slint's D3D12 device, for drawing the playback picture without taking it
/// off the GPU (t260917-66c2). `None` until `RenderingSetup` hands one over --
/// never, under `LIVEBACK_RENDERER=software` -- and `broken` once opening a
/// picture failed; either way the engine is told to read back instead.
#[derive(Default)]
struct GpuStage {
    device: Option<StageDevice>,
    broken: bool,
}

thread_local! {
    static GPU_STAGE: RefCell<GpuStage> = RefCell::default();
}

/// Takes slint's device and queue as the renderer comes up. The window's only
/// rendering notifier.
///
/// Only on the wgpu renderer: a registered notifier turns off skia's partial
/// rendering (`i-slint-renderer-skia-1.18.0/lib.rs`, `partial_rendering_state`),
/// which the software surface uses and wgpu's does not -- measured under
/// `LIVEBACK_RENDERER=software`, 38.3 fps with no notifier against 31.9 with one.
pub(super) fn install_gpu_stage(ui: &AppWindow) {
    let installed = ui
        .window()
        .set_rendering_notifier(|state, api| match (state, api) {
            (
                slint::RenderingState::RenderingSetup,
                slint::GraphicsAPI::WGPU30 { device, queue, .. },
            ) => {
                let device = StageDevice::new(device, queue);
                tracing::info!(
                    target: "t260917_66c2_stage",
                    opened = device.is_some(),
                    "the stage can take the playback picture on the GPU"
                );
                GPU_STAGE.with_borrow_mut(|stage| stage.device = device);
            }
            (slint::RenderingState::RenderingTeardown, _) => {
                GPU_STAGE.with_borrow_mut(|stage| stage.device = None);
            }
            _ => {}
        });
    if let Err(error) = installed {
        tracing::info!(target: "t260917_66c2_stage", ?error, "no rendering notifier");
    }
}

/// Whether the engine should leave the stage picture on the GPU.
pub(super) fn gpu_stage_available() -> bool {
    GPU_STAGE.with_borrow(|stage| stage.device.is_some() && !stage.broken)
}

/// The picture as a slint image, or `None` -- after which
/// [`gpu_stage_available`] says no and the engine reads back again.
pub(super) fn gpu_stage_image(picture: &SharedPicture) -> Option<slint::Image> {
    livia::insight_scope!("ui_gpu_stage_image");
    GPU_STAGE.with_borrow_mut(|stage| {
        let device = stage.device.as_mut()?;
        let image = device
            .texture(picture)
            .map_err(|error| error.to_string())
            .and_then(|texture| slint::Image::try_from(texture).map_err(|error| format!("{error:?}")));
        match image {
            Ok(image) => Some(image),
            Err(error) => {
                stage.broken = true;
                tracing::warn!(
                    target: "t260917_66c2_stage",
                    %error,
                    "the playback picture could not be opened on slint's device; reading back from now on"
                );
                None
            }
        }
    })
}

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
        //
        // The last logical size is kept so a move to a monitor at another
        // scale, which leaves it unchanged and so never rings
        // `stage-size-changed`, can recompute the target at the new factor
        // (`stage-rescaled`, rung from the winit filter; t260918-b743).
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let last_size = Rc::new(std::cell::Cell::new((0.0, 0.0)));
        let restage = {
            let last_size = last_size.clone();
            Rc::new(move |width: f32, height: f32| {
                last_size.set((width, height));
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
            })
        };
        let vm = ui.global::<ReviewVm>();
        let on_size = restage.clone();
        vm.on_stage_size_changed(move |width, height| on_size(width, height));
        vm.on_stage_rescaled(move || {
            let (width, height) = last_size.get();
            restage(width, height);
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
            // The first session only, and only once playback has
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
                // Read before `clip_pending` is let go below (t260928-bae5):
                // what the field gets back, and whether the clip job failed.
                let comment_after =
                    ex::clip_comment_after(review.clip_pending.as_ref(), next).map(str::to_owned);
                let clip_job_failed = ex::is_clip_failure(review.clip_pending.as_ref(), next);
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
                if let Some(comment) = comment_after {
                    ui.global::<ReviewVm>().set_clip_comment(comment.into());
                }
                // The video is on disk either way, so a comment that could not
                // be written is a warning over the clip's own success line,
                // not a failure.
                let clip_title = |default: &'static str| match (
                    comment_failed,
                    clip_finished,
                    clip_job_failed,
                ) {
                    (true, _, _) => ex::clip_comment_failed(tr_locale()),
                    (false, true, _) => ex::clip_saved(tr_locale()),
                    (false, false, true) => ex::clip_failed(tr_locale()),
                    (false, false, false) => default,
                };

                // `notifyIfHidden`, ported (task130): a finished job goes to
                // Windows when the user is looking elsewhere, and to the
                // corner when they are not -- one or the other, never both
                // (t260928-e309 F8, `lifecycle::notice_sinks`), so coming back
                // to the window finds no second copy.
                let sinks = livia::ui_state::lifecycle::notice_sinks(
                    super::shell::hwnd_of(ui.window()).is_some_and(super::desktop::is_foreground),
                );
                let completion =
                    ex::completion_toast(tr_locale(), review.export_status.as_ref(), next);
                let told_windows = sinks.os && completion.is_some();
                if let (true, Some((title, body))) = (sinks.os, completion) {
                    super::desktop::toast(clip_title(title), &body);
                }
                // Task169: the outcome used to be a card with no expiry in
                // the middle of the screen. A cancel has no Windows toast, so
                // it stays in the corner either way.
                if let (false, Some(corner)) = (
                    told_windows,
                    ex::outcome_toast(tr_locale(), review.export_status.as_ref(), next),
                ) {
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
                            let mut line = line.borrow_mut();
                            line.set_action_path(&path.to_string_lossy());
                            // The detail is that path's tail: one mono line.
                            line.set_detail_mono();
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
                            // t260929-40a4: the exit an earlier tap booked
                            // (dwell, then fade) is still running, and would
                            // take the band down in the middle of the hold.
                            review.feedback_timer.stop();
                            review.feedback_fade_timer.stop();
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
                            // t260929-40a4: same as the centre hold -- an
                            // earlier tap's pending exit must not blank a step.
                            review.feedback_timer.stop();
                            review.feedback_fade_timer.stop();
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
        let line = status_line.clone();
        ui.global::<ReviewVm>().on_export_cancel(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            // An error toast, not the notice card that had no close button
            // (t260928-bae5 F5). `status` stays for the control port's log.
            if let Err(error) = exporter.cancel() {
                super::publish_toast(
                    &ui,
                    &line,
                    ex::export_abort_failed(tr_locale()),
                    &error,
                    livia::ui_state::toast::ToastVariant::Error,
                    "",
                );
                review.status = Some(error);
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }

    {
        // t260923-916e: the shell call runs off the UI thread, and a failure
        // is a closable error toast rather than the notice card, which had no
        // way to close.
        let weak = ui.as_weak();
        let line = status_line.clone();
        ui.global::<ReviewVm>().on_export_reveal(move |path| {
            let path = PathBuf::from(path.as_str());
            let weak = weak.clone();
            let line = line.clone();
            super::desktop::off_ui_thread(
                move || reveal_in_directory(&path),
                move |result| {
                    let Some(ui) = weak.upgrade() else { return };
                    if result.is_err() {
                        super::publish_toast(
                            &ui,
                            &line,
                            ex::reveal_error(tr_locale()),
                            "",
                            livia::ui_state::toast::ToastVariant::Error,
                            "",
                        );
                    }
                },
            );
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
                let mut line = line.borrow_mut();
                line.set_action_path(&path.to_string_lossy());
                // The detail is the file's name: one mono line.
                line.set_detail_mono();
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
                review.send(PlaybackCommand::Pause {
                    reason: "gesture_abandoned",
                });
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
                review.send(PlaybackCommand::Pause {
                    reason: "skim_release",
                });
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
        let line = status_line.clone();
        ui.global::<ReviewVm>().on_export_start(move || {
            let Some(ui) = weak.upgrade() else { return };
            let current = settings_snapshot(&settings);
            let job = {
                let review = review_rc.borrow();
                let key = super::review::reviewed_audio_key(&review);
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
                            key,
                        )
                    })
            };
            let Some((session_id, game_name, start, end, key)) = job else {
                return;
            };
            // The saved per-track levels, read now: the mix uses what the
            // volume tab says at the moment the export is asked for
            // (t261003-b713, decision 13).
            let audio_mix = livia::export::AudioMix {
                levels: livia::export::track_levels(
                    &current,
                    key.as_deref(),
                    &exporter.session_audio_tracks(&session_id),
                ),
                mix_only: false,
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
                audio_mix,
            });
            let mut review = review_rc.borrow_mut();
            match started {
                // The pipeline publishes `Running` through the sink, so
                // there is nothing to write here on success.
                Ok(_) => review.status = None,
                Err(error) => {
                    super::publish_toast(
                        &ui,
                        &line,
                        ex::notify_failed(tr_locale()),
                        &error,
                        livia::ui_state::toast::ToastVariant::Error,
                        "",
                    );
                    review.status = Some(error);
                }
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
        status_line,
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

/// A clip that could not be started: an error toast with the reason
/// (t260928-bae5 F5) -- it used to go to the notice card, which could not be
/// closed. `status` keeps it for the control port's log line.
fn clip_refused(
    ui: &AppWindow,
    line: &Rc<RefCell<livia::ui_state::toast::Toast>>,
    review: &mut Review,
    error: String,
) {
    super::publish_toast(
        ui,
        line,
        ex::clip_failed(tr_locale()),
        &error,
        livia::ui_state::toast::ToastVariant::Error,
        "",
    );
    review.status = Some(error);
}

/// 「クリップに保存」 (task2970): the export tab's primary.
///
/// It opens no file dialog: the job goes straight into the clip folder with
/// the comment field's text. Since t260927-7d08 the field is always there
/// (DS ReviewScreen), so there is no 確定ステップ to open or close around it --
/// the button, Enter in the field and Enter on the pane all arrive here with
/// the field's text.
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
    status_line: &Rc<RefCell<livia::ui_state::toast::Toast>>,
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
        let line = status_line.clone();
        ui.global::<ReviewVm>().on_clip_committed(move |comment| {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            // The field is always open now, so `editing` is always true; the
            // range and a job already running still refuse it (the .slint side
            // checks the same two, but Enter can race a job that just began).
            let accepted = ex::clip_commit_accepted(
                true,
                review.range.is_some(),
                review.export_status.as_ref(),
            );
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
            // A clip is the mix alone (t261003-b713, decision 12).
            let audio_mix = livia::export::AudioMix {
                levels: livia::export::track_levels(
                    &current,
                    super::review::reviewed_audio_key(&review).as_deref(),
                    &exporter.session_audio_tracks(&session_id),
                ),
                mix_only: true,
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
                        audio_mix,
                    });
                    match started {
                        // Held against the job id, not as a loose "last
                        // comment": the next 指定フォルダに書き出し must not
                        // inherit it. `export_output_directory` is deliberately
                        // left alone -- a clip is unrelated to where the
                        // dialog last pointed.
                        // The field keeps the comment until the job ends:
                        // cleared on completion, left for another try on a
                        // failure or a cancel (t260928-bae5 F18,
                        // `ex::clip_comment_after`).
                        Ok(job_id) => {
                            review.status = None;
                            review.clip_pending = Some((job_id, comment.trim().to_owned()));
                        }
                        Err(error) => clip_refused(&ui, &line, &mut review, error),
                    }
                }
                Err(error) => clip_refused(&ui, &line, &mut review, error),
            }
            render_review(&ui, &mut review, &seg, &gap, &mark, &rows, &current);
        });
    }
}
