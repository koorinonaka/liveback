use serde::Serialize;
use windows::{
    core::BOOL,
    Win32::{
        Foundation::{HWND, LPARAM, RECT},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
            Gdi::{
                EnumDisplayMonitors, GetMonitorInfoW, MonitorFromWindow, HDC, HMONITOR,
                MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTONEAREST,
            },
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetAncestor, GetClientRect, GetDesktopWindow, GetShellWindow,
            GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
            IsIconic, IsWindow, IsWindowVisible, GA_ROOT, GWL_EXSTYLE, MONITORINFOF_PRIMARY,
            WS_EX_TOOLWINDOW,
        },
    },
};

mod process;
mod thumbnail;

use process::executable_for_process;
pub(crate) use process::{executable_name_for_handle, executable_path_for_handle};
pub use process::{
    executable_path_for_process, first_process_id_for_executable, running_processes,
    RunningProcesses,
};
pub use thumbnail::{bgra_to_resized_jpeg, capture_target_thumbnail_rgba};
#[cfg(test)]
use thumbnail::{compute_thumbnail_size, encode_thumbnail_jpeg};

const UNAVAILABLE_REASON: &str = "no readable process";

/// Which of the two things a target is (task165). A monitor's `window_handle`
/// carries its `HMONITOR` in the same hex form a window's carries its `HWND`,
/// so everything downstream that only forwards the handle needs no change --
/// only the two places that actually call Win32 with it do.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CaptureTargetKind {
    #[default]
    Window,
    Monitor,
}

// `PartialEq` since task a015: the drain decides whether a list poll moved
// anything by comparing the list it was given with the one it already holds,
// and every field here is part of what a tile draws.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureTarget {
    #[serde(default)]
    pub kind: CaptureTargetKind,
    /// The system's primary display. Always false for a window.
    #[serde(default)]
    pub primary: bool,
    pub id: String,
    pub window_handle: String,
    pub process_id: u32,
    pub title: String,
    pub executable_id: Option<String>,
    pub executable_name: Option<String>,
    pub minimized: bool,
    pub selectable: bool,
    pub unavailable_reason: Option<String>,
}

pub fn list_capture_targets() -> Result<Vec<CaptureTarget>, String> {
    let mut targets = Vec::new();
    let result = unsafe {
        EnumWindows(
            Some(enum_window_callback),
            LPARAM((&mut targets as *mut Vec<CaptureTarget>) as isize),
        )
    };
    result.map_err(|_| "could not list the windows".to_owned())?;
    Ok(targets)
}

/// Every display, in `EnumDisplayMonitors` order (task165). The handle is the
/// `HMONITOR`, the title is the picker's label, and `process_id` stays 0 --
/// a screen has no owning process, which is also why its audio has to come
/// from the system mix rather than a process loopback.
pub fn list_monitor_targets(
    locale: crate::ui_state::locale::Locale,
) -> Result<Vec<CaptureTarget>, String> {
    let mut monitors: Vec<MonitorInfo> = Vec::new();
    let result = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(enum_monitor_callback),
            LPARAM((&mut monitors as *mut Vec<MonitorInfo>) as isize),
        )
    };
    if !result.as_bool() {
        return Err("could not list the displays".to_owned());
    }
    Ok(monitors
        .into_iter()
        .enumerate()
        .map(|(index, monitor)| {
            let window_handle = format!("0x{:X}", monitor.handle as usize);
            CaptureTarget {
                kind: CaptureTargetKind::Monitor,
                primary: monitor.primary,
                id: format!("monitor:{window_handle}"),
                window_handle,
                process_id: 0,
                // Generated fresh on every enumeration, so it follows the UI
                // language (task1150). A session's *stored* title keeps the
                // wording it was recorded under, the same way it keeps a game
                // window's title after the game is renamed.
                title: crate::ui_state::targets::monitor_display_name(
                    locale,
                    index + 1,
                    monitor.width,
                    monitor.height,
                ),
                executable_id: None,
                executable_name: None,
                minimized: false,
                selectable: true,
                unavailable_reason: None,
            }
        })
        .collect())
}

struct MonitorInfo {
    handle: *mut std::ffi::c_void,
    width: i32,
    height: i32,
    primary: bool,
}

unsafe extern "system" fn enum_monitor_callback(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<MonitorInfo>);
    let Some(MONITORINFOEXW {
        monitorInfo: info, ..
    }) = monitor_info(monitor)
    else {
        return BOOL(1);
    };
    monitors.push(MonitorInfo {
        handle: monitor.0,
        // The *monitor* rectangle, not the work area: a recording covers the
        // taskbar too.
        width: info.rcMonitor.right - info.rcMonitor.left,
        height: info.rcMonitor.bottom - info.rcMonitor.top,
        primary: info.dwFlags & MONITORINFOF_PRIMARY != 0,
    });
    BOOL(1)
}

/// `GetMonitorInfoW` for one display, with the device name (t261025-7188).
/// `None` when the handle is not a live display.
pub fn monitor_info(monitor: HMONITOR) -> Option<MONITORINFOEXW> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }
        .as_bool()
        .then_some(info)
}

/// The display a window is on, or the nearest one when it straddles none.
pub fn window_monitor(hwnd: HWND) -> HMONITOR {
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
}

pub fn is_capture_target_valid(
    window_handle: &str,
    expected_process_id: u32,
    expected_executable_id: &str,
) -> bool {
    let Ok(hwnd) = parse_window_handle(window_handle) else {
        return false;
    };
    unsafe {
        if !is_eligible_window(hwnd) {
            return false;
        }
        let mut process_id = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
        process_id == expected_process_id
            && executable_for_process(process_id)
                .is_some_and(|(id, _)| id.eq_ignore_ascii_case(expected_executable_id))
    }
}

// Reads the window title straight from the live hwnd at capture-start time
// (rather than trusting a title the frontend sends along), so a session's
// recorded target name reflects what Windows actually reports for that
// window, the same source list_capture_targets already uses.
pub fn title_for_handle(
    locale: crate::ui_state::locale::Locale,
    window_handle: &str,
) -> Option<String> {
    let hwnd = parse_window_handle(window_handle).ok()?;
    let title = unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            // A screen's handle is an HMONITOR, which `IsWindow` rejects, so a
            // monitor session used to be recorded with no title at all: the
            // history row fell back to its start time and **the export refused
            // it outright** (`plan::validate_request` needs a non-empty name).
            // The picker's label is the same one the tile shows (task165).
            return monitor_title_for_handle(locale, window_handle);
        }
        window_title(hwnd)
    };
    (!title.is_empty()).then_some(title)
}

/// The picker's label for the screen this handle belongs to, or `None` when the
/// handle is not a live display either.
fn monitor_title_for_handle(
    locale: crate::ui_state::locale::Locale,
    window_handle: &str,
) -> Option<String> {
    list_monitor_targets(locale)
        .ok()?
        .into_iter()
        .find(|target| target.window_handle.eq_ignore_ascii_case(window_handle))
        .map(|target| target.title)
}

unsafe extern "system" fn enum_window_callback(hwnd: HWND, data: LPARAM) -> BOOL {
    let targets = &mut *(data.0 as *mut Vec<CaptureTarget>);
    if !is_eligible_window(hwnd) || !IsWindow(Some(hwnd)).as_bool() {
        return BOOL(1);
    }
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    let window_handle = format!("0x{:X}", hwnd.0 as usize);
    let title = window_title(hwnd);
    let executable = executable_for_process(process_id);
    // A minimized window is listed but cannot be picked (task2430 裁定): its
    // client rect collapses to a title-bar strip, and the video encoder refuses
    // that size, so a click would only ever produce a failure toast. Restoring
    // the window is the user's move; the picker just says why.
    let minimized = IsIconic(hwnd).as_bool();
    let selectable = executable.is_some() && !minimized;
    // A marker, not wording: the picker owns the UI language and words both
    // this case and the minimized one.
    let unavailable_reason = executable.is_none().then(|| UNAVAILABLE_REASON.to_owned());
    let (executable_id, executable_name) = executable
        .map(|(id, name)| (Some(id), Some(name)))
        .unwrap_or((None, None));
    targets.push(CaptureTarget {
        kind: CaptureTargetKind::Window,
        primary: false,
        id: format!("{process_id}:{window_handle}"),
        window_handle,
        process_id,
        title,
        executable_id,
        executable_name,
        minimized,
        selectable,
        unavailable_reason,
    });
    BOOL(1)
}

unsafe fn is_eligible_window(hwnd: HWND) -> bool {
    if !IsWindow(Some(hwnd)).as_bool()
        || !IsWindowVisible(hwnd).as_bool()
        || GetAncestor(hwnd, GA_ROOT) != hwnd
        || hwnd == GetDesktopWindow()
        || hwnd == GetShellWindow()
    {
        return false;
    }
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id == std::process::id() {
        return false;
    }
    if GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
        return false;
    }
    let title = window_title(hwnd);
    if title.is_empty() {
        return false;
    }
    let mut rect = RECT::default();
    let client_nonempty =
        GetClientRect(hwnd, &mut rect).is_ok() && rect.right > rect.left && rect.bottom > rect.top;
    is_eligible_shape(is_cloaked(hwnd), IsIconic(hwnd).as_bool(), client_nonempty)
}

/// The last three predicates of [`is_eligible_window`], without the Win32 calls
/// (task2430).
///
/// Cloaking is what a packaged app's never-shown ghost windows carry, so it is
/// an unconditional drop -- measured 2026-08-30, it is *not* what minimizing
/// sets (`DWMWA_CLOAKED` stays 0 through a minimize, for Store apps too).
///
/// The empty client rect is. A minimized Store window reports 0x0 where a
/// minimized Win32 window still reports its title-bar strip, which is the whole
/// reason `mspaint` used to vanish from the picker while `notepad` stayed. So
/// `IsIconic` overrides the size check: minimized is a state to show, not a
/// reason to hide. Whether it can be *recorded* is `selectable`'s job.
fn is_eligible_shape(cloaked: bool, iconic: bool, client_nonempty: bool) -> bool {
    !cloaked && (iconic || client_nonempty)
}

unsafe fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        &mut cloaked as *mut u32 as *mut _,
        std::mem::size_of::<u32>() as u32,
    )
    .is_ok_and(|_| cloaked != 0)
}

unsafe fn window_title(hwnd: HWND) -> String {
    let length = GetWindowTextLengthW(hwnd);
    if length <= 0 {
        return String::new();
    }
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut buffer);
    String::from_utf16_lossy(&buffer[..copied as usize])
}

pub fn sort_previous_first(
    mut targets: Vec<CaptureTarget>,
    previous: Option<&str>,
) -> Vec<CaptureTarget> {
    if let Some(previous) = previous {
        targets.sort_by_key(|target| {
            !target
                .executable_id
                .as_deref()
                .is_some_and(|id| id.eq_ignore_ascii_case(previous))
        });
    }
    targets
}

/// The `HWND` (or, for a monitor target, the `HMONITOR` bits) behind a target's
/// hex `window_handle`, `0x` prefix optional. The one parse every caller shares
/// (t261025-7188); each caller maps the error to its own `false` / `None`.
pub fn parse_window_handle(window_handle: &str) -> Result<HWND, String> {
    let raw = isize::from_str_radix(window_handle.trim_start_matches("0x"), 16)
        .map_err(|_| format!("invalid window handle: {window_handle}"))?;
    Ok(HWND(raw as *mut _))
}

/// Whether the window behind a `CaptureConfig::window_handle` string is
/// minimized. Round7 §2-7 puts a message on the review screen while it is:
/// the recording keeps running on black frames, and without the message that
/// black is indistinguishable from a broken capture.
pub(crate) fn window_is_minimized(handle: &str) -> bool {
    // The hex-with-optional-0x spelling `list_capture_targets` writes with
    // `format!("0x{:X}")`; a decimal parse silently fails and the message never
    // appears.
    let Some(hwnd) = parse_window_handle(handle)
        .ok()
        .filter(|hwnd| !hwnd.0.is_null())
    else {
        return false;
    };
    unsafe { IsWindow(Some(hwnd)).as_bool() && IsIconic(hwnd).as_bool() }
}

#[cfg(test)]
mod tests;
