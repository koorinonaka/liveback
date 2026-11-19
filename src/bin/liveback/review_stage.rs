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
use livia::ui_state::export as ex;
use livia::ui_state::playback as pb;
use slint::ComponentHandle;

use super::review::{
    hide_feedback_after, pump_playback, redraw_paused_stage, render_edge, render_rate,
    render_review, render_stage_feedback, reveal_in_directory, save_current_frame, Review,
};
use super::review_wiring::{review_handler, HandlerCtx};
use super::settings_page::{commit_settings, settings_snapshot};
use super::tr_locale;
use super::{AppWindow, ReviewVm};

type ToastLine = Rc<RefCell<livia::ui_state::toast::Toast>>;

/// slint's D3D12 device, for drawing the playback picture without taking it
/// off the GPU (t260917-66c2). `None` until `RenderingSetup` hands one over --
/// never, under `LIVEBACK_RENDERER=software` -- and `broken` once opening a
/// picture failed; either way the engine is told to read back instead.
#[derive(Default)]
struct GpuStage {
    device: Option<StageDevice>,
    broken: bool,
    /// One slint draw, `BeforeRendering` to `AfterRendering`, as the
    /// `ui_draw` insight scope (t261101-11dd). Two callbacks, so the span
    /// waits here between them.
    #[cfg(feature = "insight")]
    draw: Option<tracing::span::EnteredSpan>,
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
            #[cfg(feature = "insight")]
            (slint::RenderingState::BeforeRendering, _) => {
                let span =
                    tracing::debug_span!(target: livia::insight::TARGET, "ui_draw").entered();
                GPU_STAGE.with_borrow_mut(|stage| stage.draw = Some(span));
            }
            #[cfg(feature = "insight")]
            (slint::RenderingState::AfterRendering, _) => {
                GPU_STAGE.with_borrow_mut(|stage| stage.draw.take());
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

/// Wires the stage and export handlers.
pub(super) fn wire(
    ctx: HandlerCtx<'_>,
    // Both built in `main` since task130; the sink carries the tray handle.
    (exporter, export_slot): (&ExportController, &Arc<Mutex<Option<ExportStatus>>>),
) {
    let HandlerCtx { ui, review, .. } = ctx;
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
        // asked for its rate instead. This is only the default until the
        // first open: `review::load_review` re-reads it from the window's own
        // monitor, which has no HWND to ask yet at wiring time.
        let hz = super::desktop::display_refresh_hz(ui.window());
        review.borrow_mut().stage_gate = pb::StageGate::for_refresh_hz(hz);
    }
    wire_stage_size(ctx);
    wire_frame_pump(ctx, &hint_timer);
    wire_stage_press(ctx, &hold_timer);
    wire_stage_release(ctx, &hold_timer);
    wire_export_status(ctx, export_slot);
    wire_export(ctx, exporter);
    wire_clip_save(ctx, exporter);
    wire_screenshot(ctx);
}

/// The size the stage draws the picture at, and its rescale on a monitor
/// change (task128 / task990).
fn wire_stage_size(ctx: HandlerCtx<'_>) {
    let HandlerCtx { ui, review, .. } = ctx;
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
}

/// The frame pump, and the first-run hint it raises once playback starts.
fn wire_frame_pump(ctx: HandlerCtx<'_>, hint_timer: &Rc<slint::Timer>) {
    let HandlerCtx {
        ui,
        review,
        models,
        settings,
        ..
    } = ctx;
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let settings = settings.clone();
        let hint = hint_timer.clone();
        let models = models.clone();
        ui.global::<ReviewVm>().on_frame_ready(move || {
            let Some(ui) = weak.upgrade() else { return };
            let mut review = review_rc.borrow_mut();
            let started = pump_playback(&ui, &mut review);
            // t261026-e5c3: the moving edge and the nav lamp are written with
            // the frame, so they ride its present. From their own timers each
            // was a present of its own: two or three unsynchronised 60/s
            // writers presented 113 times a second on a 280Hz monitor.
            if review.edge_runs() {
                render_edge(&ui, &mut review, &models);
                review.edge_pumped_at = Some(std::time::Instant::now());
            }
            super::shell::ride_tally(&ui);
            if !started || review.hint_counted {
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
}

/// The export pipeline's progress, copied onto the UI thread (task129).
fn wire_export_status(ctx: HandlerCtx<'_>, export_slot: &Arc<Mutex<Option<ExportStatus>>>) {
    // The sink already folded the event into `export_slot`; this only
    // copies it onto the UI thread. No polling: unlike the WebView
    // build there is no event bus that can silently drop a packet.
    let (slot, line) = (export_slot.clone(), ctx.status_line.clone());
    review_handler!(ctx, on_export_changed, |ui, review| {
        let next = slot.lock().ok().and_then(|slot| slot.clone());
        if let Some(next) = next.as_ref() {
            // Read before `clip_pending` is let go below (t260928-bae5):
            // what the field gets back, and whether the clip job failed.
            let comment_after =
                ex::clip_comment_after(review.clip_pending.as_ref(), next).map(str::to_owned);
            let clip_job_failed = ex::is_clip_failure(review.clip_pending.as_ref(), next);
            let (clip_finished, comment_failed) = write_clip_metadata(&review, next);
            // Whichever way the job ended, it is no longer pending: a
            // failure or a cancel has nothing to write into.
            if !ex::is_busy(Some(next)) {
                review.clip_pending.take();
            }
            if let Some(comment) = comment_after {
                ui.global::<ReviewVm>().set_clip_comment(comment.into());
            }
            let clip = ClipOutcome {
                finished: clip_finished,
                comment_failed,
                job_failed: clip_job_failed,
            };
            tell_export_outcome(&ui, &line, &review, next, clip);
        }
        review.export_status = next;
    });
}

/// Task2970: a finished clip's app path and comment, written into its files
/// -- the pipeline knows nothing about comments, only about the file it
/// wrote. Returns whether a clip finished and whether its comment failed.
fn write_clip_metadata(review: &Review, next: &ExportStatus) -> (bool, bool) {
    let clip = ex::clip_finish(review.clip_pending.as_ref(), next);
    let mut failed = false;
    // Before the comment, and quietly: a clip without it only
    // falls back to the monogram, nothing the user must hear.
    if let Some((clip, app)) = clip.as_ref().zip(review.clip_app_path.as_deref()) {
        for path in clip.paths {
            if let Err(error) = livia::clips::write_app_path(path, app) {
                let error = error.replace(&path.display().to_string(), "<clip>");
                tracing::warn!(%error, "clip app path");
            }
        }
    }
    // On the UI thread deliberately: task2960 measured `Commit`
    // under a millisecond for this app's own moov-last output
    // at both 900 bytes and 0.94GB, and the property handler's
    // one-off ~45ms load is paid once per process. Every part
    // of a gap-spanning selection gets the same comment -- it
    // is one clip.
    if let Some((clip, comment)) = clip.as_ref().and_then(|clip| Some((clip, clip.comment?))) {
        for path in clip.paths {
            if let Err(error) = livia::clips::write_comment(path, comment) {
                // The toast's missing-file detail names the path; the log must not.
                let (title, detail) =
                    livia::ui_state::clips::comment_write_error_text(tr_locale(), &error);
                let detail = detail.replace(&path.display().to_string(), "<clip>");
                tracing::warn!(title = %title, %detail, "clip comment");
                failed = true;
            }
        }
    }
    (clip.is_some(), failed)
}

/// How the clip side of a job ended, for the toast's title.
#[derive(Clone, Copy)]
struct ClipOutcome {
    finished: bool,
    comment_failed: bool,
    job_failed: bool,
}

/// A job's end, told once: to Windows or to the corner.
fn tell_export_outcome(
    ui: &AppWindow,
    line: &ToastLine,
    review: &Review,
    next: &ExportStatus,
    clip: ClipOutcome,
) {
    // The video is on disk either way, so a comment that could not
    // be written is a warning over the clip's own success line,
    // not a failure.
    let clip_title =
        |default: &'static str| match (clip.comment_failed, clip.finished, clip.job_failed) {
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
    let completion = ex::completion_toast(tr_locale(), review.export_status.as_ref(), next);
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
        let variant = if clip.comment_failed {
            livia::ui_state::toast::ToastVariant::Warning
        } else {
            corner.variant
        };
        super::publish_toast(
            ui,
            line,
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

/// The stage's press: the double press to full screen, and the holds it arms.
fn wire_stage_press(ctx: HandlerCtx<'_>, hold_timer: &Rc<slint::Timer>) {
    let HandlerCtx { ui, review, .. } = ctx;
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
                // The second press of a double click goes full screen now rather
                // than on slint's release-time `double-clicked`, and undoes the
                // tap that opened the pair -- playhead included (task3270).
                let effects = {
                    let mut review = review_rc.borrow_mut();
                    let last_tap = review.last_center_tap.take();
                    let (gesture, effects) =
                        pb::stage_press(last_tap, zone, std::time::Instant::now());
                    review.gesture = gesture;
                    effects
                };
                apply_stage_effects(&ui, &review_rc, &hold, effects);
            });
    }
}

/// The hold timer's first shot: the centre's x2 (task650) or a side's first
/// seek step, which then re-arms the timer as the repeat.
fn arm_hold(
    ui: &AppWindow,
    review_rc: &Rc<RefCell<Review>>,
    hold: &Rc<slint::Timer>,
    delay: Duration,
) {
    let weak = ui.as_weak();
    let review_rc = review_rc.clone();
    let timer = hold.clone();
    hold.start(slint::TimerMode::SingleShot, delay, move || {
        let Some(ui) = weak.upgrade() else { return };
        let effects = {
            let mut review = review_rc.borrow_mut();
            let (playing, rate, position) = (review.playing, review.rate, review.position);
            match review.gesture.as_mut() {
                Some(gesture) => gesture.hold_fired(playing, rate, position),
                None => return,
            }
        };
        apply_stage_effects(&ui, &review_rc, &timer, effects);
    });
}

/// A side hold's repeat. Each step reads the engine's real position.
fn arm_repeat(
    ui: &AppWindow,
    review_rc: &Rc<RefCell<Review>>,
    hold: &Rc<slint::Timer>,
    interval: Duration,
) {
    let weak = ui.as_weak();
    let review_rc = review_rc.clone();
    let timer = hold.clone();
    hold.start(slint::TimerMode::Repeated, interval, move || {
        let Some(ui) = weak.upgrade() else { return };
        let effects = {
            let mut review = review_rc.borrow_mut();
            let position = review.position;
            match review.gesture.as_mut() {
                Some(gesture) => gesture.hold_step(position),
                None => return,
            }
        };
        apply_stage_effects(&ui, &review_rc, &timer, effects);
    });
}

/// Runs a stage transition's effects in order, then paints what they changed.
/// Full screen and the band's exit wait until the review is no longer
/// borrowed: both reach code that borrows it again.
fn apply_stage_effects(
    ui: &AppWindow,
    review_rc: &Rc<RefCell<Review>>,
    hold: &Rc<slint::Timer>,
    effects: Vec<pb::StageEffect>,
) {
    use pb::StageEffect as E;
    let (mut rate, mut feedback, mut exit, mut fullscreen) = (false, false, false, false);
    {
        let mut review = review_rc.borrow_mut();
        for effect in effects {
            match effect {
                E::ArmHold(delay) => arm_hold(ui, review_rc, hold, delay),
                E::ArmRepeat(interval) => arm_repeat(ui, review_rc, hold, interval),
                E::StopHold => hold.stop(),
                E::SetRate(value) => {
                    review.rate = value;
                    review.send(PlaybackCommand::SetRate(value));
                    rate = true;
                }
                E::Play => {
                    review.playing = true;
                    review.send(PlaybackCommand::Play);
                }
                E::Pause { reason } => {
                    review.playing = false;
                    review.send(PlaybackCommand::Pause { reason });
                }
                E::TogglePlay => review.toggle_play(),
                E::RememberCenterTap(at) => {
                    review.last_center_tap = Some((at, review.position));
                }
                E::RestoreBeforeTap(saved) => {
                    if let Some(target) =
                        pb::restore_after_double_press(review.playing, saved, review.position)
                    {
                        review.seek_clamped(target, true);
                    }
                }
                E::ToggleFullscreen => fullscreen = true,
                E::StopSettle => review.key_settle_timer.stop(),
                E::ArmSettle => review.arm_settle(review_rc),
                E::StopFeedbackExit => {
                    review.feedback_timer.stop();
                    review.feedback_fade_timer.stop();
                }
                E::Feedback(value) => {
                    review.stage_feedback = value;
                    feedback = true;
                }
                E::Seek { target, fold } => {
                    // Coarse (task2580): the settle decodes the exact frame
                    // once the flurry stops.
                    let outcome = review.seek_clamped(target, false);
                    review.stage_feedback = Some(fold.apply(review.stage_feedback, outcome));
                    feedback = true;
                }
                E::BookFeedbackExit => {
                    exit = true;
                    feedback = true;
                }
            }
        }
        if rate {
            render_rate(ui, &review);
        }
        if feedback {
            render_stage_feedback(ui, &review);
        }
    }
    if fullscreen {
        ui.global::<ReviewVm>().invoke_fullscreen_toggled();
    }
    if exit {
        hide_feedback_after(ui, review_rc);
    }
}

/// Export: start (through the save dialog), cancel, and the toast's reveal.
fn wire_export(ctx: HandlerCtx<'_>, exporter: &ExportController) {
    {
        let (exporter, line) = (exporter.clone(), ctx.status_line.clone());
        review_handler!(ctx, on_export_cancel, |ui, review| {
            // An error toast, not the notice card that had no close button
            // (t260928-bae5 F5). `status` stays for the control port's log.
            if let Err(failure) = exporter.cancel() {
                let error = livia::ui_state::export_failure::text(&failure, tr_locale());
                super::publish_error(&ui, &line, ex::export_abort_failed(tr_locale()), &error);
                review.status = Some(error);
            }
        });
    }

    {
        // t260923-916e: the shell call runs off the UI thread, and a failure
        // is a closable error toast rather than the notice card, which had no
        // way to close.
        let weak = ctx.ui.as_weak();
        let line = ctx.status_line.clone();
        ctx.ui.global::<ReviewVm>().on_export_reveal(move |path| {
            let path = PathBuf::from(path.as_str());
            let line = line.clone();
            super::desktop::open_off_thread(
                &weak,
                move || reveal_in_directory(&path),
                move |ui| {
                    super::publish_error(ui, &line, ex::reveal_error(tr_locale()), "");
                },
            );
        });
    }

    wire_export_start(ctx, exporter);
}

/// The screenshot door (the overlay button and Ctrl+S).
fn wire_screenshot(ctx: HandlerCtx<'_>) {
    let HandlerCtx {
        ui,
        review,
        status_line,
        ..
    } = ctx;
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
}

/// The stage's release -- the tap, or the end of a hold -- and its cancel.
fn wire_stage_release(ctx: HandlerCtx<'_>, hold_timer: &Rc<slint::Timer>) {
    let HandlerCtx { ui, review, .. } = ctx;
    {
        // Cancelled means the browser/compositor abandoned the gesture,
        // so its coordinate names no position the user chose: tear down
        // without acting.
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let hold = hold_timer.clone();
        ui.global::<ReviewVm>().on_stage_cancelled(move || {
            hold.stop();
            let Some(ui) = weak.upgrade() else { return };
            let Some(gesture) = review_rc.borrow_mut().gesture.take() else {
                return;
            };
            // An abandoned gesture still owes back whatever it borrowed:
            // without this the stage stays stuck at x2 (task650), or
            // playing after a paused skim (task1010).
            apply_stage_effects(&ui, &review_rc, &hold, gesture.cancel());
        });
    }

    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let hold = hold_timer.clone();
        ui.global::<ReviewVm>().on_stage_released(move || {
            let Some(ui) = weak.upgrade() else { return };
            hold.stop();
            let effects = {
                let mut review = review_rc.borrow_mut();
                let Some(gesture) = review.gesture.take() else {
                    return;
                };
                gesture.release(review.position)
            };
            apply_stage_effects(&ui, &review_rc, &hold, effects);
        });
    }
}

/// 「書き出し…」: the save dialog, then the job.
///
/// Unrolled rather than `review_handler!`: the dialog is modal and runs its
/// own message loop, so the review state must not be borrowed across it --
/// the frame pump and every other handler still fire underneath.
fn wire_export_start(ctx: HandlerCtx<'_>, exporter: &ExportController) {
    let HandlerCtx {
        ui,
        review,
        models,
        settings,
        status_line,
    } = ctx;
    {
        let weak = ui.as_weak();
        let review_rc = review.clone();
        let models = models.clone();
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
                output_directory: None,
                audio_mix,
            });
            let mut review = review_rc.borrow_mut();
            match started {
                // The pipeline publishes `Running` through the sink, so
                // there is nothing to write here on success.
                Ok(_) => review.status = None,
                Err(failure) => {
                    let error = livia::ui_state::export_failure::text(&failure, tr_locale());
                    super::publish_error(&ui, &line, ex::notify_failed(tr_locale()), &error);
                    review.status = Some(error);
                }
            }
            let current = settings_snapshot(&settings);
            render_review(&ui, &mut review, &models, &current);
        });
    }
}

/// A clip that could not be started: an error toast with the reason
/// (t260928-bae5 F5) -- it used to go to the notice card, which could not be
/// closed. `status` keeps it for the control port's log line.
fn clip_refused(ui: &AppWindow, line: &ToastLine, review: &mut Review, error: String) {
    super::publish_error(ui, line, ex::clip_failed(tr_locale()), &error);
    review.status = Some(error);
}

/// 「クリップに保存」 (task2970): the export tab's primary.
///
/// It opens no file dialog: the job goes straight into the clip folder with
/// the comment field's text. Since t260927-7d08 the field is always there
/// (DS ReviewScreen), so there is no 確定ステップ to open or close around it --
/// the button, Enter in the field and Enter on the pane all arrive here with
/// the field's text.
fn wire_clip_save(ctx: HandlerCtx<'_>, exporter: &ExportController) {
    {
        let (exporter, line, store) = (
            exporter.clone(),
            ctx.status_line.clone(),
            ctx.settings.clone(),
        );
        review_handler!(ctx, on_clip_committed, |ui, review, comment| {
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
            let current = settings_snapshot(&store);
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
            // The history list's rule, so a clip cut from a recording older than
            // the manifest's path still names a registered app's exe.
            let app_path = review.snapshot.as_ref().and_then(|snapshot| {
                super::app_meta::recorded_icon_path(
                    snapshot.target_executable.as_deref(),
                    snapshot.target_executable_path.as_deref(),
                    &current.auto_capture_executables,
                )
            });
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
                        output_directory: None,
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
                            review.clip_app_path = app_path;
                        }
                        Err(failure) => clip_refused(
                            &ui,
                            &line,
                            &mut review,
                            livia::ui_state::export_failure::text(&failure, tr_locale()),
                        ),
                    }
                }
                Err(error) => clip_refused(&ui, &line, &mut review, error),
            }
        });
    }
}
