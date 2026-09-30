use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::Path,
};

use image::{imageops::FilterType, ImageBuffer, Rgba};
use serde::Serialize;
use windows::{
    core::{BOOL, PCWSTR, PWSTR},
    Win32::{
        Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM},
        Graphics::{
            Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED},
            Gdi::{
                BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW, DeleteDC,
                DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC,
                SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HMONITOR,
                MONITORINFO, MONITORINFOEXW, SRCCOPY,
            },
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{
            EnumWindows, GetAncestor, GetClientRect, GetDesktopWindow, GetShellWindow,
            GetWindowLongW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
            IsIconic, IsWindow, IsWindowVisible, SendMessageTimeoutW, GA_ROOT, GWL_EXSTYLE,
            MONITORINFOF_PRIMARY, SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_NULL, WS_EX_TOOLWINDOW,
        },
    },
};

use super::{aspect_fit, CaptureSize};

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
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return BOOL(1);
    }
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

pub fn is_capture_target_valid(
    window_handle: &str,
    expected_process_id: u32,
    expected_executable_id: &str,
) -> bool {
    let Ok(raw_handle) = isize::from_str_radix(window_handle.trim_start_matches("0x"), 16) else {
        return false;
    };
    let hwnd = HWND(raw_handle as *mut _);
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
    let raw_handle = isize::from_str_radix(window_handle.trim_start_matches("0x"), 16).ok()?;
    let hwnd = HWND(raw_handle as *mut _);
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

unsafe fn image_path_for_process(process_id: u32) -> Option<OsString> {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).ok()?;
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let result = QueryFullProcessImageNameW(
        process,
        PROCESS_NAME_FORMAT(0),
        PWSTR(buffer.as_mut_ptr()),
        &mut length,
    );
    let _ = CloseHandle(process);
    result.ok()?;
    Some(OsString::from_wide(&buffer[..length as usize]))
}

unsafe fn executable_for_process(process_id: u32) -> Option<(String, String)> {
    let path = image_path_for_process(process_id)?;
    let name = Path::new(&path).file_name()?.to_string_lossy().into_owned();
    Some((name.to_ascii_lowercase(), name))
}

/// The full image path behind a process id (task2560). Display metadata only
/// -- matching stays on the executable name -- so the lossy conversion is fine.
/// `None` when the process is gone or cannot be opened.
pub fn executable_path_for_process(process_id: u32) -> Option<String> {
    unsafe { image_path_for_process(process_id) }.map(|path| path.to_string_lossy().into_owned())
}

/// The executable behind a captured window handle, as spelled on disk
/// (task2350). `None` for a monitor handle (an HMONITOR has no process) and
/// whenever the process cannot be opened -- the same shape `title_for_handle`
/// uses, read from the live hwnd at capture-start time rather than trusting
/// what the frontend sent.
pub fn executable_name_for_handle(window_handle: &str) -> Option<String> {
    let raw_handle = isize::from_str_radix(window_handle.trim_start_matches("0x"), 16).ok()?;
    let hwnd = HWND(raw_handle as *mut _);
    let mut process_id = 0;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    executable_name_for_process(process_id)
}

/// The full image path behind a captured window handle (2026-09-20). `None`
/// for a monitor handle and whenever the process cannot be opened, like
/// [`executable_name_for_handle`].
pub fn executable_path_for_handle(window_handle: &str) -> Option<String> {
    let raw_handle = isize::from_str_radix(window_handle.trim_start_matches("0x"), 16).ok()?;
    let hwnd = HWND(raw_handle as *mut _);
    let mut process_id = 0;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    executable_path_for_process(process_id)
}

/// The executable name behind a process id, as it is spelled on disk
/// (task1260). `None` when the process is gone or cannot be opened.
pub fn executable_name_for_process(process_id: u32) -> Option<String> {
    unsafe { executable_for_process(process_id) }.map(|(_, name)| name)
}

/// The first running process whose executable name matches, or `None` when
/// nothing by that name is running (task1260).
///
/// Case-insensitive, because Windows filenames are: a list the user typed by
/// hand into `settings.json` should not miss `Discord.exe` because the process
/// reports `discord.exe`. "First" is deliberately arbitrary -- an application
/// with several processes (every browser) has one audio session tree anyway,
/// and the loopback capture follows the process *tree* from whichever id it is
/// given.
pub fn first_process_id_for_executable(executable_name: &str) -> Option<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let wanted = executable_name.trim();
    if wanted.is_empty() {
        return None;
    }
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
                if name.eq_ignore_ascii_case(wanted) {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

/// One Toolhelp32 walk's worth of live processes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunningProcesses {
    /// Every executable name, lowercased the way `executable_id` is.
    pub names: HashSet<String>,
    /// The same names mapped to the process ids behind them (task3600), in the
    /// order the walk found them. Folder rules need a path, and a path needs a
    /// pid; the walk was already reading `th32ProcessID`, so this costs the map
    /// and nothing else. One name can carry several pids -- two copies of the
    /// same game, one of them under a registered folder.
    pub pids: HashMap<String, Vec<u32>>,
}

/// Every live process's executable name and process ids. What
/// `AutoCaptureWatch::poll` reads its launch edge from (task2450), plus the pids
/// its folder rules resolve to image paths (task3600).
///
/// Deliberately not filtered by the registration list, even though that would
/// be cheaper: `poll` needs the names it is *not* watching too, so that
/// registering an app which is already running is not an edge. A filtered set
/// would leave the newly registered name out of the previous poll's set and
/// fire the moment it was added.
///
/// `None` when the snapshot could not be taken. An empty set would read as
/// "every process just died" and re-arm every registered app on the next
/// successful poll -- the same false fire this function exists to remove.
pub fn running_processes() -> Option<RunningProcesses> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = RunningProcesses::default();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name =
                    String::from_utf16_lossy(&entry.szExeFile[..length]).to_ascii_lowercase();
                found
                    .pids
                    .entry(name.clone())
                    .or_default()
                    .push(entry.th32ProcessID);
                found.names.insert(name);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        // A walk that produced nothing did not happen: this process is always
        // in its own snapshot.
        (!found.names.is_empty()).then_some(found)
    }
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

// --- Window thumbnail capture (task 037) ---
//
// Spike result (see task 037 Execution Log): PrintWindow(PW_RENDERFULLCONTENT)
// was tested against a plain GDI window (Notepad), Chromium/Electron windows
// (VS Code, Discord), and a flip-model D3D11 borderless window standing in
// for a game -- all four rendered real content (black_ratio == 0 / near 0),
// none came back solid black. That rules out the WGC fallback described in
// the task and keeps this on Win32_Graphics_Gdi + Win32_Storage_Xps only.

// The picker's tiles are ~218px wide and up, so the old 160x90 default was
// always being scaled *up* on screen and looked it (task143). `PrintWindow`
// renders the whole window either way -- these bound the copy that comes back,
// not the work that produces it, so a sharper thumbnail costs memory and a
// larger blit, not another capture.
pub const DEFAULT_THUMBNAIL_MAX_WIDTH: u32 = 320;
pub const DEFAULT_THUMBNAIL_MAX_HEIGHT: u32 = 180;
pub const THUMBNAIL_MAX_WIDTH_LIMIT: u32 = 640;
pub const THUMBNAIL_MAX_HEIGHT_LIMIT: u32 = 360;
// "品質0.6相当" -- the `image` crate's JpegEncoder quality is 1-100.
const THUMBNAIL_JPEG_QUALITY: u8 = 60;
// The `windows` crate exposes PRINT_WINDOW_FLAGS as an opaque newtype with no
// named constant for PW_RENDERFULLCONTENT; value is from the Win32 SDK
// (winuser.h: PW_RENDERFULLCONTENT = 0x00000002).
const PW_RENDERFULLCONTENT: u32 = 2;

pub fn parse_window_handle(window_handle: &str) -> Result<HWND, String> {
    let raw = isize::from_str_radix(window_handle.trim_start_matches("0x"), 16)
        .map_err(|_| format!("invalid window handle: {window_handle}"))?;
    Ok(HWND(raw as *mut _))
}

/// Fits `input` within `max` preserving aspect ratio, never upscaling. Reuses
/// `aspect_fit`'s ratio math for the downscale case; a window already within
/// bounds is returned unchanged (a thumbnail should shrink, not enlarge).
pub fn compute_thumbnail_size(input: CaptureSize, max: CaptureSize) -> CaptureSize {
    if input.width <= 0 || input.height <= 0 || max.width <= 0 || max.height <= 0 {
        return CaptureSize {
            width: 0,
            height: 0,
        };
    }
    if input.width <= max.width && input.height <= max.height {
        return input;
    }
    let fit = aspect_fit(input, max);
    CaptureSize {
        width: fit.width.max(1),
        height: fit.height.max(1),
    }
}

pub fn encode_thumbnail_jpeg(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let image = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba.to_vec())
        .ok_or_else(|| "pixel buffer does not match dimensions".to_owned())?;
    let mut jpeg_bytes = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, THUMBNAIL_JPEG_QUALITY);
    encoder
        .encode_image(&image::DynamicImage::ImageRgba8(image))
        .map_err(|error| format!("jpeg encode failed: {error}"))?;
    Ok(jpeg_bytes)
}

/// BGRA → RGBA → resize (Triangle filter; skipped when already at the target
/// size). Stops one step short of an encoder so the slint bin can take the
/// pixels straight into a `SharedPixelBuffer` (task124) while the WebView build
/// keeps going through JPEG below.
pub fn bgra_to_resized_rgba(
    width: u32,
    height: u32,
    bgra: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, String> {
    // `livia_pixels` rather than a loop here (task2830): the same swap plus
    // opaque alpha, but written as a row `copy_from_slice` and living in a
    // package the dev profile optimizes. Its own doc records the difference
    // this makes unoptimized -- 105ms per 1080p frame for the iterator form
    // against 5.9ms for this one. Positive stride: `capture_window_bgra` and
    // the capture pipeline both hand over top-down rows.
    let rgba = livia_pixels::swizzle_bgra(bgra, width, height, (width * 4) as i32);
    let image = ImageBuffer::<Rgba<u8>, _>::from_raw(width, height, rgba)
        .ok_or_else(|| "pixel buffer does not match dimensions".to_owned())?;
    if (target_width, target_height) == (width, height) {
        return Ok(image.into_raw());
    }
    Ok(
        image::imageops::resize(&image, target_width, target_height, FilterType::Triangle)
            .into_raw(),
    )
}

/// [`bgra_to_resized_rgba`] → JPEG(q60). Shared by window-picker thumbnails
/// (the JPEG path, which computes its own aspect-preserving
/// `target_width`/`target_height` via `compute_thumbnail_size` first) and
/// Task066's segment sidecar thumbnails (which always resize to a fixed
/// 160x90, no aspect preservation).
pub fn bgra_to_resized_jpeg(
    width: u32,
    height: u32,
    bgra: &[u8],
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, String> {
    let rgba = bgra_to_resized_rgba(width, height, bgra, target_width, target_height)?;
    encode_thumbnail_jpeg(target_width, target_height, &rgba)
}

/// Whether the window behind a `CaptureConfig::window_handle` string is
/// minimized. Round7 §2-7 puts a message on the review screen while it is:
/// the recording keeps running on black frames, and without the message that
/// black is indistinguishable from a broken capture.
pub fn window_is_minimized(handle: &str) -> bool {
    // The same hex-with-optional-0x spelling `parse_hwnd` reads (targets.rs
    // writes it with `format!("0x{:X}")`); a decimal parse silently fails and
    // the message never appears.
    let Ok(raw) = isize::from_str_radix(handle.trim_start_matches("0x"), 16) else {
        return false;
    };
    if raw == 0 {
        return false;
    }
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    unsafe { IsWindow(Some(hwnd)).as_bool() && IsIconic(hwnd).as_bool() }
}

/// How long the responsiveness probe in `capture_window_bgra` waits for the
/// target's message loop to answer (task4310).
///
/// 200ms is the same wait `window_icon_rgba` already spends per tile, so the
/// two Win32 calls the picker makes per window agree with each other. It is
/// also more than one 120ms thumbnail round, so a window that is slow but alive
/// is not dropped on every single poll. And the steady-state cost of a wedged
/// window is close to zero rather than 200ms a round: `SMTO_ABORTIFHUNG` returns
/// at once once Windows has marked the thread hung (~5s of not pumping), so the
/// full 200ms is only paid while the window is freshly stuck.
const WINDOW_PROBE_TIMEOUT_MS: u32 = 200;

/// Captures `hwnd` via PrintWindow and returns raw top-down BGRA pixels at the
/// window's native client size, or `None` when the window cannot be captured
/// right now (gone, minimized, cloaked, identity mismatch, unresponsive, or a
/// GDI-level failure) -- all non-fatal per the task's Ok(None) contract.
unsafe fn capture_window_bgra(hwnd: HWND, expected_process_id: u32) -> Option<(u32, u32, Vec<u8>)> {
    if !IsWindow(Some(hwnd)).as_bool() || IsIconic(hwnd).as_bool() || is_cloaked(hwnd) {
        return None;
    }
    let mut process_id = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    if process_id != expected_process_id {
        return None;
    }

    // `PrintWindow` below `SendMessage`s WM_PRINT to the target's *own* message
    // loop, so a window that never pumps never lets go of this thread -- and
    // this thread also runs the picker's 1s lifecycle poll, so a single wedged
    // window would take the LIVE chip and the self-stop detection down with the
    // thumbnails (task4310). None of the guards above look at responsiveness:
    // they all read kernel-side window state without sending anything. So knock
    // first with the cheapest message there is, and give up when nobody answers.
    //
    // The `LRESULT` is what says whether it answered. WM_NULL's own reply is
    // always 0, so checking the out-param the way `window_icon_rgba` checks
    // WM_GETICON's would reject every window, responsive or not.
    if SendMessageTimeoutW(
        hwnd,
        WM_NULL,
        WPARAM(0),
        LPARAM(0),
        SMTO_ABORTIFHUNG | SMTO_BLOCK,
        WINDOW_PROBE_TIMEOUT_MS,
        None,
    )
    .0 == 0
    {
        return None;
    }

    let mut rect = RECT::default();
    if GetClientRect(hwnd, &mut rect).is_err() {
        return None;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width <= 0 || height <= 0 {
        return None;
    }

    let src_dc = GetDC(Some(hwnd));
    if src_dc.is_invalid() {
        return None;
    }
    let captured = capture_dc_bgra(src_dc, width, height, |mem_dc| {
        PrintWindow(hwnd, mem_dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)).as_bool()
    });
    ReleaseDC(Some(hwnd), src_dc);
    captured
}

/// Top-down BGRA pixels of a `width`x`height` area, drawn into a memory DC by
/// `draw` and read back with `GetDIBits`.
///
/// The GDI objects it needs are created and released here; the source DC
/// belongs to the caller, which is the only part the window and monitor paths
/// do differently. Any GDI failure is "no thumbnail right now", never fatal.
///
/// # Safety
///
/// `src_dc` must be a live device context, and `width`/`height` positive.
unsafe fn capture_dc_bgra(
    src_dc: HDC,
    width: i32,
    height: i32,
    draw: impl FnOnce(HDC) -> bool,
) -> Option<(u32, u32, Vec<u8>)> {
    let mem_dc = CreateCompatibleDC(Some(src_dc));
    let bitmap = CreateCompatibleBitmap(src_dc, width, height);
    if mem_dc.is_invalid() || bitmap.is_invalid() {
        if !bitmap.is_invalid() {
            let _ = DeleteObject(bitmap.into());
        }
        if !mem_dc.is_invalid() {
            let _ = DeleteDC(mem_dc);
        }
        return None;
    }
    let previous = SelectObject(mem_dc, bitmap.into());
    let drawn = draw(mem_dc);
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    let mut result = None;
    if drawn {
        let mut bitmap_info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative = top-down DIB
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let scanlines = GetDIBits(
            mem_dc,
            bitmap,
            0,
            height as u32,
            Some(pixels.as_mut_ptr() as *mut _),
            &mut bitmap_info,
            DIB_RGB_COLORS,
        );
        if scanlines == height {
            result = Some((width as u32, height as u32, pixels));
        }
    }
    SelectObject(mem_dc, previous);
    let _ = DeleteObject(bitmap.into());
    let _ = DeleteDC(mem_dc);
    result
}

/// The monitor twin of `capture_window_bgra` (task165). A screen has no
/// `PrintWindow`, so this opens a DC on the display device itself and blits it.
/// Same `Option` contract: any GDI failure is "no thumbnail right now", never
/// fatal.
unsafe fn capture_monitor_bgra(monitor: HMONITOR) -> Option<(u32, u32, Vec<u8>)> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info.monitorInfo as *mut _).as_bool() {
        return None;
    }
    let rect = info.monitorInfo.rcMonitor;
    let (width, height) = (rect.right - rect.left, rect.bottom - rect.top);
    if width <= 0 || height <= 0 {
        return None;
    }
    // A DC on the device name rather than the desktop DC: the desktop DC's
    // origin is the virtual screen, so a secondary monitor at a negative x
    // would blit from the wrong place.
    let src_dc = CreateDCW(PCWSTR(info.szDevice.as_ptr()), None, None, None);
    if src_dc.is_invalid() {
        return None;
    }
    let captured = capture_dc_bgra(src_dc, width, height, |mem_dc| {
        BitBlt(mem_dc, 0, 0, width, height, Some(src_dc), 0, 0, SRCCOPY).is_ok()
    });
    let _ = DeleteDC(src_dc);
    captured
}

/// Raw top-down RGBA at the thumbnail's own size, for callers that render the
/// pixels themselves. Same `Err`/`Ok(None)` contract as
/// the base64 JPEG form of this -- the
/// WebView needed an `<img src>`, the slint bin does not (task124).
pub fn capture_target_thumbnail_rgba(
    window_handle: &str,
    expected_process_id: u32,
    kind: CaptureTargetKind,
    max_width: Option<u32>,
    max_height: Option<u32>,
) -> Result<Option<(u32, u32, Vec<u8>)>, String> {
    let hwnd = parse_window_handle(window_handle)?;
    let max_width = max_width
        .unwrap_or(DEFAULT_THUMBNAIL_MAX_WIDTH)
        .clamp(1, THUMBNAIL_MAX_WIDTH_LIMIT);
    let max_height = max_height
        .unwrap_or(DEFAULT_THUMBNAIL_MAX_HEIGHT)
        .clamp(1, THUMBNAIL_MAX_HEIGHT_LIMIT);

    let captured = unsafe {
        match kind {
            CaptureTargetKind::Window => capture_window_bgra(hwnd, expected_process_id),
            CaptureTargetKind::Monitor => capture_monitor_bgra(HMONITOR(hwnd.0)),
        }
    };
    let Some((width, height, bgra)) = captured else {
        return Ok(None);
    };

    let target = compute_thumbnail_size(
        CaptureSize {
            width: width as i32,
            height: height as i32,
        },
        CaptureSize {
            width: max_width as i32,
            height: max_height as i32,
        },
    );
    if target.width <= 0 || target.height <= 0 {
        return Ok(None);
    }
    let (target_width, target_height) = (target.width as u32, target.height as u32);

    match bgra_to_resized_rgba(width, height, &bgra, target_width, target_height) {
        Ok(rgba) => Ok(Some((target_width, target_height, rgba))),
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests;
