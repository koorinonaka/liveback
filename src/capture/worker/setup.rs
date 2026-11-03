//! One-time capture-worker plumbing split from `worker.rs` (which keeps the
//! capture loop itself): window-handle parsing, HDR detection, D3D device
//! creation and the segment-thumbnail encode.

use crate::capture::{gpu, CaptureSize};
use windows::{
    core::Interface,
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
            Direct3D11::ID3D11Texture2D,
            Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS},
            Dxgi::{
                Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, CreateDXGIFactory1,
                IDXGIAdapter1, IDXGIFactory1, IDXGIOutput6,
            },
            Gdi::HMONITOR,
        },
        UI::WindowsAndMessaging::{
            GetWindowLongW, GetWindowRect, IsZoomed, GWL_STYLE, WS_CAPTION, WS_THICKFRAME,
        },
    },
};

use super::super::targets;

// One parse for every target handle (t261025-7188). The alias keeps
// `worker.rs`'s import as it is; its error only reaches `worker_error`'s log.
pub(super) use super::super::targets::parse_window_handle as parse_hwnd;

/// The monitor a window target is captured from. A monitor target already has
/// its `HMONITOR` (task165), so only the window arm needs resolving; both
/// `monitor_uses_hdr` and `monitor_sdr_white_point` ask about the same monitor,
/// which is why this is resolved once by the caller rather than per query.
/// Lives in `targets` since t261025-7188 so the bin can share it.
pub(super) use super::super::targets::window_monitor;

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
///
/// Zero too for a window with no frame at all: a maximized `WS_POPUP` (a game's
/// borderless mode) covers the whole monitor on purpose, and what it draws over
/// the taskbar is picture, not overhang -- cropping it cost the bottom 48 rows
/// and letterboxed the rest.
pub(super) fn maximized_overhang(hwnd: HWND, frame: CaptureSize) -> gpu::Inset {
    unsafe {
        if !IsZoomed(hwnd).as_bool() {
            return gpu::Inset::default();
        }
        let style = GetWindowLongW(hwnd, GWL_STYLE) as u32;
        if style & (WS_THICKFRAME.0 | WS_CAPTION.0) == 0 {
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
        let Some(info) = targets::monitor_info(window_monitor(hwnd)) else {
            return gpu::Inset::default();
        };
        overhang(frame, [window, extended], info.monitorInfo.rcWork)
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
        let info = targets::monitor_info(monitor)?;

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

// The size itself is `gpu::THUMBNAIL_SIZE` since task2820, because the GPU is
// what resizes to it now -- these two only exist so the encode below keeps
// reading in the units it always did. The JPEG encode still runs on
// `spawn_thumbnail_encoder`'s own thread, never on the capture thread (task202
// moved this off the hot path; do not move it back).
const SEGMENT_THUMBNAIL_WIDTH: u32 = gpu::THUMBNAIL_SIZE.width as u32;
const SEGMENT_THUMBNAIL_HEIGHT: u32 = gpu::THUMBNAIL_SIZE.height as u32;

/// The picture itself, without publishing it (task420). Publishing is
/// `ContainerWriter::append_thumbnail`, called by the index writer thread in
/// `src/capture/indexer.rs` (task450).
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
    /// A copy queued on the GPU and not yet read back (t261017-613d): when it
    /// was queued, and how many drains found it not ready.
    in_flight: Option<(std::time::Instant, u32)>,
    /// The last read-back's queue-to-collect time and not-ready count, for the
    /// boundary probe to take.
    collected: Option<(std::time::Duration, u32)>,
}

/// What a read-back attempt gives `ThumbnailRelay::drain`: `Ok(None)` is "the
/// GPU has not reached the copy yet".
pub(super) type Collected = windows::core::Result<Option<(u32, u32, Vec<u8>)>>;

impl ThumbnailRelay {
    pub(super) fn new(writer: crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)>) -> Self {
        Self {
            writer,
            pending: None,
            written_for: None,
            in_flight: None,
            collected: None,
        }
    }

    /// Queues this frame as the candidate for whichever segment opens next.
    /// Nothing waits here (t261017-613d): `drain` reads it back once the GPU
    /// is done. A queue that fails leaves the previous candidate alone -- a
    /// stale thumbnail beats none.
    pub(super) fn capture(&mut self, pipeline: &gpu::TransformPipeline, texture: &ID3D11Texture2D) {
        // The pipeline draws `texture` down to `gpu::THUMBNAIL_SIZE` before the
        // readback (task2820), so what lands here is already the picture that
        // gets encoded -- half a megabyte instead of the whole frame.
        if pipeline.queue_thumbnail(texture).is_ok() {
            self.in_flight = Some((std::time::Instant::now(), 0));
            self.collected = None;
        }
    }

    /// The last read-back's queue-to-collect time and how many drains found
    /// it not ready, once.
    pub(super) fn take_collected(&mut self) -> Option<(std::time::Duration, u32)> {
        self.collected.take()
    }

    /// Reads back a queued copy if the GPU is done with it (`collect(wait)`,
    /// `wait` only on the stop drain), then hands the candidate over once the
    /// segment it belongs to has opened.
    ///
    /// While a copy is still in flight nothing is handed over and
    /// `written_for` stays put: the segment that just opened would otherwise
    /// get the previous candidate, and the frame that is actually its own
    /// would then have nowhere to go.
    pub(super) fn drain(
        &mut self,
        open_index: Option<u64>,
        wait: bool,
        collect: impl FnOnce(bool) -> Collected,
    ) {
        crate::insight_scope!("thumbnail_handoff");
        if let Some((queued_at, not_ready)) = self.in_flight {
            match collect(wait) {
                Ok(Some(frame)) => {
                    self.pending = Some(frame);
                    self.collected = Some((queued_at.elapsed(), not_ready));
                    self.in_flight = None;
                }
                Ok(None) => {
                    self.in_flight = Some((queued_at, not_ready + 1));
                    return;
                }
                Err(_) => self.in_flight = None,
            }
        }
        let Some(open_index) = open_index else {
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

/// A thread named `name` running `handle` on every frame the capture loop hands
/// it, off the capture thread (task202). Returns the sender; the thread ends
/// when it is dropped.
fn spawn_thumbnail_thread(
    name: &str,
    mut handle: impl FnMut(u64, u32, u32, Vec<u8>) + Send + 'static,
) -> crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)> {
    // Small and lossy on purpose: a backed-up writer means the capture is
    // rotating faster than JPEGs can be written, and dropping one thumbnail is
    // strictly better than growing a queue of 8MB frames.
    let (sender, receiver) = crossbeam_channel::bounded::<(u64, u32, u32, Vec<u8>)>(2);
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            for (index, width, height, bgra) in receiver {
                handle(index, width, height, bgra);
            }
        })
        .expect("thumbnail thread");
    sender
}

/// The thumbnail encoder, off the capture thread (task202). Returns the sender
/// the capture loop hands frames to; the thread ends when it is dropped.
///
/// The JPEG goes to the index writer and is appended to the `.lvb` next to the
/// segment it belongs to (task450).
pub(super) fn spawn_thumbnail_encoder(
    index: crossbeam_channel::Sender<crate::capture::indexer::IndexEvent>,
) -> crossbeam_channel::Sender<(u64, u32, u32, Vec<u8>)> {
    spawn_thumbnail_thread(
        "thumbnail-encoder",
        move |segment_index, width, height, bgra| {
            // Encode only, no file I/O (task1940's label split; the sidecar
            // writer it was split from is gone, t261025-d118).
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
        },
    )
}

#[cfg(test)]
mod relay_tests {
    use super::*;

    fn frame(value: u8) -> (u32, u32, Vec<u8>) {
        (1, 1, vec![value; 4])
    }

    /// A relay whose segment 0 already got its thumbnail, holding `old` as the
    /// leftover candidate, with the boundary copy for segment 1 queued.
    type Handed = crossbeam_channel::Receiver<(u64, u32, u32, Vec<u8>)>;

    fn relay_at_a_boundary(old: u8) -> (ThumbnailRelay, Handed) {
        let (sender, receiver) = crossbeam_channel::bounded(4);
        let mut relay = ThumbnailRelay::new(sender);
        relay.pending = Some(frame(old));
        relay.written_for = Some(0);
        relay.in_flight = Some((std::time::Instant::now(), 0));
        (relay, receiver)
    }

    /// t261017-613d: segment 1 opens while its own picture is still on the
    /// GPU. Handing over then would give it the leftover candidate and mark
    /// it done, so the picture that is really its own would never be sent.
    #[test]
    fn a_segment_that_opens_before_its_readback_waits_for_it() {
        let (mut relay, receiver) = relay_at_a_boundary(1);

        relay.drain(Some(1), false, |_| Ok(None));
        relay.drain(Some(1), false, |_| Ok(None));
        assert!(
            receiver.try_recv().is_err(),
            "nothing may be handed over while the copy is in flight"
        );
        assert_eq!(relay.take_collected(), None);

        relay.drain(Some(1), false, |_| Ok(Some(frame(2))));
        assert_eq!(receiver.try_recv().ok(), Some((1, 1, 1, vec![2; 4])));
        assert_eq!(
            relay.take_collected().map(|(_, not_ready)| not_ready),
            Some(2),
            "both not-ready drains are counted"
        );
        relay.drain(Some(1), false, |_| panic!("nothing is in flight"));
        assert!(receiver.try_recv().is_err(), "one thumbnail per segment");
    }

    /// A read-back that fails keeps the previous candidate: a stale thumbnail
    /// beats none.
    #[test]
    fn a_failed_readback_hands_over_the_previous_candidate() {
        let (mut relay, receiver) = relay_at_a_boundary(1);
        relay.drain(Some(1), false, |_| {
            Err(windows::Win32::Foundation::E_FAIL.into())
        });
        assert_eq!(receiver.try_recv().ok(), Some((1, 1, 1, vec![1; 4])));
    }

    /// The stop drain asks to wait; the pump never does.
    #[test]
    fn only_the_stop_drain_waits() {
        let (mut relay, _receiver) = relay_at_a_boundary(1);
        relay.drain(Some(1), false, |wait| {
            assert!(!wait);
            Ok(None)
        });
        relay.drain(Some(1), true, |wait| {
            assert!(wait);
            Ok(Some(frame(2)))
        });
    }
}
