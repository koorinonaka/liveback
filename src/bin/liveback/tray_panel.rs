//! The tray panel (t261108-224b, DS TrayMenu Version 42): what a right-click
//! on the notification-area icon opens, drawn by the app instead of the
//! Windows menu `tray.slint` used to carry.
//!
//! slint's tray handles the right-click itself, in the vendored
//! `i-slint-core/items/system_tray/windows.rs` (`wnd_proc`): `WM_TRAYICON`
//! (`WM_APP + 1`) with the mouse event in lparam's low word, and on
//! `WM_RBUTTONUP` / `WM_CONTEXTMENU` it pops its HMENU -- of which there is none
//! now. Rather than patch the vendored crate, this subclasses the tray's
//! message window (`SlintSystemTrayWindow`, found by class and pid the same way
//! `desktop::forward_taskbar_created` does) and opens the panel there, ahead of
//! slint. The two private details it relies on are pinned by a test that reads
//! the vendored source.
//!
//! The panel is a second top-level slint window. It is placed over the icon
//! (`Shell_NotifyIconGetRect`; that fails while the icon sits in the overflow,
//! and the cursor stands in), made a tool window so it takes no taskbar button,
//! brought to the foreground so the keyboard reaches it and a click elsewhere
//! deactivates it, and hidden on that deactivation, on Esc and after an action.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

use slint::{ComponentHandle, Model, ModelRc, VecModel};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Shell::{
    DefSubclassProc, SetWindowSubclass, Shell_NotifyIconGetRect, NOTIFYICONIDENTIFIER,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
    ShowWindow, GWL_EXSTYLE, HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE,
    SW_SHOW, WA_INACTIVE, WM_ACTIVATE, WM_APP, WM_CONTEXTMENU, WM_RBUTTONUP, WS_EX_APPWINDOW,
    WS_EX_TOOLWINDOW,
};

use livia::ui_state::lifecycle;

use super::shell::hwnd_of;
use super::{tr_locale, TallyApp, TallyState, Tokens, TrayPanel, TrayTarget};

/// slint's tray callback message (`WM_TRAYICON` in the vendored source).
const WM_TRAYICON: u32 = WM_APP + 1;
/// slint registers its one icon under this uID.
const TRAY_UID: u32 = 1;
/// The panel's width, the DS TrayMenu README's 272 (outer, edge included).
const PANEL_WIDTH: f32 = 272.0;
/// Between the taskbar and the panel, and the panel and the work area's edges.
const GAP: f32 = 8.0;

/// A running capture as the picker knows it: session, the name shown, its mark.
pub(super) type RunningTarget = (String, String, TallyApp);

thread_local! {
    /// The captures `picker::render_titlebar_tally` last saw, in its order.
    static RUNNING: RefCell<Vec<RunningTarget>> = const { RefCell::new(Vec::new()) };
    /// When each session was first seen running: the elapsed time's origin.
    static STARTED: RefCell<HashMap<String, Instant>> = RefCell::new(HashMap::new());
    /// Opens the panel; set by `wire`, called by the tray subclass.
    static OPEN: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
    /// Re-renders an open panel; set by `wire`.
    static REFRESH: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// The picker's running list moved (or a mark arrived). Remembers when each
/// session first appeared and redraws the panel if it is open.
pub(super) fn set_running(running: Vec<RunningTarget>) {
    let now = Instant::now();
    STARTED.with(|started| {
        let mut started = started.borrow_mut();
        started.retain(|session, _| running.iter().any(|(id, _, _)| id == session));
        for (session, _, _) in &running {
            started.entry(session.clone()).or_insert(now);
        }
    });
    let changed = RUNNING.with(|current| {
        let mut current = current.borrow_mut();
        let differs = current.len() != running.len()
            || current
                .iter()
                .zip(&running)
                .any(|(a, b)| a.0 != b.0 || a.1 != b.1 || a.2 != b.2);
        *current = running;
        differs
    });
    if changed {
        if let Some(refresh) = REFRESH.with(|r| r.borrow().clone()) {
            refresh();
        }
    }
}

/// Everything the panel's actions reach.
pub(super) struct Actions {
    pub open_window: Box<dyn Fn()>,
    pub start: Box<dyn Fn()>,
    pub stop: Box<dyn Fn(&str)>,
    pub stop_all: Box<dyn Fn()>,
    pub quit: Box<dyn Fn()>,
    /// Whether an auto-capture rule is registered (the standby heading).
    pub standby: Box<dyn Fn() -> bool>,
    /// The app's theme, read when the panel opens.
    pub light: Box<dyn Fn() -> bool>,
    pub reduce_motion: bool,
}

/// Builds the panel and hooks the tray's right-click to it. The panel lives
/// for the rest of the process (it is held by the closures below).
pub(super) fn wire(actions: Actions) -> Result<(), slint::PlatformError> {
    let panel = TrayPanel::new()?;
    panel
        .global::<Tokens>()
        .set_reduce_motion(actions.reduce_motion);
    let actions = Rc::new(actions);
    // The panel's window, once slint has made it. slint's `hide()` drops the
    // OS window and the next `show()` builds a fresh one -- at (0,0), 16x16,
    // without the tool-window style or the subclass (measured on the first
    // run of verify.ps1) -- so after the first `show()` the window is only
    // ever hidden and shown with Win32.
    let hwnd: Rc<Cell<Option<HWND>>> = Rc::new(Cell::new(None));
    let visible = Rc::new(Cell::new(false));
    let ticker = Rc::new(slint::Timer::default());

    let render = {
        let weak = panel.as_weak();
        let actions = actions.clone();
        move || {
            let Some(panel) = weak.upgrade() else { return };
            render(&panel, (actions.standby)());
        }
    };
    let render: Rc<dyn Fn()> = Rc::new(render);

    let hide: Rc<dyn Fn()> = {
        let weak = panel.as_weak();
        let ticker = ticker.clone();
        let hwnd = hwnd.clone();
        let visible = visible.clone();
        Rc::new(move || {
            ticker.stop();
            visible.set(false);
            if let Some(panel) = weak.upgrade() {
                panel.set_shown(false);
            }
            if let Some(hwnd) = hwnd.get() {
                // SAFETY: our own window.
                let _ = unsafe { ShowWindow(hwnd, SW_HIDE) };
            }
        })
    };

    let refresh: Rc<dyn Fn()> = {
        let render = render.clone();
        let visible = visible.clone();
        Rc::new(move || {
            if visible.get() {
                render();
            }
        })
    };
    {
        // A row came or went: the window changed height. Read here, after
        // slint has settled the new height -- read straight after the model
        // changed, `panel-height` still held the old one (verify.ps1: placed
        // for 150, drawn 186 tall, 28px over the taskbar).
        let weak = panel.as_weak();
        let visible = visible.clone();
        panel.on_relayout(move || {
            if let Some(panel) = weak.upgrade().filter(|_| visible.get()) {
                place(&panel, None);
            }
        });
    }
    REFRESH.with(|r| *r.borrow_mut() = Some(refresh));

    let open: Rc<dyn Fn()> = {
        let weak = panel.as_weak();
        let actions = actions.clone();
        let render = render.clone();
        let ticker = ticker.clone();
        let hide = hide.clone();
        Rc::new(move || {
            let Some(panel) = weak.upgrade() else { return };
            let mut cursor = POINT::default();
            // SAFETY: a valid out pointer.
            let _ = unsafe { GetCursorPos(&mut cursor) };
            panel.global::<Tokens>().set_light((actions.light)());
            render();
            place(&panel, Some(cursor));
            if hwnd.get().is_none() {
                if panel.show().is_err() {
                    return;
                }
                let Some(made) = hwnd_of(panel.window()) else {
                    return;
                };
                hwnd.set(Some(made));
                become_tool_window(made);
                super::desktop::round_window_corners(made);
                watch_deactivation(made, hide.clone());
            }
            if let Some(hwnd) = hwnd.get() {
                set_border_color(hwnd, panel.global::<Tokens>().get_light());
                // SAFETY: plain calls on our own window.
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOW);
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                    // Without the foreground the keyboard never reaches the
                    // panel and a click elsewhere never deactivates it (the
                    // same reason slint's own menu calls it, Q135788).
                    let _ = SetForegroundWindow(hwnd);
                }
            }
            visible.set(true);
            panel.set_opened(panel.get_opened().wrapping_add(1));
            panel.set_shown(true);
            let weak = panel.as_weak();
            let render = render.clone();
            ticker.start(
                slint::TimerMode::Repeated,
                std::time::Duration::from_secs(1),
                move || {
                    if weak.upgrade().is_some() {
                        render();
                    }
                },
            );
            tracing::info!(event = "tray_panel_opened", "tray panel opened");
        })
    };
    OPEN.with(|o| *o.borrow_mut() = Some(open));

    {
        let hide = hide.clone();
        panel.on_dismiss(move || hide());
    }
    {
        // Stays open: the row going away is the answer (DS TrayMenu 閉じ方).
        let actions = actions.clone();
        panel.on_stop(move |session| (actions.stop)(session.as_str()));
    }
    {
        let actions = actions.clone();
        let hide = hide.clone();
        panel.on_stop_all(move || {
            hide();
            (actions.stop_all)();
        });
    }
    {
        let actions = actions.clone();
        let hide = hide.clone();
        panel.on_start(move || {
            hide();
            (actions.start)();
        });
    }
    {
        let actions = actions.clone();
        let hide = hide.clone();
        panel.on_open_window(move || {
            hide();
            (actions.open_window)();
        });
    }
    {
        let actions = actions.clone();
        panel.on_quit(move || {
            hide();
            (actions.quit)();
        });
    }

    // The tray window is created with the `TrayIcon` component; looked for
    // until found, like the main window's chrome timer.
    let finder = slint::Timer::default();
    let tries = Cell::new(0u32);
    finder.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(100),
        move || {
            tries.set(tries.get() + 1);
            let windows = super::desktop::own_tray_windows();
            for tray in &windows {
                subclass_tray_window(*tray);
            }
            if !windows.is_empty() || tries.get() >= 100 {
                tracing::info!(
                    event = "tray_panel_hooked",
                    windows = windows.len(),
                    tries = tries.get(),
                    "tray right-click hooked"
                );
                FINDER.with(|f| f.borrow_mut().take());
            }
        },
    );
    FINDER.with(|f| *f.borrow_mut() = Some(finder));
    PANEL.with(|p| *p.borrow_mut() = Some(panel));
    Ok(())
}

thread_local! {
    static FINDER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    static PANEL: RefCell<Option<TrayPanel>> = const { RefCell::new(None) };
}

/// Pushes the words, the heading and the rows.
fn render(panel: &TrayPanel, standby: bool) {
    let locale = tr_locale();
    let now = Instant::now();
    let rows: Vec<TrayTarget> = RUNNING.with(|running| {
        STARTED.with(|started| {
            let started = started.borrow();
            running
                .borrow()
                .iter()
                .map(|(session, name, app)| TrayTarget {
                    session: session.as_str().into(),
                    name: name.as_str().into(),
                    app: app.clone(),
                    elapsed: lifecycle::tray_elapsed(
                        started
                            .get(session)
                            .map_or(0, |at| now.duration_since(*at).as_secs()),
                    )
                    .into(),
                })
                .collect()
        })
    });
    let count = rows.len();
    panel.set_state(match lifecycle::tray_lamp(count, standby) {
        lifecycle::TrayLamp::Live => TallyState::Live,
        lifecycle::TrayLamp::Standby => TallyState::Standby,
        lifecycle::TrayLamp::Off => TallyState::Off,
    });
    panel.set_heading(lifecycle::tray_heading(locale, count, standby).into());
    panel.set_stop_label(lifecycle::tray_stop_capture(locale).into());
    panel.set_stop_all_label(lifecycle::tray_stop_all(locale).into());
    panel.set_start_label(lifecycle::tray_start(locale).into());
    panel.set_open_label(lifecycle::tray_open(locale).into());
    panel.set_quit_label(lifecycle::tray_quit_label(locale, count).into());
    // The elapsed times change every second. Writing them into the rows in
    // place keeps each row (and its stop button's hover and press) alive; a
    // new model would rebuild every row once a second, and a press spanning
    // the rebuild was lost (verify.ps1: the quit button vanished under UIA's
    // Invoke once in three runs).
    let current = panel.get_targets();
    let same_rows = current.row_count() == rows.len()
        && rows.iter().enumerate().all(|(i, row)| {
            current
                .row_data(i)
                .is_some_and(|c| c.session == row.session)
        });
    if same_rows {
        for (i, row) in rows.into_iter().enumerate() {
            if current.row_data(i).as_ref() != Some(&row) {
                current.set_row_data(i, row);
            }
        }
    } else {
        panel.set_targets(ModelRc::new(VecModel::from(rows)));
    }
}

/// Sizes and places the panel window. `cursor` is where the right-click
/// landed, used when the icon's own rect cannot be had; `None` keeps the
/// anchor the last opening used.
fn place(panel: &TrayPanel, cursor: Option<POINT>) {
    thread_local! {
        static ANCHOR: Cell<(i32, i32)> = const { Cell::new((0, 0)) };
    }
    if let Some(cursor) = cursor {
        ANCHOR.with(|a| a.set((cursor.x, cursor.y)));
    }
    let cursor = ANCHOR.with(Cell::get);
    let icon = icon_rect();
    let probe = icon.map_or(
        POINT {
            x: cursor.0,
            y: cursor.1,
        },
        |r| POINT {
            x: (r.left + r.right) / 2,
            y: (r.top + r.bottom) / 2,
        },
    );
    // SAFETY: plain queries with valid out structs.
    let (work, monitor, dpi) = unsafe {
        let hmon = MonitorFromPoint(probe, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(hmon, &mut info);
        let (mut x, mut y) = (96, 96);
        let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut x, &mut y);
        (info.rcWork, info.rcMonitor, x.max(1))
    };
    let scale = dpi as f32 / 96.0;
    let size = (
        (PANEL_WIDTH * scale).round() as i32,
        (panel.get_panel_height() * scale).round() as i32,
    );
    let (x, y) = position(
        icon.map(Rect::from),
        cursor,
        Rect::from(work),
        Rect::from(monitor),
        size,
        (GAP * scale).round() as i32,
    );
    panel
        .window()
        .set_position(slint::PhysicalPosition::new(x, y));
    tracing::debug!(
        event = "tray_panel_placed",
        x,
        y,
        width = size.0,
        height = size.1,
        dpi,
        icon_rect = icon.is_some(),
        "tray panel placed"
    );
}

/// The icon's rect in physical screen pixels, or `None` while it sits in the
/// overflow flyout (`Shell_NotifyIconGetRect` fails then; KNOWLEDGE.md).
fn icon_rect() -> Option<RECT> {
    let tray = *super::desktop::own_tray_windows().first()?;
    let id = NOTIFYICONIDENTIFIER {
        cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: tray,
        uID: TRAY_UID,
        ..Default::default()
    };
    // SAFETY: a filled-in identifier for our own icon.
    unsafe { Shell_NotifyIconGetRect(&id) }.ok()
}

/// A screen rectangle, edges in physical pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

impl From<RECT> for Rect {
    fn from(r: RECT) -> Self {
        Self {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

/// Where the panel's top-left goes (DS TrayMenu 位置と動き). Over the icon,
/// `gap` clear of the taskbar, its right edge on the icon's right edge; on the
/// far side of a taskbar at the top or a side; from the cursor when the icon's
/// rect is unknown. Always pulled inside the work area, `gap` off its edges.
fn position(
    icon: Option<Rect>,
    cursor: (i32, i32),
    work: Rect,
    monitor: Rect,
    (w, h): (i32, i32),
    gap: i32,
) -> (i32, i32) {
    let anchor_x = icon.map_or(cursor.0, |r| r.right);
    let (x, y) = if work.bottom < monitor.bottom {
        (anchor_x - w, work.bottom - gap - h)
    } else if work.top > monitor.top {
        (anchor_x - w, work.top + gap)
    } else if work.left > monitor.left {
        (work.left + gap, icon.map_or(cursor.1, |r| r.bottom) - h)
    } else if work.right < monitor.right {
        (
            work.right - gap - w,
            icon.map_or(cursor.1, |r| r.bottom) - h,
        )
    } else {
        // The taskbar hides itself: open over the click, like a menu.
        (cursor.0 - w, cursor.1 - h)
    };
    (
        x.clamp(work.left + gap, (work.right - gap - w).max(work.left + gap)),
        y.clamp(work.top + gap, (work.bottom - gap - h).max(work.top + gap)),
    )
}

/// DWM draws its own 1px border over the window's edge (Windows 11), dark by
/// default -- measured `#3a3a3a` around the light panel. It is given the DS
/// edge instead: `line-strong` over `overlay`, `0x00bbggrr`.
///   light rgba(20,24,33,.16) over #ffffff -> #d9dadc
///   dark  rgba(255,255,255,.14) over #232528 -> #424446
fn set_border_color(hwnd: HWND, light: bool) {
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_BORDER_COLOR};
    let color: u32 = if light { 0x00dc_dad9 } else { 0x0046_4442 };
    // SAFETY: a u32 attribute on our own window.
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            std::ptr::from_ref(&color).cast(),
            std::mem::size_of_val(&color) as u32,
        )
    };
    if let Err(error) = result {
        tracing::warn!(%error, "tray panel border colour could not be set");
    }
}

/// No taskbar button: a tool window, and not an app window.
fn become_tool_window(hwnd: HWND) {
    // SAFETY: style bits on our own window; hidden while they change so the
    // shell drops the button winit's first show created.
    unsafe {
        let _ = ShowWindow(hwnd, SW_HIDE);
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let style = (style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_APPWINDOW.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

thread_local! {
    static ON_DEACTIVATE: RefCell<Option<Rc<dyn Fn()>>> = const { RefCell::new(None) };
}

/// Hides the panel when it loses the activation: a click anywhere else.
fn watch_deactivation(hwnd: HWND, hide: Rc<dyn Fn()>) {
    const SUBCLASS_ID: usize = 0x224b_0001;
    unsafe extern "system" fn proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_ACTIVATE && (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE {
            // Out of the window procedure first: hiding from inside it would
            // re-enter winit's event handling.
            slint::Timer::single_shot(std::time::Duration::ZERO, || {
                if let Some(hide) = ON_DEACTIVATE.with(|h| h.borrow().clone()) {
                    hide();
                }
            });
        }
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
    ON_DEACTIVATE.with(|h| *h.borrow_mut() = Some(hide));
    // SAFETY: our own window, a static procedure.
    if !unsafe { SetWindowSubclass(hwnd, Some(proc), SUBCLASS_ID, 0) }.as_bool() {
        tracing::warn!(
            "could not watch the tray panel's activation; it will not close on a click elsewhere"
        );
    }
}

/// Whether a `WM_TRAYICON` lparam is the right-click slint would answer with
/// its menu.
fn is_context_click(lparam: LPARAM) -> bool {
    let event = (lparam.0 as u32) & 0xFFFF;
    event == WM_RBUTTONUP || event == WM_CONTEXTMENU
}

/// Takes the right-click from slint's tray window and opens the panel. The
/// left-click and everything else go on to slint.
pub(super) fn subclass_tray_window(hwnd: HWND) {
    const SUBCLASS_ID: usize = 0x224b_0002;
    unsafe extern "system" fn proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        _id: usize,
        _data: usize,
    ) -> LRESULT {
        if msg == WM_TRAYICON && is_context_click(lparam) {
            if let Some(open) = OPEN.with(|o| o.borrow().clone()) {
                open();
            }
            return LRESULT(0);
        }
        unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
    }
    // SAFETY: our own window, a static procedure. Idempotent per id.
    if !unsafe { SetWindowSubclass(hwnd, Some(proc), SUBCLASS_ID, 0) }.as_bool() {
        tracing::warn!("could not hook the tray's right-click; it will open nothing");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two slint-private details the subclass leans on. A slint upgrade
    /// that moves either breaks the right-click silently, so this reads the
    /// vendored source and fails loudly instead.
    #[test]
    fn slint_tray_still_sends_the_message_the_subclass_takes() {
        let source = include_str!("../../../vendor/i-slint-core/items/system_tray/windows.rs");
        assert!(source.contains("const WM_TRAYICON: u32 = WM_APP + 1;"));
        assert!(source.contains("const TRAY_UID: u32 = 1;"));
        assert!(source.contains("let event = (lparam.0 as u32) & 0xFFFF;"));
        assert!(source.contains("if event == WM_RBUTTONUP || event == WM_CONTEXTMENU"));
    }

    /// A right-click reaching the tray window opens the panel and stops there;
    /// a left-click passes through to the window's own procedure untouched.
    #[test]
    fn the_tray_right_click_opens_the_panel_and_the_left_click_passes() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use windows::core::w;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SendMessageW,
            HWND_MESSAGE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_LBUTTONUP, WNDCLASSW,
        };
        static REACHED_WINDOW: AtomicU32 = AtomicU32::new(0);
        unsafe extern "system" fn record(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
            if m == WM_TRAYICON {
                REACHED_WINDOW.fetch_add(1, Ordering::SeqCst);
            }
            unsafe { DefWindowProcW(h, m, w, l) }
        }
        let class = w!("LivebackTrayPanelTestWindow");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(record),
            lpszClassName: class,
            ..Default::default()
        };
        assert_ne!(unsafe { RegisterClassW(&wc) }, 0, "register the test class");
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class,
                w!(""),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .expect("a message-only window");
        let opened = Rc::new(Cell::new(0));
        {
            let opened = opened.clone();
            OPEN.with(|o| *o.borrow_mut() = Some(Rc::new(move || opened.set(opened.get() + 1))));
        }
        subclass_tray_window(hwnd);
        let send = |event: u32| unsafe {
            SendMessageW(
                hwnd,
                WM_TRAYICON,
                Some(WPARAM(0)),
                Some(LPARAM(event as isize)),
            )
        };
        send(WM_RBUTTONUP);
        send(WM_CONTEXTMENU);
        send(WM_LBUTTONUP);
        let _ = unsafe { DestroyWindow(hwnd) };
        assert_eq!(opened.get(), 2, "both right-click events open the panel");
        assert_eq!(
            REACHED_WINDOW.load(Ordering::SeqCst),
            1,
            "only the left-click reaches slint's procedure"
        );
    }

    const WORK: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1032,
    };
    const MONITOR: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    };

    #[test]
    fn the_panel_sits_over_the_icon_clear_of_a_bottom_taskbar() {
        let icon = Rect {
            left: 1700,
            top: 1040,
            right: 1724,
            bottom: 1072,
        };
        assert_eq!(
            position(Some(icon), (0, 0), WORK, MONITOR, (272, 300), 8),
            (1724 - 272, 1032 - 8 - 300)
        );
    }

    #[test]
    fn without_the_icon_rect_the_cursor_anchors_and_the_work_area_clamps() {
        // The overflow flyout sits near the right edge: the panel would run
        // off it, so it is pulled back inside.
        assert_eq!(
            position(None, (1915, 1000), WORK, MONITOR, (272, 300), 8),
            (1920 - 8 - 272, 1032 - 8 - 300)
        );
        assert_eq!(
            position(None, (100, 1000), WORK, MONITOR, (272, 300), 8),
            (8, 1032 - 8 - 300)
        );
    }

    #[test]
    fn a_top_taskbar_opens_the_panel_downward() {
        let work = Rect {
            top: 48,
            bottom: 1080,
            ..WORK
        };
        assert_eq!(
            position(None, (1800, 20), work, MONITOR, (272, 300), 8),
            (1800 - 272, 48 + 8)
        );
    }

    #[test]
    fn a_right_taskbar_opens_the_panel_to_its_left() {
        let work = Rect {
            right: 1860,
            bottom: 1080,
            ..WORK
        };
        assert_eq!(
            position(None, (1890, 900), work, MONITOR, (272, 300), 8),
            (1860 - 8 - 272, 900 - 300)
        );
    }
}
