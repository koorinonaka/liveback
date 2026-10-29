//! Toolhelp32 and process-image lookups behind the picker and the capture
//! start (moved out of `targets.rs`, t261025-7188).

use std::{
    collections::{HashMap, HashSet},
    ffi::OsString,
    os::windows::ffi::OsStringExt,
    path::Path,
};

use windows::{
    core::PWSTR,
    Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
        UI::WindowsAndMessaging::{GetWindowThreadProcessId, IsWindow},
    },
};

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

pub(super) unsafe fn executable_for_process(process_id: u32) -> Option<(String, String)> {
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
pub(crate) fn executable_name_for_handle(window_handle: &str) -> Option<String> {
    executable_name_for_process(process_id_for_handle(window_handle)?)
}

/// The full image path behind a captured window handle (2026-09-20). `None`
/// for a monitor handle and whenever the process cannot be opened, like
/// [`executable_name_for_handle`].
pub(crate) fn executable_path_for_handle(window_handle: &str) -> Option<String> {
    executable_path_for_process(process_id_for_handle(window_handle)?)
}

/// The process behind a live window handle. `None` for a monitor handle (an
/// HMONITOR is not a window) and for a handle that does not parse.
fn process_id_for_handle(window_handle: &str) -> Option<u32> {
    let hwnd = super::parse_window_handle(window_handle).ok()?;
    let mut process_id = 0;
    unsafe {
        if !IsWindow(Some(hwnd)).as_bool() {
            return None;
        }
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    Some(process_id)
}

/// The executable name behind a process id, as it is spelled on disk
/// (task1260). `None` when the process is gone or cannot be opened.
fn executable_name_for_process(process_id: u32) -> Option<String> {
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
    let wanted = executable_name.trim();
    if wanted.is_empty() {
        return None;
    }
    let mut found = None;
    walk_processes(|name, process_id| {
        if name.eq_ignore_ascii_case(wanted) {
            found = Some(process_id);
            return false;
        }
        true
    })?;
    found
}

/// One Toolhelp32 walk: `visit` gets every process's executable name (as the
/// snapshot spells it) and id, in snapshot order, until it answers `false`.
/// `None` when the snapshot could not be taken.
pub(super) fn walk_processes(mut visit: impl FnMut(&str, u32) -> bool) -> Option<()> {
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
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let length = entry
                    .szExeFile
                    .iter()
                    .position(|value| *value == 0)
                    .unwrap_or(entry.szExeFile.len());
                let name = String::from_utf16_lossy(&entry.szExeFile[..length]);
                if !visit(&name, entry.th32ProcessID)
                    || Process32NextW(snapshot, &mut entry).is_err()
                {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    Some(())
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
    let mut found = RunningProcesses::default();
    walk_processes(|name, process_id| {
        let name = name.to_ascii_lowercase();
        found.pids.entry(name.clone()).or_default().push(process_id);
        found.names.insert(name);
        true
    })?;
    // A walk that produced nothing did not happen: this process is always in
    // its own snapshot.
    (!found.names.is_empty()).then_some(found)
}
