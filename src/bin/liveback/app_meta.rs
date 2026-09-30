//! Display metadata for a registered executable (task2560): the full path of a
//! running process, the `FileDescription` out of its version resource, and the
//! file's icon. Metadata only -- registration matching stays on the executable
//! name, and every resolver here degrades to `None` rather than failing a row.

use livia::capture::targets as capture_targets;
use livia::settings::AutoCaptureApp;
use windows::core::HSTRING;

/// The entry the review toggle registers: the name as handed in, plus path and
/// display name resolved from a running process of it -- or bare, when nothing
/// by that name is running (the registration then draws like it always did).
pub(super) fn resolve_entry(executable: &str) -> AutoCaptureApp {
    let path = capture_targets::first_process_id_for_executable(executable)
        .and_then(capture_targets::executable_path_for_process);
    let display_name = path.as_deref().and_then(describe_executable);
    AutoCaptureApp {
        display_name,
        name: executable.to_owned(),
        path,
        ..AutoCaptureApp::default()
    }
}

/// `FileDescription` -- what Task Manager shows as the app's name -- falling
/// back to `ProductName`. `None` for a file without version info (or one that
/// is gone), which the caller shows as the bare exe name.
pub(super) fn describe_executable(path: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
    };

    let file = HSTRING::from(path);
    unsafe {
        let size = GetFileVersionInfoSizeW(&file, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&file, None, size, data.as_mut_ptr().cast()).ok()?;

        // `\VarFileInfo\Translation` lists (language, codepage) pairs; the
        // strings live under a block named by one of them. First pair wins --
        // multi-language executables put their primary language first. The
        // appended fallbacks cover files whose string table is not listed in
        // (or has no) Translation entry -- Store apps ship neutral-language
        // version info that way. Same set .NET's FileVersionInfo falls back to.
        let mut buffer: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut length: u32 = 0;
        let mut translations: Vec<(u16, u16)> = if VerQueryValueW(
            data.as_ptr().cast(),
            &HSTRING::from(r"\VarFileInfo\Translation"),
            &mut buffer,
            &mut length,
        )
        .as_bool()
            && length >= 4
        {
            std::slice::from_raw_parts(buffer.cast::<u16>(), (length / 2) as usize)
                .chunks_exact(2)
                .map(|pair| (pair[0], pair[1]))
                .collect()
        } else {
            Vec::new()
        };
        translations.extend([(0x0409, 0x04b0), (0x0409, 0x04e4), (0x0000, 0x04b0)]);
        // A FileDescription that merely repeats the file's own name says
        // nothing the row's exe line does not already say -- Store Paint ships
        // exactly that -- so it is passed over for ProductName.
        let file_name = std::path::Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        for key in ["FileDescription", "ProductName"] {
            for (language, codepage) in &translations {
                let subblock = format!(r"\StringFileInfo\{language:04x}{codepage:04x}\{key}");
                if let Some(value) = version_string(&data, &subblock) {
                    if key == "FileDescription" && value.eq_ignore_ascii_case(&file_name) {
                        break;
                    }
                    return Some(value);
                }
            }
        }
        None
    }
}

/// One string out of an already-read version-info block. Empty or
/// whitespace-only counts as absent, so a lazy `FileDescription: ""` falls
/// through to `ProductName` rather than being stored.
unsafe fn version_string(data: &[u8], subblock: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::VerQueryValueW;

    let mut buffer: *mut core::ffi::c_void = std::ptr::null_mut();
    // In characters for string values, per the API's contract.
    let mut length: u32 = 0;
    if !VerQueryValueW(
        data.as_ptr().cast(),
        &HSTRING::from(subblock),
        &mut buffer,
        &mut length,
    )
    .as_bool()
        || length == 0
    {
        return None;
    }
    let characters = std::slice::from_raw_parts(buffer.cast::<u16>(), length as usize);
    let text = String::from_utf16_lossy(characters);
    let text = text.trim_end_matches('\0').trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The file's small icon as RGBA, for the settings row's 16px slot.
/// `ExtractIconExW` rather than `SHGetFileInfoW`: it reads the exe's own icon
/// resource -- the same picture the taskbar shows -- and needs no COM.
pub(super) fn file_icon_rgba(path: &str) -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::UI::Shell::ExtractIconExW;
    use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};

    unsafe {
        let mut small = HICON::default();
        let extracted = ExtractIconExW(&HSTRING::from(path), 0, None, Some(&mut small), 1);
        if extracted == 0 || small.is_invalid() {
            return None;
        }
        // This HICON is ours, unlike the window-owned ones the picker reads.
        let pixels = super::picker::icon_rgba(small);
        if let Err(error) = DestroyIcon(small) {
            tracing::warn!(%error, "could not free an extracted file icon");
        }
        pixels
    }
}
