//! The desktop integration the Tauri plugins used to provide (task130):
//! single-instance, global hotkeys, OS toasts, login-item registration and the
//! explorer reveal. Everything here is either a few lines of Win32 or the very
//! crate the corresponding Tauri plugin wraps, so the behaviour matches the
//! shipping build rather than merely resembling it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;

use global_hotkey::hotkey::HotKey;
use global_hotkey::GlobalHotKeyManager;
use livia::ui_state::hotkeys::Registrar;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HWND, POINT, RECT, WAIT_OBJECT_0,
};
use windows::Win32::Foundation::{GetLastError, WIN32_ERROR};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    ClientToScreen, CreateBitmap, CreateDIBSection, DeleteObject, EnumDisplaySettingsW,
    MonitorFromPoint, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DEVMODEW, DIB_RGB_COLORS,
    ENUM_CURRENT_SETTINGS, MONITOR_DEFAULTTONULL,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY, HKEY_CURRENT_USER, REG_ROUTINE_FLAGS, RRF_RT_REG_DWORD, RRF_RT_REG_SZ,
};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    ILCreateFromPathW, ILFree, ITaskbarList3, SHOpenFolderAndSelectItems, ShellExecuteW,
    TaskbarList,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetAncestor, GetClientRect, GetForegroundWindow, GetWindow, IsIconic,
    IsWindowVisible, SetForegroundWindow, ShowWindow, WindowFromPoint, GA_ROOT, GW_OWNER, HICON,
    ICONINFO, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNORMAL,
};

/// Identifier from `tauri.conf.json`, which is also the AUMID the installer's
/// Start Menu shortcut registers. A toast sent under an unregistered AUMID is
/// dropped by Windows without an error (`Toast::show` still returns `Ok`), so
/// on a machine that has **never been installed** nothing appears -- same as
/// the Tauri build. Registration is per-user and outlives the installed copy:
/// the installer writes `DisplayName` under
/// `HKCU\Software\Classes\AppUserModelId\com.liveback.desktop`, so once a
/// machine has been through the installer even a raw `cargo run` exe *does*
/// show toasts. `docs/development.md`「OS 通知は AUMID の登録が要る」 is the
/// source; the sentence this line used to quote at `README.md` is not in it.
const APP_USER_MODEL_ID: &str = "com.liveback.desktop";
/// Login-item name under `HKCU\...\Run`.
const AUTOSTART_NAME: &str = "Liveback";

// `Local\` rather than `Global\`: one instance per logon session, which is what
// tauri-plugin-single-instance enforces too.
const INSTANCE_MUTEX: &str = "Local\\livia-single-instance";
const ACTIVATE_EVENT: &str = "Local\\livia-activate";

/// The two kernel-object names this launch claims (task3910).
///
/// Without an override they are [`INSTANCE_MUTEX`] and [`ACTIVATE_EVENT`]
/// unchanged, so a second launch still hands off to the running instance the
/// way it always did. With one, both names carry a suffix derived from the
/// override, so a launch pointed at a different buffer starts its *own*
/// instance instead of being swallowed -- KNOWLEDGE.md already records that
/// failure for `LIVEBACK_CRASH_TEST`, where a resident `liveback.exe` turns the
/// launch into a silent `exit 0` and the environment variable is never read.
/// Two launches carrying the *same* override still collapse into one instance,
/// which is what makes it a single-instance rule per buffer rather than none.
pub(super) fn single_instance_names(override_root: Option<&Path>) -> (String, String) {
    let Some(root) = override_root else {
        return (INSTANCE_MUTEX.to_owned(), ACTIVATE_EVENT.to_owned());
    };
    // FNV-1a rather than `DefaultHasher`: the two launches that have to agree
    // may be two different builds of the exe, and `DefaultHasher`'s output is
    // explicitly not stable across releases. Case-folded because Windows paths
    // are.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.to_string_lossy().to_lowercase().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (
        format!("{INSTANCE_MUTEX}-{hash:016x}"),
        format!("{ACTIVATE_EVENT}-{hash:016x}"),
    )
}

/// A null-terminated UTF-16 buffer for a Win32 name. The `Vec` has to outlive
/// every `PCWSTR` taken from it, so callers bind it before pointing at it.
fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Held for the process lifetime by the first instance. A later launch finds the
/// mutex taken, signals [`ACTIVATE_EVENT`] and exits; this instance notices on
/// its next drain tick and raises its window.
pub(super) struct SingleInstance {
    /// Never touched again -- owning the name for as long as the process lives
    /// *is* the guard. Released by the kernel at exit.
    _mutex: HANDLE,
    event: HANDLE,
}

impl SingleInstance {
    /// Auto-reset, so each second launch is reported exactly once.
    pub(super) fn activation_requested(&self) -> bool {
        unsafe { WaitForSingleObject(self.event, 0) == WAIT_OBJECT_0 }
    }
}

/// The `.lvb` the launch that just signalled was started for, if it was started
/// for one at all (task580). Taken, so a later raise-only launch does not
/// re-open the same file.
pub(super) fn take_open_request() -> Option<std::path::PathBuf> {
    livia::handoff::take_open_request(&livia::handoff::request_directory()?)
}

/// `None` when another instance already holds the name -- the caller must exit
/// without opening a window.
///
/// `open_request` is the `.lvb` this launch was started for (task580). A second
/// instance leaves it where the first will find it *before* signalling, so the
/// activation and the file it is about can never arrive out of order.
pub(super) fn claim_single_instance(open_request: Option<&Path>) -> Option<SingleInstance> {
    let (mutex_name, event_name) = single_instance_names(livia::capture::buffer_root_override());
    // Bound here, not inline: the `PCWSTR`s below borrow these buffers.
    let mutex_name = wide(&mutex_name);
    let event_name = wide(&event_name);
    let (mutex_name, event_name) = (PCWSTR(mutex_name.as_ptr()), PCWSTR(event_name.as_ptr()));
    unsafe {
        let mutex = CreateMutexW(None, true, mutex_name);
        // Read before anything else can overwrite it: the call succeeds either
        // way, and this is the only thing that distinguishes the two outcomes.
        let taken = GetLastError() == WIN32_ERROR(ERROR_ALREADY_EXISTS.0);
        let mutex = mutex.ok()?;
        if taken {
            if let Some(path) = open_request {
                // A failure here costs the load, not the raise: the window
                // still comes forward below, showing whatever it already had.
                if let Some(directory) = livia::handoff::request_directory() {
                    if let Err(error) = livia::handoff::write_open_request(&directory, path) {
                        tracing::warn!(%error, "could not hand the file to the running instance");
                    }
                }
            }
            if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, event_name) {
                // The only thing this second instance exists to do. If it
                // fails the first window never comes forward and the launch
                // looks like nothing happened at all (task260).
                if let Err(error) = SetEvent(event) {
                    tracing::warn!(%error, "could not signal the running instance to come forward");
                }
                // Both handles belong to a process that returns `None` here and
                // exits immediately after; the kernel closes them either way,
                // and there is no later call that could notice a leak.
                let _ = CloseHandle(event);
            }
            let _ = CloseHandle(mutex);
            return None;
        }
        let event = CreateEventW(None, false, false, event_name).ok()?;
        Some(SingleInstance {
            _mutex: mutex,
            event,
        })
    }
}

/// Restores a window that was minimized or stashed in the tray and puts it in
/// front. `SW_SHOW` rather than `SW_RESTORE` for the non-minimized case: a
/// window that was maximized and then hidden must come back maximized.
pub(super) fn show_window(hwnd: HWND) {
    unsafe {
        // `ShowWindow`'s BOOL says whether the window *was* visible before the
        // call, not whether the call worked -- there is no failure here to
        // report (task260).
        let _ = ShowWindow(
            hwnd,
            if IsIconic(hwnd).as_bool() {
                SW_RESTORE
            } else {
                SW_SHOW
            },
        );
        // Windows can refuse this to a process without foreground rights, in
        // which case the taskbar button flashes instead. Nothing to do about it.
        let _ = SetForegroundWindow(hwnd);
    }
    // A window stashed in the tray loses its taskbar button, and the one
    // Windows builds for the reappearing window carries no overlay (task2860).
    repush_taskbar_recording_badge(hwnd);
}

/// Asks DWM for the Windows 11 rounded corners (task142). Applied
/// unconditionally: `DWMWCP_ROUND` is what the OS already gives a normal
/// window, so on a build that rounds `no-frame` windows by itself this is a
/// no-op, and on one that does not it is the fix. `DWMWCP_ROUNDSMALL` is the
/// tighter radius to fall back to if the default reads as too round.
/// Pre-Windows-11 the attribute is simply unsupported and the call fails.
pub(super) fn round_window_corners(hwnd: HWND) {
    let preference = DWMWCP_ROUND;
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            std::ptr::from_ref(&preference).cast(),
            std::mem::size_of_val(&preference) as u32,
        )
    };
    // Logged rather than swallowed (task231): this call used to run before the
    // window existed, so it silently did nothing for as long as task142 had
    // been "implemented".
    if let Err(error) = result {
        tracing::warn!(%error, "window corner preference could not be set");
    }
}

/// The window's dpi, 96 (= 100%) if Windows will not say (`GetDpiForWindow`
/// answers 0 on an invalid handle). Falling back rather than propagating: the
/// only caller turns this into the resize band, and a 0 there would take the
/// band away entirely -- worse than the too-narrow band task4120 is fixing.
pub(super) fn window_dpi(hwnd: HWND) -> u32 {
    match unsafe { GetDpiForWindow(hwnd) } {
        0 => {
            tracing::warn!("window dpi unavailable, assuming 96");
            96
        }
        dpi => dpi,
    }
}

/// The client area's real size in physical px, straight from `GetClientRect`
/// (task t260914-a025). `None` when the call fails. This is the number
/// `slint::Window::size()` is supposed to agree with; the stage log puts the
/// two side by side so a disagreement is read off one line, not inferred.
pub(super) fn client_size(hwnd: HWND) -> Option<(i32, i32)> {
    let mut client = RECT::default();
    unsafe { GetClientRect(hwnd, &mut client) }.ok()?;
    Some((client.right - client.left, client.bottom - client.top))
}

// The "Show animations in Windows" switch (Settings > Accessibility > Visual
// effects) used to be read here, as this platform's `prefers-reduced-motion`,
// and it gated the empty state's drifting waves and its breathing glyph
// (round7 §3). task3940 stopped both unconditionally -- they were presenting at
// the monitor's refresh rate on an idle screen -- so there is no longer any
// perpetual motion for the switch to turn off, and the query went with it.

/// Whether the notification area is painted on a light background (task2220).
///
/// `SystemUsesLightTheme` is the taskbar's own setting, deliberately not
/// `AppsUseLightTheme` (in-window chrome) and not slint's `Palette.color-scheme`
/// -- the tray component has no window to read a scheme from. A missing or
/// unreadable value means dark, the Windows 11 default and the single asset the
/// tray shipped before this.
///
/// Read at start-up and again whenever the capture pipeline pushes a tooltip.
/// Known limitation: flipping the system theme while nothing starts or stops
/// leaves the previous asset up until the next such push.
pub(super) fn system_uses_light_theme() -> bool {
    personalize_flag(w!("SystemUsesLightTheme"))
}

/// Whether the *window* chrome should be painted light (round16).
///
/// `AppsUseLightTheme` is the in-window half of the same Personalize key the
/// tray reads, and the two are set independently -- a machine with a light
/// taskbar and dark apps is a supported Windows configuration, so the theme
/// setting's 「システム」 must not borrow the tray's answer.
///
/// Read once at start-up. Flipping the system theme while the app runs leaves
/// the window on the previous theme until the next launch -- the same known
/// limitation the tray asset has carried since task2220, and what round16 §0
/// leaves to this side to decide.
pub(super) fn apps_use_light_theme() -> bool {
    personalize_flag(w!("AppsUseLightTheme"))
}

/// One registry value, read for nothing but the answer the caller wants
/// (task4140 generalised what the theme flag below had inlined).
///
/// `hive` / `subkey` / `name` are the three coordinates that vary, and `kind`
/// is the `RRF_RT_*` type filter `RegGetValueW` applies before it reports
/// anything. A **default** value is the empty name, which `PCWSTR::null()`
/// spells -- that is how the keys the installer writes as
/// `WriteRegStr HKCU "..." ""` have to be read.
///
/// `dword` is `Some` only for an `RRF_RT_REG_DWORD` read. `None` asks
/// `RegGetValueW` whether the value is there and nothing more: handed no
/// buffer it returns `ERROR_SUCCESS` plus the size the value *would* need.
/// That is the only safe shape for a string whose length this side does not
/// control -- reading one into a fixed buffer turns any value longer than it
/// into `ERROR_MORE_DATA`, which this function would report as "missing", and
/// the thumbnail handler's `InprocServer32` default is an absolute path under
/// `$INSTDIR`. On a machine where nothing is registered that mistake is
/// invisible, because the reader answers `false` either way.
///
/// Read-only by construction: `RegGetValueW` is the whole of it, and nothing
/// in this module opens a key for writing.
fn registry_value(
    hive: HKEY,
    subkey: PCWSTR,
    name: PCWSTR,
    kind: REG_ROUTINE_FLAGS,
    dword: Option<&mut u32>,
) -> bool {
    let mut dword_size = std::mem::size_of::<u32>() as u32;
    let data = dword.map(|value| std::ptr::from_mut(value).cast::<std::ffi::c_void>());
    // Paired with `data`: a size without a buffer would ask for the length of
    // something nobody is reading.
    let size = data.map(|_| std::ptr::from_mut(&mut dword_size));
    let status = unsafe { RegGetValueW(hive, subkey, name, kind, None, data, size) };
    status.is_ok()
}

/// Whether one REG_SZ value exists, contents unread. See `registry_value` for
/// why the contents stay unread.
fn registry_string_present(hive: HKEY, subkey: PCWSTR, name: PCWSTR) -> bool {
    registry_value(hive, subkey, name, RRF_RT_REG_SZ, None)
}

/// One `HKCU\...\Themes\Personalize` DWORD, as a bool. A missing or unreadable
/// value means dark, the Windows 11 default.
fn personalize_flag(name: PCWSTR) -> bool {
    let mut value: u32 = 0;
    registry_value(
        HKEY_CURRENT_USER,
        w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
        name,
        RRF_RT_REG_DWORD,
        Some(&mut value),
    ) && value != 0
}

/// Whether the installer's two *silently* dying registrations are in the
/// registry (task4140).
///
/// **These fields say what the registry holds, never whether the feature
/// works.** A `true` means the key is there. It does not mean `lvb_thumb.dll`
/// still exists at the path `InprocServer32` names, nor that the PNG `IconUri`
/// points at is on disk -- task1250 has a case where `InprocServer32` pointed
/// at an old DLL and thumbnails simply never appeared. A broken install
/// therefore reads as registered here, which is why the settings row this
/// feeds claims only that a registration is *missing*.
///
/// The other groups the installer writes are deliberately absent. The `.lvb`
/// association does not die silently -- Explorer's own icon and type name
/// change, so the user has already seen it. The uninstall entry is not a
/// feature of this app. And the login item is not the installer's to begin
/// with: `sync_auto_start` writes it at runtime, so it works uninstalled.
#[derive(Clone, Copy, Debug)]
pub(super) struct InstallerRegistration {
    /// Group A: `DisplayName` under
    /// `HKCU\Software\Classes\AppUserModelId\<AUMID>`. Without it Windows
    /// drops every toast this app sends *and* `Toast::show` still returns
    /// `Ok`, so no other signal exists -- see the `os_toast_result` log, which
    /// reports `handed_to_windows = true` for a toast nobody ever saw.
    /// `DisplayName` and not `IconUri`: a missing icon file leaves the toast
    /// working without a picture, which is a different failure.
    pub(super) aumid: bool,
    /// Group C: the thumbnail handler's `InprocServer32` default value. Read
    /// through the plain 64-bit view and needing no `KEY_WOW64_*`: the
    /// installer flips `SetRegView 64` because makensis is 32-bit and would
    /// otherwise land under `Wow6432Node`, where 64-bit Explorer never looks,
    /// but this process is already 64-bit and so reads where Explorer reads.
    pub(super) thumbnail_handler: bool,
}

/// Read once per launch, and logged once (task4140). The registration cannot
/// change under a running app: changing it means running the installer, which
/// stops this process first.
pub(super) fn installer_registration() -> InstallerRegistration {
    static READ: std::sync::OnceLock<InstallerRegistration> = std::sync::OnceLock::new();
    *READ.get_or_init(|| {
        // Built from the const rather than spelled out, so renaming the AUMID
        // cannot leave the detection reading a key nothing writes any more.
        let aumid_subkey = HSTRING::from(format!(
            r"Software\Classes\AppUserModelId\{APP_USER_MODEL_ID}"
        ));
        // `THUMBCLSID` in `installer/liveback.nsi`, which is
        // `CLSID_LVB_THUMBNAIL` in `crates/lvb-thumb/src/lib.rs`. Spelled out
        // because this binary depends on neither: the DLL is in the workspace
        // for NSIS to pick up, and nothing in the app calls it.
        let found = InstallerRegistration {
            aumid: registry_string_present(
                HKEY_CURRENT_USER,
                PCWSTR(aumid_subkey.as_ptr()),
                w!("DisplayName"),
            ),
            thumbnail_handler: registry_string_present(
                HKEY_CURRENT_USER,
                w!(r"Software\Classes\CLSID\{0D9EC9D9-F746-4C28-8662-3B546BB83CB9}\InprocServer32"),
                PCWSTR::null(),
            ),
        };
        // Both fields, every launch, whatever they are. A line that only
        // appeared for the missing half would read the way
        // `handed_to_windows = true` does in the toast log (task3630): never
        // false, so never evidence of anything.
        tracing::info!(
            event = "installer_registration",
            aumid = found.aumid,
            thumbnail_handler = found.thumbnail_handler,
            "installer registry keys as read at start-up; present means the key \
             is there, not that the feature works"
        );
        found
    })
}

/// How many times a second the display refreshes, or 0 when Windows will not
/// say (task1580). The stage's frame gate paces itself off this rather than a
/// hardcoded 60Hz, so a 120Hz or 144Hz panel is not throttled to 60.
///
/// The primary display, not the one the window happens to be on: catching a
/// drag onto a second panel of a different rate would mean a syscall per frame,
/// and being one refresh out only changes how much of the saving is collected,
/// never whether the picture is correct.
pub(super) fn display_refresh_hz() -> u32 {
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).unwrap_or_default(),
        ..Default::default()
    };
    let ok = unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) };
    if !ok.as_bool() {
        return 0;
    }
    // 0 and 1 are documented as "the hardware default", i.e. no answer.
    mode.dmDisplayFrequency
}

/// The 1px outline DWM draws just outside the client area (task231). Without
/// it a `no-frame` window on a dark desktop has no edge at all: the title bar
/// is `#0d1015` and so is whatever is behind it.
///
/// The colour is `chrome-hairline-strong` (blue-white 24%) already composited over
/// `chrome-titlebar` -- `DWMWA_BORDER_COLOR` takes an opaque `COLORREF` and
/// blends nothing, so the alpha has to be resolved here. `COLORREF` is
/// `0x00bbggrr`, the reverse of the `#rrggbb` the tokens are written in.
///
/// Windows 11 only. Earlier builds fail the call and keep their own border,
/// which is the same "unsupported attribute is a no-op" contract
/// `round_window_corners` relies on.
pub(super) fn set_window_border_color(hwnd: HWND) {
    // #3a4048: base + (fg - base) * 0.24 per channel, with fg = rgb(199,214,235)
    // over the #0d1015 title bar (round13 1c 藍鉄).
    let color = 0x0048_403a_u32;
    let result = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            std::ptr::from_ref(&color).cast(),
            std::mem::size_of_val(&color) as u32,
        )
    };
    if let Err(error) = result {
        tracing::warn!(%error, "window border colour could not be set");
    }
}

/// Blurs what shows through the see-through ground (t260919-fffd, round25
/// §4-5 「背後の屈折」). The ground is drawn at 85 % / light 92 % into a
/// premultiplied composition swapchain (t260916-ebdb); this puts a DWM blur of
/// the desktop under it, so the remaining 15 % / 8 % is the blurred backdrop
/// rather than a sharp one.
///
/// Undocumented on purpose: user32's `SetWindowCompositionAttribute`
/// (`WCA_ACCENT_POLICY`, `ACCENT_ENABLE_BLURBEHIND`), looked up at run time.
/// The documented `DWMWA_SYSTEMBACKDROP_TYPE` Acrylic is an opaque solid layer
/// under this swapchain -- `(84,84,84)` with no trace of the window behind, on
/// both desks it was measured on (the GTX 1080 desk on 22631, the RTX 4070 /
/// 1080p desk on 26200). The
/// accent blurs and keeps the colour behind (evidence `t260919-fffd-corners-blur`).
/// `BLURBEHIND` rather than `ACRYLICBLURBEHIND`: the two read the same pixels and
/// cost here, and the acrylic one is the accent with the history of lagging
/// behind a dragged window. The tint is alpha 1/255, i.e. none: the ground
/// supplies the colour. The blur is DWM's own; its radius and the design's
/// `saturate(150%)` are not adjustable. DWM clips it to the rounded corners.
///
/// A missing export or a refused call is logged and leaves the unblurred
/// see-through ground, which is what the window had before.
pub(super) fn blur_window_backdrop(hwnd: HWND) {
    #[repr(C)]
    struct AccentPolicy {
        state: u32,
        flags: u32,
        gradient_abgr: u32,
        animation: u32,
    }
    #[repr(C)]
    struct CompositionAttribute {
        attribute: u32,
        data: *mut std::ffi::c_void,
        size: usize,
    }
    type SetWindowCompositionAttribute =
        unsafe extern "system" fn(HWND, *mut CompositionAttribute) -> windows::core::BOOL;
    const WCA_ACCENT_POLICY: u32 = 19;
    const ACCENT_ENABLE_BLURBEHIND: u32 = 3;

    let Ok(user32) = (unsafe { GetModuleHandleW(w!("user32.dll")) }) else {
        tracing::warn!("window backdrop blur: user32 not loaded");
        return;
    };
    let Some(address) =
        (unsafe { GetProcAddress(user32, windows::core::s!("SetWindowCompositionAttribute")) })
    else {
        tracing::warn!("window backdrop blur: SetWindowCompositionAttribute not exported");
        return;
    };
    // SAFETY: the export's signature has been this since Windows 7.
    let set: SetWindowCompositionAttribute = unsafe { std::mem::transmute(address) };
    let mut policy = AccentPolicy {
        state: ACCENT_ENABLE_BLURBEHIND,
        flags: 0,
        gradient_abgr: 0x0100_0000,
        animation: 0,
    };
    let mut attribute = CompositionAttribute {
        attribute: WCA_ACCENT_POLICY,
        data: std::ptr::from_mut(&mut policy).cast(),
        size: std::mem::size_of::<AccentPolicy>(),
    };
    if !unsafe { set(hwnd, &mut attribute) }.as_bool() {
        tracing::warn!("window backdrop blur was refused");
    }
}

/// Stashes the window in the tray. Win32 rather than slint's `Window::hide()`
/// so the winit window survives: it keeps position, size and maximized state,
/// and the event loop never sees its last window go away.
pub(super) fn hide_window(hwnd: HWND) {
    unsafe {
        // Previous visibility, not success -- see `show_window` (task260).
        let _ = ShowWindow(hwnd, SW_HIDE);
    }
}

pub(super) fn is_foreground(hwnd: HWND) -> bool {
    unsafe { GetForegroundWindow() == hwnd }
}

/// Whether anything of the window is on screen at all. Stashed in the tray it
/// is `SW_HIDE`n behind slint's back (see `hide_window`), so slint's own
/// `is_visible` still says yes -- Win32 is the one that knows.
pub(super) fn is_window_on_screen(hwnd: HWND) -> bool {
    unsafe { IsWindowVisible(hwnd).as_bool() && !IsIconic(hwnd).as_bool() }
}

/// Minimized to the taskbar -- *not* the same as stashed in the tray, which is
/// `SW_HIDE` (see `hide_window`). Used to hold the renderer back while there is
/// no window to draw into (see the `RedrawRequested` arm in
/// `accept_dropped_sessions`).
pub(super) fn is_window_minimized(hwnd: HWND) -> bool {
    unsafe { IsIconic(hwnd).as_bool() }
}

/// Points sampled across the client area to decide `is_window_covered`: the
/// four corners pulled in far enough to clear the resize border, plus the
/// centre.
const COVER_SAMPLES: usize = 5;
/// How far in from each edge a corner sample sits. Wide enough to be inside
/// the client area proper on every DPI this ships at.
const COVER_INSET: i32 = 8;

/// Whether every sampled point of the window's client area belongs to some
/// other window -- something is fully on top of us (task2800).
///
/// Win32 has no "am I occluded" call. `DXGI_STATUS_OCCLUDED` is the swap
/// chain owner's to read, `SHQueryUserNotificationState` only sees exclusive
/// fullscreen D3D (the case that started this was a *maximised borderless*
/// editor, which it does not report), and winit's `WindowEvent::Occluded` is
/// documented `Unsupported` on Windows (winit 0.30.13, `src/event.rs`) even
/// though `unstable-winit-030` gives us the backend. Hit-testing our own
/// client area is what is left.
///
/// Client rect, not window rect: the DWM frame's rounded corners are not ours
/// and would report the desktop, which reads as covered forever. Five points
/// rather than an exhaustive region walk: one window on top is the case worth
/// catching, this runs on the UI tick, and a false negative only costs the
/// playback that was already running.
pub(super) fn is_window_covered(hwnd: HWND) -> bool {
    unsafe {
        let mut client = RECT::default();
        if GetClientRect(hwnd, &mut client).is_err() {
            return false;
        }
        let (width, height) = (client.right - client.left, client.bottom - client.top);
        // A zero-sized client area has nothing to sample, and answering
        // "covered" there would pause playback for a window that is merely
        // mid-resize.
        if width <= COVER_INSET * 2 || height <= COVER_INSET * 2 {
            return false;
        }
        let mut origin = POINT { x: 0, y: 0 };
        if !ClientToScreen(hwnd, &mut origin).as_bool() {
            return false;
        }
        let samples: [POINT; COVER_SAMPLES] = [
            POINT {
                x: origin.x + COVER_INSET,
                y: origin.y + COVER_INSET,
            },
            POINT {
                x: origin.x + width - 1 - COVER_INSET,
                y: origin.y + COVER_INSET,
            },
            POINT {
                x: origin.x + COVER_INSET,
                y: origin.y + height - 1 - COVER_INSET,
            },
            POINT {
                x: origin.x + width - 1 - COVER_INSET,
                y: origin.y + height - 1 - COVER_INSET,
            },
            POINT {
                x: origin.x + width / 2,
                y: origin.y + height / 2,
            },
        ];
        // One point that is still ours settles it; the rest only matter if
        // none is. Points over no monitor say nothing either way and are
        // skipped -- see `hit_is_ours`.
        let mut seen_other = false;
        for point in samples {
            match hit_is_ours(hwnd, point) {
                Some(true) => return false,
                Some(false) => seen_other = true,
                None => {}
            }
        }
        seen_other
    }
}

/// Whether the topmost window at `point` is this window or something this
/// window owns. `None` for a point that is on no monitor -- evidence of
/// nothing, because nobody can see that pixel in the first place.
///
/// The monitor test is not paranoia, it is the whole correctness of this
/// function (measured 2026-09-03): `WindowFromPoint` is **not** clipped to
/// the visible desktop. Asked about a coordinate past the right edge of the
/// only display, it happily returns the window whose rect covers it -- which,
/// for a window dragged half off screen, is *us*. Without this check those
/// corners report "still ours" and a stage buried under a fullscreen game
/// never counts as covered at all.
///
/// The owner hop is what keeps our own tooltips, menus and toasts from
/// reading as somebody else's window sitting on the stage -- they are
/// separate HWNDs, so `GA_ROOT` alone would call them a cover.
unsafe fn hit_is_ours(hwnd: HWND, point: POINT) -> Option<bool> {
    unsafe {
        if MonitorFromPoint(point, MONITOR_DEFAULTTONULL).is_invalid() {
            return None;
        }
        let hit = WindowFromPoint(point);
        if hit.is_invalid() {
            return None;
        }
        let root = GetAncestor(hit, GA_ROOT);
        Some(root == hwnd || GetWindow(root, GW_OWNER).is_ok_and(|owner| owner == hwnd))
    }
}

/// The payload half of [`toast`], apart so a template typo fails a test instead
/// of failing silently on the user's machine -- Windows answers a payload it
/// cannot parse by simply showing nothing.
///
/// Task4220: `SetInnerText` rather than the `format!` into XML that
/// tauri-winrt-notification did, because a session name carrying `&` or `<` is
/// ordinary here and the DOM escapes it for us.
fn toast_xml(
    title: &str,
    body: &str,
) -> windows::core::Result<windows::Data::Xml::Dom::XmlDocument> {
    use windows::core::HSTRING;
    use windows::Data::Xml::Dom::XmlDocument;

    let xml = XmlDocument::new()?;
    xml.LoadXml(&HSTRING::from(
        r#"<toast><visual><binding template="ToastGeneric"><text></text><text></text></binding></visual></toast>"#,
    ))?;
    let texts = xml.GetElementsByTagName(&HSTRING::from("text"))?;
    texts.GetAt(0)?.SetInnerText(&HSTRING::from(title))?;
    texts.GetAt(1)?.SetInnerText(&HSTRING::from(body))?;
    Ok(xml)
}

/// The `notifyIfHidden` half of `src/lib/notifications.ts` lives at the call
/// site; this is the `sendNotification` half.
///
/// **`handed_to_windows = true` means the call returned `Ok`, not that anything
/// appeared on screen** (spelled out 2026-09-09, task4150). There are at least
/// two ways to get `true` and a blank screen, and this function can tell them
/// apart from neither:
///
/// - the AUMID is not registered, so Windows drops the notification silently
///   -- `Show` still returns `Ok` (see `APP_USER_MODEL_ID`), which is
///   the state of every machine that has never run the installer;
/// - focus assist holds it while a game is in front.
///
/// So a log carrying `handed_to_windows = true` supports "the app got this
/// far", and nothing about delivery. The field name stays as it is -- it is
/// already named for what it proves, and renaming it would break the match
/// against every `task3630_stop_report` line recorded so far.
pub(super) fn toast(title: &str, body: &str) {
    use windows::core::HSTRING;
    use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
    // Task3630: both sides of the call, because `show()` returning `Ok` is not
    // the banner appearing -- Windows takes the notification and may hold it
    // (focus assist while a game is in front), or drop it outright for an
    // unregistered AUMID, so `Ok` plus "the user saw nothing" is the signature
    // of the OS suppressing it, and no line at all is the signature of the app
    // never getting here. `handed_to_windows` is named for what it actually
    // proves, since the earlier `warn!`-only shape could be read as a delivery
    // receipt.
    tracing::info!(
        target: "task3630_stop_report",
        event = "os_toast_show",
        %title,
        "handing a notification to Windows"
    );
    // Task4220: `windows` directly instead of `tauri-winrt-notification`, which
    // was the same two calls behind a crate whose name kept reading as "tauri is
    // still in this build". `SetInnerText` on the two nodes rather than the
    // crate's `format!` into XML, so a session name carrying `&` or `<` needs no
    // escape branch of ours.
    let shown = (|| -> windows::core::Result<()> {
        let xml = toast_xml(title, body)?;
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(APP_USER_MODEL_ID))?
            .Show(&ToastNotification::CreateToastNotification(&xml)?)
    })();
    match shown {
        Ok(()) => tracing::info!(
            target: "task3630_stop_report",
            event = "os_toast_result",
            handed_to_windows = true,
            "Windows accepted the notification"
        ),
        Err(error) => {
            tracing::info!(
                target: "task3630_stop_report",
                event = "os_toast_result",
                handed_to_windows = false,
                %error,
                "Windows refused the notification"
            );
            tracing::warn!(event = "notification_failed", %error, "OS notification failed");
        }
    }
}

/// `syncAutoStart` from `src/lib/settings.ts`: flip only when the registry
/// disagrees, then report what the registry actually holds -- a failed write
/// must not leave the toggle claiming success.
pub(super) fn sync_auto_start(enabled: bool) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let launcher =
        auto_launch::AutoLaunch::new(AUTOSTART_NAME, &exe.to_string_lossy(), &[] as &[&str]);
    let active = launcher.is_enabled().unwrap_or(false);
    let changed = if enabled && !active {
        launcher.enable()
    } else if !enabled && active {
        launcher.disable()
    } else {
        Ok(())
    };
    if let Err(error) = changed {
        tracing::warn!(event = "autostart_failed", %error, enabled, "Login item update failed");
    }
    launcher.is_enabled().unwrap_or(false)
}

fn open_directory_failed() -> &'static str {
    livia::ui_state::settings::open_directory_failed(super::tr_locale())
}

/// Opens an explorer window on `folder`, selecting `item` inside it when given.
///
/// The reveal replaces the `explorer.exe /select,` spawn: no quoting rules, no
/// second process, and a path holding a comma or a quote survives.
pub(super) fn open_in_explorer(folder: &Path, item: Option<&Path>) -> Result<(), String> {
    let Some(item) = item else {
        return open_folder(folder);
    };
    reveal_items(folder, &[item])
}

/// Opens an explorer window on `folder` with every one of `items` selected in
/// it -- one file, or the clip screen's whole selection (t260916-5568).
/// `SHOpenFolderAndSelectItems` takes the selection as absolute item PIDLs in
/// one call, so several files are one window, not several.
pub(super) fn reveal_items(folder: &Path, items: &[&Path]) -> Result<(), String> {
    unsafe {
        // The session worker calls this off the UI thread, where no apartment
        // has been entered yet. An already-initialized thread answers `S_FALSE`
        // or `RPC_E_CHANGED_MODE`; both mean "someone else set it up", which is
        // fine here -- the shell call works either way.
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let folder_pidl = ILCreateFromPathW(&HSTRING::from(folder.as_os_str()));
        if folder_pidl.is_null() {
            return Err(open_directory_failed().into());
        }
        let item_pidls: Vec<_> = items
            .iter()
            .map(|item| ILCreateFromPathW(&HSTRING::from(item.as_os_str())))
            .collect();
        let result = if item_pidls.is_empty() || item_pidls.iter().any(|pidl| pidl.is_null()) {
            Err(windows::core::Error::from_win32())
        } else {
            let selection: Vec<*const ITEMIDLIST> = item_pidls
                .iter()
                .map(|pidl| *pidl as *const ITEMIDLIST)
                .collect();
            SHOpenFolderAndSelectItems(folder_pidl, Some(selection.as_slice()), 0)
        };
        ILFree(Some(folder_pidl));
        for pidl in item_pidls {
            if !pidl.is_null() {
                ILFree(Some(pidl));
            }
        }
        result.map_err(|error| error.message())
    }
}

/// Opens the folder itself. Deliberately *not* `SHOpenFolderAndSelectItems`
/// with an empty selection: that opens the parent with this folder selected
/// (verified against `%LOCALAPPDATA%\Liveback\buffer`), which is a reveal,
/// not an open. `ShellExecuteW` also reuses an explorer window already showing
/// the folder instead of stacking another one.
fn open_folder(folder: &Path) -> Result<(), String> {
    if shell_open(folder) {
        Ok(())
    } else {
        Err(open_directory_failed().into())
    }
}

/// `ShellExecuteW`'s `open` verb, and its legacy "greater than 32 means
/// success" contract -- still what the API documents.
fn shell_open(path: &Path) -> bool {
    let instance = unsafe {
        ShellExecuteW(
            None,
            w!("open"),
            &HSTRING::from(path.as_os_str()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    instance.0 as isize > 32
}

/// Draws the 16x16 red-dot recording badge at runtime (task2510). No asset:
/// `assets/tray-icon/` is fenced off by build.rs for the tray alone, and
/// `assets/app-icon/` carries no recording variant -- a filled circle is
/// shorter to draw than to ship. round14 §2-3 kept the circle and declined an
/// asset, so this stays the drawing path.
fn recording_overlay_icon(light_taskbar: bool) -> Option<HICON> {
    const SIZE: i32 = 16;
    // Inset from the 16px square the icon must be: round14 §2-3 described a
    // dot filling the face, and the user asked for a slightly smaller one
    // (2026-09-01). The margin also keeps the circle off the taskbar ground,
    // which is the separation §2-4 is still asking about.
    const RADIUS: f32 = 6.5;
    unsafe {
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: SIZE,
                // Negative height = top-down rows, so the pixel index math
                // below reads naturally.
                biHeight: -SIZE,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = match CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            Ok(bitmap) if !bits.is_null() => bitmap,
            Ok(bitmap) => {
                let _ = DeleteObject(bitmap.into());
                tracing::warn!("recording badge DIB came back without pixel bits");
                return None;
            }
            Err(error) => {
                tracing::warn!(%error, "recording badge DIB could not be created");
                return None;
            }
        };
        // Premultiplied BGRA, red dot centred with a 1px anti-aliased rim.
        // The two reds are the tray's recording assets, pixel for pixel
        // (round14 §2-1): `color-error` over a dark taskbar, the deeper red
        // over a light one. Both faces show the same state, so they share the
        // same colour.
        let (red, green, blue) = if light_taskbar {
            (0xBEu32, 0x23, 0x23)
        } else {
            (0xF1u32, 0x64, 0x5F)
        };
        let pixels = std::slice::from_raw_parts_mut(bits.cast::<u32>(), (SIZE * SIZE) as usize);
        for y in 0..SIZE {
            for x in 0..SIZE {
                let dx = x as f32 - 7.5;
                let dy = y as f32 - 7.5;
                let coverage = (RADIUS - (dx * dx + dy * dy).sqrt()).clamp(0.0, 1.0);
                let alpha = (coverage * 255.0) as u32;
                let premul = |channel: u32| channel * alpha / 255;
                pixels[(y * SIZE + x) as usize] =
                    (alpha << 24) | (premul(red) << 16) | (premul(green) << 8) | premul(blue);
            }
        }
        // The alpha channel of the colour plane drives transparency; the AND
        // mask only has to exist. CreateBitmap with no bits would leave it
        // uninitialised, so hand it explicit zeroes.
        let mask_bits = [0u8; (SIZE * SIZE / 8) as usize];
        let mask = CreateBitmap(SIZE, SIZE, 1, 1, Some(mask_bits.as_ptr().cast()));
        let icon = CreateIconIndirect(&ICONINFO {
            fIcon: true.into(),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        });
        let _ = DeleteObject(color.into());
        if !mask.is_invalid() {
            let _ = DeleteObject(mask.into());
        }
        match icon {
            Ok(icon) => Some(icon),
            Err(error) => {
                tracing::warn!(%error, "recording badge icon could not be created");
                None
            }
        }
    }
}

/// Shows or clears the red recording badge on the taskbar button (task2510).
/// Runs on the UI thread (called from an `invoke_from_event_loop` closure), so
/// the COM proxy and the icon live in thread-locals and are built once. Every
/// failure ends at a warn -- the badge is decoration, never worth the UI.
///
/// The taskbar theme is read per call and picks one of two cached icons, the
/// same two reds the tray swaps between (round14 §2-1/§2-2). That inherits the
/// tray's limitation, which round14 accepted: flipping the system theme while
/// nothing starts or stops leaves the previous colour up until the next push.
///
/// Known limit, by design: a window hidden to the tray has no taskbar button,
/// so no overlay can show there; the tray icon keeps that duty. The badge is
/// only pushed when the state changes, so the button Windows rebuilds when the
/// window comes back would stay bare -- [`repush_taskbar_recording_badge`]
/// replays the last push from `show_window` (task2860).
pub(super) fn set_taskbar_recording_badge(hwnd: HWND, recording: bool, description: &str) {
    LAST_BADGE.with(|last| {
        *last.borrow_mut() = Some((recording, description.to_owned()));
    });
    thread_local! {
        // `Some(None)` = tried and failed; stay quiet instead of re-warning on
        // every tooltip update.
        static TASKBAR: RefCell<Option<Option<ITaskbarList3>>> = const { RefCell::new(None) };
        // One icon per taskbar theme, indexed by `light_taskbar as usize`.
        static BADGE: RefCell<[Option<Option<HICON>>; 2]> = const { RefCell::new([None, None]) };
    }
    TASKBAR.with(|taskbar| {
        let mut taskbar = taskbar.borrow_mut();
        let taskbar = taskbar.get_or_insert_with(|| unsafe {
            // Same contract as the explorer reveal above: `S_FALSE` /
            // `RPC_E_CHANGED_MODE` both mean "already initialised", fine here.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_ALL)
                .and_then(|list| list.HrInit().map(|()| list))
                .map_err(|error| {
                    tracing::warn!(%error, "taskbar list unavailable; recording badge disabled");
                })
                .ok()
        });
        let Some(taskbar) = taskbar else { return };
        let icon = if recording {
            let light = system_uses_light_theme();
            let badge = BADGE.with(|badge| {
                *badge.borrow_mut()[usize::from(light)]
                    .get_or_insert_with(|| recording_overlay_icon(light))
            });
            match badge {
                Some(icon) => icon,
                // Creation already warned once; nothing to overlay.
                None => return,
            }
        } else {
            HICON::default()
        };
        let description = if recording {
            HSTRING::from(description)
        } else {
            HSTRING::new()
        };
        if let Err(error) = unsafe { taskbar.SetOverlayIcon(hwnd, icon, &description) } {
            tracing::warn!(%error, recording, "taskbar recording badge could not be updated");
        }
    });
}

thread_local! {
    /// Last badge handed to `SetOverlayIcon`, replayed on show (task2860).
    /// Lives beside the badge's other thread-locals: both writers run on the UI
    /// thread (`set_tray_tooltip`'s `invoke_from_event_loop`, the drain timer,
    /// the tray menu's `on_open`).
    static LAST_BADGE: RefCell<Option<(bool, String)>> = const { RefCell::new(None) };
}

/// Puts the last known badge back on a taskbar button Windows has just rebuilt
/// (task2860). Nothing to do before the first push, or when that push cleared
/// the badge -- a fresh button starts bare.
pub(super) fn repush_taskbar_recording_badge(hwnd: HWND) {
    let Some((true, description)) = LAST_BADGE.with(|last| last.borrow().clone()) else {
        return;
    };
    set_taskbar_recording_badge(hwnd, true, &description);
}

/// The platform end of [`Registrar`]. `global_hotkey` unregisters by `HotKey`
/// rather than by id, so the ids handed to the pure state live here too.
pub(super) struct HotkeyRegistrar {
    manager: GlobalHotKeyManager,
    registered: RefCell<HashMap<u32, HotKey>>,
}

impl HotkeyRegistrar {
    pub(super) fn new() -> Option<Self> {
        match GlobalHotKeyManager::new() {
            Ok(manager) => Some(Self {
                manager,
                registered: RefCell::new(HashMap::new()),
            }),
            Err(error) => {
                tracing::warn!(event = "hotkey_manager_unavailable", %error, "Global hotkeys unavailable");
                None
            }
        }
    }
}

impl Registrar for HotkeyRegistrar {
    fn register(&self, accelerator: &str) -> Result<u32, String> {
        // The stored accelerators are Tauri's format ("Ctrl+Shift+R"), which
        // `global_hotkey` parses too: its `parse_key` accepts the bare letter as
        // well as the `KeyR` code name.
        let hotkey = HotKey::from_str(accelerator).map_err(|error| error.to_string())?;
        self.manager
            .register(hotkey)
            .map_err(|error| error.to_string())?;
        self.registered.borrow_mut().insert(hotkey.id(), hotkey);
        Ok(hotkey.id())
    }

    fn unregister(&self, id: u32) -> Result<(), String> {
        let Some(hotkey) = self.registered.borrow_mut().remove(&id) else {
            return Ok(());
        };
        self.manager
            .unregister(hotkey)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Task3910. The no-override names are the exact literals the shipping
    /// build has always used -- an accidental change here would split every
    /// ordinary second launch into a second window instead of raising the
    /// first.
    #[test]
    fn without_a_buffer_override_the_instance_names_are_the_ones_the_app_always_had() {
        assert_eq!(
            single_instance_names(None),
            (
                "Local\\livia-single-instance".to_owned(),
                "Local\\livia-activate".to_owned()
            )
        );
    }

    #[test]
    fn a_buffer_override_moves_both_names_and_two_launches_with_the_same_one_still_agree() {
        let one = single_instance_names(Some(Path::new(r"C:\Temp\lvb-a")));
        let same = single_instance_names(Some(Path::new(r"C:\Temp\lvb-a")));
        let other = single_instance_names(Some(Path::new(r"C:\Temp\lvb-b")));

        assert_eq!(one, same, "the same override is the same instance");
        assert_ne!(one.0, other.0, "a different buffer is a different instance");
        assert_ne!(one.1, other.1);
        assert_ne!(one, single_instance_names(None));
        assert!(one.0.starts_with("Local\\livia-single-instance-"));
        assert!(one.1.starts_with("Local\\livia-activate-"));
    }

    /// Windows paths are case-insensitive, so two launches that spell the same
    /// folder differently must not each start their own instance on the same
    /// buffer.
    #[test]
    fn the_instance_name_folds_case_the_way_windows_paths_do() {
        assert_eq!(
            single_instance_names(Some(Path::new(r"C:\Temp\LVB-A"))),
            single_instance_names(Some(Path::new(r"c:\temp\lvb-a")))
        );
    }

    /// Task4220. The template is a string literal Windows parses at show time:
    /// get it wrong and the call still returns `Ok` while nothing appears. The
    /// `&` is the case tauri-winrt-notification needed its own escape pass for.
    #[test]
    fn the_toast_payload_carries_both_lines_with_xml_metacharacters_intact() {
        let xml = toast_xml("Stop & save", "<clip> ready").expect("build the payload");
        let rendered = xml.GetXml().expect("render").to_string();

        assert!(
            rendered.contains(r#"template="ToastGeneric""#),
            "{rendered}"
        );
        assert!(rendered.contains("Stop &amp; save"), "{rendered}");
        assert!(rendered.contains("&lt;clip&gt; ready"), "{rendered}");
    }
}
