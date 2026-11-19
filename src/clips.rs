//! Reading and writing the comment carried *inside* an exported mp4 (task2960).
//!
//! Decision (user, 2026-09-05): a clip's comment lives in the mp4 itself, never
//! in a sidecar -- a sidecar dies the moment the file is moved out of the app's
//! folder.
//!
//! Route: the Windows shell property store, reached with
//! `SHGetPropertyStoreFromParsingName` and keyed on `System.Comment` -- the
//! same path Explorer's 詳細 tab reads and writes. The task held a hand-written
//! `moov/udta/©cmt` fallback in reserve for the case where the mp4 property
//! handler turned out to be read-only. It is not needed:
//! the spike measured `GPS_READWRITE` opening, `SetValue`, `Commit` and a
//! Japanese/emoji read-back all succeeding on this app's own export output, on
//! ffmpeg mp4s with `moov` at either end, and on a 0.94GB file.
//!
//! Cost, for callers deciding what thread to run on: with `moov` last -- which
//! is what this app exports -- `Commit` stays under a millisecond at 900 bytes
//! and at 0.94GB, appending ~50 bytes rather than rewriting the container. The
//! one `moov`-first (faststart) file measured took 7.6ms at 1.5KB, so a
//! `moov`-first layout may well cost in proportion to its size; nothing here
//! produces one. The reliable cost is the ~45ms the first
//! `SHGetPropertyStoreFromParsingName` of a process pays to load the handler;
//! warm calls land near 4ms. See the task's Execution Log for the numbers.
//! Staying on the shell route means the string Explorer shows is exactly the
//! string the app reads back, and no container of ours gets rewritten.
//!
//! The store is opened per call. That is deliberate: the handler holds the file
//! open while the store lives, so a clip list that cached stores would block
//! deletes and renames.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use windows::{
    core::{BSTR, PCWSTR},
    Win32::{
        Storage::EnhancedStorage::{PKEY_ApplicationName, PKEY_Comment, PKEY_Media_Duration},
        System::Com::StructuredStorage::PROPVARIANT,
        UI::Shell::PropertiesSystem::{
            IPropertyStore, SHGetPropertyStoreFromParsingName, GETPROPERTYSTOREFLAGS, GPS_DEFAULT,
            GPS_READWRITE,
        },
    },
};

/// Where 「クリップに保存」 writes: the configured folder, or a default that
/// tracks the recording buffer folder (task3030, round18 design §4-4).
///
/// Task2970 first set the default to `<default export directory>\Clips`,
/// deliberately blind to `export_output_directory` so the clip pile would not
/// drift to wherever the user last exported. round18 moved the default again,
/// this time to `<buffer root>\Clips` -- a clip is cut from a buffered
/// recording, so it sits beside the buffer rather than beside exports, and the
/// same "known place" argument now points there instead. `buffer_root()` is a
/// bare associated function backed by a process-wide static that answers a
/// sane default even when nothing has ever pointed it anywhere (no
/// `CaptureController` required), so calling it here -- including from unit
/// tests that never touch a controller -- is safe.
///
/// The directory is not created here; the caller does that right before saving
/// (`export` does the same), and [`list`] creates it on the read side.
/// `LIVEBACK_BUFFER_ROOT` also overrides a *configured* `clipDirectory`
/// (task3910) -- see [`resolve_clip_directory`].
pub fn clip_directory(settings: &crate::settings::AppSettings) -> Result<PathBuf, String> {
    Ok(resolve_clip_directory(
        settings.clip_directory.as_deref(),
        crate::capture::buffer_root_override(),
        &crate::capture::CaptureController::buffer_root(),
    ))
}

/// The rule, as a pure function so it can be tested by passing the override in
/// rather than through `buffer_root()`'s process-wide static (task3910).
///
/// With the override set, `settings.clip_directory` is ignored. Without that,
/// an agent run would isolate its *sessions* and still drop its clips into the
/// user's real clip folder -- which is exactly the leftover the 2026-09-08
/// sweep had to hand back to the user. With no override the configured folder
/// wins, as it always did.
pub(crate) fn resolve_clip_directory(
    configured: Option<&str>,
    override_root: Option<&Path>,
    buffer_root: &Path,
) -> PathBuf {
    match configured {
        Some(configured) if override_root.is_none() && !configured.trim().is_empty() => {
            PathBuf::from(configured)
        }
        _ => buffer_root.join("Clips"),
    }
}

/// A free file name for a new clip in `directory` (task2970). Delegates to the
/// export pipeline's own allocator so a clip is named exactly like an export --
/// see [`crate::export::allocate_export_path`] for why a clip has to allocate
/// up front at all.
pub fn allocate_clip_path(directory: &Path, game: &str) -> PathBuf {
    crate::export::allocate_export_path(directory, game)
}

/// One saved clip, as the list screen sees it (task2980).
///
/// Deliberately only what `fs::metadata` already knows. Comment and duration
/// are *not* here: both come off the shell property store, which costs ~4ms a
/// file warm, and paying that for every clip on every scan is what makes a
/// folder of a few hundred freeze the pane on open. They arrive per row through
/// [`details`] instead, for the rows actually on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipEntry {
    pub path: PathBuf,
    /// The file name with its extension, exactly as Explorer shows it. The
    /// export allocator already bakes the game name and the timestamp into it
    /// (task2970), so there is nothing to re-derive.
    pub name: String,
    /// When the clip was *cut*, which is what the list orders and dates by
    /// (user, 2026-09-15: 「コメント更新するとクリップの日時も更新される。
    /// 動画の作成日基準で並べたい」). Not the modification time: writing a
    /// comment goes through the shell property store, which appends to the
    /// file and moves its mtime, so an ordering keyed on that jumped the clip
    /// to the top of the grid and re-dated it the moment a memo was typed.
    /// Falls back to `modified` on a filesystem that does not keep a creation
    /// time, which is the pre-2026-09-15 behaviour rather than a missing row.
    pub created: SystemTime,
    /// The modification time, kept only as the staleness stamp the detail
    /// cache files its answers under (`bin/liveback/clips.rs`). It has to stay
    /// the *mutable* one: a comment written while a detail read is in flight
    /// has to invalidate the cached answer, and `created` never moves.
    pub modified: SystemTime,
    pub bytes: u64,
}

/// Every `.mp4` directly in `directory`, newest first.
///
/// No index, no database: the folder *is* the list (the same decision
/// `ring_buffer::sessions` made). A file the user drops in with Explorer shows
/// up on the next scan and a file they delete stops being listed, so there is
/// no such thing as a dangling entry. A missing directory is not an error --
/// it is an empty list, and the caller says so in words.
///
/// Anything unreadable is skipped rather than reported: one file with its
/// metadata locked is not a reason for the other forty to disappear. The
/// folder itself is different (t260928-6080 F9): one that will not read is
/// an `Err` with the OS's reason, and the pane says 「一覧を取得できませんでした」
/// rather than the empty folder's 「まだありません」.
pub fn list(directory: &Path) -> Result<Vec<ClipEntry>, String> {
    // Task3030: the folder is created here, on the read side, the same way
    // the save side already created it (task2970) -- round18 dropped the
    // "フォルダがまだありません" screen, so opening the pane before anything has
    // been saved must land on the same 0件 an emptied-but-existing folder
    // shows, not a different one. A failure here falls through to `read_dir`,
    // whose own error is the one reported.
    let _ = std::fs::create_dir_all(directory);
    let entries = std::fs::read_dir(directory).map_err(|error| error.to_string())?;
    let mut clips: Vec<ClipEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            // Case-insensitively, because Explorer's own casing is not the
            // app's business -- the same rule `shell::drop_hint` follows.
            if !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("mp4"))
            {
                return None;
            }
            // `metadata` rather than the `DirEntry`'s file type, so a directory
            // named `something.mp4` and a file whose metadata will not read are
            // rejected by the same check.
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified = metadata.modified().ok()?;
            Some(ClipEntry {
                name: path.file_name()?.to_string_lossy().into_owned(),
                created: metadata.created().unwrap_or(modified),
                modified,
                bytes: metadata.len(),
                path,
            })
        })
        .collect();
    // Newest first, by creation time -- see `ClipEntry::created`. The name is
    // the tie-break so two clips cut inside the same filesystem timestamp still
    // come back in a stable order rather than whatever the directory happened
    // to hand over.
    clips.sort_by(|left, right| {
        right
            .created
            .cmp(&left.created)
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(clips)
}

/// What a clip says about itself beyond its size and date (task2980): the
/// comment written by 「クリップに保存」 and how long it runs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClipDetails {
    pub comment: Option<String>,
    /// In 100ns units, which is what `ui_state::timeline::format_duration`
    /// takes -- the shell reports `System.Media.Duration` in exactly those, so
    /// nothing is converted on the way.
    pub duration_100ns: Option<i64>,
    /// The recorded app's executable, as [`write_app_path`] left it. `None`
    /// for a clip saved before it was written, and for a monitor recording.
    pub app_path: Option<String>,
}

/// Every property in one store open. Two calls would pay the ~4ms twice for no
/// second answer.
/// Never fails: a file no handler claims, or one with neither property, reads
/// as an empty `ClipDetails` and the row simply shows less.
pub fn details(path: &Path) -> ClipDetails {
    let Ok(store) = open_store(path, GPS_DEFAULT) else {
        return ClipDetails::default();
    };
    ClipDetails {
        comment: read_key(&store, &PKEY_Comment)
            .and_then(|value| BSTR::try_from(&value).ok())
            .map(|text| text.to_string())
            .filter(|text| !text.is_empty()),
        // `System.Media.Duration` is VT_UI8; a file whose handler leaves it
        // unset (or is not a media handler at all) has no honest answer and
        // the row leaves the column blank.
        duration_100ns: read_key(&store, &PKEY_Media_Duration)
            .and_then(|value| u64::try_from(&value).ok())
            .and_then(|value| i64::try_from(value).ok())
            .filter(|value| *value > 0),
        app_path: read_key(&store, &PKEY_ApplicationName)
            .and_then(|value| BSTR::try_from(&value).ok())
            .map(|text| text.to_string())
            .filter(|text| !text.trim().is_empty()),
    }
}

/// Writes the recorded app's executable into `path`, for the clip card's app
/// icon -- the clip outlives its session, so the manifest's path is not there
/// to read later. Under `System.ApplicationName`: the mp4 handler keeps any key
/// in its `Xtra` box, so this travels with the file like the comment does
/// (measured 2026-10-24 alongside six other keys, all round-tripping).
pub fn write_app_path(path: &Path, app_path: &str) -> Result<(), String> {
    let store = open_store(path, GPS_READWRITE).map_err(|(_, detail)| detail)?;
    let value = PROPVARIANT::from(app_path);
    // SAFETY: as in `write_comment`.
    unsafe {
        store
            .SetValue(&PKEY_ApplicationName, &value)
            .map_err(|error| error.to_string())?;
        store.Commit().map_err(|error| error.to_string())
    }
}

/// `GetValue` for a key that may legitimately be absent.
fn read_key(
    store: &IPropertyStore,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> Option<PROPVARIANT> {
    // SAFETY: `store` is a live COM interface and the key is a static. The
    // PROPVARIANT that comes back owns its contents and clears itself on drop.
    let value = unsafe { store.GetValue(key) }.ok()?;
    (!value.is_empty()).then_some(value)
}

/// The comment stored in `path`, or `None` when the file carries none, cannot
/// be read, or is not a file at all. Never panics: a missing path, a directory
/// and a file no property handler claims all read as `None`.
#[cfg(test)]
pub fn read_comment(path: &Path) -> Option<String> {
    let store = open_store(path, GPS_DEFAULT).ok()?;
    let value = read_key(&store, &PKEY_Comment)?;
    BSTR::try_from(&value).ok().map(|text| text.to_string())
}

/// Writes `comment` into `path`'s own metadata, replacing whatever was there.
/// Writing `""` is how a comment is removed: the handler drops the property
/// rather than storing an empty string, so `read_comment` then answers `None`
/// (measured, task2960).
///
/// The `Err` is a kind and its detail (t261101-5ff3): `ui_state::clips`'s
/// `comment_write_error_text` words the toast's title and second line
/// (t260928-6080 F13) -- what failed, then why.
pub fn write_comment(path: &Path, comment: &str) -> Result<(), CommentWriteError> {
    let refused = |store: StoreFailure, detail: String| CommentWriteError {
        kind: CommentWriteFailure::WriteRefused(store),
        detail,
    };
    let store =
        open_store(path, GPS_READWRITE).map_err(|(store, detail)| refused(store, detail))?;
    let value = PROPVARIANT::from(comment);
    // SAFETY: `store` is live, the key is static, and `value` outlives both
    // calls -- `SetValue` copies it.
    unsafe {
        store
            .SetValue(&PKEY_Comment, &value)
            .map_err(|error| refused(StoreFailure::Refused, error.to_string()))?;
        store.Commit().map_err(|error| CommentWriteError {
            kind: CommentWriteFailure::SaveFailed,
            detail: error.to_string(),
        })?;
    }
    Ok(())
}

/// Why `write_comment` refused (t260928-6080 F13, t261101-5ff3): the kind is
/// the toast's title and the start of its second line, `detail` the engine's
/// own words (an HRESULT, a path) after it. `Display` is the log line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentWriteError {
    pub kind: CommentWriteFailure,
    pub detail: String,
}

/// Which step of `write_comment` refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommentWriteFailure {
    /// The store would not open read-write, or would not take the value.
    WriteRefused(StoreFailure),
    /// The value was set but `Commit` refused.
    SaveFailed,
}

/// Why the property store would not open (or take a value).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreFailure {
    /// The path is not a file; `detail` is the path.
    FileMissing,
    /// `std::path::absolute` failed; `detail` is its error.
    PathUnresolvable,
    /// The shell refused; `detail` is the HRESULT.
    Refused,
}

impl std::fmt::Display for CommentWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.detail)
    }
}

fn open_store(
    path: &Path,
    flags: GETPROPERTYSTOREFLAGS,
) -> Result<IPropertyStore, (StoreFailure, String)> {
    // Directories and missing paths are rejected here rather than handed to the
    // shell, whose answers for them are not ours to interpret.
    if !path.is_file() {
        return Err((StoreFailure::FileMissing, path.display().to_string()));
    }
    // The shell namespace parses a plain absolute path; `canonicalize` would
    // hand it a `\\?\` prefix it does not accept.
    let absolute: PathBuf = std::path::absolute(path)
        .map_err(|error| (StoreFailure::PathUnresolvable, error.to_string()))?;
    // The shell wants an initialized apartment; the store works in either.
    crate::com::init_sta();
    let wide = to_wide(&absolute.to_string_lossy());
    // SAFETY: the wide path is nul-terminated and outlives the call.
    unsafe { SHGetPropertyStoreFromParsingName(PCWSTR(wide.as_ptr()), None, flags) }
        .map_err(|error| (StoreFailure::Refused, format!("{error}")))
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// One clip opened for the hover storyboard (task3540): seek, decode one
/// frame, shrink it to the tile. The caller owns the COM apartment and
/// `MFStartup`; this is the RGB32 route of task3480's spike, because a handful
/// of stills does not earn the NV12 path's own colour conversion.
pub struct FrameReader {
    reader: windows::Win32::Media::MediaFoundation::IMFSourceReader,
    width: u32,
    height: u32,
    stride: i32,
}

impl FrameReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        use windows::core::HSTRING;
        use windows::Win32::Media::MediaFoundation::*;
        let attributes = Self::attributes()?;
        let reader =
            unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), &attributes) }
                .map_err(|e| e.to_string())?;
        Self::configure(reader)
    }

    /// The same reader over a byte stream: a ring-buffer segment for the
    /// history's hover preview, which has no file of its own to name.
    pub fn from_byte_stream(
        stream: &windows::Win32::Media::MediaFoundation::IMFByteStream,
    ) -> Result<Self, String> {
        use windows::Win32::Media::MediaFoundation::*;
        let attributes = Self::attributes()?;
        let reader = unsafe { MFCreateSourceReaderFromByteStream(stream, &attributes) }
            .map_err(|e| e.to_string())?;
        Self::configure(reader)
    }

    fn attributes() -> Result<windows::Win32::Media::MediaFoundation::IMFAttributes, String> {
        use windows::Win32::Media::MediaFoundation::*;
        unsafe {
            let mut attributes: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attributes, 1).map_err(|e| e.to_string())?;
            let attributes = attributes.ok_or("attributes missing")?;
            attributes
                .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
                .map_err(|e| e.to_string())?;
            Ok(attributes)
        }
    }

    fn configure(
        reader: windows::Win32::Media::MediaFoundation::IMFSourceReader,
    ) -> Result<Self, String> {
        use windows::Win32::Media::MediaFoundation::*;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        unsafe {
            let video = MFCreateMediaType().map_err(|e| e.to_string())?;
            video
                .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
                .map_err(|e| e.to_string())?;
            video
                .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32)
                .map_err(|e| e.to_string())?;
            reader
                .SetCurrentMediaType(stream, None, &video)
                .map_err(|e| e.to_string())?;
            let current = reader
                .GetCurrentMediaType(stream)
                .map_err(|e| e.to_string())?;
            let packed = current.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
            let width = (packed >> 32) as u32;
            let height = (packed & 0xFFFF_FFFF) as u32;
            let stride = current
                .GetUINT32(&MF_MT_DEFAULT_STRIDE)
                .map(|value| value as i32)
                .unwrap_or((width * 4) as i32);
            if width == 0 || height == 0 {
                return Err("no video frame size".into());
            }
            Ok(FrameReader {
                reader,
                width,
                height,
                stride,
            })
        }
    }

    /// The first frame at or after `at_100ns`, as RGBA fitted inside
    /// `max` (nearest-neighbour: a 16px-wide scrub cell does not show the
    /// difference a filter would make). `None` when the file has no frame there.
    pub fn frame_at(&mut self, at_100ns: i64, max: (u32, u32)) -> Option<(u32, u32, Vec<u8>)> {
        use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
        use windows::Win32::System::Variant::VT_I8;
        let mut position = PROPVARIANT::default();
        unsafe {
            (*position.Anonymous.Anonymous).vt = VT_I8;
            (*position.Anonymous.Anonymous).Anonymous.hVal = at_100ns.max(0);
            self.reader
                .SetCurrentPosition(&windows::core::GUID::zeroed(), &position)
                .ok()?;
        }
        // The seek lands on the sync sample before the target; decode forward
        // to it (task3710's 35-327ms is exactly this tail).
        self.next_frame(at_100ns - 10_000, max)
            .map(|(_, width, height, rgba)| (width, height, rgba))
    }

    /// Decodes forward to the first frame stamped at or after `from_100ns`,
    /// with its timestamp. Only that frame is converted, so a caller thinning
    /// the frame rate pays the decode for the frames it skips but not the copy.
    pub fn next_frame(
        &mut self,
        from_100ns: i64,
        max: (u32, u32),
    ) -> Option<(i64, u32, u32, Vec<u8>)> {
        use windows::Win32::Media::MediaFoundation::*;
        // The walk stops by handing the frame back as its `Err`: `Ok` means
        // the stream ended first, `Err(None)` that a read failed.
        let found = unsafe {
            crate::encoder::mf_sample::for_each_sample(
                &self.reader,
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                |_| None,
                |timestamp, sample| {
                    if timestamp >= from_100ns {
                        Err(Some((timestamp, sample)))
                    } else {
                        Ok(())
                    }
                },
            )
        };
        let Err(Some((timestamp, sample))) = found else {
            return None;
        };
        let (out_w, out_h) = fit((self.width, self.height), max);
        unsafe {
            let buffer = sample.ConvertToContiguousBuffer().ok()?;
            let mut bytes = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut bytes, None, Some(&mut length)).ok()?;
            let source = std::slice::from_raw_parts(bytes, length as usize);
            let rgba = shrink_bgra(
                source,
                (self.width, self.height),
                self.stride,
                (out_w, out_h),
            );
            let _ = buffer.Unlock();
            rgba.map(|rgba| (timestamp, out_w, out_h, rgba))
        }
    }
}

/// `source` scaled to fit inside `max`, aspect kept, never enlarged.
fn fit(source: (u32, u32), max: (u32, u32)) -> (u32, u32) {
    let scale = (max.0 as f64 / source.0 as f64)
        .min(max.1 as f64 / source.1 as f64)
        .min(1.0);
    (
        ((source.0 as f64 * scale).round() as u32).max(1),
        ((source.1 as f64 * scale).round() as u32).max(1),
    )
}

/// BGRA rows (negative `stride` = bottom-up) to a nearest-neighbour RGBA
/// image of `out`. `None` if the buffer is shorter than the frame claims.
fn shrink_bgra(source: &[u8], size: (u32, u32), stride: i32, out: (u32, u32)) -> Option<Vec<u8>> {
    let pitch = stride.unsigned_abs() as usize;
    if source.len() < pitch * size.1 as usize {
        return None;
    }
    let mut rgba = Vec::with_capacity(out.0 as usize * out.1 as usize * 4);
    for y in 0..out.1 as usize {
        let sy = y * size.1 as usize / out.1 as usize;
        let row = if stride < 0 {
            size.1 as usize - 1 - sy
        } else {
            sy
        };
        for x in 0..out.0 as usize {
            let at = row * pitch + (x * size.0 as usize / out.0 as usize) * 4;
            rgba.extend_from_slice(&[source[at + 2], source[at + 1], source[at], 255]);
        }
    }
    Some(rgba)
}

/// `pub(crate)` for the fixture mp4 alone: `playback::tests` opens the same
/// 900-byte export to prove the clip engine reads a standalone file (task3500),
/// and the alternative was a second copy of the hex below. Same reason
/// `capture::tests` is `pub(crate)`.
#[cfg(test)]
pub(crate) mod tests;
