use super::*;

#[test]
fn target_serializes_handle_as_string() {
    let target = CaptureTarget {
        kind: CaptureTargetKind::Window,
        primary: false,
        id: "1:0x10".into(),
        window_handle: "0x10".into(),
        process_id: 1,
        title: "Test".into(),
        executable_id: Some("test.exe".into()),
        executable_name: Some("test.exe".into()),
        minimized: false,
        selectable: true,
        unavailable_reason: None,
    };
    assert!(serde_json::to_string(&target)
        .unwrap()
        .contains("\"windowHandle\":\"0x10\""));
}

#[test]
fn previous_executable_is_prioritized_case_insensitively() {
    let target = |id: &str| CaptureTarget {
        kind: CaptureTargetKind::Window,
        primary: false,
        id: id.into(),
        window_handle: "0x1".into(),
        process_id: 1,
        title: id.into(),
        executable_id: Some(id.into()),
        executable_name: Some(id.into()),
        minimized: false,
        selectable: true,
        unavailable_reason: None,
    };
    let sorted = sort_previous_first(
        vec![target("other.exe"), target("game.exe")],
        Some("GAME.EXE"),
    );
    assert_eq!(sorted[0].executable_id.as_deref(), Some("game.exe"));
}

#[test]
fn windows_enumeration_returns_safe_snapshot() {
    let targets = list_capture_targets().expect("window enumeration should be nonfatal");
    assert!(targets.iter().all(|target| {
        !target.window_handle.is_empty()
            && target.process_id != std::process::id()
            && !target.title.is_empty()
    }));
}

#[test]
fn parse_window_handle_accepts_0x_prefixed_hex() {
    let hwnd = parse_window_handle("0x1A2B").expect("valid handle");
    assert_eq!(hwnd.0 as isize, 0x1A2B);
    assert!(parse_window_handle("not-a-handle").is_err());
    assert!(parse_window_handle("").is_err());
}

#[test]
fn thumbnail_size_downscales_landscape_preserving_ratio() {
    let bounds = CaptureSize {
        width: 160,
        height: 90,
    };
    for ((width, height), expected) in [
        // Landscape: downscaled, ratio kept.
        ((1920, 1080), (160, 90)),
        // Portrait: downscaled, ratio kept.
        ((1080, 1920), (51, 90)),
        // Already within bounds: never upscaled.
        ((64, 36), (64, 36)),
        // Degenerate input: nothing.
        ((0, 100), (0, 0)),
    ] {
        assert_eq!(
            compute_thumbnail_size(CaptureSize { width, height }, bounds),
            CaptureSize {
                width: expected.0,
                height: expected.1,
            },
            "{width}x{height}"
        );
    }
}

#[test]
fn encode_thumbnail_jpeg_produces_valid_jpeg_signature() {
    let width = 4u32;
    let height = 4u32;
    let rgba = vec![128u8; (width * height * 4) as usize];
    let jpeg = encode_thumbnail_jpeg(width, height, &rgba).expect("encode succeeds");
    assert_eq!(&jpeg[0..3], &[0xFF, 0xD8, 0xFF]);
    // A buffer that does not match width x height is refused.
    assert!(encode_thumbnail_jpeg(4, 4, &[0u8; 4]).is_err());
}

/// Real enumeration, like `windows_enumeration_returns_safe_snapshot` next to
/// it: this machine has displays, and the invariant worth pinning is that
/// exactly one of them is primary (task165).
#[test]
fn monitor_enumeration_returns_one_primary_screen() {
    let monitors =
        list_monitor_targets(crate::ui_state::locale::Locale::Ja).expect("monitors enumerate");
    assert!(!monitors.is_empty(), "a running desktop has a display");
    assert_eq!(
        monitors.iter().filter(|monitor| monitor.primary).count(),
        1,
        "exactly one display is primary"
    );
    for monitor in &monitors {
        assert_eq!(monitor.kind, CaptureTargetKind::Monitor);
        assert!(monitor.selectable, "a screen is always selectable");
        assert_eq!(monitor.process_id, 0, "a screen has no owning process");
        assert!(
            monitor.title.starts_with("画面 "),
            "unexpected label: {}",
            monitor.title
        );
        // The handle round-trips through the same hex form a window's does, so
        // the capture worker can parse both with one function.
        assert!(monitor.window_handle.starts_with("0x"));
    }
}

/// The GDI blit really reads the screen (task165). Not a pixel-exact check --
/// what the display shows is whatever is running -- but a thumbnail of the
/// right size that is not uniformly one colour is evidence the blit landed
/// rather than handing back an empty compatible bitmap.
#[test]
fn a_monitor_thumbnail_carries_actual_screen_pixels() {
    let monitors =
        list_monitor_targets(crate::ui_state::locale::Locale::Ja).expect("monitors enumerate");
    let monitor = monitors
        .iter()
        .find(|monitor| monitor.primary)
        .expect("a primary display");
    let thumbnail = capture_target_thumbnail_rgba(
        &monitor.window_handle,
        monitor.process_id,
        monitor.kind,
        Some(320),
        Some(180),
    )
    .expect("thumbnail capture does not error");
    let (width, height, rgba) = thumbnail.expect("the primary display is readable");
    assert!(width > 0 && height > 0);
    assert_eq!(rgba.len(), (width * height * 4) as usize);
    let distinct = rgba
        .chunks_exact(4)
        .map(|pixel| pixel[0])
        .collect::<std::collections::HashSet<_>>();
    assert!(
        distinct.len() > 1,
        "a blank thumbnail means the blit never reached the display"
    );
}

/// task2430. The four combinations `is_eligible_shape` exists to separate.
/// Measured 2026-08-30: minimizing never sets `DWMWA_CLOAKED`, it collapses a
/// Store window's client rect to 0x0 -- which is why the size check alone used
/// to hide `mspaint` while `notepad` (148x22 while minimized) stayed listed.
#[test]
fn minimizing_keeps_a_window_eligible_but_cloaking_never_does() {
    // A normal visible window.
    assert!(is_eligible_shape(false, false, true));
    // A packaged app's never-shown ghost window: dropped whatever its size.
    assert!(!is_eligible_shape(true, false, true));
    assert!(!is_eligible_shape(true, true, false));
    // The task's subject: a minimized Store window, client rect 0x0.
    assert!(is_eligible_shape(false, true, false));
    // Not minimized and no client area of its own: still nothing to show.
    assert!(!is_eligible_shape(false, false, false));
}

/// The fixture for task4310: a window whose owning thread the test controls,
/// so the test decides whether that thread pumps messages -- which is the one
/// thing `capture_window_bgra`'s responsiveness probe looks at.
///
/// The two cases differ in exactly that and nothing else: same class, same
/// styles, same size, both shown the same way. That is what makes the hung case
/// meaningful -- an asymmetry anywhere else could reject the hung window at an
/// earlier guard and never reach the probe at all.
///
/// It does not take the real desktop. The window is a `WS_EX_TOOLWINDOW` (no
/// taskbar button, no Alt+Tab entry) parked far off the virtual screen and
/// shown with `SW_SHOWNOACTIVATE`, so nothing is drawn where the user can see
/// it and focus never moves. It has to be *shown* rather than left hidden
/// because `PrintWindow` draws nothing for a window that was never shown
/// (measured 2026-09-11: every guard and the probe pass, only `PrintWindow`
/// returns FALSE), which would make the positive control unusable.
struct ProbeWindow {
    hwnd: isize,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Big enough that `GetClientRect` is non-empty. A 0x0 window is rejected
/// before the probe, so the hung case would pass without ever timing out.
const PROBE_WINDOW_WIDTH: i32 = 320;
const PROBE_WINDOW_HEIGHT: i32 = 180;

impl ProbeWindow {
    fn spawn(pump: bool) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{mpsc, Arc};
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, ShowWindow, MSG,
            PM_REMOVE, SW_SHOWNOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
        };

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            // The window has to be created *on this thread*. A same-thread
            // `SendMessage` is a direct call that can never time out, so a
            // window created on the test thread would make the hung case pass
            // for the wrong reason.
            let hwnd = unsafe {
                CreateWindowExW(
                    WS_EX_TOOLWINDOW,
                    windows::core::w!("STATIC"),
                    windows::core::w!("Liveback probe window"),
                    WS_POPUP,
                    -32000, // off every monitor: shown, but nowhere the user looks
                    -32000,
                    PROBE_WINDOW_WIDTH,
                    PROBE_WINDOW_HEIGHT,
                    None,
                    None,
                    None,
                    None,
                )
                .expect("the probe window should be created")
            };
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
            tx.send(hwnd.0 as isize)
                .expect("the probe thread should be able to report its hwnd");
            while !thread_stop.load(Ordering::Relaxed) {
                if pump {
                    // Messages another thread *sent* are delivered inside
                    // `PeekMessageW`, so this is what answers the probe's
                    // WM_NULL and `PrintWindow`'s WM_PRINT.
                    let mut msg = MSG::default();
                    unsafe {
                        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                            let _ = DispatchMessageW(&msg);
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            // `DestroyWindow` only works from the owning thread, which is why
            // the thread is torn down with a flag rather than a posted message:
            // the non-pumping one would never see a message.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        });
        let hwnd = rx
            .recv()
            .expect("the probe thread should report its hwnd before the test proceeds");
        Self {
            hwnd,
            stop,
            thread: Some(thread),
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd as *mut core::ffi::c_void)
    }
}

impl Drop for ProbeWindow {
    /// `cargo test` runs the whole module in one process, so a leaked fixture
    /// thread would outlive its test. The failure path needs the teardown as
    /// much as the success path does, hence `Drop` rather than a call at the
    /// end of the test.
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// task4310. `PrintWindow` waits on the target's message loop, and the thread
/// that calls it also runs the picker's 1s lifecycle poll -- so one wedged
/// window used to stop the LIVE chip and the self-stop detection, not just the
/// thumbnails.
#[test]
fn a_window_whose_thread_never_pumps_gives_up_instead_of_freezing_the_poller() {
    let window = ProbeWindow::spawn(false);

    let started = std::time::Instant::now();
    let captured = unsafe { capture_window_bgra(window.hwnd(), std::process::id()) };
    let elapsed = started.elapsed();

    assert!(
        captured.is_none(),
        "a window whose thread never pumps cannot have produced pixels"
    );
    // Upper bound only, and a loose one: this asserts that the call *returns*,
    // not how fast. `SMTO_ABORTIFHUNG` can cut the wait short the moment
    // Windows has marked the thread hung, so there is no lower bound to make.
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "capture_window_bgra took {elapsed:?} on a non-pumping window -- the probe did not time out"
    );
}

/// The positive control for the test above, and the only thing that shows the
/// negative was not vacuous: the same window in every respect except that this
/// thread answers messages, so it proves this shape clears `is_cloaked` and
/// `GetClientRect` and really does reach the probe.
#[test]
fn a_window_whose_thread_pumps_is_still_captured() {
    let window = ProbeWindow::spawn(true);

    let Some((width, height, pixels)) =
        (unsafe { capture_window_bgra(window.hwnd(), std::process::id()) })
    else {
        panic!("a window that answers messages should still be captured");
    };

    assert_eq!(
        (width, height),
        (PROBE_WINDOW_WIDTH as u32, PROBE_WINDOW_HEIGHT as u32),
        "the capture should come back at the window's own client size"
    );
    assert_eq!(pixels.len(), (width * height * 4) as usize);
}
