//! Window-chrome and OS-shell helpers (task120): native drag / maximize,
//! the title-bar clock, window-state persistence, `explorer.exe` launching,
//! and the chrome callback wiring moved out of `main`.

use std::cell::RefCell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use livia::settings::{self, AppSettings, WindowState};
use livia::ui_state::shell as shell_state;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::ComponentHandle;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    MonitorFromPoint, MonitorFromRect, ScreenToClient, HMONITOR, MONITOR_DEFAULTTONULL,
    MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, IsZoomed, PostMessageW, ShowWindow, HTCAPTION, SW_MAXIMIZE, SW_MINIMIZE,
    SW_RESTORE, WM_NCLBUTTONDOWN,
};

use super::settings_page::settings_path;
use super::{AppWindow, Cmd, ReviewVm, Tokens};

/// Frame pacing for the empty review screen's drift (t260927-9865): 60 writes
/// a second whatever the monitor runs at, and a slow look at the stop
/// conditions while it is still.
const DRIFT_TICK: Duration = Duration::from_micros(16_667);
const DRIFT_IDLE: Duration = Duration::from_millis(500);

thread_local! {
    // `wire` returns nothing to hold it in, and it lives as long as the UI.
    static DRIFT_TIMER: slint::Timer = slint::Timer::default();
}

/// Advances the empty review screen's drifting scene from a clock rather than
/// `animation-tick()`: every property write here is one redraw, so the window
/// presents at most 60 times a second, and nothing at all while another pane
/// is shown, the window is minimized or in the tray, or motion is reduced.
fn start_drift(ui: &AppWindow) {
    use livia::ui_state::timeline as tl;
    let weak = ui.as_weak();
    let clock = RefCell::new(tl::DriftClock::default());
    DRIFT_TIMER.with(|timer| {
        timer.start(slint::TimerMode::Repeated, DRIFT_IDLE, move || {
            let Some(ui) = weak.upgrade() else { return };
            let running = tl::drift_runs(
                ui.get_active_pane() == 0 && !ui.get_session_loaded(),
                hwnd_of(ui.window()).is_some_and(super::desktop::is_window_on_screen),
                ui.global::<Tokens>().get_reduce_motion(),
            );
            let run = clock.borrow_mut().tick(Instant::now(), running);
            let want = if running { DRIFT_TICK } else { DRIFT_IDLE };
            DRIFT_TIMER.with(|timer| {
                if timer.interval() != want {
                    timer.set_interval(want);
                }
            });
            if running {
                let phases = tl::drift_phases(run);
                let vm = ui.global::<ReviewVm>();
                vm.set_drift_back(phases.back);
                vm.set_drift_front(phases.front);
                vm.set_drift_pulse(phases.pulse);
            }
        });
    });
}

pub(super) fn hwnd_of(window: &slint::Window) -> Option<HWND> {
    let handle = window.window_handle();
    match handle.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(win32) => Some(HWND(win32.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// The `task1580_stage` line, shared by the review stage and the clip stage so
/// the two callers of `stage_target` can never log different things
/// (task t260914-a025). Besides the target and the `scale_factor()` it was
/// derived from, it carries the window's dpi, the size slint believes is
/// physical (`Window::size()`) and the size that really is (`GetClientRect`):
/// `client_physical / slint_physical` of 1.0 means the stage rasterises for
/// the surface it is actually drawn on. Instrumentation only -- nothing here
/// feeds back into the target.
pub(super) fn log_stage_resize(
    window: &slint::Window,
    page: &'static str,
    target: Option<(u32, u32)>,
) {
    let hwnd = hwnd_of(window);
    let dpi = hwnd.map(super::desktop::window_dpi);
    let client = hwnd.and_then(super::desktop::client_size);
    let believed = window.size();
    tracing::info!(
        target: "task1580_stage",
        page,
        width = target.map(|(width, _)| width),
        height = target.map(|(_, height)| height),
        scale = window.scale_factor(),
        fullscreen = window.is_fullscreen(),
        dpi,
        slint_physical_width = believed.width,
        slint_physical_height = believed.height,
        client_physical_width = client.map(|(width, _)| width),
        client_physical_height = client.map(|(_, height)| height),
        "stage resize"
    );
}

/// Hands the drag back to Windows instead of tracking the pointer here, so Aero
/// Snap, edge-drag and Win+arrow behave exactly as they do for any other window.
fn begin_native_drag(hwnd: HWND) {
    unsafe {
        // Fails only when this thread held no capture, which is the ordinary
        // case: the press that got us here was already released by the time
        // slint reported it. Nothing to warn about (task260).
        let _ = ReleaseCapture();
        // Posted, never sent (task1610). `SendMessageW` runs `DefWindowProc`'s
        // modal move loop *inside* this TouchArea callback, i.e. inside winit's
        // event dispatch -- and winit buffers every event it gets while a
        // dispatch is already on the stack. So the WM_SIZE from the drag-restore
        // never reaches slint: no relayout, no repaint, and the swap chain keeps
        // presenting the maximized-size frame stretched into the smaller window,
        // which is the whole UI appearing to shrink until the button comes up.
        // Posting lets this callback return first, so the move loop starts from
        // the pump at the normal dispatch point -- where a native title bar
        // starts it -- and redraws keep running for the length of the drag.
        let _ = PostMessageW(
            Some(hwnd),
            WM_NCLBUTTONDOWN,
            WPARAM(HTCAPTION as usize),
            LPARAM(0),
        );
    }
}

/// Where the pointer is, in the window's own logical pixels -- what slint's
/// `dispatch_event` wants. `None` when Windows will not say.
///
/// The dpi comes from the hwnd, not from `Window::scale_factor()`: task4120
/// measured that factor reporting 1.0 while `GetDpiForWindow` said 144, which
/// is the same trap `apply_resize_band` sidesteps.
fn pointer_logical_position(hwnd: HWND) -> Option<slint::LogicalPosition> {
    let mut pt = POINT::default();
    unsafe {
        GetCursorPos(&mut pt).ok()?;
        // `BOOL`, not a `Result` like the call above it.
        if !ScreenToClient(hwnd, &mut pt).as_bool() {
            return None;
        }
    }
    let scale = super::desktop::window_dpi(hwnd) as f32 / 96.0;
    Some(slint::LogicalPosition::new(
        pt.x as f32 / scale,
        pt.y as f32 / scale,
    ))
}

fn toggle_maximize(hwnd: HWND) {
    unsafe {
        // Previous visibility, not success (task260).
        let _ = ShowWindow(
            hwnd,
            if IsZoomed(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_MAXIMIZE
            },
        );
    }
}

/// How much of the top of the window has to land on a monitor for the saved
/// position to still be usable: roughly a title bar, i.e. enough to grab and
/// drag the window back (task2650).
const GRABBABLE_TITLE_BAR_PX: i64 = 40;

/// The monitor the saved rectangle's title bar still overlaps, if any.
/// Guards the monitor-configuration change (task2650): a position saved on a
/// 4K screen restores off-screen on a 1080p setup, with no way to reach the
/// window. A plain `clamp` is wrong here -- x=2229 is a legitimate coordinate
/// on the second of two side-by-side monitors and must survive untouched.
/// `MONITOR_DEFAULTTONULL` answers exactly this question: null means the rect
/// intersects no monitor at all.
fn title_bar_monitor(state: &WindowState) -> Option<HMONITOR> {
    let rect = RECT {
        left: state.x as i32,
        top: state.y as i32,
        right: state.x.saturating_add(state.width) as i32,
        bottom: state.y.saturating_add(GRABBABLE_TITLE_BAR_PX) as i32,
    };
    let monitor = unsafe { MonitorFromRect(&rect, MONITOR_DEFAULTTONULL) };
    (!monitor.is_invalid()).then_some(monitor)
}

/// The effective scale of `monitor` (dpi / 96); 1.0 if Windows cannot say.
fn monitor_scale(monitor: HMONITOR) -> f64 {
    let (mut dpi_x, mut dpi_y) = (0u32, 0u32);
    match unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) } {
        Ok(()) if dpi_x > 0 => f64::from(dpi_x) / 96.0,
        _ => 1.0,
    }
}

pub(super) fn restore_window_state(window: &slint::Window, state: Option<WindowState>) {
    let Some(state) = state else { return };
    if state.width <= 0 || state.height <= 0 {
        return;
    }
    // Position is dropped -- not clamped -- when it is unreachable: the size is
    // still what the user chose, and letting the OS place the window puts it
    // somewhere visible.
    let monitor = match title_bar_monitor(&state) {
        Some(monitor) => {
            window.set_position(slint::PhysicalPosition::new(state.x as i32, state.y as i32));
            monitor
        }
        // The OS places the window on the primary (winit creates at CW_USEDEFAULT).
        None => unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) },
    };
    // Logical, not physical: slint 1.18 keeps a pre-window physical size as
    // physical, winit applies it on the primary and then keeps the *logical*
    // size across the move to the saved monitor, dividing by the primary's
    // scale (t260918-3a94). Stored values stay physical (`persist_window_state`).
    let (width, height) =
        shell_state::restore_logical_size(state.width, state.height, monitor_scale(monitor));
    window.set_size(slint::LogicalSize::new(width, height));
    if state.maximized {
        // Runs before `ui.run()`, so there is no HWND yet; slint's own API
        // stores this on the winit attributes and the window is created
        // maximized. Set last, so the geometry above becomes the restore rect.
        window.set_maximized(true);
    }
}

pub(super) fn persist_window_state(window: &slint::Window, settings: &Mutex<AppSettings>) {
    // Full screen reports the whole monitor (task152). Storing that would
    // restore a monitor-sized window next launch, with no way back to the
    // geometry the user actually chose.
    if window.is_fullscreen() {
        return;
    }
    let Some(path) = settings_path() else { return };
    let position = window.position();
    let size = window.size();
    if size.width == 0 || size.height == 0 {
        return;
    }
    let Ok(mut current) = settings.lock() else {
        return;
    };
    let live = WindowState {
        height: i64::from(size.height),
        maximized: false,
        width: i64::from(size.width),
        x: i64::from(position.x),
        y: i64::from(position.y),
    };
    // While maximized the reported frame *is* the monitor, so writing it would
    // throw away the geometry the user actually chose -- one maximize-and-quit
    // would be enough to lose it forever (task2650). Keep whatever is already
    // stored and only raise the flag; the live frame is the fallback for a
    // first run that has never saved anything.
    let next = if window.is_maximized() {
        WindowState {
            maximized: true,
            ..current.window_state.unwrap_or(live)
        }
    } else {
        live
    };
    if current.window_state == Some(next) {
        return;
    }
    current.window_state = Some(next);
    if let Err(error) = settings::save(&path, &current) {
        eprintln!("failed to persist window state: {error}");
    }
}

/// Opens a folder in explorer. Task130 moved this off `explorer.exe <path>`
/// onto the shell API, which reuses an already-open window on that folder
/// instead of stacking a new one.
pub(super) fn open_directory(root: &std::path::Path) -> Result<(), String> {
    super::desktop::open_in_explorer(root, None)
}

/// Opens `folder` with `item` selected -- the container session's "show in
/// folder" (task450), where there is one file rather than a folder to land in.
pub(super) fn reveal_file(folder: &std::path::Path, item: &std::path::Path) -> Result<(), String> {
    super::desktop::open_in_explorer(folder, Some(item))
}

/// Writes the geometry back, then hides the window. Geometry has to be written
/// *now*: while stashed the window may sit hidden for hours, and a later crash
/// would otherwise lose the last position the user chose.
pub(super) fn stash_in_tray(ui: &AppWindow, settings: &Mutex<AppSettings>) {
    // Leave full screen before anything else: it is what lets the geometry be
    // written at all, and coming back out of the tray still full screen with no
    // title bar would be a confusing place to land (task152).
    if ui.window().is_fullscreen() {
        super::review_wiring::set_fullscreen(ui, false, settings);
    }
    persist_window_state(ui.window(), settings);
    if let Some(hwnd) = hwnd_of(ui.window()) {
        super::desktop::hide_window(hwnd);
    }
}

/// Wires the title bar, the window buttons, and the nav rail. Everything
/// below moved verbatim out of `main` (the blocks only borrow what they used
/// to capture from it).
pub(super) fn wire(ui: &AppWindow, settings: &Arc<Mutex<AppSettings>>, cmd_tx: &Sender<Cmd>) {
    {
        // The title bar hands the press to Windows, which enters a modal move
        // loop and swallows the second click -- so slint's `double-clicked`
        // never fires on it. The double-click has to be detected before the drag
        // starts (task120).
        let weak = ui.as_weak();
        let last_press: RefCell<Option<Instant>> = RefCell::new(None);
        ui.on_titlebar_pressed(move || {
            let Some(hwnd) = weak.upgrade().and_then(|ui| hwnd_of(ui.window())) else {
                return;
            };
            let mut last = last_press.borrow_mut();
            let double = last.is_some_and(|at| at.elapsed() < Duration::from_millis(500));
            *last = Some(Instant::now());
            drop(last);
            if double {
                toggle_maximize(hwnd);
            } else {
                begin_native_drag(hwnd);
                // Windows' modal move loop eats the button-up, so slint never
                // sees the release and the title bar's TouchArea keeps the mouse
                // grab -- every later click is routed to it and the rest of the
                // UI goes dead. Release it ourselves.
                //
                // A synthetic *release*, not the `PointerExited` this used to
                // send (task120). Exit was true enough while the bar was a
                // permanent strip, but round22 (task4370) made the bar's own
                // hover the thing that keeps it out: exit clears `has_hover` on
                // every TouchArea, so `tbhover` went false at drag start and the
                // 0.3s+0.24s retract played out *during* the move loop -- the bar
                // was already gone when the button came up, with the pointer
                // still sitting on it. A release only drops the grab
                // (`i-slint-core` `handle_mouse_grab`: `EventAccepted` clears
                // `grabbed` and re-emits a `Moved` at this position), so hover is
                // recomputed from where the pointer actually is. `clicked` cannot
                // fire from it -- the drag area declares no handler for it.
                //
                // Position is read now rather than after the loop: the cursor
                // does not move relative to the window during a caption drag, so
                // it is the same answer either side of it, and it is correct
                // whichever of the two queued items the pump reaches first.
                // Deferred: we are still inside the TouchArea's own callback.
                let position = pointer_logical_position(hwnd);
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    match position {
                        Some(position) => ui.window().dispatch_event(
                            slint::platform::WindowEvent::PointerReleased {
                                position,
                                button: slint::platform::PointerEventButton::Left,
                            },
                        ),
                        // No cursor position to be truthful with; the old
                        // teardown is still better than a stuck grab.
                        None => ui
                            .window()
                            .dispatch_event(slint::platform::WindowEvent::PointerExited),
                    }
                });
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_minimize_clicked(move || {
            if let Some(hwnd) = weak.upgrade().and_then(|ui| hwnd_of(ui.window())) {
                unsafe {
                    // Previous visibility, not success (task260).
                    let _ = ShowWindow(hwnd, SW_MINIMIZE);
                }
            }
        });
    }

    {
        let weak = ui.as_weak();
        ui.on_maximize_clicked(move || {
            if let Some(hwnd) = weak.upgrade().and_then(|ui| hwnd_of(ui.window())) {
                toggle_maximize(hwnd);
            }
        });
    }

    {
        // Closing stashes the window in the tray rather than quitting, like the
        // Tauri build's `CloseRequested` -> `prevent_close()` + `hide()`
        // (task130). Quitting is the tray's 終了 item.
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.on_close_clicked(move || {
            if let Some(ui) = weak.upgrade() {
                stash_in_tray(&ui, &settings);
            }
        });
    }

    {
        // The same for Alt+F4 and anything else that asks the window to close.
        // `KeepWindowShown` plus an explicit `SW_HIDE`, not slint's
        // `HideWindow`: see `desktop::hide_window` for why the winit window has
        // to survive.
        let weak = ui.as_weak();
        let settings = settings.clone();
        ui.window().on_close_requested(move || {
            if let Some(ui) = weak.upgrade() {
                stash_in_tray(&ui, &settings);
            }
            slint::CloseRequestResponse::KeepWindowShown
        });
    }

    {
        let weak = ui.as_weak();
        let cmd_tx = cmd_tx.clone();
        ui.on_rail_clicked(move |index| {
            let Some(ui) = weak.upgrade() else { return };
            match shell_state::rail_action(index) {
                shell_state::RailAction::Show(pane) => show_pane(&ui, &cmd_tx, pane),
                // 対象 is not a destination: the pane underneath stays exactly
                // where it was, which is what makes closing the picker free --
                // and what makes the button a toggle (the user, 2026-09-20):
                // pressing キャプチャ again closes the flyout and hands the rail
                // mark back to the pane that was showing all along.
                shell_state::RailAction::OpenPicker => set_picker_open(&ui, !ui.get_picker_open()),
            }
        });
    }

    {
        // t260927-9865: the empty review screen's 「対象を選ぶ」 opens the same
        // flyout as the rail's キャプチャ -- but only opens it; a second press
        // does not close it the way the rail's toggle does.
        let weak = ui.as_weak();
        ui.global::<ReviewVm>().on_pick_target(move || {
            if let Some(ui) = weak.upgrade() {
                set_picker_open(&ui, true);
            }
        });
    }
    start_drift(ui);

    {
        let weak = ui.as_weak();
        ui.on_picker_closed(move || {
            let Some(ui) = weak.upgrade() else { return };
            if shell_state::closes_picker(ui.get_picker_open()) {
                set_picker_open(&ui, false);
            }
        });
    }
}

/// The one place the pane changes (task169): the rail, the empty state's
/// 履歴を開く, and a session load after the history closes all funnel through
/// here, so the list request can never disagree with what is on screen.
pub(super) fn show_pane(ui: &AppWindow, cmd_tx: &Sender<Cmd>, next: shell_state::Pane) {
    // task2130: a running export blocks the screen, and leaving the review pane
    // would take the cancel button with it -- the rail's cover would then block
    // the way back. Guarded here rather than on the rail because the toast's
    // action button sits above the cover and can call in too.
    if ui.global::<ReviewVm>().get_export_busy() {
        return;
    }
    // The rail stays clickable while the picker is open (its catcher stops at
    // the rail's right edge), so every route into a pane is also a route out of
    // the picker -- otherwise the flyout would hang over the pane it just
    // switched to.
    set_picker_open(ui, false);
    let previous = shell_state::Pane::from_index(ui.get_active_pane());
    ui.set_active_pane(next.index());
    ui.set_rail_current(shell_state::rail_current(next));
    // Once per open, like the React overlay's mount effect -- not a permanent
    // poll, which task074 ruled out.
    if shell_state::opens_sessions(previous, next) {
        let _ = cmd_tx.send(Cmd::ListSessions);
    }
    // The clip list is the folder itself, with no index behind it (task2980),
    // so it is re-scanned the same way and for the same reason -- a file added
    // or removed in Explorer has to show up without restarting the app.
    if shell_state::opens_clips(previous, next) {
        let _ = cmd_tx.send(Cmd::ListClips);
    }
    // task4030: leaving destroys the pane's search field, so its query goes
    // with it -- through the field's own handler, which also re-renders the
    // list unfiltered and drops what the filter was holding open.
    //
    // Deferred to the next turn of the event loop: those handlers borrow the
    // history / clips `RefCell`, and the drain tick calls in here holding both
    // (a session load closing the history) -- invoking them inline panicked
    // with `RefCell already borrowed` on every playback from the history.
    let leaves_sessions = shell_state::leaves(previous, next, shell_state::Pane::Sessions);
    let leaves_clips = shell_state::leaves(previous, next, shell_state::Pane::Clips);
    if leaves_sessions || leaves_clips {
        let weak = ui.as_weak();
        slint::Timer::single_shot(std::time::Duration::ZERO, move || {
            let Some(ui) = weak.upgrade() else { return };
            if leaves_sessions {
                ui.invoke_sessions_search_changed("".into());
            }
            if leaves_clips {
                ui.invoke_clips_search_changed("".into());
            }
        });
    }
}

/// Opening and closing the picker leaves the pane behind -- and, since
/// t260927-4c04, the rail's mark -- untouched, which is the whole reason 対象
/// is a bool and not a pane. The rail reads `picker-open` itself for
/// キャプチャ's open look.
pub(super) fn set_picker_open(ui: &AppWindow, open: bool) {
    // The target worker's thumbnail round-robin rides on this (task1170). This
    // is the only function that writes `picker-open`, so the flag and the modal
    // cannot disagree.
    super::picker::PICKER_VISIBLE.store(open, std::sync::atomic::Ordering::Relaxed);
    ui.set_picker_open(open);
}
