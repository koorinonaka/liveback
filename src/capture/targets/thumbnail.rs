//! Picker thumbnails: GDI capture of a window or a display, then resize and
//! JPEG (moved out of `targets.rs`, t261025-7188).

use image::{imageops::FilterType, ImageBuffer, Rgba};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HWND, LPARAM, RECT, WPARAM},
        Graphics::Gdi::{
            BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateDCW, DeleteDC, DeleteObject,
            GetDC, GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
            DIB_RGB_COLORS, HDC, HMONITOR, SRCCOPY,
        },
        Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS},
        UI::WindowsAndMessaging::{
            GetClientRect, GetWindowThreadProcessId, IsIconic, IsWindow, SendMessageTimeoutW,
            SMTO_ABORTIFHUNG, SMTO_BLOCK, WM_NULL,
        },
    },
};

use super::{is_cloaked, parse_window_handle, CaptureTargetKind};
use crate::capture::{aspect_fit, CaptureSize};

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
const DEFAULT_THUMBNAIL_MAX_WIDTH: u32 = 320;
const DEFAULT_THUMBNAIL_MAX_HEIGHT: u32 = 180;
const THUMBNAIL_MAX_WIDTH_LIMIT: u32 = 640;
const THUMBNAIL_MAX_HEIGHT_LIMIT: u32 = 360;
// "品質0.6相当" -- the `image` crate's JpegEncoder quality is 1-100.
const THUMBNAIL_JPEG_QUALITY: u8 = 60;
// The `windows` crate exposes PRINT_WINDOW_FLAGS as an opaque newtype with no
// named constant for PW_RENDERFULLCONTENT; value is from the Win32 SDK
// (winuser.h: PW_RENDERFULLCONTENT = 0x00000002).
const PW_RENDERFULLCONTENT: u32 = 2;

/// Fits `input` within `max` preserving aspect ratio, never upscaling. Reuses
/// `aspect_fit`'s ratio math for the downscale case; a window already within
/// bounds is returned unchanged (a thumbnail should shrink, not enlarge).
pub(super) fn compute_thumbnail_size(input: CaptureSize, max: CaptureSize) -> CaptureSize {
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

pub(super) fn encode_thumbnail_jpeg(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, String> {
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
fn bgra_to_resized_rgba(
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
pub(super) unsafe fn capture_window_bgra(
    hwnd: HWND,
    expected_process_id: u32,
) -> Option<(u32, u32, Vec<u8>)> {
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
    let info = super::monitor_info(monitor)?;
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
