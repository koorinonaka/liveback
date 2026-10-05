//! The crash trap (task750).
//!
//! On 2026-08-20 the app died mid-session with nothing to show for it: the log
//! stopped between two 1Hz playback ticks, the next line was the *next* run's
//! `application_started`, and Windows Error Reporting had no event either. That
//! is what a process with no crash handling looks like from the outside, and it
//! is unfalsifiable -- an access violation, an `abort`, and someone calling
//! `TerminateProcess` all leave exactly the same nothing.
//!
//! So this installs the two traps that make those three distinguishable next
//! time:
//!
//! - a panic hook, because a `windows_subsystem = "windows"` build has no
//!   stderr for the default one to print to, and a panicking worker thread
//!   otherwise disappears without even a line saying the thread is gone;
//! - an unhandled exception filter, which catches the native faults that never
//!   reach Rust at all.
//!
//! A death that still leaves *nothing* after this is itself the answer: not a
//! fault in this process, so an outside kill or the machine itself. That
//! distinction is the whole point -- this box runs pre-fix Raptor Lake
//! microcode, and "the CPU did it" has to be provable rather than assumed.

use std::sync::atomic::{AtomicBool, Ordering};

/// What `LIVEBACK_CRASH_TEST` asked for. The traps are only worth having if
/// they are known to fire, and the only way to know is to cause the thing they
/// catch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashTest {
    /// A Rust panic on the main thread: exercises the hook.
    Panic,
    /// A null dereference: exercises the exception filter.
    AccessViolation,
    /// A panic the hook treats as if it came from skia's `d3d_surface.rs`:
    /// exercises the window diagnostics (task2930). The location of a
    /// `panic!()` is baked in at compile time, so nothing outside
    /// `i-slint-renderer-skia` can honestly claim that file name -- this
    /// raises [`FORCE_D3D_TEST_MATCH`] instead and then panics normally.
    D3dPanic,
    /// The same panic as [`CrashTest::D3dPanic`], but fired from a timer once
    /// `ui.run()` is going and the app owns a titled top-level window
    /// (task2940). [`CrashTest::D3dPanic`] fires from startup, before winit has
    /// created the window, so `own_window_rect()` can only ever report
    /// `unavailable` there -- this variant is what proves the success path of
    /// those diagnostics in-process rather than by reasoning about it.
    D3dPanicAfterRun,
}

/// Reads the crash-test request out of the environment variable's value.
///
/// Separate from the acting on it so the parsing is testable, and deliberately
/// strict: anything unrecognised is `None`, so a typo is inert rather than
/// "close enough to crash".
pub fn crash_test_from(value: Option<&str>) -> Option<CrashTest> {
    match value?.trim() {
        "panic" => Some(CrashTest::Panic),
        "av" => Some(CrashTest::AccessViolation),
        "d3d_panic" => Some(CrashTest::D3dPanic),
        "d3d_panic_after_run" => Some(CrashTest::D3dPanicAfterRun),
        _ => None,
    }
}

/// Whether a panic came from the D3D surface skia draws through (task2930).
///
/// The one on 2026-09-04 was an `Option::unwrap()` at
/// `...\i-slint-renderer-skia-1.17.1\src\d3d_surface.rs:139:53`, with no record
/// of the window state around it. Matching on the file name is what gates the
/// extra fields: every other panic keeps the record it always had.
pub fn is_d3d_surface_panic(file: &str) -> bool {
    file.contains("d3d_surface.rs")
}

/// Whether a window title is our own shell's (task2950).
///
/// `ui/app.slint` sets `title: "Liveback"` on the one `inherits Window` in the
/// app and nothing calls `set_title` at runtime, so this is an exact match
/// rather than a "non-empty" test: this process also owns an ownerless,
/// hidden `Slint Window` that a laxer predicate matches by accident. If the
/// title ever changes, the diagnostics go back to saying `unavailable` --
/// which is the degradation task2930 asked for, over reporting the wrong
/// window's rect.
pub fn is_main_window_title(title: &str) -> bool {
    title == "Liveback"
}

/// A readable name for the exception codes worth recognising by sight. Anything
/// else is reported by its number, which is all the filter really needs to say.
pub fn exception_name(code: u32) -> Option<&'static str> {
    Some(match code {
        0xC000_0005 => "ACCESS_VIOLATION",
        0xC000_001D => "ILLEGAL_INSTRUCTION",
        0xC000_0025 => "NONCONTINUABLE_EXCEPTION",
        0xC000_008C => "ARRAY_BOUNDS_EXCEEDED",
        0xC000_008E => "FLT_DIVIDE_BY_ZERO",
        0xC000_0094 => "INT_DIVIDE_BY_ZERO",
        0xC000_00FD => "STACK_OVERFLOW",
        0xC000_0374 => "HEAP_CORRUPTION",
        0xC000_0409 => "STACK_BUFFER_OVERRUN",
        0x8000_0003 => "BREAKPOINT",
        _ => return None,
    })
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::ffi::OsStringExt;

    use windows::core::{BOOL, PCWSTR, PWSTR};
    use windows::Win32::Foundation::{CloseHandle, HMODULE, HWND, LPARAM, RECT};
    use windows::Win32::System::Diagnostics::Debug::{
        SetUnhandledExceptionFilter, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
    };
    use windows::Win32::System::LibraryLoader::{
        GetModuleFileNameW, GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetForegroundWindow, GetWindow, GetWindowRect, GetWindowTextW,
        GetWindowThreadProcessId, IsWindowVisible, GW_OWNER,
    };

    /// The module an address lives in, as a path. This is the single most
    /// useful field in the whole record: it separates our own code from a
    /// media/graphics driver without anyone having to symbolise an address.
    fn module_for(address: *const std::ffi::c_void) -> Option<String> {
        let mut module = HMODULE::default();
        // SAFETY: the address is only used as a lookup key, never dereferenced,
        // and the UNCHANGED_REFCOUNT flag means nothing has to be released.
        unsafe {
            GetModuleHandleExW(
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                    | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                PCWSTR(address as *const u16),
                &mut module,
            )
            .ok()?;
        }
        let mut buffer = [0u16; 260];
        // SAFETY: `module` came back from the call above, and the length is the
        // buffer's own.
        let written = unsafe { GetModuleFileNameW(Some(module), &mut buffer) } as usize;
        (written > 0).then(|| String::from_utf16_lossy(&buffer[..written]))
    }

    /// Runs on the faulting thread with the process already unwell, so it does
    /// as little as it can get away with. It does allocate -- `tracing` and the
    /// module path both do -- which is not what a textbook filter would do, but
    /// a record that reaches the log is worth more here than a handler that is
    /// safe in cases it will never see. Heap corruption is the one case that
    /// can lose the line, and heap corruption is not the failure being chased.
    unsafe extern "system" fn on_unhandled_exception(info: *const EXCEPTION_POINTERS) -> i32 {
        // SAFETY: Windows hands us a valid pointer for the duration of the
        // call; the records are only read.
        let record = unsafe { info.as_ref().and_then(|info| info.ExceptionRecord.as_ref()) };
        match record {
            Some(record) => {
                let code = record.ExceptionCode.0 as u32;
                let address = record.ExceptionAddress;
                tracing::error!(
                    target: "task750_crash",
                    code = format!("{code:#010X}"),
                    name = exception_name(code).unwrap_or("UNKNOWN"),
                    address = format!("{address:p}"),
                    module = module_for(address).unwrap_or_else(|| "unknown".into()),
                    thread = std::thread::current().name().unwrap_or("unnamed"),
                    "unhandled exception, the process is going down"
                );
            }
            None => {
                tracing::error!(
                    target: "task750_crash",
                    "unhandled exception with no record attached"
                );
            }
        }
        // Let the normal machinery run afterwards: this is a trap for evidence,
        // not a place to swallow the crash.
        EXCEPTION_CONTINUE_SEARCH
    }

    pub(super) fn install_exception_filter() {
        // SAFETY: installing a process-wide filter; the previous one (there is
        // none) is deliberately dropped.
        unsafe { SetUnhandledExceptionFilter(Some(on_unhandled_exception)) };
    }

    /// Our own top-level window, found by asking Windows what exists right now
    /// rather than by remembering a handle (task2930).
    ///
    /// A global registered at startup would have to be registered *somewhere*,
    /// and task2900 already found the trap: at `restore_window_state` time
    /// `ui.run()` has not been entered and there is no hwnd yet. `EnumWindows`
    /// at panic time has no such window -- it sees whatever is really there.
    ///
    /// The title has to be exactly `"Liveback"` (task2950). Visibility is
    /// deliberately *not* a filter: while the shell is stashed in the tray the
    /// main window still exists at its real rect, only hidden, and that
    /// background case is exactly what task2900's exclusive-fullscreen
    /// hypothesis is about -- reporting `unavailable` there loses the one
    /// panic these fields exist to explain. Measured 2026-09-05 (task2930's
    /// Z-order dump): with visibility dropped, this process shows two hidden
    /// ownerless titled windows -- our `Liveback` shell and winit's helper
    /// `Slint Window` at 38,38,860,694 -- so a "non-empty title" predicate
    /// would match the helper and report a rect that is not the shell's.
    /// Whether the match was visible travels back with the rect instead.
    unsafe extern "system" fn first_own_window(hwnd: HWND, out: LPARAM) -> BOOL {
        let mut process_id = 0;
        // SAFETY: `hwnd` is Windows' own, and the out-params are ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
            if process_id != std::process::id()
                // An owner means a dialog or a tool window, not the shell.
                || GetWindow(hwnd, GW_OWNER).is_ok()
            {
                return BOOL(1);
            }
            let mut title = [0u16; 64];
            let written = GetWindowTextW(hwnd, &mut title) as usize;
            if !is_main_window_title(&String::from_utf16_lossy(&title[..written])) {
                return BOOL(1);
            }
            let mut rect = RECT::default();
            if GetWindowRect(hwnd, &mut rect).is_err() {
                return BOOL(1);
            }
            *(out.0 as *mut Option<(RECT, bool)>) = Some((rect, IsWindowVisible(hwnd).as_bool()));
        }
        // Stop at the first match.
        BOOL(0)
    }

    /// The shell's rect and whether it was visible at that same instant, or
    /// `None` when no window of ours answers to the name.
    pub(super) fn own_window_rect() -> Option<((i32, i32, i32, i32), bool)> {
        let mut found: Option<(RECT, bool)> = None;
        // SAFETY: the callback only writes through the pointer handed here,
        // and `found` outlives the enumeration. The `Result` is deliberately
        // ignored: `EnumWindows` reports failure whenever the callback returns
        // FALSE, which is exactly what a *successful* find does here.
        let _ = unsafe {
            EnumWindows(
                Some(first_own_window),
                LPARAM(&mut found as *mut Option<(RECT, bool)> as isize),
            )
        };
        found.map(|(rect, visible)| ((rect.left, rect.top, rect.right, rect.bottom), visible))
    }

    /// The executable behind whatever window is in front (task2930). This is
    /// the direct evidence for task2900's exclusive-fullscreen hypothesis: if
    /// another app owned the foreground when the D3D surface fell over, its
    /// name is the thing that says so.
    pub(super) fn foreground_process() -> Option<String> {
        // SAFETY: every handle below is checked before use, and the process
        // handle is closed on both paths.
        unsafe {
            let hwnd = GetForegroundWindow();
            if hwnd.is_invalid() {
                return None;
            }
            let mut process_id = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut process_id));
            // Same sequence as `capture::targets::image_path_for_process`,
            // written out here rather than shared: the crash trap must not
            // grow a dependency on the capture stack.
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
            let path = std::ffi::OsString::from_wide(&buffer[..length as usize]);
            Some(
                std::path::Path::new(&path)
                    .file_name()?
                    .to_string_lossy()
                    .into_owned(),
            )
        }
    }

    pub(super) fn deliberate_access_violation() -> ! {
        // SAFETY: none -- faulting on purpose is the entire point, and this is
        // only reachable behind `LIVEBACK_CRASH_TEST=av`.
        unsafe {
            std::ptr::null_mut::<u8>().write_volatile(1);
        }
        unreachable!("the write above faults");
    }
}

#[cfg(not(windows))]
mod platform {
    pub(super) fn install_exception_filter() {}
    pub(super) fn deliberate_access_violation() -> ! {
        panic!("no native fault to raise off Windows");
    }
    pub(super) fn own_window_rect() -> Option<((i32, i32, i32, i32), bool)> {
        None
    }
    pub(super) fn foreground_process() -> Option<String> {
        None
    }
}

/// Guards against the hook re-entering itself if logging is what panicked.
static PANICKING: AtomicBool = AtomicBool::new(false);

/// Set only by `LIVEBACK_CRASH_TEST=d3d_panic`, so the window diagnostics can
/// be shown to fire without waiting for the real thing to happen again
/// (task2930). Never raised on a normal launch.
static FORCE_D3D_TEST_MATCH: AtomicBool = AtomicBool::new(false);

/// Installs both traps. Call once, right after the subscriber exists -- before
/// it, the records would go nowhere.
pub fn install() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if !PANICKING.swap(true, Ordering::SeqCst) {
            let thread = std::thread::current();
            let at = info.location();
            let d3d = at.is_some_and(|at| is_d3d_surface_panic(at.file()))
                || FORCE_D3D_TEST_MATCH.load(Ordering::SeqCst);
            if d3d {
                // Two call sites rather than one with optional fields:
                // `tracing`'s fields are fixed per call, and an unrelated
                // panic should not carry two empty window columns.
                //
                // Both lookups allocate, for the same reason `module_for`
                // does: a record that reaches the log is worth more than a
                // hook that is austere in cases it will never see. Each fails
                // to `None` on its own without taking the other -- or the
                // panic record -- down with it.
                //
                // Only one call in either of them sends a window message:
                // `GetWindowTextW` on a same-process window sends
                // `WM_GETTEXT`. Skia's D3D surface panics on the winit
                // event-loop thread, which is that window's own thread, so it
                // is dispatched inline rather than waiting on a pump that is
                // no longer running. Dropping the visibility filter (task2950)
                // widens who gets asked, but not across threads: task2930's
                // Z-order dump measured our hidden ownerless windows to be
                // winit's own (`Liveback` and `Slint Window`), created on that
                // same event-loop thread.
                let window = platform::own_window_rect();
                tracing::error!(
                    target: "task750_crash",
                    thread = thread.name().unwrap_or("unnamed"),
                    location = at.map(|at| at.to_string()),
                    payload = %info,
                    window_rect = window
                        .map(|((l, t, r, b), _)| format!("{l},{t},{r},{b}"))
                        .unwrap_or_else(|| "unavailable".into()),
                    // `Option<bool>` rather than the rect's string placeholder:
                    // `tracing::Value`'s blanket impl for `Option<T>` skips the
                    // field entirely on `None` and records a real bool on
                    // `Some`, so this prints unquoted `window_visible=false`
                    // instead of the quoted `window_visible="false"` a
                    // `String` placeholder would give, and drops the field
                    // altogether when no window was found (task2950).
                    window_visible = window.map(|(_, visible)| visible),
                    foreground_process = platform::foreground_process()
                        .unwrap_or_else(|| "unknown".into()),
                    "panic"
                );
            } else {
                tracing::error!(
                    target: "task750_crash",
                    thread = thread.name().unwrap_or("unnamed"),
                    location = at.map(|at| at.to_string()),
                    payload = %info,
                    "panic"
                );
            }
            PANICKING.store(false, Ordering::SeqCst);
        }
        // Keep whatever the default did. It writes to a stderr this build does
        // not have, but that is the shell's business, not ours to remove.
        default_hook(info);
    }));
    platform::install_exception_filter();
}

/// Acts on `LIVEBACK_CRASH_TEST`, which exists so the traps above can be shown
/// to work. Unset -- the only case that ships -- returns immediately.
pub fn run_crash_test_if_requested() {
    let Some(test) = requested_crash_test() else {
        return;
    };
    if test == CrashTest::D3dPanicAfterRun {
        // Deferred on purpose: firing here would beat the window into
        // existence, which is exactly what this variant exists to avoid. The
        // bin arms a timer just before `ui.run()` instead (task2940).
        return;
    }
    fire_crash_test(test);
}

/// What `LIVEBACK_CRASH_TEST` currently asks for. Pure -- reading the
/// environment twice is cheaper than threading the answer through the startup
/// path, so the caller that arms task2940's timer just asks again.
pub fn requested_crash_test() -> Option<CrashTest> {
    crash_test_from(std::env::var("LIVEBACK_CRASH_TEST").ok().as_deref())
}

/// Crashes the process the way `test` asks. Never returns.
pub fn fire_crash_test(test: CrashTest) -> ! {
    tracing::warn!(
        target: "task750_crash",
        ?test,
        "LIVEBACK_CRASH_TEST is set; crashing on purpose"
    );
    // The appender is non-blocking, so the warning above is still in the
    // writer's queue. Give it a moment to reach the file before the process
    // stops existing -- without this the deliberate crash proves nothing.
    std::thread::sleep(std::time::Duration::from_millis(300));
    match test {
        CrashTest::Panic => panic!("task750: deliberate panic from LIVEBACK_CRASH_TEST"),
        CrashTest::AccessViolation => platform::deliberate_access_violation(),
        // One arm: the two differ only in when they are reached, never in what
        // they raise.
        CrashTest::D3dPanic | CrashTest::D3dPanicAfterRun => {
            FORCE_D3D_TEST_MATCH.store(true, Ordering::SeqCst);
            panic!("task2930: deliberate d3d-flavoured panic from LIVEBACK_CRASH_TEST")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_known_spellings_ask_for_a_crash() {
        assert_eq!(crash_test_from(Some("panic")), Some(CrashTest::Panic));
        assert_eq!(
            crash_test_from(Some("av")),
            Some(CrashTest::AccessViolation)
        );
        assert_eq!(
            crash_test_from(Some("d3d_panic")),
            Some(CrashTest::D3dPanic)
        );
        assert_eq!(
            crash_test_from(Some("d3d_panic_after_run")),
            Some(CrashTest::D3dPanicAfterRun)
        );
        // Trimmed, because a value set from a script picks up whitespace.
        assert_eq!(
            crash_test_from(Some(" av ")),
            Some(CrashTest::AccessViolation)
        );
    }

    /// The window diagnostics pick a window by name, so the name has to be
    /// exact: `Slint Window` is a real hidden ownerless window of ours
    /// (task2930's Z-order dump) and matching it would put the wrong rect in
    /// the crash record.
    #[test]
    fn only_the_shell_answers_to_the_main_window_title() {
        assert!(is_main_window_title("Liveback"));
        assert!(!is_main_window_title("Slint Window"));
        assert!(!is_main_window_title(""));
        assert!(!is_main_window_title("liveback"));
        // `GetWindowTextW` reports the length it copied, so a stray trailing
        // NUL or space would mean the buffer was sliced wrong, not a match.
        assert!(!is_main_window_title("Liveback "));
        assert!(!is_main_window_title("Liveback\0"));
    }

    /// The shipped case. Anything unrecognised has to be inert: a typo here
    /// would take down a user's session for nothing.
    #[test]
    fn anything_else_is_inert() {
        assert_eq!(crash_test_from(None), None);
        assert_eq!(crash_test_from(Some("")), None);
        assert_eq!(crash_test_from(Some("1")), None);
        assert_eq!(crash_test_from(Some("true")), None);
        assert_eq!(crash_test_from(Some("PANIC")), None);
        assert_eq!(crash_test_from(Some("access_violation")), None);
        // The d3d entry gets the same strictness as the other two: a near
        // miss must not fire the diagnostics path either.
        assert_eq!(crash_test_from(Some("d3dpanic")), None);
        assert_eq!(crash_test_from(Some("D3D_PANIC")), None);
        assert_eq!(crash_test_from(Some("d3d")), None);
        // task2940's spelling gets the same treatment, and must not be reached
        // by loosening the plain `d3d_panic` match into a prefix test either.
        assert_eq!(crash_test_from(Some("d3dpanicafterrun")), None);
        assert_eq!(crash_test_from(Some("D3D_PANIC_AFTER_RUN")), None);
        assert_eq!(crash_test_from(Some("d3d_panic_after")), None);
        assert_eq!(crash_test_from(Some("d3d_panic-after-run")), None);
    }

    /// The gate for the extra window fields. Real locations arrive with
    /// backslashes on Windows and forward slashes from a cargo-vendored path,
    /// so neither separator may decide it.
    #[test]
    fn only_the_d3d_surface_file_carries_the_window_fields() {
        assert!(is_d3d_surface_panic(
            r"C:\Users\x\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\i-slint-renderer-skia-1.17.1\src\d3d_surface.rs"
        ));
        assert!(is_d3d_surface_panic(
            "/cargo/registry/src/i-slint-renderer-skia-1.17.1/src/d3d_surface.rs"
        ));
        assert!(!is_d3d_surface_panic("src/crash.rs"));
        assert!(!is_d3d_surface_panic("src/gpu/d3d.rs"));
        assert!(!is_d3d_surface_panic("src/capture/worker/surface.rs"));
    }

    #[test]
    fn the_codes_worth_naming_are_named() {
        assert_eq!(exception_name(0xC000_0005), Some("ACCESS_VIOLATION"));
        // The one this machine's microcode erratum produces in compilers, and
        // the reason the module path in the record matters.
        assert_eq!(exception_name(0xC000_001D), Some("ILLEGAL_INSTRUCTION"));
        assert_eq!(exception_name(0xDEAD_BEEF), None);
    }
}
