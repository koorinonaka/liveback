//! One-time capture-worker plumbing split from `worker.rs` (which keeps the
//! capture loop itself): window-handle parsing, HDR detection, D3D device
//! creation and the segment-thumbnail sidecar writes.

use crate::{
    capture::{gpu, CaptureSize},
    encoder, ring_buffer,
};
use windows::{
    core::Interface,
    Graphics::DirectX::Direct3D11::IDirect3DDevice,
    Win32::{
        Devices::Display::{
            DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
            DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
            DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_MODE_INFO,
            DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SDR_WHITE_LEVEL,
            DISPLAYCONFIG_SOURCE_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
        },
        Foundation::{HWND, RECT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_DEBUG,
                D3D11_CREATE_DEVICE_FLAG, D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
            },
            Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            Dxgi::{
                Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, CreateDXGIFactory1,
                IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput6,
            },
            Gdi::{
                GetMonitorInfoW, MonitorFromWindow, HMONITOR, MONITORINFO, MONITORINFOEXW,
                MONITOR_DEFAULTTONEAREST,
            },
        },
        System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice,
        UI::WindowsAndMessaging::{GetWindowRect, IsZoomed},
    },
};

use super::super::targets;

pub(super) fn parse_hwnd(value: &str) -> Result<HWND, String> {
    let raw = isize::from_str_radix(value.trim_start_matches("0x"), 16)
        .map_err(|_| "the capture target is invalid".to_owned())?;
    Ok(HWND(raw as *mut _))
}

/// The monitor a window target is captured from. A monitor target already has
/// its `HMONITOR` (task165), so only the window arm needs resolving; both
/// `monitor_uses_hdr` and `monitor_sdr_white_point` ask about the same monitor,
/// which is why this is resolved once by the caller rather than per query.
pub(super) fn window_monitor(hwnd: HWND) -> HMONITOR {
    unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) }
}

/// The part of a maximized window's WGC frame that Windows hangs past the
/// monitor work area (task2770). A maximized window is placed one frame
/// thickness outside the work area on every side; what WGC delivers for that
/// overhang is black (measured 2026-09-03: a custom-frame window maximized on a
/// 1920x1080 monitor recorded 1920x1040 with the 8 rows under the taskbar
/// black; a UE5 editor at 4K/150% recorded 3862x2110 with 11 black rows top
/// and bottom). Zero for anything not maximized, so a normal window records
/// exactly as before.
///
/// Which rect WGC sized the frame by differs between windows -- the extended
/// frame bounds for a standard frame, the window rect for a custom one -- so
/// the inset is taken from whichever of the two matches `frame`, and is zero
/// when neither does.
pub(super) fn maximized_overhang(hwnd: HWND, frame: CaptureSize) -> gpu::Inset {
    unsafe {
        if !IsZoomed(hwnd).as_bool() {
            return gpu::Inset::default();
        }
        let mut window = RECT::default();
        if GetWindowRect(hwnd, &mut window).is_err() {
            return gpu::Inset::default();
        }
        let mut extended = window;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            std::ptr::addr_of_mut!(extended).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
        .is_err()
        {
            extended = window;
        }
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if !GetMonitorInfoW(window_monitor(hwnd), &mut info).as_bool() {
            return gpu::Inset::default();
        }
        overhang(frame, [window, extended], info.rcWork)
    }
}

/// The pure part of [`maximized_overhang`]: how far the candidate rect whose
/// size matches `frame` sticks out of `work` on each side, never negative.
pub(in crate::capture) fn overhang(
    frame: CaptureSize,
    candidates: [RECT; 2],
    work: RECT,
) -> gpu::Inset {
    candidates
        .into_iter()
        .find(|rect| {
            rect.right - rect.left == frame.width && rect.bottom - rect.top == frame.height
        })
        .map(|rect| gpu::Inset {
            left: (work.left - rect.left).max(0),
            top: (work.top - rect.top).max(0),
            right: (rect.right - work.right).max(0),
            bottom: (rect.bottom - work.bottom).max(0),
        })
        .unwrap_or_default()
}

/// `verbose` logs the monitor's DXGI luminance range; the once-a-second poll
/// during a recording (t260917-7faa) passes `false` so it does not flood the log.
pub(super) fn monitor_uses_hdr(monitor: HMONITOR, verbose: bool) -> bool {
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return false;
        };
        for adapter_index in 0..32 {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                break;
            };
            let adapter: IDXGIAdapter1 = adapter;
            for output_index in 0..32 {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                let Ok(output6) = output.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let Ok(desc) = output6.GetDesc1() else {
                    continue;
                };
                if desc.Monitor == monitor {
                    // task2850: what the display itself reports, next to the
                    // white level the display-config path reports. Neither is
                    // the SDR content brightness on its own, but a run that
                    // disagrees with the recorded pixels needs both on record.
                    if verbose {
                        tracing::info!(
                            event = "capture_monitor_luminance",
                            color_space = desc.ColorSpace.0,
                            min_nits = desc.MinLuminance,
                            max_nits = desc.MaxLuminance,
                            max_full_frame_nits = desc.MaxFullFrameLuminance,
                            bits_per_color = desc.BitsPerColor,
                            "the capture monitor's DXGI luminance range"
                        );
                    }
                    return desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
                }
            }
        }
        false
    }
}

/// `monitor`'s SDR content white level, as the scRGB linear value the HDR tone
/// curve should map to output white (task1790).
///
/// Windows composites SDR content on an HDR monitor at the "SDR content
/// brightness" slider, typically 200-240 nits rather than the 80-nit scRGB
/// reference white the shader used to assume. `DISPLAYCONFIG_SDR_WHITE_LEVEL`
/// reports that in units of 1/1000 of reference white, so the scRGB white point
/// is simply `SDRWhiteLevel / 1000`.
///
/// Never fatal: any failure to find the monitor's display path returns the
/// floor, which is the behaviour that predates this function, and logs once --
/// this runs exactly once per capture session (same non-fatal stance as
/// `configure_rate_control`).
pub(super) fn monitor_sdr_white_point(monitor: HMONITOR) -> f32 {
    match query_sdr_white_point(monitor, true) {
        Some(white_point) => {
            tracing::info!(
                event = "capture_sdr_white_level",
                white_point,
                "tone mapping HDR capture against the monitor's SDR content white level"
            );
            white_point.max(gpu::HDR_WHITE_POINT_FLOOR)
        }
        None => {
            tracing::warn!(
                event = "capture_sdr_white_level_unavailable",
                white_point = gpu::HDR_WHITE_POINT_FLOOR,
                "SDR content white level query failed; falling back to the 80-nit reference white"
            );
            gpu::HDR_WHITE_POINT_FLOOR
        }
    }
}

/// A `[u16; N]` device name as a `String`, stopping at the NUL. Only for the
/// task2840 log lines -- the matching itself compares the arrays.
fn wide_to_string(name: &[u16]) -> String {
    let end = name.iter().position(|c| *c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..end])
}

/// What a recording's tone mapping depends on, re-read once a second while it
/// runs so a Windows HDR toggle mid-recording is followed (t260917-7faa).
/// Quiet: the caller logs one line when this changes.
pub(super) fn monitor_hdr_state(monitor: HMONITOR) -> super::HdrState {
    let hdr = monitor_uses_hdr(monitor, false);
    let white_point = if hdr {
        query_sdr_white_point(monitor, false)
            .unwrap_or(gpu::HDR_WHITE_POINT_FLOOR)
            .max(gpu::HDR_WHITE_POINT_FLOOR)
    } else {
        gpu::HDR_WHITE_POINT_FLOOR
    };
    super::HdrState { hdr, white_point }
}

fn query_sdr_white_point(monitor: HMONITOR, verbose: bool) -> Option<f32> {
    unsafe {
        let mut info = MONITORINFOEXW {
            monitorInfo: MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
                ..Default::default()
            },
            ..Default::default()
        };
        GetMonitorInfoW(monitor, std::ptr::addr_of_mut!(info).cast())
            .ok()
            .ok()?;

        let (mut path_count, mut mode_count) = (0u32, 0u32);
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
            .ok()
            .ok()?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
        .ok()
        .ok()?;

        // What the search is looking for, and what it walked past (task2840).
        // Without this a wrong answer is indistinguishable from a right one:
        // the caller only ever saw the number, never which display it came
        // from, and this machine has more than one.
        let wanted = wide_to_string(&info.szDevice);
        let mut seen: Vec<String> = Vec::new();
        for path in paths.iter().take(path_count as usize) {
            // GET_SOURCE_NAME is keyed on the *source* adapter/id and
            // GET_SDR_WHITE_LEVEL on the *target* one -- they are different
            // fields of the same path and swapping them silently returns
            // someone else's monitor.
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                    size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                    adapterId: path.sourceInfo.adapterId,
                    id: path.sourceInfo.id,
                },
                ..Default::default()
            };
            let source_rc = DisplayConfigGetDeviceInfo(&mut source.header);
            seen.push(format!(
                "{}(rc={source_rc})",
                wide_to_string(&source.viewGdiDeviceName)
            ));
            if source_rc != 0 || source.viewGdiDeviceName != info.szDevice {
                continue;
            }
            let mut white = DISPLAYCONFIG_SDR_WHITE_LEVEL {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SDR_WHITE_LEVEL,
                    size: std::mem::size_of::<DISPLAYCONFIG_SDR_WHITE_LEVEL>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            let white_rc = DisplayConfigGetDeviceInfo(&mut white.header);
            // task2850: the advanced-colour block off the same target, so a
            // white level that disagrees with the recorded pixels can be read
            // next to whether the OS even thinks advanced colour is active.
            let mut advanced = DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO {
                header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                    r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
                    size: std::mem::size_of::<DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO>() as u32,
                    adapterId: path.targetInfo.adapterId,
                    id: path.targetInfo.id,
                },
                ..Default::default()
            };
            let advanced_rc = DisplayConfigGetDeviceInfo(&mut advanced.header);
            if verbose {
                tracing::info!(
                    event = "capture_sdr_white_level_path",
                    device = %wanted,
                    paths = path_count,
                    candidates = %seen.join(","),
                    white_level_raw = white.SDRWhiteLevel,
                    white_level_rc = white_rc,
                    advanced_rc = advanced_rc,
                    advanced_bits = advanced.Anonymous.value,
                    advanced_encoding = advanced.colorEncoding.0,
                    advanced_bits_per_channel = advanced.bitsPerColorChannel,
                    "matched the capture monitor's display path"
                );
            }
            if white_rc != 0 || white.SDRWhiteLevel == 0 {
                return None;
            }
            return Some(white.SDRWhiteLevel as f32 / 1000.0);
        }
        if verbose {
            tracing::warn!(
                event = "capture_sdr_white_level_no_path",
                device = %wanted,
                paths = path_count,
                candidates = %seen.join(","),
                "no active display path matched the capture monitor"
            );
        }
        None
    }
}

pub(crate) fn create_d3d_device(
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    create_d3d_device_with_flags(
        D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
    )
}

pub(in crate::capture) fn create_d3d_device_with_flags(
    flags: D3D11_CREATE_DEVICE_FLAG,
) -> windows::core::Result<(ID3D11Device, ID3D11DeviceContext, IDirect3DDevice)> {
    unsafe {
        let mut device = None;
        let mut context = None;
        let mut level = D3D_FEATURE_LEVEL(0);
        let mut create = |flags| {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut level),
                Some(&mut context),
            )
        };
        // task4280: the debug layer is what lets `leak_probe` list the live
        // objects. Missing SDK layers is not a reason to fail the recording.
        if super::super::leak_probe::enabled() {
            if let Err(error) = create(flags | D3D11_CREATE_DEVICE_DEBUG) {
                tracing::warn!(event = "d3d_debug_device_unavailable", %error, "debug layer refused");
                create(flags)?;
            }
        } else {
            create(flags)?;
        }
        let device = device.ok_or_else(windows::core::Error::from_win32)?;
        let context = context.ok_or_else(windows::core::Error::from_win32)?;
        let dxgi: IDXGIDevice = device.cast()?;
        let direct3d = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        Ok((device, context, direct3d))
    }
}

// The size itself is `gpu::THUMBNAIL_SIZE` since task2820, because the GPU is
// what resizes to it now -- these two only exist so the encode below keeps
// reading in the units it always did. The JPEG encode still runs on
// `spawn_thumbnail_writer`'s own thread, never on the capture thread (task202
// moved this off the hot path; do not move it back).
const SEGMENT_THUMBNAIL_WIDTH: u32 = gpu::THUMBNAIL_SIZE.width as u32;
const SEGMENT_THUMBNAIL_HEIGHT: u32 = gpu::THUMBNAIL_SIZE.height as u32;

/// Writes `bgra` (as captured by `gpu::read_bgra_texture`) as `index`'s
/// thumbnail sidecar, `.partial.jpg` → `fs::rename` just like the segment mp4
/// itself so a reader (the thumbnail protocol route, Task066 Steps8-10) never
/// observes a half-written file. Never fatal to capture: failures are logged
/// by the caller and simply leave that segment without a thumbnail (existing
/// UI already falls back to a placeholder for a missing/absent thumbnail).
pub(in crate::capture) fn write_segment_thumbnail(
    output_dir: &std::path::Path,
    index: u64,
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<(), String> {
    // No create_dir_all here: by the time drain_pending_segment_thumbnail can
    // observe `open_index` for `index`, Mp4SegmentWriter::create_with_audio
    // has already created `output_dir` while opening that segment's mp4.
    let jpeg = encode_segment_thumbnail(width, height, bgra)?;
    let store = ring_buffer::SessionStore::new(output_dir.to_path_buf());
    let (partial, _) = store.thumbnail_paths(index);
    std::fs::write(&partial, jpeg).map_err(|error| error.to_string())?;
    store
        .publish_thumbnail(&partial, index)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// The picture itself, without publishing it (task420).
///
/// Split out of `write_segment_thumbnail` because a container session has no
/// `.partial.jpg` to write: it appends the bytes as a `Thumbnail` record. The
/// resize and the JPEG encode are identical either way, so only the publishing
/// half differs -- and since task450 that half is
/// `ContainerWriter::append_thumbnail`, called by the index writer thread in
/// `src/capture/indexer.rs`.
pub(in crate::capture) fn encode_segment_thumbnail(
    width: u32,
    height: u32,
    bgra: &[u8],
) -> Result<Vec<u8>, String> {
    targets::bgra_to_resized_jpeg(
        width,
        height,
        bgra,
        SEGMENT_THUMBNAIL_WIDTH,
        SEGMENT_THUMBNAIL_HEIGHT,
    )
}

/// Call after every `poll_to_muxer`: notices exactly when `muxer` has moved
/// on to a new open segment and, the first time that happens for a given
/// index, writes out whatever thumbnail candidate was captured earlier for
/// it (captured either at the video clean point that requested this
/// rotation via `should_force_keyframe`, or — for segment 0, which never
/// goes through `should_force_keyframe` — the latest frame seen before any
/// segment had opened yet; see the two call sites in `run_capture`). A few
/// frames' worth of encoder latency separates "boundary requested" from
/// "segment actually open," which is why this can't just capture at
/// `should_force_keyframe` time and write immediately: the new index isn't
/// known yet.
/// The segment-thumbnail hand-off (task202): the writer thread's channel, the
/// frame waiting to become the next segment's thumbnail, and the segment that
/// already got one.
///
/// One type because the three only ever move together -- a candidate is only
/// meaningful against the segment index it has not been written for yet, and
/// both are meaningless without the channel.
pub(super) struct ThumbnailRelay {
    writer: crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)>,
    pending: Option<(u32, u32, Vec<u8>)>,
    written_for: Option<u64>,
}

impl ThumbnailRelay {
    pub(super) fn new(writer: crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)>) -> Self {
        Self {
            writer,
            pending: None,
            written_for: None,
        }
    }

    /// Keeps this frame as the candidate for whichever segment opens next. A
    /// readback that fails leaves the previous candidate alone -- a stale
    /// thumbnail beats none.
    pub(super) fn capture(&mut self, pipeline: &gpu::TransformPipeline, texture: &ID3D11Texture2D) {
        // The pipeline draws `texture` down to `gpu::THUMBNAIL_SIZE` before the
        // readback (task2820), so what lands here is already the picture that
        // gets encoded -- half a megabyte instead of the whole frame.
        self.pending = pipeline.thumbnail_bgra(texture).ok();
    }

    /// Hands the candidate over once the segment it belongs to has opened.
    pub(super) fn drain(&mut self, muxer: &encoder::SegmentMuxer) {
        crate::insight_scope!("thumbnail_handoff");
        let Some(open_index) = muxer.open_index() else {
            return;
        };
        if self.written_for == Some(open_index) {
            return;
        }
        self.written_for = Some(open_index);
        let Some((width, height, bgra)) = self.pending.take() else {
            return;
        };
        // Handed to the writer thread rather than encoded here (task202). Doing
        // it inline cost a measured 237-245ms of the capture thread once per
        // segment -- no frame reached the encoder for that whole time, which is
        // the 250-290ms hole every segment used to open with. A thumbnail is
        // cosmetic and its frame is already in CPU memory, so it can be
        // finished late.
        if self
            .writer
            .try_send((open_index, width, height, bgra))
            .is_err()
        {
            tracing::warn!(
                event = "segment_thumbnail_dropped",
                index = open_index,
                "thumbnail writer is busy; skipping this segment's thumbnail"
            );
        }
    }
}

/// The thumbnail encoder/writer, off the capture thread (task202). Returns the
/// sender the capture loop hands frames to; the thread ends when it is dropped.
pub(super) fn spawn_thumbnail_writer(
    output_dir: std::path::PathBuf,
) -> crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)> {
    // Small and lossy on purpose: a backed-up writer means the capture is
    // rotating faster than JPEGs can be written, and dropping one thumbnail is
    // strictly better than growing a queue of 8MB frames.
    let (sender, receiver) = crossbeam_channel::bounded::<(u64, u32, u32, Vec<u8>)>(2);
    std::thread::Builder::new()
        .name("thumbnail-writer".into())
        .spawn(move || {
            for (index, width, height, bgra) in receiver {
                // This encode+write is what task202 moved off the capture
                // thread; measured here to confirm it stays off it (task205).
                //
                // Label split in task1940: both arms used to be
                // `thumbnail_write`, so the 1890/1900 evidence cannot say which
                // one it measured. This arm is encode + sidecar I/O.
                crate::insight_scope!("thumbnail_write_file");
                if let Err(error) =
                    write_segment_thumbnail(&output_dir, index, width, height, &bgra)
                {
                    tracing::warn!(
                        event = "segment_thumbnail_write_failed",
                        index,
                        %error,
                        "segment thumbnail write failed"
                    );
                }
            }
        })
        .expect("thumbnail writer thread");
    sender
}

/// The container arm of `spawn_thumbnail_writer` (task450).
///
/// Same thread, same lossy `bounded(2)`, same encode -- it just has nowhere of
/// its own to publish to, so the JPEG goes to the index writer and is appended
/// to the `.lvb` next to the segment it belongs to.
pub(super) fn spawn_thumbnail_encoder(
    index: crossbeam_channel::Sender<crate::capture::indexer::IndexEvent>,
) -> crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)> {
    let (sender, receiver) = crossbeam_channel::bounded::<(u64, u32, u32, Vec<u8>)>(2);
    std::thread::Builder::new()
        .name("thumbnail-encoder".into())
        .spawn(move || {
            for (segment_index, width, height, bgra) in receiver {
                // The other half of task1940's label split: encode only, no
                // file I/O. Every real recording takes this arm --
                // `CaptureController::start` always sets `container_path`
                // (task450) -- so a `thumbnail_write` figure from an app run
                // is this one, and carries no disk time at all.
                crate::insight_scope!("thumbnail_encode_container");
                match encode_segment_thumbnail(width, height, &bgra) {
                    Ok(jpeg) => {
                        let _ = index.send(crate::capture::indexer::IndexEvent::Thumbnail(
                            segment_index,
                            jpeg,
                        ));
                    }
                    Err(error) => tracing::warn!(
                        event = "segment_thumbnail_encode_failed",
                        index = segment_index,
                        %error,
                        "segment thumbnail encode failed"
                    ),
                }
            }
        })
        .expect("thumbnail encoder thread");
    sender
}
