//! The application. Screens live one module per task (124-129); the window
//! shell, the tray and the rest of the desktop integration are wired up in
//! `main` below.

// Prevents an additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// `#[path]` because this file is a crate root: without it, `mod x;` here would
// look for `src/bin/x.rs` beside the other binaries instead of in this
// binary's own `liveback/` directory.
#[path = "liveback/app_meta.rs"]
mod app_meta;
#[path = "liveback/clips.rs"]
mod clips;
#[path = "liveback/desktop.rs"]
mod desktop;
#[path = "liveback/history.rs"]
mod history;
#[path = "liveback/hotkeys.rs"]
mod hotkeys;
#[path = "liveback/picker.rs"]
mod picker;
#[path = "liveback/review.rs"]
mod review;
#[path = "liveback/review_stage.rs"]
mod review_stage;
#[path = "liveback/review_wiring.rs"]
mod review_wiring;
#[path = "liveback/settings_page.rs"]
mod settings_page;
#[path = "liveback/shell.rs"]
mod shell;
#[path = "liveback/update_check.rs"]
mod update_check;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use livia::capture::targets::CaptureTarget;
use livia::capture::{CaptureController, CaptureLifecycleStatus};
use livia::export::{ExportController, ExportStatus};
use livia::playback::PlaybackCommand;
use livia::ring_buffer::SessionSummary;
use livia::settings;
use livia::ui_state::auto_capture;
use livia::ui_state::lifecycle;
use livia::ui_state::playback as pb;
use livia::ui_state::sessions;
use livia::ui_state::shell as ui_shell;
use livia::ui_state::targets as ui_targets;
use livia::ui_state::toast;
use slint::{ComponentHandle, Image, Model, ModelRc, VecModel};

use self::clips::{spawn_clip_worker, ClipJob, Clips};
use self::history::{
    apply_session, live_relist_due, queue_visible_thumbnails, render_sessions,
    spawn_session_worker, History,
};
use self::picker::{apply, render, spawn_target_worker, Picker};
use self::review::{load_review, render_review, Review, SlintSink};
use self::settings_page::{settings_path, settings_snapshot};
use self::shell::{hwnd_of, persist_window_state, restore_window_state};

slint::include_modules!();

/// The history screen's static labels. Beside the other pushes rather than in
/// `history/render.rs` because that module renders per row; these are set once
/// per language.
fn push_session_labels(ui: &AppWindow) {
    let labels = ui.global::<SessionLabels>();
    labels.set_recording(sessions::status_recording(tr_locale()).into());
    labels.set_thumbnail_placeholder(sessions::thumbnail_placeholder(tr_locale()).into());
    labels.set_note_placeholder(sessions::note_placeholder(tr_locale()).into());
    labels.set_unreadable_row(sessions::unreadable_row(tr_locale()).into());
    labels.set_search_placeholder(sessions::search_placeholder(tr_locale()).into());
    labels.set_view_list(sessions::view_mode_list(tr_locale()).into());
    labels.set_view_thumbnail(sessions::view_mode_thumbnail(tr_locale()).into());
    labels.set_open_directory(sessions::open_directory(tr_locale()).into());
    labels.set_confirm_ok(sessions::discard_confirm(tr_locale()).into());
    labels.set_confirm_cancel(sessions::discard_cancel(tr_locale()).into());
}

/// Every label this file owns, pushed for the language now in force (task1150).
/// Called at startup and again whenever the language setting changes; the
/// screens' own `init_*_labels` are re-run beside it by `push_all_labels`.
fn push_shell_labels(ui: &AppWindow) {
    ui.set_rail_review_label(rail_review(tr_locale()).into());
    ui.set_rail_capture_label(rail_capture(tr_locale()).into());
    ui.set_rail_sessions_label(rail_sessions(tr_locale()).into());
    ui.set_rail_clips_label(livia::ui_state::clips::rail_clips(tr_locale()).into());
    self::clips::push_labels(ui);
    ui.set_rail_settings_label(rail_settings(tr_locale()).into());
    ui.set_search_placeholder(ui_targets::search_placeholder(tr_locale()).into());
    ui.set_clear_label(ui_targets::search_clear(tr_locale()).into());
    ui.set_stop_label(stop_label(tr_locale()).into());
    ui.set_recording_label(ui_targets::recording(tr_locale()).into());
    // round5 §11: the picker modal's own strings (task170).
    ui.set_picker_tab_labels(slint::VecModel::from_slice(&[
        ui_targets::tab_windows(tr_locale()).into(),
        ui_targets::tab_monitors(tr_locale()).into(),
    ]));
    ui.set_picker_capture_label(ui_targets::capture_action(tr_locale()).into());
    ui.set_picker_stop_label(ui_targets::stop_action(tr_locale()).into());
    // round7 §2-13: the drop overlay's two headings and the refusal's reason
    // (task870). The name it shows comes from the dragged file.
    let drop = ui.global::<DropVm>();
    drop.set_accept_title(ui_shell::drop_accept_title(tr_locale()).into());
    drop.set_reject_title(ui_shell::drop_reject_title(tr_locale()).into());
    drop.set_reject_detail(ui_shell::drop_reject_detail(tr_locale()).into());
}

/// Re-runs every label push in the app for the language now in force. The one
/// thing a language change has to do besides `set_active`: nothing in `.slint`
/// knows what language it is showing, so switching is re-pushing (task1150).
pub(crate) fn push_all_labels(ui: &AppWindow, tray: &slint::Weak<TrayIcon>) {
    push_shell_labels(ui);
    push_session_labels(ui);
    review::init_review_labels(ui);
    settings_page::push_settings_labels(ui);
    update_check::push_labels(ui);
    // The tray menu is not part of the window, so nothing above reaches it:
    // its three captions were set once at startup and stayed in the language
    // the app booted in until the next restart.
    if let Some(tray) = tray.upgrade() {
        push_tray_labels(&tray);
    }
}

/// The tray menu's captions, for the language now in force.
fn push_tray_labels(tray: &TrayIcon) {
    tray.set_open_label(tray_open(tr_locale()).into());
    // Zero, not the live count: a language switch re-pushes this and has no
    // count to hand: the 1s poll below rewrites it within the second.
    tray.set_stop_label(lifecycle::tray_stop_label(tr_locale(), 0).into());
    tray.set_quit_label(tray_quit(tr_locale()).into());
}

/// The language every label push in this binary reads (task1150).
///
/// The bin is the UI thread: it sets `locale::set_active` at startup and again
/// the moment the language setting changes, always before it re-pushes. Making
/// each of the ~150 push sites take a `Locale` parameter instead would mean
/// most of them locking the settings mutex and cloning the whole record to
/// read one string. `ui_state` keeps its explicit `Locale` arguments, which is
/// where a test can see them.
pub(crate) fn tr_locale() -> livia::ui_state::locale::Locale {
    livia::ui_state::locale::active()
}

/// What the OS says, for the `system` setting.
fn system_locale() -> livia::ui_state::locale::Locale {
    // SAFETY: a pure query with no arguments and no handles.
    let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    livia::ui_state::locale::from_langid(langid)
}

/// The language in force, from the stored setting and the OS.
pub(crate) fn resolve_locale(
    stored: &livia::settings::AppSettings,
) -> livia::ui_state::locale::Locale {
    livia::ui_state::locale::resolve(&stored.language, system_locale())
}

/// How often the UI thread drains the worker channel.
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

/// How long the exit waits for running captures to close their containers
/// (task3200). Past it the process leaves anyway and the next launch repairs
/// what is left -- see `CaptureController::shutdown` for why the wait is
/// bounded at all.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

livia::tr! {
    stop_label { ja: "録画を停止", en: "Stop recording" }
    // The rail, top to bottom (task169, round5 §3-A). キャプチャ (was 対象)
    // moved to the head of the rail in round21 §1-1 (task t260913-b43e): it is
    // the one item that opens something rather than switching the pane.
    rail_review { ja: "確認", en: "Review" }
    rail_capture { ja: "キャプチャ", en: "Capture" }
    rail_sessions { ja: "履歴", en: "History" }
    rail_settings { ja: "設定", en: "Settings" }
    // クリップ sits under 履歴 (task2980); its own label lives in
    // `ui_state::clips` with the rest of that screen's wording.
    // Tray menu, verbatim from the Tauri build's `setup()` (task130).
    tray_open { ja: "Liveback を開く", en: "Open Liveback" }
    tray_quit { ja: "終了", en: "Quit" }
}

/// Worker → UI. Everything that touches Win32 or the capture hardware runs off
/// the event loop and reports back through this; `slint::Image` is not `Send`,
/// so thumbnails travel as raw pixels and become images on the UI thread.
enum Msg {
    Targets(Result<Vec<CaptureTarget>, String>),
    Thumbnail(String, Option<(u32, u32, Vec<u8>)>),
    /// The picker badge's process icon, keyed by executable id (task170): one
    /// icon serves every window of the same process.
    AppIcon(String, Option<(u32, u32, Vec<u8>)>),
    Capturing {
        /// Every capture running right now as `(session id, target window
        /// handle)`, oldest start first (task2030). It replaced a bare
        /// `active: bool`, which could say only *that* something was recording
        /// -- the picker needs to know which tiles, the history which rows, and
        /// the warning bar how many.
        running: Vec<(String, String)>,
        /// The captured window is minimized (round7 section 2-7). Still the
        /// last-started capture's; the review screen overrides it for the
        /// session it is actually showing.
        target_minimized: bool,
    },
    /// The same 1s poll, read for its stop reason rather than its bool: the
    /// only thing that can report a recording that ended on its own (task141).
    /// The bool is "held" (t260928-5cb2): the picker is waiting to see whether
    /// the rearm this stop scheduled replaces the recording, so it is not
    /// reported yet -- or, once a replacement ran, ever.
    Lifecycle(CaptureLifecycleStatus, bool),
    /// The settings a tile's badge draws, read on the list poll's own lock
    /// (task t260911-66da). `Msg::Targets(Ok)` used to re-read these three
    /// fields itself on every poll -- the only path by which a removal made on
    /// the settings screen reached the picker, and therefore the reason the
    /// list could not be sent only when it changed. The poller sends this only
    /// when one of the three moved, and always ahead of the list message of
    /// the same tick.
    PickerRegistrations {
        last_capture_executable: Option<String>,
        apps: Vec<livia::settings::AutoCaptureApp>,
        folders: Vec<livia::settings::AutoCaptureFolder>,
    },
    /// A registered executable launched and one of its windows is ready to be
    /// recorded (task1970). Carries the target id, the same string a tile
    /// click carries, so the start goes down the manual path rather than a
    /// second one of its own. `executable` rides along for a monitor target
    /// (task2570): an HMONITOR names no process, and the toast still has to
    /// say which app's launch started the recording. `handed_over` (task3840)
    /// is the watch's answer to "is this the far side of a handoff", which only
    /// the toast reads: the start itself is the same either way.
    AutoCaptureStart {
        id: String,
        executable: Option<String>,
        handed_over: bool,
    },
    /// `validate_capture_target` said no, or `start` refused.
    /// Why the start failed, so the banner and the log can both say it
    /// (task157). Carries the localized sentence built by
    /// `ui_targets::start_failure_text` (since 53188da8), not `start`'s own
    /// (English, engine-side) message.
    StartFailed(String),
    Started {
        id: String,
        settings_saved: bool,
    },
    Sessions(Result<Vec<SessionSummary>, String>),
    /// A clip-folder scan (task2980). `livia::clips::list` now creates the
    /// folder itself when it is missing (task3030), so there is no longer a
    /// "the folder is not there" screen to distinguish from "empty". A folder
    /// that will not read is an `Err` with the reason (t260928-6080 F9).
    Clips {
        entries: Result<Vec<livia::clips::ClipEntry>, String>,
    },
    /// One clip row's comment, duration and thumbnail, from the clip worker.
    /// The `SystemTime` is the modification time the request was made against,
    /// carried back so the answer is cached against the file it was asked about
    /// -- a comment written meanwhile moves the mtime, and filing a pre-write
    /// answer under the post-write key would leave the row stale (task2990).
    ClipDetails(
        std::path::PathBuf,
        std::time::SystemTime,
        livia::clips::ClipDetails,
        Option<(u32, u32, Vec<u8>)>,
    ),
    /// A clip could not be opened, revealed, edited or deleted. A toast, like
    /// `SessionFileFailed`: the row is still there and still fine. What
    /// failed and why, as the toast's title and second line (t260928-6080 F13).
    ClipError((String, String)),
    /// A comment write on this one clip was refused (t260918-f7a6). The toast
    /// is `ClipError`'s, sent right behind it on the same channel; this is the
    /// half that reaches the grid, dropping that clip's optimistic comment so
    /// the tile goes back to the text its file still holds.
    ClipCommentFailed(std::path::PathBuf),
    /// A clip操作 that worked and has nothing left on screen to point at --
    /// the delete, whose row is gone by the time this is read (task2990).
    /// Title and detail, as `SessionNotice` (t260928-6080 F8).
    ClipNotice((String, String)),
    SessionThumbnail(String, Option<(u32, u32, Vec<u8>)>),
    /// Hover-preview frame for the review timeline, keyed by segment index.
    ReviewThumbnail(u64, Option<(u32, u32, Vec<u8>)>),
    /// A history action that failed -- a comment, the protection, a load, the
    /// folder -- as an error toast's title (t260928-e309 F4, DS 知らせ方: 操作が
    /// 失敗した → Toast). It used to be the pane's Alert above the list.
    SessionError(&'static str),
    /// A `Cmd::LoadFile` that failed, as the toast's title and second line. A
    /// toast because the file was named from outside the app (task570); the
    /// catalog's own reason is English and goes to the log, not here
    /// (t260928-e309 F9).
    SessionFileFailed((String, String)),
    /// A discard that partly failed (task116), title and detail (t260928-e309,
    /// the split t260928-23aa left for it). An error toast since t260927-73fd.
    SessionFailed((String, String)),
    /// A discard's (title, detail), the DS toast's two lines (t260927-73fd).
    /// One tuple rather than two fields so the matches that only pass it by
    /// keep their `SessionNotice(_)`.
    SessionNotice((String, String)),
    /// The retention sweep deleted recordings to stay inside the capacity
    /// budget (task700). Its own message rather than `SessionNotice` because
    /// that one is a success toast, and losing a recording nobody asked to lose
    /// is not a success.
    CapacityReclaimed(String, String),
    Loaded {
        session_id: String,
        /// Loaded through `LoadActive`, i.e. the session is still recording
        /// (task162). The review screen parks at the live edge for these
        /// instead of autoplaying from the head.
        live: bool,
        /// Loaded through `LoadStarted`: the recording began moments ago
        /// because the user picked its target. Not derivable from an empty
        /// manifest -- the first segment can beat the load here -- so the
        /// reason for the load has to travel with it.
        fresh: bool,
    },
}

/// UI → session worker. Listing and discarding walk a few hundred directories
/// and delete files; neither belongs on the event loop, and neither should
/// share a thread with the 1s capture poll.
/// The captures the user has asked to stop, whose stops have not landed yet
/// (task2030 判断7). Shared by every stop door -- the picker's LIVE tile, the
/// live capsule, the history's LIVE rows, the tray -- because the toast that
/// confirms a stop is raised where the drain notices the session leaving the
/// active set, not where the click happened: `stop_session_async` hands the
/// join to its own thread and returns before anything has stopped.
///
/// One entry per capture, which is what makes the count right when the tray
/// stops three at once.
type StopRequests = Rc<RefCell<Vec<String>>>;

enum Cmd {
    ListSessions,
    /// Re-scan the clip folder (task2980). On this worker rather than the clip
    /// one because it needs the settings this thread already holds, and because
    /// `fs::metadata` over a folder is the same class of work as `ListSessions`
    /// -- the slow per-file shell reads are what got their own thread.
    ListClips,
    Thumbnail(String, u64),
    /// Same read as `Thumbnail`, but the reply carries the segment index so
    /// the review hover cache can file it (task127).
    ReviewThumbnail(String, u64),
    Discard(Vec<String>),
    Load(String),
    /// A `.lvb` named by path rather than by id (task570): the command line, and
    /// later the drop target. Everything after the load is `Load`'s path.
    LoadFile(std::path::PathBuf),
    /// Loads a recording that is still running (task162). Separate from `Load`
    /// because only this one may run while capturing. `None` is whatever
    /// started last, which is all the hotkey can name; the history's LIVE rows
    /// name one, and that is the only way to move the review screen from one
    /// running capture to another (task2030).
    LoadActive(Option<String>),
    /// `LoadActive` for the recording the user just started (round5 §5: the
    /// tile click lands on 確認). Same load; what differs is where the review
    /// opens, which is the head rather than a live edge that barely exists.
    LoadStarted,
    OpenDirectory(String),
    /// Runs at startup and after a recording stops -- not at exit, because a
    /// crash or force-quit is exactly when orphaned sessions appear and exactly
    /// the run an exit-time sweep never gets.
    ///
    /// Carries the session the review screen has loaded, which the sweep must
    /// not delete (task3970): since task3700 a past session can sit on the
    /// review screen while a recording stops. `None` at startup, when nothing
    /// is loaded yet.
    Reclaim(Option<String>),
}

fn image_from_rgba(width: u32, height: u32, rgba: &[u8]) -> Option<Image> {
    // A full-frame copy on the UI thread, once per presented frame (task205).
    livia::insight_scope!("ui_image_from_rgba");
    // The buffer is built in `livia-pixels`, not here: slint fills a
    // `SharedPixelBuffer` element by element, and that loop is compiled into
    // whichever crate instantiates it. Written inline it landed in this
    // unoptimized binary at 38.6ms a 1920x1032 frame -- 4.1 of every 5 seconds
    // of UI thread during playback. From the optimized leaf package it is
    // 1.06ms. The guards it needs travel with it.
    livia_pixels::image_from_rgba(width, height, rgba)
}

/// Monotonic milliseconds since the first call. The status line's hold must not
/// be measured against the wall clock, which can jump.
fn now_ms() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Keeps the capture's own bookkeeping ticking (task169). The diagnostics *UI*
/// is gone -- eight measured values nobody was reading, with thresholds and
/// wording to maintain forever -- but the call itself is load-bearing:
/// `diagnostics()` is what folds finalized segments into the manifest and what
/// lazily notices a capture that stopped on its own.
fn pump_capture(controller: &CaptureController) {
    let _ = controller.diagnostics();
}

/// Puts an operation result in the corner. Re-publishing the same content does
/// not restart the hold, so a poll that repeats itself cannot pin a toast open.
pub(crate) fn publish_status(
    ui: &AppWindow,
    line: &Rc<RefCell<toast::Toast>>,
    text: &str,
    is_error: bool,
) {
    publish_toast(
        ui,
        line,
        text,
        "",
        // A finished operation is `Success` (t260928-e309 F16, DS 知らせ方
        // 「操作が終わった → success」); it fades after `HOLD_MS` like `Info` did.
        if is_error {
            toast::ToastVariant::Error
        } else {
            toast::ToastVariant::Success
        },
        "",
    );
}

/// `action` is the label of the toast's second row, empty for the usual toast
/// that only reports. What the button does is decided in `on_toast_action_
/// clicked`, since only one producer has ever needed one.
/// Task1970: an auto-capture launch is only news once the recording actually
/// began -- `spawn_start_capture` revalidates and can refuse, and announcing a
/// start that did not happen would be worse than saying nothing. Any resolution
/// clears the watch, so a manual start landing in between cannot inherit the
/// announcement.
fn announce_auto_capture(
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    message: &Msg,
    pending: &mut Option<(String, String, bool)>,
    hwnd: Option<windows::Win32::Foundation::HWND>,
) {
    match message {
        Msg::Started { id, .. } => {
            if let Some((_, executable, handed_over)) =
                pending.take().filter(|(pending, _, _)| pending == id)
            {
                // Round21 §4-1 (task3840): the second recording of a handoff
                // says the recording moved rather than announcing a fresh
                // automatic start. The first one -- the launcher's -- is left
                // as it was on purpose, so a launch that never reaches a game
                // is still announced.
                let (title, body) = if handed_over {
                    (
                        auto_capture::handed_over_title(tr_locale()),
                        auto_capture::handed_over_body(&executable),
                    )
                } else {
                    (
                        auto_capture::started_title(tr_locale()),
                        auto_capture::started_body(tr_locale(), &executable),
                    )
                };
                tracing::info!(
                    target: "task1970_auto_capture",
                    event = "auto_capture_started",
                    %executable,
                    %id,
                    %handed_over,
                    "auto-capture recording started"
                );
                // task1550's split: the app that launched
                // is the foreground window, so this is
                // normally the OS toast -- which is the
                // only one the user would see from inside
                // a game. The same rule every notice takes
                // since t260928-e309 (`notice_sinks`).
                if lifecycle::notice_sinks(hwnd.is_some_and(desktop::is_foreground)).corner {
                    publish_toast(
                        ui,
                        status_line,
                        title,
                        &body,
                        toast::ToastVariant::Success,
                        "",
                    );
                } else {
                    desktop::toast(title, &body);
                }
            }
        }
        Msg::StartFailed(_) => *pending = None,
        _ => {}
    }
}

/// Re-renders whatever this tick changed, and takes the polls that run every
/// tick regardless: the live chip, the capture pump and the audio notice.
#[allow(clippy::too_many_arguments)]
fn render_changed_panes(
    ui: &AppWindow,
    (picker, history, review, clips): (&mut Picker, &mut History, &Review, &mut Clips),
    (tiles, session_rows, clip_rows): PaneModels<'_>,
    (controller, cmd_tx, status_line): (
        &CaptureController,
        &Sender<Cmd>,
        &Rc<RefCell<toast::Toast>>,
    ),
    audio_notified: &mut Vec<String>,
    flags: &DrainFlags,
    hwnd: Option<windows::Win32::Foundation::HWND>,
) {
    if flags.targets {
        render(ui, picker, tiles);
        picker::render_titlebar_tally(ui, picker, controller);
    }
    // Belt and braces for the live chip, the same way `poll_range_guard`
    // above is (task670): now that the chip outlives the live edge,
    // the frame pump is no longer enough to keep it current -- parked
    // at the edge there are no frames, and a paused review pane never
    // pumps at all.
    review::sync_live_chip(&ui.global::<ReviewVm>(), review);
    // The chip is permanent, so this runs on every tick, recording or
    // not -- an idle chip full of dashes is the resting state, not a
    // hidden element (round 4 §2-A3).
    pump_capture(controller);
    // Task2140: a recording whose audio trial failed keeps going without
    // sound, and until now the only account of it was a `warn!` line.
    // Beside `pump_capture` because it is the same kind of poll -- and
    // it pumps every running session, where `pump_capture` reaches the
    // last-started one only.
    let audio_status = controller.audio_status();
    let running: Vec<lifecycle::AudioStatus<'_>> = audio_status
        .iter()
        .map(|(id, title, mute)| (id.as_str(), title.as_deref(), *mute))
        .collect();
    if let Some(body) = lifecycle::audio_unavailable_toast(tr_locale(), audio_notified, &running) {
        // Warning, not Error: nothing failed -- the video is being
        // recorded exactly as asked (task198 / task1430). It waits to be
        // dismissed, because a recording is silent for good.
        publish_toast(
            ui,
            status_line,
            lifecycle::audio_unavailable_title(tr_locale()),
            &body,
            toast::ToastVariant::Warning,
            "",
        );
    }
    expire_status(ui, status_line);
    if flags.sessions {
        history
            .state
            .set_active_ids(controller.active_session_ids());
        render_sessions(ui, history, session_rows);
        // The first listing arrives before the .slint side has
        // reported a visible range, so this is where the first
        // screenful's thumbnails come from (task188).
        queue_visible_thumbnails(history, cmd_tx);
    }
    if flags.clips {
        // Same rule as the sessions above: the first scan lands before
        // the viewport has reported anything, so the first screenful's
        // details are queued from here (task2980).
        clips::refresh(ui, clips, clip_rows);
    }
    // Every tick, the same reasoning as `on_review` above: `active-pane` is
    // the one thing every route away from クリップ agrees on, so it is
    // cheaper to poll here than to thread a teardown through every one of
    // them (task2990 follow-up).
    if ui.get_active_pane() != ui_shell::Pane::Clips.index() {
        // Task3520 put the in-place player behind the same poll: the surface is
        // drawn inside the expanded tile, so leaving クリップ destroys it, and
        // an engine left running would keep the audio going for a screen nobody
        // is on. Nothing is remembered -- the expansion is not persisted, and
        // coming back lands on a freshly scanned grid.
        clips::left_pane(ui, clips);
    } else {
        // On the pane, and playing: the same argument the review's own stage
        // takes (task2800) -- a window in the tray, minimized, or buried under
        // the game being recorded is not being watched, and decoding for it
        // costs the machine what the app exists to give back.
        clips::offscreen_tick(ui, clips, hwnd.is_some_and(desktop::is_window_on_screen));
    }
    // Same reasoning on the 履歴 side: leaving destroys `SessionsPane` with
    // the open `InlineEdit` in it, so no blur is ever delivered and a return
    // would otherwise find the previous visit's editor still standing
    // (task3000). Leaving discards what was typed -- the same answer task3430
    // gave every other way out of a field.
    if ui.get_active_pane() != ui_shell::Pane::Sessions.index() {
        history.close_editor();
    }
}

/// The page state a target message reaches for.
struct PageRefs<'a> {
    status_line: &'a Rc<RefCell<toast::Toast>>,
    starting: &'a Rc<AtomicBool>,
    controller: &'a CaptureController,
}

/// The messages that are about targets and captures rather than sessions:
/// what they change on screen, and what they are worth saying out loud.
#[allow(clippy::too_many_arguments)]
fn apply_target_message(
    ui: &AppWindow,
    other: Msg,
    (picker, history, review): (&mut Picker, &mut History, &mut Review),
    flags: &mut DrainFlags,
    memory: &mut DrainMemory,
    page: PageRefs<'_>,
    (stop_requests, tray_weak): (&StopRequests, &slint::Weak<TrayIcon>),
    hwnd: Option<windows::Win32::Foundation::HWND>,
) {
    let PageRefs {
        status_line,
        starting,
        controller,
    } = page;
    // A capture that starts or stops changes which row is
    // live, which changes what is selectable.
    if matches!(other, Msg::Capturing { .. } | Msg::Started { .. }) {
        flags.sessions = true;
    }
    // Task1970: the launch is only news once the recording
    // actually began -- `spawn_start_capture` revalidates
    // and can refuse, and announcing a start that did not
    // happen would be worse than saying nothing. Cleared
    // on any resolution, so a manual start landing in
    // between cannot inherit the announcement.
    announce_auto_capture(
        ui,
        status_line,
        &other,
        &mut memory.auto_capture_pending,
        hwnd,
    );
    picker::announce_start(ui, status_line, &other);
    // The transport's live chip reads these off `Review`
    // rather than locking the controller on every frame
    // (task237).
    if let Msg::Capturing {
        running,
        target_minimized,
        ..
    } = &other
    {
        flags.sessions |= apply_capturing(
            ui,
            status_line,
            review,
            history,
            controller,
            stop_requests,
            &mut memory.running_count,
            tray_weak,
            running,
            *target_minimized,
        );
    }
    // task a015: `apply` answers whether it moved anything the grid draws.
    // This used to be an unconditional `= true`, which turned the 1s list
    // poll and the 1s `Msg::Capturing` into a `present` a second with the
    // picker open (task4290's remaining >=1.87/s).
    flags.targets |= apply(picker, starting, other);
}

/// The three row models the panes draw from, in rail order.
type PaneModels<'a> = (
    &'a Rc<VecModel<TargetTile>>,
    &'a Rc<VecModel<SessionRow>>,
    &'a Rc<VecModel<ClipRow>>,
);

/// What the drain tick found: which panes need re-rendering before it ends.
///
/// **The convention is `|=` off an `apply` that answers 「自分は状態を変えたか」**
/// -- `flags.sessions |= apply_capturing(..)`, `flags.review |=
/// control_touched_review`, and since task a015 `flags.targets |= apply(..)`.
/// The unconditional `= true` sites left on the other panes were measured by
/// t260918-c3ea (2026-09-26) and **deliberately kept**: a raise that changes
/// nothing costs no `present`, because every render behind these flags writes
/// only what differs -- `render_sessions` / `clips::render` diff their rows,
/// `render_review` its cached spans, and slint's `Property::set` marks nothing
/// dirty on an equal value. Measured: `Msg::Capturing` raises `sessions` once a
/// second and changed the pane on 10 of 427 ticks, yet hollowing that raise out
/// left the 履歴 pane at 0 presents/60s against 0/60s (3 legs each,
/// interleaved); its whole cost is `ui_render_sessions` at ~22us a second. The
/// event sites mostly change their pane when they raise (`ReviewThumbnail`
/// 66/66, `ClipDetails` 11/11, the session messages 5/5), and the ones that do
/// not are a handful per session (`Loaded` 1/2, marker presses 1/3). So
/// converting one is
/// worth it only when its render stops being a diff. Numbers, harness and the
/// instrument patch: `.agents/tasks/evidence/t260918-c3ea-drain-flags-measured/`.
#[derive(Default)]
struct DrainFlags {
    targets: bool,
    sessions: bool,
    clips: bool,
    review: bool,
    /// The session id a `Msg::Loaded` named, not just "something loaded"
    /// (task3700). `refresh_review_pane` needs the id to ask the controller for
    /// *that* manifest: `timeline()` reads `last_timeline`, which every running
    /// capture's index writer overwrites each pass, so a past session loaded
    /// mid-recording would be swapped out from under the review pane.
    loaded: Option<String>,
    live: bool,
    fresh: bool,
}

/// The drain tick's memory between ticks: what has already been toasted, the
/// auto-capture waiting to be announced, and the running count the disk sweep
/// reports.
#[derive(Default)]
struct DrainMemory {
    /// The `seq` of the last stop toasted, so a status the 1s poll keeps
    /// re-reading is only reported once (task141) while a genuinely new stop
    /// still rings (task2090).
    notified: Option<u64>,
    /// The tile id an auto-start was asked for, the executable name the toast
    /// says (task1970), and whether that start is the far side of a handoff
    /// (task3840, which only changes the toast's wording). Only a `Started` for
    /// that same id proves it began.
    auto_capture_pending: Option<(String, String, bool)>,
    /// Task2030: the disk guard takes every capture within the same second;
    /// this remembers that the sweep has been reported.
    disk_sweep: bool,
    /// Task3630: the `seq` of the last stop *written to the log*, which is not
    /// `notified` -- a stop the user asked for is deliberately silent, so
    /// `notified` never moves for it and would let the 1s poll write the same
    /// line every second for as long as the status sits there. Its own slot
    /// because the measurement has to see the silent stops too: "the toast was
    /// skipped" is one of the answers this is being added to tell apart.
    stop_logged: Option<u64>,
    running_count: lifecycle::RunningCount,
    /// t260927-2aa4: the 1s capture polls counted while something records,
    /// for `live_relist_due`.
    live_relist_polls: u32,
}

/// The references the message arms reach for beyond the three borrows.
struct MessageRefs<'a> {
    status_line: &'a Rc<RefCell<toast::Toast>>,
    settings: &'a Arc<Mutex<livia::settings::AppSettings>>,
    starting: &'a Rc<AtomicBool>,
    controller: &'a CaptureController,
    cmd_tx: &'a Sender<Cmd>,
    tx_ui: &'a Sender<Msg>,
    stop_requests: &'a StopRequests,
    tray_weak: &'a slint::Weak<TrayIcon>,
}

/// Drains everything the workers have sent since the last tick.
#[allow(clippy::too_many_arguments)]
fn drain_messages(
    ui: &AppWindow,
    rx: &Receiver<Msg>,
    (picker, history, review, clips): (&mut Picker, &mut History, &mut Review, &mut Clips),
    flags: &mut DrainFlags,
    memory: &mut DrainMemory,
    refs: MessageRefs<'_>,
    hwnd: Option<windows::Win32::Foundation::HWND>,
) {
    let MessageRefs {
        status_line,
        settings,
        starting,
        controller,
        cmd_tx,
        tx_ui,
        stop_requests,
        tray_weak,
    } = refs;
    while let Ok(message) = rx.try_recv() {
        // t260927-2aa4: a recording's size moves only on a listing. Here rather
        // than in `apply_target_message` because this is where `cmd_tx` is.
        if let Msg::Capturing { running, .. } = &message {
            if live_relist_due(&mut memory.live_relist_polls, !running.is_empty()) {
                let _ = cmd_tx.send(Cmd::ListSessions);
            }
        }
        match message {
            Msg::ReviewThumbnail(index, pixels) => {
                let image =
                    pixels.and_then(|(width, height, rgba)| image_from_rgba(width, height, &rgba));
                review.thumbnails.insert(index, image);
                // Hover fills this one segment at a time and only
                // `reset` empties it, so it is a retention suspect for
                // the memory a stopped playback does not give back
                // (task212).
                livia::insight_images!(Review, review.thumbnails.len());
                flags.review = true;
            }
            Msg::Lifecycle(status, held) => {
                memory.running_count.observe_lifecycle(&status);
                if !held {
                    report_lifecycle_stop(
                        ui,
                        status_line,
                        &status,
                        (&mut memory.notified, &mut memory.stop_logged),
                        &mut memory.disk_sweep,
                        memory.running_count.count(),
                        hwnd,
                    );
                }
            }
            Msg::Loaded {
                live,
                fresh,
                ref session_id,
                ..
            } => {
                flags.live = live;
                flags.fresh = fresh;
                // Taken before `message` moves into `apply_session`.
                flags.loaded = Some(session_id.clone());
                apply_session(history, message);
                flags.sessions = true;
            }
            // Task169: the discard result is the one session message
            // with nothing to point at -- the rows it was about are
            // gone -- so it goes to the corner instead of the pane. A
            // partial failure goes there too, as an error that stays
            // until closed (t260927-73fd, DS HistoryScreen); its text
            // still says the failed sessions are left selected.
            // `ClipError` is the same kind of report, next: a clip that would
            // not open, reveal, delete or take its comment is still listed and
            // still fine, so there is nothing on the pane to point at. It
            // carries its reason as the toast's second line (t260928-6080
            // F13). The grid's half of a refused comment write is
            // `ClipCommentFailed`, below.
            // A history action's failure joins them (t260928-e309 F4): the
            // row it was about is still there, and the toast says what failed.
            Msg::SessionError(title) => {
                publish_toast(ui, status_line, title, "", toast::ToastVariant::Error, "");
            }
            Msg::SessionFileFailed((title, detail))
            | Msg::SessionFailed((title, detail))
            | Msg::ClipError((title, detail)) => {
                publish_toast(
                    ui,
                    status_line,
                    &title,
                    &detail,
                    toast::ToastVariant::Error,
                    "",
                );
            }
            // A refused comment write has to reach the grid (t260916-568d):
            // the tile has been showing the comment the user typed since the
            // commit, and dropping that clip's guess is what puts the file's
            // own text back under it -- but only a re-render draws that, which
            // is what `flags.clips` asks for. Measured on the real app with
            // the file held open: without the flag the rollback happened in
            // `Clips` and the tile went on showing the text that was never
            // written. Its own message rather than `ClipError` (t260918-f7a6),
            // so a Reveal / Delete / unopenable-clip refusal landing inside the
            // write's round trip leaves every guess standing.
            Msg::ClipCommentFailed(path) => {
                flags.clips |= clips.comment_write_failed(&path);
            }
            Msg::Clips { entries } => {
                match entries {
                    Ok(entries) => clips.listed(entries),
                    Err(error) => clips.list_failed(error),
                }
                flags.clips = true;
            }
            Msg::ClipDetails(path, modified, details, pixels) => {
                clips.detailed(path, modified, details, pixels);
                flags.clips = true;
            }
            Msg::SessionNotice((title, detail)) => {
                publish_toast(
                    ui,
                    status_line,
                    &title,
                    &detail,
                    toast::ToastVariant::Success,
                    "",
                );
            }
            Msg::ClipNotice((title, detail)) => {
                publish_toast(
                    ui,
                    status_line,
                    &title,
                    &detail,
                    toast::ToastVariant::Success,
                    "",
                );
            }
            Msg::CapacityReclaimed(notice, detail) => {
                // Warning, not Error: nothing failed. It is the one
                // thing the sweep does that the user did not ask for,
                // so it is said out loud, in amber, and it waits to be
                // dismissed (task700, round7 §2-11).
                publish_toast(
                    ui,
                    status_line,
                    &notice,
                    &detail,
                    toast::ToastVariant::Warning,
                    sessions::settings_action(tr_locale()),
                );
            }
            Msg::Sessions(_) | Msg::SessionThumbnail(..) => {
                apply_session(history, message);
                flags.sessions = true;
            }
            // A registered executable launched (task1970). Handled
            // here rather than in `apply` because the start needs the
            // controller and the two channels, and because the toast
            // it produces is this loop's job.
            Msg::AutoCaptureStart {
                id,
                executable,
                handed_over,
            } => {
                // Only a dispatch that actually spawned a start writes
                // here, and only `Started` / `StartFailed` clear it.
                // The watch stays armed until the recording is running,
                // so a start slower than the 1s poll is asked for again
                // and refused by the `starting` guard -- overwriting on
                // that `None` would drop the announcement for exactly
                // the slow starts this feature exists for.
                let announce = picker::start_auto_capture(
                    picker,
                    starting,
                    controller,
                    settings,
                    (tx_ui, cmd_tx),
                    &id,
                    executable,
                );
                if let Some(executable) = announce {
                    memory.auto_capture_pending = Some((id, executable, handed_over));
                }
            }
            other => apply_target_message(
                ui,
                other,
                (picker, history, review),
                flags,
                memory,
                PageRefs {
                    status_line,
                    starting,
                    controller,
                },
                (stop_requests, tray_weak),
                hwnd,
            ),
        }
    }
}

/// Everything the 100ms drain tick reads or writes.
struct DrainInputs<'a> {
    ui: &'a AppWindow,
    picker: &'a Rc<RefCell<Picker>>,
    history: &'a Rc<RefCell<History>>,
    review: &'a Rc<RefCell<Review>>,
    clips: &'a Rc<RefCell<Clips>>,
    starting: &'a Rc<AtomicBool>,
    settings: &'a Arc<Mutex<livia::settings::AppSettings>>,
    tiles: &'a Rc<VecModel<TargetTile>>,
    session_rows: &'a Rc<VecModel<SessionRow>>,
    clip_rows: &'a Rc<VecModel<ClipRow>>,
    models: ReviewModels<'a>,
    controller: &'a CaptureController,
    hotkeys: &'a Rc<RefCell<hotkeys::Hotkeys>>,
    status_line: &'a Rc<RefCell<toast::Toast>>,
    stop_requests: &'a StopRequests,
    tray: &'a TrayIcon,
    cmd_tx: &'a Sender<Cmd>,
    tx: &'a Sender<Msg>,
}

/// The `tracing` target every control response carries, so a sweep can filter
/// the log down to its own commands in one grep (task3750).
#[cfg(feature = "control")]
const CONTROL_TARGET: &str = "task3750_control";

/// Takes at most one pending control command and runs it, returning whether the
/// review pane now needs re-rendering (task3750).
///
/// Four operations and no more: opening a `.lvb`, setting the range, saving it
/// as a commented clip, and adding or removing an auto-capture folder rule.
/// Each answers with exactly one `info!` line -- `debug!` never reaches the log
/// file, whose filter is `INFO` (`src/logging.rs`, task3340/3630) -- and an
/// unparsable line answers with a refusal rather than silence.
///
/// Every command routes through the same internal path its button does. The
/// folder guard especially: a control surface looser than the screen would let
/// a sweep verify a route production does not have.
#[cfg(feature = "control")]
fn apply_control_request(
    ui: &AppWindow,
    review: &Rc<RefCell<Review>>,
    settings: &Arc<Mutex<settings::AppSettings>>,
    cmd_tx: &Sender<Cmd>,
) -> bool {
    use livia::ui_state::control::{parse_control_command, ControlCommand};
    use livia::ui_state::timeline as tl;

    let Some(directory) = livia::handoff::request_directory() else {
        return false;
    };
    let Some(line) = livia::handoff::take_control_request(&directory) else {
        return false;
    };
    // The folder is in every response: the sender prints the path it wrote, and
    // the two only mean the same file when both halves see the same filesystem
    // (task3720's shadowed `%APPDATA%`).
    let directory = directory.display().to_string();
    let command = match parse_control_command(&line) {
        Ok(command) => command,
        Err(error) => {
            tracing::info!(
                target: CONTROL_TARGET,
                event = "control_rejected",
                ok = false,
                directory = %directory,
                request = %line,
                reason = %error,
                "the control request was read and refused"
            );
            return false;
        }
    };
    let event = command.event_name();
    match command {
        ControlCommand::Open(path) => {
            let path = std::path::PathBuf::from(path);
            if !path.is_file() {
                tracing::info!(
                    target: CONTROL_TARGET,
                    event, ok = false,
                    directory = %directory,
                    path = %path.display(),
                    reason = "no such file",
                    "the session to open is not there"
                );
                return false;
            }
            // The same command a double-clicked `.lvb` produces -- minus the
            // `show_window` that only the activation branch does.
            let _ = cmd_tx.send(Cmd::LoadFile(path.clone()));
            tracing::info!(
                target: CONTROL_TARGET,
                event, ok = true,
                directory = %directory,
                path = %path.display(),
                "load requested; the window was not raised"
            );
            false
        }
        ControlCommand::Range {
            start_100ns,
            end_100ns,
        } => {
            let mut review = review.borrow_mut();
            let Some(snapshot) = review.snapshot.clone() else {
                tracing::info!(
                    target: CONTROL_TARGET,
                    event, ok = false,
                    directory = %directory,
                    reason = "no session is loaded",
                    "the range has nothing to sit on"
                );
                return false;
            };
            // Both edges at once against the recording, not `move_range_edge`
            // twice: a new range disjoint from the old one clamps each edge
            // against a boundary that is about to move anyway.
            let origin = snapshot.start_100ns();
            let Some(range) = snapshot.clamp_range(
                origin + start_100ns,
                origin + end_100ns,
                tl::min_frame_100ns(),
            ) else {
                tracing::info!(
                    target: CONTROL_TARGET,
                    event, ok = false,
                    directory = %directory,
                    reason = "outside the recording",
                    "the seconds asked for are not in this session"
                );
                return false;
            };
            review.range = Some(range);
            // A hand-set boundary stops following the live edge, exactly as
            // `apply_range_edit` does for a typed one.
            review.quick = -1;
            if let Some(target) =
                tl::range_follow(range, review.position, Some(tl::RangeEdge::Start))
            {
                review.seek_clamped(target, true);
            }
            tracing::info!(
                target: CONTROL_TARGET,
                event, ok = true,
                directory = %directory,
                start_100ns = range.0,
                end_100ns = range.1,
                "the range was set from seconds off the session start"
            );
            true
        }
        ControlCommand::Clip(comment) => {
            // Through the panel's own callbacks, so the guard, the clip folder,
            // the allocated name and the comment's write-on-completion are the
            // ones the button uses. No borrow may be held across these.
            //
            // What is pending *before* the invoke, because a clip refused for
            // running on top of another export leaves the previous job's
            // `clip_pending` in place: reading it afterwards without this would
            // report the old job as if it were the new one's success.
            let before = review.borrow().clip_pending.clone();
            let vm = ui.global::<ReviewVm>();
            vm.invoke_clip_committed(comment.as_str().into());
            let review = review.borrow();
            match review
                .clip_pending
                .as_ref()
                .filter(|now| before.as_ref().is_none_or(|before| before.0 != now.0))
            {
                Some((job_id, _)) => tracing::info!(
                    target: CONTROL_TARGET,
                    event, ok = true,
                    directory = %directory,
                    job_id = %job_id,
                    comment = %comment,
                    "the clip export started; the comment lands when it completes"
                ),
                None => tracing::info!(
                    target: CONTROL_TARGET,
                    event, ok = false,
                    directory = %directory,
                    reason = review.status.as_deref().unwrap_or("no range, or an export is already running"),
                    "the clip was not started"
                ),
            }
            false
        }
        ControlCommand::FolderAdd(path) => {
            // The screen's guard, on the screen's own machine folders: a rule
            // this refuses is one the auto-capture poll would ignore anyway.
            if !auto_capture::folder_rule_allowed_with(&path, auto_capture::FolderGuard::machine())
            {
                tracing::info!(
                    target: CONTROL_TARGET,
                    event = "control_folder_rejected",
                    ok = false,
                    directory = %directory,
                    folder = %path,
                    reason = "wider than this feature acts on",
                    "the folder rule was refused, the same way the settings list refuses it"
                );
                return false;
            }
            let next = settings_page::commit_settings(settings, |current| {
                current.auto_capture_folders =
                    auto_capture::add_folder(&current.auto_capture_folders, &path);
            });
            settings_page::push_auto_capture_list(ui, settings);
            review::render_autorec(
                ui,
                &next.auto_capture_executables,
                &next.auto_capture_folders,
            );
            tracing::info!(
                target: CONTROL_TARGET,
                event, ok = true,
                directory = %directory,
                folder = %path,
                "the folder rule was added"
            );
            false
        }
        ControlCommand::FolderRemove(path) => {
            let next = settings_page::commit_settings(settings, |current| {
                current.auto_capture_folders =
                    auto_capture::unregister_folder(&current.auto_capture_folders, &path);
            });
            settings_page::push_auto_capture_list(ui, settings);
            review::render_autorec(
                ui,
                &next.auto_capture_executables,
                &next.auto_capture_folders,
            );
            tracing::info!(
                target: CONTROL_TARGET,
                event, ok = true,
                directory = %directory,
                folder = %path,
                "the folder rule was removed"
            );
            false
        }
    }
}

/// Starts the 100ms tick that drains the worker queue and re-renders what
/// changed. The timer is returned because dropping it stops the tick.
///
/// One drain, then one render: the worker sends a burst (a list, a capture flag
/// and a thumbnail) and re-rendering per message would rebuild the grid three
/// times for one visible change.
fn start_drain(
    inputs: DrainInputs<'_>,
    rx: Receiver<Msg>,
    instance: desktop::SingleInstance,
) -> slint::Timer {
    let DrainInputs {
        ui,
        picker,
        history,
        review,
        clips,
        starting,
        settings,
        tiles,
        session_rows,
        clip_rows,
        models: (review_segments, review_gaps, review_markers, review_marker_rows),
        controller,
        hotkeys,
        status_line,
        stop_requests,
        tray,
        cmd_tx,
        tx,
    } = inputs;
    let drain = slint::Timer::default();
    {
        let weak = ui.as_weak();
        let picker = picker.clone();
        let starting = starting.clone();
        let settings = settings.clone();
        let tiles = tiles.clone();
        let history = history.clone();
        let session_rows = session_rows.clone();
        let clips = clips.clone();
        let clip_rows = clip_rows.clone();
        let cmd_tx = cmd_tx.clone();
        // The drain starts captures of its own now (task1970's auto-start), so
        // it needs the worker→UI sender the same way the picker's callbacks do.
        let tx_ui = tx.clone();
        let review = review.clone();
        let review_segments = review_segments.clone();
        let review_gaps = review_gaps.clone();
        let review_markers = review_markers.clone();
        let review_marker_rows = review_marker_rows.clone();
        let controller = controller.clone();
        let hotkeys = hotkeys.clone();
        let status_line = status_line.clone();
        // App-lifetime, not picker-page state: what has already been said out
        // loud belongs to the recording, not to whichever screen happens to be
        // up (task141/task1970/task2140/task2030).
        let mut memory = DrainMemory::default();
        // The captures already told about their missing audio (task2140).
        let mut audio_notified: Vec<String> = Vec::new();
        let stop_requests = stop_requests.clone();
        let tray_weak = tray.as_weak();
        drain.start(slint::TimerMode::Repeated, DRAIN_INTERVAL, move || {
            let Some(ui) = weak.upgrade() else { return };
            let hwnd = hwnd_of(ui.window());
            // A second launch signalled and exited; this is where its request
            // to come forward lands (task130).
            if instance.activation_requested() {
                if let Some(hwnd) = hwnd {
                    desktop::show_window(hwnd);
                }
                // That launch may have been a double-click on a `.lvb` rather
                // than someone starting the app twice (task580).
                if let Some(path) = desktop::take_open_request() {
                    let _ = cmd_tx.send(Cmd::LoadFile(path));
                }
            }
            // Unconditional, and *before* every borrow below: the receiver
            // invokes the panel's own callbacks, which take the same
            // `RefCell`s. Not inside `activation_requested()` either -- taking
            // a control command must never raise the window (task3750).
            #[cfg(feature = "control")]
            let control_touched_review = apply_control_request(&ui, &review, &settings, &cmd_tx);
            let mut picker = picker.borrow_mut();
            let mut history = history.borrow_mut();
            let mut review = review.borrow_mut();
            let mut clips = clips.borrow_mut();
            let mut flags = DrainFlags::default();
            #[cfg(feature = "control")]
            {
                flags.review |= control_touched_review;
            }
            // Belt and braces for the selection's hold on the playhead
            // (task153 repeat, task3150 the boundaries): parked at the live
            // edge the engine stops rendering frames, so `pump_playback` --
            // the other caller -- stops being rung exactly when a whole-session
            // loop needs to wrap.
            if review::poll_range_guard(&mut review) {
                flags.review = true;
            }
            // Same tick as the worker drain: a 100ms lag on a keypress is not
            // perceptible, and it saves a second timer on the loop (task130).
            // The marker goes into the session on screen (task2190) -- the same
            // one the panel's ＋ button marks -- and only falls back to the
            // last-started recording when the review pane has nothing loaded.
            // Passed as an id because `review` is borrowed mutably right here:
            // a Slint invoke would re-enter that borrow and panic.
            // t260921-6ba9: the playhead and the LIVE badge travel with it,
            // read the way `review.rs` reads them for the badge itself. No
            // engine (or a poisoned status) counts as not loaded rather than
            // as "badge off", so the press keeps the live edge.
            let status = review
                .engine
                .as_ref()
                .and_then(|engine| engine.shared().status.lock().ok().map(|status| *status));
            let playhead_marks = hotkeys::drain(
                &hotkeys,
                &controller,
                ui.global::<SettingsVm>().get_armed() != 0,
                review
                    .snapshot
                    .as_ref()
                    .map(|snapshot| hotkeys::DisplayedReview {
                        session_id: snapshot.session_id.as_str(),
                        position_100ns: review.position,
                        start_100ns: snapshot.start_100ns(),
                        on_review_pane: ui.get_active_pane() == ui_shell::Pane::Review.index(),
                        loaded: status.is_some(),
                        // t260926-cc7e: the chip's ● ライブ追従中 half -- the
                        // mode, not `at_live_edge`, which a steady follow
                        // never raises.
                        live_badge_visible: review.follow.on && review.recording,
                    }),
                &ui,
                &status_line,
                hwnd,
            );
            // What the ＋ button does after `add_marker`: a stopped session
            // has no manifest refresh to bring the marker back on its own.
            for time in playhead_marks {
                if !review
                    .markers
                    .iter()
                    .any(|marker| marker.time_100ns == time)
                {
                    review.markers.push(livia::ring_buffer::MarkerRecord {
                        time_100ns: time,
                        label: String::new(),
                        color_index: None,
                    });
                    review.markers.sort_by_key(|marker| marker.time_100ns);
                }
                flags.review = true;
            }
            drain_messages(
                &ui,
                &rx,
                (&mut picker, &mut history, &mut review, &mut clips),
                &mut flags,
                &mut memory,
                MessageRefs {
                    status_line: &status_line,
                    settings: &settings,
                    starting: &starting,
                    controller: &controller,
                    cmd_tx: &cmd_tx,
                    tx_ui: &tx_ui,
                    stop_requests: &stop_requests,
                    tray_weak: &tray_weak,
                },
                hwnd,
            );
            render_changed_panes(
                &ui,
                (&mut picker, &mut history, &review, &mut clips),
                (&tiles, &session_rows, &clip_rows),
                (&controller, &cmd_tx, &status_line),
                &mut audio_notified,
                &flags,
                hwnd,
            );
            refresh_review_pane(
                &ui,
                &mut review,
                &controller,
                &settings,
                &cmd_tx,
                (
                    &review_segments,
                    &review_gaps,
                    &review_markers,
                    &review_marker_rows,
                ),
                hwnd,
                flags.loaded.as_deref(),
                flags.live,
                flags.fresh,
                flags.review,
            );
        });
    }
    drain
}

/// The four models the review pane draws itself from, in track order.
type ReviewModels<'a> = (
    &'a Rc<VecModel<TrackSpan>>,
    &'a Rc<VecModel<GapSpan>>,
    &'a Rc<VecModel<MarkerSpan>>,
    &'a Rc<VecModel<MarkerRow>>,
);

/// The half of the drain tick that runs after the worker queue is empty:
/// load a session that just arrived, follow the live edge, park the
/// transport when nobody is looking at the stage, and re-render the review
/// pane if any of that moved it.
#[allow(clippy::too_many_arguments)]
fn refresh_review_pane(
    ui: &AppWindow,
    review: &mut Review,
    controller: &CaptureController,
    settings: &Mutex<livia::settings::AppSettings>,
    cmd_tx: &Sender<Cmd>,
    models: ReviewModels<'_>,
    hwnd: Option<windows::Win32::Foundation::HWND>,
    loaded: Option<&str>,
    live: bool,
    fresh: bool,
    mut changed: bool,
) {
    let (review_segments, review_gaps, review_markers, review_marker_rows) = models;
    if let Some(session_id) = loaded {
        // By id, not `controller.timeline()` (task3700). `timeline()` answers
        // with `last_timeline`, and every running capture's index writer
        // overwrites that with its own manifest each pass -- so a past session
        // loaded while a recording runs would arrive here as the *recording's*
        // manifest, or get swapped for it a tick later. Same hazard task2030
        // fixed in `follow_live_edge`, same fix: ask for the session by name.
        // Every `Msg::Loaded` producer has already put the id in the
        // controller's catalog, so this resolves whenever `timeline()` would.
        if let Some((manifest, folded_at)) = controller.timeline_and_fold_for(session_id) {
            let current = settings_snapshot(settings);
            load_review(
                review, &manifest, folded_at, controller, &current, ui, live, fresh,
            );
            ui.set_session_loaded(true);
            // Loading a session is the answer to the question the
            // history overlay was asking, so it closes -- and closing it
            // is also what drops the status bar's session count (round 4
            // §2-A3: the count belongs to the screen that is showing).
            shell::show_pane(ui, cmd_tx, ui_shell::Pane::Review);
            changed = true;
        }
    }
    // After the load, so a session that just arrived is not immediately
    // asked what it gained: LiveReview (task162) grows the loaded
    // timeline from the manifest `refresh_diagnostics` just folded into.
    if review::follow_live_edge(review, controller) {
        changed = true;
    }
    // t260917-fe49: a stop armed in `apply_capturing` skips the backlog. The
    // End key's seek, not a new one; the manifest is only cloned while armed.
    if let (Some((armed_id, skipped_to)), Some(snapshot)) =
        (review.live_skip.clone(), review.snapshot.as_ref())
    {
        let final_edge = controller
            .timeline_for(&snapshot.session_id)
            .filter(|manifest| manifest.closed)
            .map(|manifest| manifest.segments.last().map_or(0, |s| s.end_100ns));
        let (session_id, live_edge) = (snapshot.session_id.clone(), snapshot.live_edge_100ns);
        let skip = pb::live_skip(
            Some((&armed_id, skipped_to)),
            &session_id,
            live_edge,
            final_edge,
        );
        review.live_skip = skip.stay_armed.then_some((session_id, live_edge));
        if let Some(target) = skip.seek_to {
            // Parked by the offscreen gate, the resume would `Play` from
            // `live_edge - 1`, which `restarts_from_head` answers by replaying
            // the backlog from the top. Landing paused at the end instead is
            // the End key's end state.
            review.paused_offscreen = false;
            tracing::info!(
                from_100ns = review.position,
                target_100ns = target,
                closed = final_edge.is_some(),
                "stop_skip_to_live"
            );
            // Not the user's seek (t260926-cc7e): the mode it was armed on
            // survives the skip.
            let follow = review.follow;
            review.seek_clamped(target, true);
            review.follow = follow;
            changed = true;
        }
    }
    // Nobody is looking at the stage from 履歴 or 設定, and decoding for
    // an audience of nobody is not free: a 1080p session publishes an
    // 8MB frame to this thread ~50 times a second, which is what made
    // the window sluggish while a game had the rest of the machine.
    // Checked here rather than in `show_pane` because every route into
    // a pane -- the rail, the empty state, a session load -- lands on
    // `active-pane`, and this sees all of them for one integer compare.
    {
        // The tray is the same argument as the wrong pane, and worse:
        // stashing the window is exactly what this app is for while a
        // game has the machine.
        // And a window buried under a maximised game is the same argument
        // again (task2800): `is_window_on_screen` only knows about hidden and
        // minimized, so a stage covered pixel for pixel by the very app being
        // recorded still counted as watched. Measured on this machine, that
        // cost the recorded editor 60fps -> 43-57fps and 11ms -> 16-22ms of
        // game thread, in a release build
        // (`.agents/tasks/evidence/2800-occluded-playback/`).
        let on_screen = hwnd.is_some_and(desktop::is_window_on_screen);
        // Only asked of a window that is on screen at all: the hit test needs
        // a client rect, and a minimized window has none worth sampling.
        let covered = on_screen && hwnd.is_some_and(desktop::is_window_covered);
        let on_review = ui.get_active_pane() == ui_shell::Pane::Review.index()
            && on_screen
            && !review.cover.hidden(covered, std::time::Instant::now());
        match pb::offscreen_transport(on_review, review.playing, review.paused_offscreen) {
            pb::OffscreenTransport::Pause => {
                review.paused_offscreen = true;
                review.playing = false;
                review.send(PlaybackCommand::Pause {
                    reason: "offscreen",
                });
                changed = true;
            }
            pb::OffscreenTransport::Resume => {
                review.paused_offscreen = false;
                // t260917-1a00 rejoined the live edge here whenever the stage
                // session recorded: a bare `Play` trailed the recording by the
                // covered time for the rest of it (measured: `behind_ms` flat
                // at ~29s after a 30s cover). Since t260926-cc7e that is the
                // mode's call -- following, it rejoins at the follow landing;
                // rewound, it plays on from where the cover caught it. The
                // gate's pause is not the user's pause (fe49), so neither
                // touches the mode.
                if !(review.recording && review.follow.on && review.follow_live("gate_resume")) {
                    review.playing = true;
                    review.send(PlaybackCommand::Play);
                }
                changed = true;
            }
            pb::OffscreenTransport::Leave => {}
        }
        // t260928-b301: while the session on the stage records, the bar and
        // the total grow on the apparent live edge every drain -- 10 renders
        // a second at most, and only while someone can see them. One more
        // after the stop clears the LIVE frame's lag, which only a render or
        // a frame writes.
        let lag_left =
            !review.recording && !ui.global::<ReviewVm>().get_live_behind_text().is_empty();
        if on_review && (review.recording || lag_left) {
            changed = true;
        }
    }
    if changed {
        let current = settings_snapshot(settings);
        render_review(
            ui,
            review,
            review_segments,
            review_gaps,
            review_markers,
            review_marker_rows,
            &current,
        );
    }
}

/// What one `Msg::Capturing` poll changes: the transport's live chip, the
/// stop toasts that have settled, the history's LIVE rows and the tray's stop
/// label. Returns whether the session rows need re-rendering.
///
/// Everything the capsule says is about **the session on the stage**
/// (task2030 判断5), never "something somewhere is recording": with two
/// captures those are different answers, and the capsule's stop acts on this
/// one.
#[allow(clippy::too_many_arguments)]
fn apply_capturing(
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    review: &mut Review,
    history: &mut History,
    controller: &CaptureController,
    stop_requests: &StopRequests,
    running_count: &mut lifecycle::RunningCount,
    tray_weak: &slint::Weak<TrayIcon>,
    running: &[(String, String)],
    target_minimized: bool,
) -> bool {
    let mut changed = false;
    let loaded = review
        .snapshot
        .as_ref()
        .map(|snapshot| snapshot.session_id.clone());
    // Everything the capsule says is about **the
    // session on the stage** (task2030 判断5), never
    // "something somewhere is recording": with two
    // captures those are different answers, and the
    // capsule's stop acts on this one.
    review.recording = loaded
        .as_deref()
        .is_some_and(|id| running.iter().any(|(active, _)| active == id));
    // t260917-fe49: the stage session stopped recording, by whichever route --
    // every stop lands here, so the skip is armed here and nowhere else. Fired
    // in `refresh_review_pane`, after `follow_live_edge` has the tail.
    if pb::stop_skip_arms(
        review.recording_session.as_deref(),
        loaded.as_deref(),
        review.recording,
        review.follow.on,
    ) {
        review.live_skip = loaded.clone().map(|id| (id, i64::MIN));
    }
    review.recording_session = loaded.clone().filter(|_| review.recording);
    // Cleared by the capture leaving the set, so the
    // process-wide `is_stopping()` -- raised while some
    // *other* capture is being joined -- never puts
    // 停止中… on a recording nobody touched.
    review.stopping &= review.recording;
    review.target_minimized = match loaded.as_deref() {
        Some(id) => controller.target_minimized_for(id),
        None => target_minimized,
    };

    // Task2030 判断7: one toast per capture the user
    // asked to stop, raised where the stop actually
    // lands. `auto_stop_toast` deliberately says
    // nothing about a requested stop, and until now
    // only the capsule's stop was reported at all.
    let active_ids: Vec<String> = running
        .iter()
        .map(|(session_id, _)| session_id.clone())
        .collect();
    let settled = lifecycle::settled_stops(&mut stop_requests.borrow_mut(), &active_ids);
    // What the disk-full sweep counts (task2200). The
    // stops the user asked for shrink it, everything
    // else only raises it; the decisions are all in
    // `RunningCount`, this is the wiring.
    running_count.settled(settled);
    running_count.observe(active_ids.len());
    for _ in 0..settled {
        publish_status(
            ui,
            status_line,
            lifecycle::stop_done_toast(tr_locale()),
            false,
        );
    }
    // The history's LIVE rows follow the poll, not the
    // session list: a capture that ended on its own
    // sends no `ListSessions`, and its row would have
    // kept its LIVE pill and its stop button.
    if history.state.active_ids() != active_ids.as_slice() {
        history.state.set_active_ids(active_ids.clone());
        changed = true;
    }
    if let Some(tray) = tray_weak.upgrade() {
        tray.set_stop_label(lifecycle::tray_stop_label(tr_locale(), active_ids.len()).into());
    }
    changed
}

/// Says out loud that a capture stopped on its own (task193/task1420/task2030):
/// the corner toast, the OS toast, or both, depending on whether the window is
/// in front of the user. `notified` and `disk_sweep` are what keep a status the
/// 1s poll re-reads from ringing twice.
fn report_lifecycle_stop(
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    status: &CaptureLifecycleStatus,
    (notified, stop_logged): (&mut Option<u64>, &mut Option<u64>),
    disk_sweep: &mut bool,
    running: usize,
    hwnd: Option<windows::Win32::Foundation::HWND>,
) {
    // Task3630: read once, and use this same value in both branches below.
    // Which side of it a stop went out on is the question the measurement
    // exists to answer, so what gets logged has to be what was acted on --
    // re-reading `GetForegroundWindow` a few lines later can answer
    // differently, since closing the target window is exactly when focus moves.
    let foreground = hwnd.is_some_and(desktop::is_foreground);
    let notified_before = *notified;
    let toast = lifecycle::stop_toast(tr_locale(), notified, disk_sweep, status, running);
    log_stop_report(
        status,
        (notified_before, stop_logged),
        &toast,
        running,
        (hwnd.is_some(), foreground),
    );
    // Nothing on screen changes: a stop the user did not
    // ask for is news only because they are looking
    // somewhere else. `hwnd == None` means the window is in
    // the tray, which is precisely the case this exists for
    // -- so an absent handle notifies, same as `notifyIfHidden`.
    if let Some((body, sub)) = toast {
        // Task3920: one decision, taken by `lifecycle::notice_sinks` and read by
        // both this and `log_stop_report`, so the line in the log cannot
        // describe a route the toast did not take. Since t260928-e309 it is
        // the same rule every notice takes: in front the corner, behind Windows.
        let sinks = lifecycle::notice_sinks(foreground);
        // Task2030 判断7: several captures on one disk hit
        // the guard together, and the corner holds one
        // toast -- so the sweep is reported once, with the
        // count, and 設定を開く is the way out of it.
        if let Some(sub) = sub {
            if sinks.corner {
                publish_toast(
                    ui,
                    status_line,
                    &body,
                    sub,
                    toast::ToastVariant::Error,
                    sessions::settings_action(tr_locale()),
                );
            }
            if sinks.os {
                desktop::toast(&body, sub);
            }
        } else {
            // Task193: a capture that failed used to be silent
            // on screen. The error variant never fades, so it
            // stays until the user closes it.
            //
            // Task1420: a stop the user did not ask for is `Warning`, like
            // the retention sweep: nothing failed, but it is not a result they
            // asked for. Unlike the sweep it fades after `HOLD_MS`
            // (t260928-983d, the user's call): a stop loses nothing, so it
            // need not wait to be clicked away.
            if sinks.corner {
                if lifecycle::is_capture_failure(status) {
                    publish_toast(
                        ui,
                        status_line,
                        lifecycle::capture_failed_title(tr_locale()),
                        &body,
                        toast::ToastVariant::Error,
                        "",
                    );
                } else {
                    publish_toast(
                        ui,
                        status_line,
                        lifecycle::auto_stop_title(tr_locale(), status),
                        &body,
                        toast::ToastVariant::Warning,
                        "",
                    );
                    status_line.borrow_mut().set_hold_ms(toast::HOLD_MS);
                }
            }
            if sinks.os {
                desktop::toast(lifecycle::auto_stop_title(tr_locale(), status), &body);
            }
        }
    }
}

/// Task3630: one line per stop event saying where the report went, because
/// until now the stop path wrote nothing at all -- a whole day of
/// `liveback.log` held zero `CAP-*` stop codes -- so "the user never saw the
/// toast" and "the toast was never produced" were indistinguishable from the
/// outside. Paired with the two lines in `desktop::toast`, the timestamps say
/// whether an unseen stop was ours or the OS's.
///
/// Once per event, keyed on `seq` in a slot of its own: `notified` does not
/// move for a stop the user asked for, and this has to cover those too --
/// `sink = none` is one of the answers being told apart -- so borrowing that
/// slot would write the same line every second the poll re-read the status.
///
/// `sink` is read off `lifecycle::notice_sinks`, the same call
/// `report_lifecycle_stop` routes on (task3920). It used to be a hand-written
/// copy of those branches -- a description of the decision that was structurally
/// free to drift into a second one -- which is precisely the thing an
/// investigation reading this log has to be able to trust.
fn log_stop_report(
    status: &CaptureLifecycleStatus,
    (notified_before, stop_logged): (Option<u64>, &mut Option<u64>),
    toast: &Option<(String, Option<&'static str>)>,
    running: usize,
    (has_hwnd, foreground): (bool, bool),
) {
    if status.state != "stopped" || *stop_logged == Some(status.seq) {
        return;
    }
    *stop_logged = Some(status.seq);
    let (corner, os) = match toast {
        None => (false, false),
        Some(_) => {
            let sinks = lifecycle::notice_sinks(foreground);
            (sinks.corner, sinks.os)
        }
    };
    let sink = match (corner, os) {
        (true, true) => "both",
        (true, false) => "corner",
        (false, true) => "os",
        (false, false) => "none",
    };
    tracing::info!(
        target: "task3630_stop_report",
        event = "stop_report",
        state = %status.state,
        code = ?status.diagnostic_code,
        seq = status.seq,
        ?notified_before,
        running,
        has_hwnd,
        foreground,
        failure = lifecycle::is_capture_failure(status),
        reported = toast.is_some(),
        consolidated = matches!(toast, Some((_, Some(_)))),
        sink,
        "auto-capture stop reported"
    );
}

pub(crate) fn publish_toast(
    ui: &AppWindow,
    line: &Rc<RefCell<toast::Toast>>,
    title: &str,
    detail: &str,
    variant: toast::ToastVariant,
    action: &str,
) {
    if line
        .borrow_mut()
        .publish(title, detail, variant, action, now_ms())
    {
        // Drawn on the next turn of the event loop, not here (t260928-5f3b):
        // a producer sets the card's hold, path or mono right after
        // `publish`, and a row once pushed is never read again -- its draining
        // line starts as it mounts.
        let (ui, line) = (ui.as_weak(), Rc::clone(line));
        slint::Timer::single_shot(Duration::ZERO, move || {
            if let Some(ui) = ui.upgrade() {
                show_toasts(&ui, &line.borrow());
            }
        });
    }
}

/// Clears the cards whose hold is up. Warnings and errors without a hold of
/// their own stay until closed or pushed out by three newer toasts.
fn expire_status(ui: &AppWindow, line: &Rc<RefCell<toast::Toast>>) {
    let mut line = line.borrow_mut();
    if line.expire(now_ms()) {
        show_toasts(ui, &line);
    }
}

/// Brings the corner's rows in line with the stack (t260927-73fd). Edited in
/// place, never replaced: a replaced model remounts every card, which replays
/// the entry rise and restarts the timeout line on the ones already showing.
/// The stack only loses cards (anywhere) and gains them at the end with a
/// larger id, so dropping the gone rows and appending the newer ids is the
/// whole diff.
fn show_toasts(ui: &AppWindow, line: &toast::Toast) {
    let model = ui.get_toasts();
    let Some(rows) = model.as_any().downcast_ref::<VecModel<ToastItem>>() else {
        ui.set_toasts(ModelRc::new(VecModel::<ToastItem>::default()));
        return show_toasts(ui, line);
    };
    let mut row = 0;
    while row < rows.row_count() {
        let id = rows.row_data(row).map_or(0, |item| item.id);
        if line.get(id).is_some() {
            row += 1;
        } else {
            rows.remove(row);
        }
    }
    let last = rows
        .row_count()
        .checked_sub(1)
        .and_then(|row| rows.row_data(row))
        .map_or(0, |item| item.id);
    for entry in line.entries().iter().filter(|entry| entry.id() > last) {
        rows.push(ToastItem {
            id: entry.id(),
            title: entry.title().into(),
            detail: entry.detail().into(),
            mono: entry.detail_mono(),
            action: entry.action().into(),
            variant: entry.variant().index(),
            hold_ms: i32::try_from(entry.fade_ms()).unwrap_or(i32::MAX),
        });
    }
}

/// The `.lvb` this launch was asked to open, if any (task570). Explorer passes
/// the double-clicked file as the sole argument; anything else -- the export
/// helper's flag, `--liveback-inspect`, a stray argument -- is not a container
/// path and is ignored here, having been handled (or not) before this point.
fn requested_session_file() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(std::env::args_os().nth(1)?);
    livia::ring_buffer::is_container_path(&path).then_some(path)
}

/// A `.lvb` dropped anywhere on the window opens, same as one double-clicked in
/// Explorer (task600). The whole window takes the drop rather than the review
/// screen alone: a successful load switches to 確認 by itself, so restricting
/// where the file may land would only add a way to miss.
///
/// Through winit rather than `DragAcceptFiles`/`WM_DROPFILES`, which would mean
/// subclassing slint's window. winit already delivers the drop, one file per
/// event; anything that is not a container is ignored.
///
/// The same subscription draws the drop overlay (task870): winit reports the
/// file while it is still held over the window (`HoveredFile`), which is the
/// only moment the app has to say whether it would take it.
fn accept_dropped_sessions(ui: &AppWindow, cmd_tx: &Sender<Cmd>) {
    use slint::winit_030::{
        winit::event::{ElementState, MouseButton, WindowEvent},
        EventResult, WinitWindowAccessor,
    };

    let cmd_tx = cmd_tx.clone();
    let weak = ui.as_weak();
    // Where the pointer last was, in physical pixels: winit's press carries no
    // position of its own.
    let cursor = std::cell::Cell::new((0.0_f64, 0.0_f64));
    ui.window().on_winit_window_event(move |_, event| {
        // Runs on the UI thread, so the handle is always live here.
        let Some(ui) = weak.upgrade() else {
            return EventResult::Propagate;
        };
        let drop = ui.global::<DropVm>();
        match event {
            WindowEvent::HoveredFile(path) => {
                let hint = ui_shell::drop_hint(path);
                drop.set_accepted(hint.accepted);
                drop.set_name(hint.name.into());
                drop.set_active(true);
            }
            // Leaving the window and letting go both end the drag; the overlay
            // has nothing to say about what happens after the file lands.
            WindowEvent::HoveredFileCancelled => drop.set_active(false),
            WindowEvent::DroppedFile(path) => {
                drop.set_active(false);
                if livia::ring_buffer::is_container_path(path) {
                    let _ = cmd_tx.send(Cmd::LoadFile(path.clone()));
                }
            }
            // Dragged to a monitor at another scale: the resize band is in
            // physical pixels (task4120), so it has to be recomputed or the
            // edges go back to being half as wide as the OS frame. The event's
            // own `scale_factor` rather than `window().scale_factor()`, which
            // is still the old one at this point.
            //
            // It lives in the drop subscription because winit has exactly one
            // window event filter slot: `on_winit_window_event` *replaces* the
            // previous closure rather than adding to it, so a second
            // subscription here would silently turn off file drops. Anything
            // else that needs a winit event joins this match.
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                apply_resize_band(&ui, (scale_factor * 96.0).round() as u32);
                // The stages' targets too (t260918-b743): a move that keeps
                // the logical size never rings `stage-size-changed`, so the
                // target would keep the old factor. On the next loop turn,
                // because the stages compute from `window().scale_factor()`
                // -- the factor `log_stage_resize` prints -- and that only
                // takes the new value once this event has been handled.
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.global::<ReviewVm>().invoke_stage_rescaled();
                        ui.global::<ClipVm>().invoke_stage_rescaled();
                    }
                });
            }
            // Nothing is drawn while the window is minimized. The d3d surface
            // presents into a flip-model swap chain
            // (`i-slint-renderer-skia/d3d_surface.rs`: `CreateSwapChainForHwnd`
            // + `FLIP_DISCARD`), and a present made while the window is iconic
            // throws away the last full-size frame DWM would otherwise put back
            // on screen. Restoring from the taskbar then shows one composed
            // frame of DWM's own fallback -- the window's content blown up
            // roughly ten times, which reads as "the icons went huge for an
            // instant". Measured on this machine, 4ms-cadence screen capture
            // over five restores each: 5/5 restores showed that frame, 0/5 with
            // this arm. Neither the swap effect nor `Scaling` is the lever (both
            // tried in a vendored renderer: `FLIP_SEQUENTIAL` keeps the blow-up,
            // `SCALING_NONE` only turns it black) -- what matters is that the
            // good frame is still the last one presented.
            //
            // The cost is that slint's redraw stays pending while minimized, so
            // its frame-throttle timer keeps ticking at the display's refresh
            // (measured 31ms of CPU per second here at 280Hz, against 7.8
            // without). No GPU work, and the window it would draw is not on
            // screen. That spin is upstream's own shape for an occluded window,
            // not something this arm invents: `winitwindowadapter.rs:711` re-arms
            // the redraw on every draw that does not return `Success`, and
            // `wgpu_28_surface.rs:209` returns `Occluded` for exactly this state.
            // Stashed in the tray is `SW_HIDE`, not iconic, and slint already
            // skips drawing for a hidden window, so that path is unaffected.
            //
            // ponytail: the whole arm is deletable if slint's d3d surface grows
            // the same `Occluded` answer and skips re-arming for an iconic
            // window -- the restore's own `WM_SIZE` asks for a redraw anyway.
            // `.agents/tools/restore-flicker-probe.ps1` is what says whether it
            // is still needed after a slint bump.
            WindowEvent::RedrawRequested
                if hwnd_of(ui.window()).is_some_and(desktop::is_window_minimized) =>
            {
                return EventResult::PreventDefault;
            }
            WindowEvent::CursorMoved { position, .. } => cursor.set((position.x, position.y)),
            // t260929-40a4: every left press, so the review's comment field can
            // let go of the keyboard when one lands outside it (`ReviewVm`'s
            // `window-press-tick`). On the next loop turn: slint handles the
            // press after this filter returns, and a press into another text
            // field must have taken the focus before the field looks.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                let (x, y) = cursor.get();
                let scale = f64::from(ui.window().scale_factor());
                let weak = ui.as_weak();
                slint::Timer::single_shot(Duration::ZERO, move || {
                    if let Some(ui) = weak.upgrade() {
                        let vm = ui.global::<ReviewVm>();
                        vm.set_window_press_x((x / scale) as f32);
                        vm.set_window_press_y((y / scale) as f32);
                        vm.set_window_press_tick(vm.get_window_press_tick().wrapping_add(1));
                    }
                });
            }
            _ => {}
        }
        EventResult::Propagate
    });
}

fn main() -> Result<(), slint::PlatformError> {
    // The export pipeline re-launches *this* executable with a flag and talks
    // to it over stdio (task129). Without this branch the helper would start a
    // second GUI instead of encoding, and the parent would fail the job on a
    // stopped heartbeat. Must come before anything that touches a window.
    let Some(Startup {
        instance,
        requested,
        stored,
        settings,
        controller,
        wgpu_renderer,
    }) = start_up()
    else {
        return Ok(());
    };
    let ui = AppWindow::new()?;
    if wgpu_renderer {
        review_stage::install_gpu_stage(&ui);
    }
    restore_window_state(ui.window(), stored.window_state);
    // Written once, here, rather than from `push_shell_labels`: the override is
    // read from the environment exactly once per process (task3910), so this
    // cannot change while the window lives and re-pushing it on every language
    // switch would only suggest otherwise.
    ui.set_agent_run(livia::capture::buffer_root_override().is_some());
    push_shell_labels(&ui);

    // ---------- tray, event sink, hotkeys (task130) ----------
    // The tray instance has to outlive `run()`: dropping it removes the icon,
    // and with the window stashed there would be no way back into the app.
    let tray = TrayIcon::new()?;
    push_tray_labels(&tray);
    tray.set_tooltip_text(livia::ui_state::lifecycle::tray_tooltip_idle(tr_locale()).into());
    tray.set_light_taskbar(desktop::system_uses_light_theme());
    // t260927-f2eb: the system (primary) DPI, not the window's -- the tray sits
    // on the primary taskbar wherever the window is. Read once: a DPI-aware
    // process sees no system DPI change until sign-out. Logged because a
    // DPI-unaware process reads 96 and would never take the 24px path.
    // SAFETY: no arguments, no preconditions.
    let tray_dpi = unsafe { windows::Win32::UI::HiDpi::GetDpiForSystem() };
    tracing::info!(dpi = tray_dpi, "tray dpi read");
    tray.set_large(lifecycle::tray_uses_24px(tray_dpi));
    // t260927-01d1: the OS "Animation effects" switch, read once like the
    // theme. Logged so an isolated launch shows what was read.
    let reduce_motion = !desktop::animations_enabled();
    tracing::info!(reduce_motion, "motion preference read");
    ui.global::<Tokens>().set_reduce_motion(reduce_motion);

    // One sink for both pipelines, like `TauriSink`: capture pushes the tray
    // tooltip through it, export pushes status and progress.
    let export_slot: Arc<Mutex<Option<ExportStatus>>> = Arc::new(Mutex::new(None));
    let sink = Arc::new(SlintSink {
        status: export_slot.clone(),
        ui: ui.as_weak(),
        tray: tray.as_weak(),
    });
    controller.set_event_sink(sink.clone());
    let exporter = ExportController::new(sink, controller.clone());

    let hotkeys = Rc::new(RefCell::new(hotkeys::Hotkeys::new()));
    let hotkey_registration = hotkeys.borrow_mut().apply(&stored.hotkey);
    if let Err(error) = &hotkey_registration {
        tracing::warn!(event = "default_hotkey_unavailable", %error, "Hotkey registration failed");
    }

    push_session_labels(&ui);

    let state = spawn_workers(&ui, &controller, &settings, requested);
    let AppState {
        ref picker,
        ref history,
        ref clips,
        ref starting,
        ref tiles,
        ref session_rows,
        ref clip_rows,
        ref tx,
        ref cmd_tx,
        ..
    } = state;
    let rx = state.rx.clone();

    let WiredPages {
        review,
        review_segments,
        review_gaps,
        review_markers,
        review_marker_rows,
        status_line,
        stop_requests,
    } = wire_pages(
        &ui,
        &state,
        &controller,
        &settings,
        &stored,
        &hotkeys,
        &hotkey_registration,
        (&exporter, &export_slot),
        &tray,
    );
    let _drain = start_drain(
        DrainInputs {
            ui: &ui,
            picker,
            history,
            review: &review,
            clips,
            starting,
            settings: &settings,
            tiles,
            session_rows,
            clip_rows,
            models: (
                &review_segments,
                &review_gaps,
                &review_markers,
                &review_marker_rows,
            ),
            controller: &controller,
            hotkeys: &hotkeys,
            status_line: &status_line,
            stop_requests: &stop_requests,
            tray: &tray,
            cmd_tx,
            tx,
        },
        rx,
        instance,
    );

    // The rounded corners and the outline border, applied from inside the event
    // loop (task231). winit creates the OS window in `resumed()`, so `hwnd_of`
    // is `None` everywhere before `run()` -- including right after `show()`,
    // which is why task142's call at construction time silently did nothing for
    // as long as it had been "implemented". A repeating timer that stops on the
    // first success is deliberate over a single deferred call: whether the
    // first loop iteration lands before or after window creation is exactly the
    // timing this has already lost to twice.
    let _dwm_timer = start_window_chrome_timer(&ui);
    let _crash_test_timer = start_d3d_panic_after_run_timer(&ui);
    ui.run()?;
    // Geometry first, ahead of the bounded stop below: this is the only place the
    // quit path writes it now, and it must not sit behind a wait that can last
    // `SHUTDOWN_TIMEOUT`. Reading it here is sound -- `run()` ends with `hide()`,
    // and off Wayland slint's hide is only `set_visible(false)`
    // (`winitwindowadapter.rs:1161-1175`), so the HWND is still alive: `position()`
    // reads the real rect (task2950 measured exactly that on a tray-stashed
    // window) and `size()` is the value the adapter has cached all along.
    persist_window_state(ui.window(), &settings);
    // The tray's 「終了」 only calls `slint::quit_event_loop()`, and `Closed` is
    // written by `Indexer::finish` alone, so without stopping here a session that
    // was still recording is left `closed=false` and the next launch pays for it
    // in `container::recover`, which streams the `.lvb`'s records through one
    // reused buffer rather than reading it whole (task3200, observed 2026-09-06;
    // streaming since 1cd88ca0 / task t260913-1e2e). This is the one join point every exit
    // passes through, and the window is off the screen by now.
    if !controller.shutdown(SHUTDOWN_TIMEOUT) {
        tracing::warn!(
            event = "shutdown_stop_timed_out",
            timeout_s = SHUTDOWN_TIMEOUT.as_secs(),
            "the container could not be closed; it will be recovered on the next launch"
        );
    }
    Ok(())
}

/// The tray menu (task130): open, stop everything, quit.
fn wire_tray_menu(
    tray: &TrayIcon,
    ui: &AppWindow,
    controller: &CaptureController,
    stop_requests: &StopRequests,
) {
    {
        let weak = ui.as_weak();
        tray.on_open(move || {
            if let Some(hwnd) = weak.upgrade().and_then(|ui| hwnd_of(ui.window())) {
                desktop::show_window(hwnd);
            }
        });
    }
    {
        // Always enabled: stopping while idle is a safe no-op, same as the
        // Tauri menu item.
        //
        // **The one "stop everything" door left** (task2030 判断5): the capsule,
        // the history LIVE rows and the picker tiles each stop a single capture
        // now, so this is where "just make it stop" lives. No confirmation
        // (判断4) -- every recording it ends is kept and playable.
        let controller = controller.clone();
        let stop_requests = stop_requests.clone();
        tray.on_stop(move || {
            // One request per capture, so the toast count is right (判断7).
            let running = controller.active_session_ids();
            tracing::info!(
                event = "capture_stop_requested",
                captures = running.len(),
                "stop asked for by the tray menu"
            );
            let mut pending = stop_requests.borrow_mut();
            for session_id in &running {
                lifecycle::request_stop(&mut pending, session_id);
            }
            let _ = controller.stop_async();
        });
    }
    // No `persist_window_state` here: `main` writes the geometry once `ui.run()`
    // returns, which is the join point every exit passes through, and the window
    // is still readable there. Doing it here as well (task130 mirrored
    // `stash_in_tray`, where writing *now* does matter because the window then
    // sits hidden for hours) only wrote the same values a second time.
    tray.on_quit(|| {
        let _ = slint::quit_event_loop();
    });

    // Clock only. Window geometry is written on close, not on a timer: this is
    // the same `settings.json` the Tauri app has open, and plugin-store keeps
    // its copy in memory, so a per-second rewrite would trade clobbers with a
    // running instance for no gain -- the requirement is only that position
    // survives a restart.
    // One drain, then one render: the worker sends a burst (list + capture flag
    // + a thumbnail) and re-rendering per message would rebuild the grid three
    // times for one visible change.
    // Held to the end of : dropping the timer stops the tick.
}

/// Hands the .slint side its resize band, already in physical pixels
/// (task4120 -- `AppWindow::resize-band-width` says why it cannot be scaled
/// over there).
///
/// The `scale_factor` is logged next to the dpi because task4120's second
/// failed attempt set `8.0 * ui.window().scale_factor()` from this same timer
/// and measured 8 physical px afterwards, i.e. a scale factor of 1.0 while
/// `GetDpiForWindow` said 144. Reading the dpi from the hwnd sidesteps whatever
/// that is; the log line is what will finally name it.
fn apply_resize_band(ui: &AppWindow, dpi: u32) {
    let band = ui_shell::resize_band_slint_px(dpi);
    tracing::info!(
        dpi,
        slint_scale_factor = ui.window().scale_factor(),
        band_logical_px = band,
        band_physical_px = ui_shell::resize_band_physical_px(dpi),
        "resize band set"
    );
    ui.set_resize_band_width(band);
    // The same 8, unscaled, for the layout side (task4320): a flush-right
    // `AppScroll` moves its lane in by this so the thumb is not sitting in the
    // band. Set beside the physical one so the pair cannot drift -- that drift
    // is the bug. dpi-independent, so re-setting it on every dpi change is a
    // no-op; it rides along to keep the two writes in one place. The *inset* is
    // not written here -- `app.slint` derives it from this, gated on
    // maximized/full screen, and pushes that.
    ui.global::<Tokens>()
        .set_resize_band_logical(ui_shell::RESIZE_BAND_LOGICAL_PX);
}

/// The rounded corners and the outline border, applied from inside the event
/// loop (task231). winit creates the OS window in `resumed()`, so the handle
/// is `None` everywhere before `run()`. A repeating timer that stops on the
/// first success is deliberate over a single deferred call: whether the first
/// loop iteration lands before or after window creation is exactly the race
/// timing this has already lost to twice.
fn start_window_chrome_timer(ui: &AppWindow) -> Rc<slint::Timer> {
    let dwm_weak = ui.as_weak();
    let dwm_timer = Rc::new(slint::Timer::default());
    let dwm_stop = dwm_timer.clone();
    dwm_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            let Some(ui) = dwm_weak.upgrade() else {
                return;
            };
            let Some(hwnd) = hwnd_of(ui.window()) else {
                return;
            };
            desktop::round_window_corners(hwnd);
            desktop::set_window_border_color(
                hwnd,
                ui.global::<Tokens>().get_light(),
                ui.get_agent_run(),
            );
            desktop::blur_window_backdrop(hwnd);
            desktop::watch_taskbar_button(hwnd);
            apply_resize_band(&ui, desktop::window_dpi(hwnd));
            dwm_stop.stop();
        },
    );

    dwm_timer
}

/// Task2940: `LIVEBACK_CRASH_TEST=d3d_panic_after_run` waits for the window,
/// then crashes from inside the event loop.
///
/// The startup call at `run_crash_test_if_requested` fires before winit has
/// created the OS window, so the panic hook's `window_rect` can only report
/// `unavailable` from there. Firing from a timer instead puts the panic on the
/// same event-loop thread a real `d3d_surface.rs` panic lands on, with a real
/// hwnd to read -- which is the only way the diagnostics' success path gets
/// exercised in-process.
///
/// `None` unless that exact value is set: normal startup arms no timer.
fn start_d3d_panic_after_run_timer(ui: &AppWindow) -> Option<Rc<slint::Timer>> {
    if livia::crash::requested_crash_test()? != livia::crash::CrashTest::D3dPanicAfterRun {
        return None;
    }
    // Same wait as the chrome timer, and for the same reason: `hwnd_of` is
    // `None` until `resumed()` builds the window.
    let weak = ui.as_weak();
    let timer = Rc::new(slint::Timer::default());
    let stop = timer.clone();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(50),
        move || {
            if weak.upgrade().and_then(|ui| hwnd_of(ui.window())).is_none() {
                return;
            }
            stop.stop();
            // ponytail: 固定5秒。この検証自体には要らない猶予で、task2950 が
            // 「窓が出てから PostMessage(WM_CLOSE) でトレイへ収納し、収納後に
            // 撃つ」手順を組めるようにするためだけに置いてある。task2950 の
            // 手順が要らなくなったらこの遅延ごと消してよい。
            // `single_shot` は自由関数でハンドルを持たない -- ここで
            // `Timer` を作って束縛すると、このコールバックを抜けた時点で
            // drop されて発火しない。
            slint::Timer::single_shot(Duration::from_secs(5), || {
                livia::crash::fire_crash_test(livia::crash::CrashTest::D3dPanicAfterRun)
            });
        },
    );

    Some(timer)
}

/// Task195: startup registration runs with no settings page open, so the
/// page's own reporting could never reach the user -- another app holding the
/// chord left the hotkeys quietly dead until someone went looking. The corner
/// says it instead, and the settings rows keep it visible.
fn report_hotkey_startup(
    ui: &AppWindow,
    status_line: &Rc<RefCell<toast::Toast>>,
    hotkey_registration: &Result<(), String>,
    chord: &str,
    cmd_tx: &Sender<Cmd>,
) {
    if hotkey_registration.is_err() {
        settings_page::flash_hotkey_failure(ui, status_line, chord);
    }
    {
        // Closing an error is the only way it ever goes away, so this is not a
        // convenience: it is the error variant's exit.
        let weak = ui.as_weak();
        let line = status_line.clone();
        ui.on_toast_dismissed(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let mut line = line.borrow_mut();
            line.dismiss(id);
            show_toasts(&ui, &line);
        });
    }
    {
        let weak = ui.as_weak();
        let line = status_line.clone();
        let cmd_for_toast = cmd_tx.clone();
        ui.on_toast_action_clicked(move |id| {
            let Some(ui) = weak.upgrade() else { return };
            let (action, path) = match line.borrow().get(id) {
                Some(entry) => (entry.action().to_owned(), entry.action_path().to_owned()),
                None => return,
            };
            // Two producers now, told apart by the label they asked for: the
            // export's フォルダで表示 and the capacity sweep's 設定を開く
            // (round7 §2-11).
            if action == livia::ui_state::sessions::settings_action(tr_locale()) {
                shell::show_pane(&ui, &cmd_for_toast, ui_shell::Pane::Settings);
            } else {
                // Whatever *this* toast is pointing at -- a finished export or
                // a saved screenshot (task1040). It used to read the export
                // panel's own list, so a screenshot's フォルダで表示 opened the
                // last export instead of the shot.
                if !path.is_empty() {
                    ui.global::<ReviewVm>().invoke_export_reveal(path.into());
                }
            }
            let mut line = line.borrow_mut();
            line.dismiss(id);
            show_toasts(&ui, &line);
        });
    }

    // ---------- settings (task126) ----------
}

/// What `spawn_workers` hands back: the page state, the models and the two
/// channel ends the UI keeps.
struct AppState {
    picker: Rc<RefCell<Picker>>,
    history: Rc<RefCell<History>>,
    clips: Rc<RefCell<Clips>>,
    starting: Rc<AtomicBool>,
    tiles: Rc<VecModel<TargetTile>>,
    session_rows: Rc<VecModel<SessionRow>>,
    clip_rows: Rc<VecModel<ClipRow>>,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    cmd_tx: Sender<Cmd>,
}

/// The models the two grids draw from, the channels the workers talk over,
/// and the workers themselves.
fn spawn_workers(
    ui: &AppWindow,
    controller: &CaptureController,
    settings: &Arc<Mutex<livia::settings::AppSettings>>,
    requested: Option<std::path::PathBuf>,
) -> AppState {
    let picker = Rc::new(RefCell::new(Picker::default()));
    let history = Rc::new(RefCell::new(History::default()));
    let starting = Rc::new(AtomicBool::new(false));
    let tiles = Rc::new(VecModel::<TargetTile>::default());
    let session_rows = Rc::new(VecModel::<SessionRow>::default());
    let clip_rows = Rc::new(VecModel::<ClipRow>::default());
    ui.set_targets(ModelRc::from(tiles.clone()));
    ui.set_sessions(ModelRc::from(session_rows.clone()));
    ui.set_clips(ModelRc::from(clip_rows.clone()));
    let (tx, rx): (Sender<Msg>, Receiver<Msg>) = unbounded();
    let (cmd_tx, cmd_rx): (Sender<Cmd>, Receiver<Cmd>) = unbounded();
    // The clip screen's slow half gets its own thread: a shell thumbnail
    // extraction can take hundreds of milliseconds, and behind `Load` on the
    // session queue that would be delay the user feels on a different screen
    // (task2980).
    let (clip_tx, clip_rx): (Sender<ClipJob>, Receiver<ClipJob>) = unbounded();
    let clips = Rc::new(RefCell::new(Clips::new(
        clip_tx,
        tx.clone(),
        cmd_tx.clone(),
    )));
    spawn_clip_worker(clip_rx, tx.clone(), cmd_tx.clone());
    spawn_target_worker(controller.clone(), settings.clone(), tx.clone());
    spawn_session_worker(controller.clone(), settings.clone(), cmd_rx, tx.clone());
    // Startup sweep, then the first listing. Reclaim re-lists itself when it
    // actually removed something, so a no-op sweep costs one round trip.
    let _ = cmd_tx.send(Cmd::Reclaim(None));
    let _ = cmd_tx.send(Cmd::ListSessions);
    // `liveback.exe <path.lvb>`, i.e. a double-click in Explorer (task570).
    // After the listing so the history behind the review screen is the usual
    // one; the load itself is the same command a row click sends.
    if let Some(path) = requested {
        let _ = cmd_tx.send(Cmd::LoadFile(path));
    }
    accept_dropped_sessions(ui, &cmd_tx);
    render(ui, &picker.borrow(), &tiles);

    // Shared by every stop door; see `StopRequests`.
    AppState {
        picker,
        history,
        clips,
        starting,
        tiles,
        session_rows,
        clip_rows,
        tx,
        rx,
        cmd_tx,
    }
}

/// What `wire_pages` leaves behind for the drain tick.
struct WiredPages {
    review: Rc<RefCell<Review>>,
    review_segments: Rc<VecModel<TrackSpan>>,
    review_gaps: Rc<VecModel<GapSpan>>,
    review_markers: Rc<VecModel<MarkerSpan>>,
    review_marker_rows: Rc<VecModel<MarkerRow>>,
    status_line: Rc<RefCell<toast::Toast>>,
    stop_requests: StopRequests,
}

/// Wires every page to the state behind it, and hands back what the drain
/// tick needs afterwards: the review pane and the models it draws from.
#[allow(clippy::too_many_arguments)]
fn wire_pages(
    ui: &AppWindow,
    state: &AppState,
    controller: &CaptureController,
    settings: &Arc<Mutex<livia::settings::AppSettings>>,
    stored: &livia::settings::AppSettings,
    hotkeys: &Rc<RefCell<hotkeys::Hotkeys>>,
    hotkey_registration: &Result<(), String>,
    (exporter, export_slot): (&ExportController, &Arc<Mutex<Option<ExportStatus>>>),
    tray: &TrayIcon,
) -> WiredPages {
    let AppState {
        picker,
        history,
        clips,
        starting,
        tiles,
        session_rows,
        clip_rows,
        tx,
        cmd_tx,
        ..
    } = state;
    // Shared by every stop door; see `StopRequests`.
    let stop_requests: StopRequests = Rc::new(RefCell::new(Vec::new()));

    // ---------- review timeline (task127) ----------
    // Declared before the history page rather than beside its own wiring
    // below: discarding the session the review pane has open has to let that
    // pane's engine (and with it, its lease) go first (task1520). Before the
    // picker too: its tile stop tells the reclaim sweep what is loaded
    // (task3970).
    let review = Rc::new(RefCell::new(Review {
        rate: 1.0,
        quick: 0,
        ..Review::default()
    }));

    picker::wire(
        ui,
        picker,
        tiles,
        starting,
        controller,
        settings,
        (tx, cmd_tx, &stop_requests, &review),
    );

    // ---------- session history ----------
    history::wire(
        ui,
        history,
        session_rows,
        cmd_tx,
        controller,
        &review,
        &stop_requests,
        settings,
        tx,
    );

    // ---------- clips (task2980) ----------
    clips::wire(ui, clips, clip_rows, settings);

    // One toast, shared by every producer: the settings page publishes to it,
    // the drain loop retires it (task169, round5 §3-C).
    let status_line = Rc::new(RefCell::new(toast::Toast::default()));
    // Task195: startup registration runs with no settings page open, so the
    // page's own reporting could never reach the user. Another app holding the
    // chord left the hotkeys quietly dead until someone went looking in the log.
    // t260928-7e9a: one toast, naming the chord, with 「設定を開く」 -- the
    // second, plainer line that used to replace it right after is gone.
    report_hotkey_startup(
        ui,
        &status_line,
        hotkey_registration,
        &stored.hotkey,
        cmd_tx,
    );
    settings_page::wire(
        ui,
        settings,
        stored,
        hotkeys,
        &status_line,
        controller,
        &tray.as_weak(),
    );
    // After the settings wiring: the rows live on the same screen, and the
    // first-run consent has to find `SettingsVm` already pushed (task t260920-2be2).
    update_check::wire(ui, settings);

    // ---------- review timeline (task127), continued ----------
    let review_segments = Rc::new(VecModel::<TrackSpan>::default());
    let review_gaps = Rc::new(VecModel::<GapSpan>::default());
    let review_markers = Rc::new(VecModel::<MarkerSpan>::default());
    let review_marker_rows = Rc::new(VecModel::<MarkerRow>::default());
    review_wiring::wire(
        ui,
        &review,
        (
            &review_segments,
            &review_gaps,
            &review_markers,
            &review_marker_rows,
        ),
        settings,
        cmd_tx,
        controller,
        &stop_requests,
        &status_line,
    );
    review_stage::wire(
        ui,
        &review,
        &review_segments,
        &review_gaps,
        &review_markers,
        &review_marker_rows,
        settings,
        (exporter, export_slot),
        &status_line,
    );

    shell::wire(ui, settings, cmd_tx);

    // ---------- tray menu (task130) ----------
    wire_tray_menu(tray, ui, controller, &stop_requests);
    WiredPages {
        review,
        review_segments,
        review_gaps,
        review_markers,
        review_marker_rows,
        status_line,
        stop_requests,
    }
}

/// The sending half of the control surface (task3750): writes the command
/// beside `settings.json` and exits, so the running instance picks it up on its
/// next drain tick. Returns whether this process was a send.
///
/// The command is validated here as well as there. A typo is then a non-zero
/// exit on the sweep's own line rather than a refusal buried in the app's log,
/// and a malformed request never reaches the file at all.
///
/// The path it wrote is printed because that is the only thing that tells a
/// caller which filesystem view it landed in: a sandboxed shell's `%APPDATA%`
/// can be a shadow of the real one with the same mtime and size (task3720), and
/// a command written into the shadow is one the running app will never see.
#[cfg(feature = "control")]
fn run_control_request_if_requested() -> bool {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let Some(index) = args.iter().position(|arg| arg == "--liveback-control") else {
        return false;
    };
    let Some(command) = args.get(index + 1) else {
        eprintln!(r#"--liveback-control needs a command, e.g. "range 12 34.5""#);
        return true;
    };
    let command = command.to_string_lossy();
    if let Err(error) = livia::ui_state::control::parse_control_command(&command) {
        eprintln!("--liveback-control: {error}");
        return true;
    }
    let Some(directory) = livia::handoff::request_directory() else {
        eprintln!("--liveback-control: no settings folder to leave the request in");
        return true;
    };
    match livia::handoff::write_control_request(&directory, &command) {
        Ok(()) => println!(
            "{}",
            livia::handoff::control_request_path(&directory).display()
        ),
        Err(error) => eprintln!("--liveback-control: {error}"),
    }
    true
}

/// What `start_up` hands the rest of `main`.
struct Startup {
    instance: desktop::SingleInstance,
    requested: Option<std::path::PathBuf>,
    stored: livia::settings::AppSettings,
    settings: Arc<Mutex<livia::settings::AppSettings>>,
    controller: CaptureController,
    /// The wgpu (Direct3D 12) renderer was selected, so the stage can take the
    /// playback picture on the GPU (t260917-66c2).
    wgpu_renderer: bool,
}

/// Direct3D 12 through wgpu, presented so the window's alpha reaches the
/// desktop (t260916-ebdb): the ground is drawn at 85 % and whatever is behind
/// the window shows through the remaining 15 %.
///
/// Two switches, and the first is what makes the second do anything:
///
/// - `DxgiFromVisual`: wgpu-hal's default DX12 swapchain is
///   `CreateSwapChainForHwnd`, whose only composite alpha mode is `Opaque`.
///   The visual kind goes through DirectComposition
///   (`CreateSwapChainForComposition`) and offers `PreMultiplied`
///   (`wgpu-hal-30.0.1/src/dx12/adapter.rs::surface_capabilities`).
/// - `with_transparent(true)`: slint's winit backend only asks its surface for
///   a translucent alpha mode when the window attributes say `transparent`,
///   and on Windows slint never sets that itself (`wants_transparent` is
///   macOS-only). The surface's `set_transparent` then picks `PreMultiplied`
///   if it is offered and **silently does nothing** if it is not -- so without
///   the first switch this one is a no-op, with no error anywhere
///   (`i-slint-renderer-skia-1.18.0/wgpu_30_surface.rs`).
///
/// `require_d3d()` also rendered through wgpu's DX12 backend, but with the
/// HWND swapchain and no way to pick another. What `Automatic` changes besides
/// the swapchain: the device is requested with slint's `WGPUSettings`
/// defaults (no optional features, `downlevel_webgl2_defaults` raised to the
/// adapter's texture size) instead of everything the adapter offers. Skia
/// draws through the raw D3D12 device, so nothing here depends on either.
fn select_see_through_d3d() -> Result<(), slint::PlatformError> {
    use slint::wgpu_30::{wgpu, WGPUConfiguration, WGPUSettings};
    use slint::winit_030::winit::platform::windows::WindowAttributesExtWindows;

    let mut settings = WGPUSettings::default();
    settings.backends = wgpu::Backends::DX12;
    settings.backend_options.dx12.presentation_system = wgpu::Dx12SwapchainKind::DxgiFromVisual;
    slint::BackendSelector::new()
        .require_wgpu_30(WGPUConfiguration::Automatic(settings))
        .with_winit_window_attributes_hook(|attributes| {
            attributes
                .with_transparent(true)
                .with_no_redirection_bitmap(true)
        })
        .select()
}

/// Everything that has to happen before a window exists: the two side
/// launches this executable answers, the single-instance claim, the backend
/// choice, logging and crash traps, and the settings the rest is built from.
///
/// `None` means this process was one of those side launches (or a second
/// launch that only had to raise the running window) and should exit quietly.
fn start_up() -> Option<Startup> {
    if livia::run_export_helper_if_requested() {
        return None;
    }

    // `--liveback-inspect <path.lvb>`: dump a session container and exit
    // (task401). Beside the helper branch and for the same reason -- it must
    // never reach the single-instance claim, or asking about a file while the
    // app is running would just raise the existing window.
    if livia::run_container_inspect_if_requested() {
        return None;
    }

    // `--liveback-control "<command>"`: leave one line for the running instance
    // and exit (task3750). Beside the inspect branch and for the same reason --
    // past the single-instance claim this process would either raise the running
    // window (which is exactly what a control command must not do) or become a
    // second app.
    #[cfg(feature = "control")]
    if run_control_request_if_requested() {
        return None;
    }

    // Second launch: the running instance was told to come forward, and this
    // process has nothing left to do. Must come before the window and before
    // the tray, or a second icon would flash into the notification area
    // (task130). After the helper branch, though -- an export helper is a
    // deliberate second process.
    let requested = requested_session_file();
    let instance = desktop::claim_single_instance(requested.as_deref())?;

    // Direct3D has to be asked for explicitly (task120). Under slint 1.17 that
    // was because the Windows `DefaultSurface` was Skia's *software* surface --
    // OpenGL was excluded there, Metal is Apple-only and Vulkan off by default --
    // so `LIVEBACK_RENDERER=software` only had to *skip* the request to land on
    // software. slint 1.18 deleted the hand-written native surfaces and renders
    // through wgpu (task2920), and its Windows candidate list is wgpu first,
    // software last (`i-slint-renderer-skia-1.18.0/lib.rs::create_default_surface`),
    // so skipping now lands on D3D12-via-wgpu. The software hook -- a debug hook
    // for telling drawing-side artefacts apart from playback-side ones
    // (task2760) -- therefore names the renderer instead of relying on the
    // fallback order. Which way it went is logged below, not here -- no
    // subscriber exists yet.
    //
    // The D3D branch is `select_see_through_d3d`, not `require_d3d()`: the
    // window's ground is drawn at 85 % (t260916-ebdb) and has to reach the
    // desktop that way.
    let renderer = if std::env::var("LIVEBACK_RENDERER").as_deref() == Ok("software") {
        if let Err(error) = slint::BackendSelector::new()
            .renderer_name("skia-software".into())
            .select()
        {
            eprintln!("Skia's software renderer unavailable, falling back to the default backend: {error}");
            "default"
        } else {
            "software"
        }
    } else if let Err(error) = select_see_through_d3d() {
        eprintln!("Direct3D unavailable, falling back to the default backend: {error}");
        "default"
    } else {
        "d3d"
    };

    livia::logging::configure_logging();

    // Immediately after the subscriber, and before anything that could fault:
    // the traps write through `tracing`, so installing them any earlier would
    // send a crash record nowhere (task750).
    livia::crash::install();
    livia::crash::run_crash_test_if_requested();

    tracing::info!(event = "renderer_selected", renderer);

    // `load_and_migrate`, not plain `load`: a pre-v2 settings file is migrated
    // in memory either way, but only this persists the result, so the
    // migration (and its view-mode reset) does not re-run on every launch.
    let stored = settings_path()
        .map(|path| {
            let (stored, error) = settings::load_and_migrate(&path);
            if let Some(error) = error {
                tracing::warn!(%error, "settings migration write-back failed");
            }
            stored
        })
        .unwrap_or_default();
    let settings = Arc::new(Mutex::new(stored.clone()));
    // Before the controller: `new()` scans the buffer for closed sessions, and
    // scanning the default folder when the user moved it would list nothing
    // (task164).
    if let Some(directory) = stored.buffer_directory.clone() {
        if let Err(error) = CaptureController::apply_buffer_root(Some(directory.into())) {
            tracing::warn!(%error, "buffer directory from settings could not be used");
        }
    }
    let controller = CaptureController::new();

    // Before anything is pushed: every label read below asks `tr_locale()`,
    // and the capture worker asks `locale::active()` when it names a monitor
    // session (task1150).
    livia::ui_state::locale::set_active(resolve_locale(&stored));

    Some(Startup {
        instance,
        requested,
        stored,
        settings,
        controller,
        wgpu_renderer: renderer == "d3d",
    })
}
