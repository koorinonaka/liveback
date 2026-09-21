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
        Storage::EnhancedStorage::{PKEY_Comment, PKEY_Media_Duration},
        System::Com::{CoInitializeEx, StructuredStorage::PROPVARIANT, COINIT_APARTMENTTHREADED},
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
/// metadata locked is not a reason for the other forty to disappear.
pub fn list(directory: &Path) -> Vec<ClipEntry> {
    // Task3030: the folder is created here, on the read side, the same way
    // the save side already created it (task2970) -- round18 dropped the
    // "フォルダがまだありません" screen, so opening the pane before anything has
    // been saved must land on the same 0件 an emptied-but-existing folder
    // shows, not a different one. Failure is silent and falls through to
    // `read_dir` failing too: a locked parent still opens as an empty list
    // rather than an error screen.
    let _ = std::fs::create_dir_all(directory);
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
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
    clips
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
}

/// Both properties in one store open. Two calls would pay the ~4ms twice for no
/// second answer; `read_comment` stays for callers that only want the comment.
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
pub fn read_comment(path: &Path) -> Option<String> {
    let store = open_store(path, GPS_DEFAULT).ok()?;
    let value = read_key(&store, &PKEY_Comment)?;
    BSTR::try_from(&value).ok().map(|text| text.to_string())
}

/// Writes `comment` into `path`'s own metadata, replacing whatever was there.
/// Writing `""` is how a comment is removed: the handler drops the property
/// rather than storing an empty string, so `read_comment` then answers `None`
/// (measured, task2960).
/// The `Err` string is the message the UI shows as-is.
pub fn write_comment(path: &Path, comment: &str) -> Result<(), String> {
    let locale = crate::ui_state::locale::active();
    let store = open_store(path, GPS_READWRITE).map_err(|error| {
        format!(
            "{}: {error}",
            crate::ui_state::clips::comment_write_failed(locale)
        )
    })?;
    let value = PROPVARIANT::from(comment);
    // SAFETY: `store` is live, the key is static, and `value` outlives both
    // calls -- `SetValue` copies it.
    unsafe {
        store.SetValue(&PKEY_Comment, &value).map_err(|error| {
            format!(
                "{}: {error}",
                crate::ui_state::clips::comment_write_failed(locale)
            )
        })?;
        store.Commit().map_err(|error| {
            format!(
                "{}: {error}",
                crate::ui_state::clips::comment_save_failed(locale)
            )
        })?;
    }
    Ok(())
}

fn open_store(path: &Path, flags: GETPROPERTYSTOREFLAGS) -> Result<IPropertyStore, String> {
    let locale = crate::ui_state::locale::active();
    // Directories and missing paths are rejected here rather than handed to the
    // shell, whose answers for them are not ours to interpret.
    if !path.is_file() {
        return Err(format!(
            "{}: {}",
            crate::ui_state::clips::file_missing(locale),
            path.display()
        ));
    }
    // The shell namespace parses a plain absolute path; `canonicalize` would
    // hand it a `\\?\` prefix it does not accept.
    let absolute: PathBuf = std::path::absolute(path).map_err(|error| {
        format!(
            "{}: {error}",
            crate::ui_state::clips::path_unresolvable(locale)
        )
    })?;
    ensure_com();
    let wide = to_wide(&absolute.to_string_lossy());
    // SAFETY: the wide path is nul-terminated and outlives the call.
    unsafe { SHGetPropertyStoreFromParsingName(PCWSTR(wide.as_ptr()), None, flags) }
        .map_err(|error| format!("{error}"))
}

/// The shell wants an initialized apartment. `RPC_E_CHANGED_MODE` means this
/// thread is already in the other one, which is fine -- the store works either
/// way -- so the result is ignored, and `CoUninitialize` is never called.
fn ensure_com() {
    // SAFETY: no arguments, and the return value is deliberately dropped.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `pub(crate)` for the fixture mp4 alone: `playback::tests` opens the same
/// 900-byte export to prove the clip engine reads a standalone file (task3500),
/// and the alternative was a second copy of the hex below. Same reason
/// `capture::tests` is `pub(crate)`.
#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A real mp4, because the property handler will not open anything less:
    /// a hand-built `ftyp`+`mdat`+`moov` skeleton is refused with
    /// `0x8007000D ERROR_INVALID_DATA` (measured, task2960). These 900 bytes are
    /// this project's own export output -- the two passthrough frames
    /// `export::tests::fixtures`'s video-only case writes through `export_part`
    /// -- captured once, the same way that module keeps its real H.264 samples
    /// as hex. So the tests need neither ffmpeg on PATH nor an encode session.
    const FIXTURE_MP4_HEX: &[&str] = &[
        "00000018667479706d703432000000006d70343169736f6d0000002775756964",
        "5ca708fb328e4205a861650eca0a95960000000b362e322e30393230302e3000",
        "0000816d64617400000000000000100000000209100000005665b80406bc4628",
        "00052f31c00072ae38000e55c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9",
        "c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9c9",
        "c9c9c9c9c9c9c9c9c9c9c9c9c9c9e00000000209300000000761e02018f028c0",
        "000002c46d6f6f760000006c6d76686400000000e6c19e1ee6c19e1e00007530",
        "000007cf00010000010000000000000000000000000100000000000000000000",
        "0000000000010000000000000000000000000000400000000000000000000000",
        "00000000000000000000000000000000000000020000020b7472616b0000005c",
        "746b686400000001e6c19e1ee6c19e1e0000000100000000000007cf00000000",
        "0000000000000000000000000001000000000000000000000000000000010000",
        "0000000000000000000000004000000000a0000000780000000001a76d646961",
        "000000206d64686400000000e6c19e1ee6c19e1e00007530000007cf55c40000",
        "0000002d68646c72000000000000000076696465000000000000000000000000",
        "566964656f48616e646c657200000001526d696e6600000014766d6864000000",
        "0100000000000000000000002464696e660000001c6472656600000000000000",
        "010000000c75726c2000000001000001127374626c0000009673747364000000",
        "0000000001000000866176633100000000000000010000000000000000000000",
        "000000000000a0007800480000004800000000000000010a41564320436f6469",
        "6e670000000000000000000000000000000000000000000018ffff0000003061",
        "7663430142c020ffe100196742c02095a0a11f966a020202800001f400007530",
        "078e155001000468ce3c80000000187374747300000000000000010000000200",
        "0003e80000001c73747363000000000000000100000001000000020000000100",
        "00001c7374737a00000000000000000000000200000060000000110000001473",
        "74636f00000000000000010000004f0000001073747373000000000000000000",
        "00004575647461000000356d657461000000000000002168646c720000000000",
        "0000006d6469720000000000000000000000000000000008696c737400000008",
        "58747261",
    ];

    pub(crate) fn fixture_bytes() -> Vec<u8> {
        let hex = FIXTURE_MP4_HEX.concat();
        (0..hex.len() / 2)
            .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap())
            .collect()
    }

    /// Each test gets its own copy: writing a comment mutates the file.
    pub(crate) fn fixture(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("livia-clips-{}-{}", std::process::id(), name));
        std::fs::create_dir_all(&dir).expect("fixture dir");
        let path = dir.join("clip.mp4");
        std::fs::write(&path, fixture_bytes()).expect("fixture mp4");
        path
    }

    pub(crate) fn cleanup(path: &Path) {
        let _ = std::fs::remove_dir_all(path.parent().expect("fixture parent"));
    }

    #[test]
    fn a_japanese_comment_survives_the_round_trip() {
        let path = fixture("roundtrip");
        let comment = "テスト録画🎬 2026-09-05 の検証";
        write_comment(&path, comment).expect("write");
        assert_eq!(read_comment(&path).as_deref(), Some(comment));
        cleanup(&path);
    }

    #[test]
    fn an_empty_comment_reads_back_empty() {
        let path = fixture("empty");
        write_comment(&path, "書いてから消す").expect("write");
        write_comment(&path, "").expect("clear");
        // Measured: the handler drops the property rather than storing an empty
        // string, so this reads back as `None`. Asserted as "empty either way"
        // because both answers mean "no comment" to a caller.
        assert!(read_comment(&path).unwrap_or_default().is_empty());
        cleanup(&path);
    }

    /// Task2990's edit is a *re*-write, not a first write: the list screen
    /// commits over whatever the file already says. Each step is read back
    /// through `details`, which is the call the row actually renders from.
    #[test]
    fn a_comment_can_be_rewritten_from_the_list() {
        let path = fixture("rewrite");
        write_comment(&path, "最初のコメント").expect("first write");
        assert_eq!(details(&path).comment.as_deref(), Some("最初のコメント"));

        write_comment(&path, "書き換えたコメント").expect("rewrite");
        assert_eq!(
            details(&path).comment.as_deref(),
            Some("書き換えたコメント")
        );

        // The clear, which is the same commit with an empty field -- and the
        // duration survives all of it, because the row shows both.
        let duration = details(&path).duration_100ns;
        write_comment(&path, "").expect("clear");
        assert_eq!(details(&path).comment, None);
        assert_eq!(details(&path).duration_100ns, duration);
        cleanup(&path);
    }

    #[test]
    fn an_untouched_mp4_has_no_comment() {
        let path = fixture("untouched");
        assert_eq!(read_comment(&path), None);
        cleanup(&path);
    }

    /// Task2970's first bold warning still holds -- the default must not
    /// follow the save dialog's memory around -- but round18 (task3030) moved
    /// what it follows instead: the buffer root, not the export directory.
    #[test]
    fn the_default_clip_folder_tracks_the_buffer_root_not_the_export_directory() {
        let expected = crate::capture::CaptureController::buffer_root().join("Clips");

        let bare = crate::settings::AppSettings::default();
        assert_eq!(clip_directory(&bare).expect("resolves"), expected);

        let drifted = crate::settings::AppSettings {
            export_output_directory: Some(r"D:\somewhere\else".into()),
            ..crate::settings::AppSettings::default()
        };
        assert_eq!(clip_directory(&drifted).expect("resolves"), expected);

        // A configured folder is used verbatim, and a blank one is not a
        // folder -- it falls back rather than resolving to the process's cwd.
        let configured = crate::settings::AppSettings {
            clip_directory: Some(r"D:\Clips".into()),
            ..crate::settings::AppSettings::default()
        };
        assert_eq!(
            clip_directory(&configured).expect("resolves"),
            PathBuf::from(r"D:\Clips")
        );
        let blank = crate::settings::AppSettings {
            clip_directory: Some("   ".into()),
            ..crate::settings::AppSettings::default()
        };
        assert_eq!(clip_directory(&blank).expect("resolves"), expected);
    }

    /// Acceptance criterion: moving the buffer root moves the *default* clip
    /// folder with it. Guarded by `capture::tests::BUFFER_ROOT_LOCK` because
    /// `apply_buffer_root` writes a process-wide static that
    /// `capture::tests::controller` also points elsewhere under the same lock
    /// -- without sharing it, this test and one of those could each read the
    /// other's root mid-test.
    #[test]
    fn the_default_clip_folder_follows_a_moved_buffer_root() {
        let _root_lock = crate::capture::tests::BUFFER_ROOT_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let moved =
            std::env::temp_dir().join(format!("livia-clip-buffer-root-{}", std::process::id()));

        crate::capture::CaptureController::apply_buffer_root(Some(moved.clone()))
            .expect("root applies");
        let result = clip_directory(&crate::settings::AppSettings::default());
        // Put it back before asserting, same discipline `controller.rs`'s own
        // buffer-root tests use: a failed assert must not strand every later
        // test in this binary pointed at a temp folder.
        crate::capture::CaptureController::apply_buffer_root(None).expect("root resets");

        assert_eq!(result.expect("resolves"), moved.join("Clips"));
        let _ = std::fs::remove_dir_all(&moved);
    }

    /// Two saves inside one second must not overwrite each other: the name only
    /// has second resolution. The suffix shape is the export pipeline's
    /// (`_01`, `_02`), because this *is* the export pipeline's allocator.
    #[test]
    fn a_clip_never_lands_on_an_existing_file() {
        let dir = std::env::temp_dir().join(format!("livia-clip-alloc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("fixture dir");

        let first = allocate_clip_path(&dir, "mspaint");
        std::fs::write(&first, b"").expect("occupy");
        let second = allocate_clip_path(&dir, "mspaint");
        assert_ne!(second, first);
        std::fs::write(&second, b"").expect("occupy");
        let third = allocate_clip_path(&dir, "mspaint");
        assert_ne!(third, first);
        assert_ne!(third, second);
        // A `.partial` counts as taken too, so a clip cannot be allocated on
        // top of a job that is still writing.
        std::fs::write(third.with_extension("partial"), b"").expect("occupy");
        let fourth = allocate_clip_path(&dir, "mspaint");
        assert_ne!(fourth, third);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The scan is the whole clip index (task2980), so what it filters and how
    /// it orders is the contract: `.mp4` files only, newest first, and anything
    /// unreadable skipped rather than fatal.
    ///
    /// "Newest" is the *creation* time since 2026-09-15. The modification times
    /// here run in the exact opposite order on purpose: that is the shape a
    /// folder takes once comments have been written into it, and it is what the
    /// user reported -- a memo typed on the oldest clip used to send it to the
    /// top of the grid with today's date on it.
    #[test]
    fn the_scan_returns_only_mp4_files_newest_first() {
        use std::os::windows::fs::FileTimesExt;

        let dir = std::env::temp_dir().join(format!("livia-clip-list-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("fixture dir");

        // Explicit times: two files written in the same second would otherwise
        // make the ordering a coin toss.
        let epoch = SystemTime::UNIX_EPOCH;
        let at = |age_secs: u64| epoch + std::time::Duration::from_secs(1_700_000_000 - age_secs);
        for (name, created_age, modified_age) in [
            ("old.mp4", 300u64, 100u64),
            ("new.MP4", 100, 300),
            ("mid.mp4", 200, 200),
        ] {
            std::fs::write(dir.join(name), b"not really an mp4").expect("clip bytes");
            std::fs::File::options()
                .write(true)
                .open(dir.join(name))
                .expect("clip handle")
                .set_times(
                    std::fs::FileTimes::new()
                        .set_modified(at(modified_age))
                        .set_created(at(created_age)),
                )
                .expect("clip times");
        }
        // Not a clip: wrong extension, no extension, and -- the awkward one --
        // a *directory* that happens to be named like one.
        std::fs::write(dir.join("notes.txt"), b"x").expect("txt");
        std::fs::write(dir.join("bare"), b"x").expect("bare");
        std::fs::create_dir(dir.join("folder.mp4")).expect("decoy dir");

        let scanned = list(&dir);
        let names: Vec<&str> = scanned.iter().map(|clip| clip.name.as_str()).collect();
        assert_eq!(names, ["new.MP4", "mid.mp4", "old.mp4"]);
        assert_eq!(scanned[0].path, dir.join("new.MP4"));
        assert_eq!(scanned[0].bytes, "not really an mp4".len() as u64);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The clip folder does not exist until something is saved into it or the
    /// pane is opened -- either way the pane has to open on 0件, never an
    /// error (task2980's acceptance criteria, task3030's auto-create).
    #[test]
    fn a_missing_clip_folder_is_created_and_scans_as_empty() {
        let missing = std::env::temp_dir().join("livia-clips-no-such-folder-4b1e");
        let _ = std::fs::remove_dir_all(&missing);
        assert!(list(&missing).is_empty());
        assert!(
            missing.is_dir(),
            "list() should create the folder it just scanned"
        );
        let _ = std::fs::remove_dir_all(&missing);

        // A file handed in where a directory was expected is the same nothing
        // -- `create_dir_all` simply fails and is ignored.
        let file = fixture("as-directory");
        assert!(list(&file).is_empty());
        cleanup(&file);
    }

    /// `details` is the per-row read, so it has to survive everything the
    /// folder can hold -- including files no property handler will open.
    #[test]
    fn details_reads_both_properties_and_never_panics() {
        let path = fixture("details");
        let fresh = details(&path);
        assert_eq!(fresh.comment, None, "a fresh export carries no comment");
        // The fixture is this project's own two-frame export, and the handler
        // does report its length -- which is the duration column proved end to
        // end. The number itself is the fixture's, not a contract, so only its
        // sign is asserted.
        assert!(
            fresh.duration_100ns.is_some_and(|value| value > 0),
            "System.Media.Duration should come back for a real mp4, got {:?}",
            fresh.duration_100ns
        );

        // Writing a comment must not cost the duration: the row shows both.
        write_comment(&path, "検証クリップ").expect("write");
        let read = details(&path);
        assert_eq!(read.comment.as_deref(), Some("検証クリップ"));
        assert_eq!(read.duration_100ns, fresh.duration_100ns);

        assert_eq!(
            details(Path::new("Z:/nope/clip.mp4")),
            ClipDetails::default()
        );
        assert_eq!(details(&std::env::temp_dir()), ClipDetails::default());
        cleanup(&path);
    }

    /// Task3360's probe, and the whole reason its wording changed.
    ///
    /// The report was 「ビデオの再生中はコメント書き込めないみたい？」. Playing a
    /// clip hands it to the shell's default player, which keeps the mp4 open
    /// with a share mode that denies writers -- so `open_store`'s
    /// `GPS_READWRITE` cannot get a store at all, and the message the user
    /// sees is `comment_write_failed` plus a bare HRESULT. Not
    /// `comment_save_failed`: the store never opens, so `Commit` is never
    /// reached, which is what pins the wording onto the one key.
    ///
    /// The second half is the cause's own proof: the identical write succeeds
    /// the moment the holder lets go. That is why task3360 fixed wording
    /// rather than adding a retry -- the failure is temporary by construction.
    #[test]
    fn a_file_another_process_holds_open_refuses_the_comment() {
        use std::os::windows::fs::OpenOptionsExt;
        /// `FILE_SHARE_READ`: other readers welcome, writers refused. What a
        /// player streaming the file leaves behind, and the narrowest hold
        /// that still reproduces the report.
        const FILE_SHARE_READ: u32 = 0x0000_0001;

        let path = fixture("held-open");
        let locale = crate::ui_state::locale::active();
        let holder = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(&path)
            .expect("hold the fixture open");

        let error = write_comment(&path, "再生中に書く").expect_err("a held file refuses");
        println!("held by another process: {error}");
        assert!(
            error.starts_with(crate::ui_state::clips::comment_write_failed(locale)),
            "the store should refuse to open read-write, got {error}"
        );
        assert_eq!(
            read_comment(&path),
            None,
            "nothing was written, so the file still says what it said"
        );

        drop(holder);
        write_comment(&path, "閉じてから書く").expect("the same write, once the holder let go");
        assert_eq!(read_comment(&path).as_deref(), Some("閉じてから書く"));
        cleanup(&path);
    }

    #[test]
    fn missing_paths_and_directories_do_not_panic() {
        let missing = std::env::temp_dir().join("livia-clips-nope-9d3f/clip.mp4");
        assert_eq!(read_comment(&missing), None);
        assert!(write_comment(&missing, "x").is_err());

        let directory = std::env::temp_dir();
        assert_eq!(read_comment(&directory), None);
        assert!(write_comment(&directory, "x").is_err());
    }
    /// Task3910. The override has to reach clips too: isolating an agent run's
    /// sessions while its clips still land in the user's real clip folder is
    /// exactly the leftover the 2026-09-08 sweep had to hand back.
    #[test]
    fn a_buffer_root_override_takes_the_clip_folder_with_it() {
        let over = Path::new(r"C:\Temp\lvb-buffer-3910");
        let configured = r"D:\Apps\Liveback\Clips";

        assert_eq!(
            resolve_clip_directory(Some(configured), Some(over), over),
            over.join("Clips"),
            "the override outranks a configured clipDirectory"
        );
        assert_eq!(
            resolve_clip_directory(None, Some(over), over),
            over.join("Clips")
        );
    }

    /// With no override the configured folder wins exactly as it always did,
    /// and a blank one still falls through to the buffer root.
    #[test]
    fn without_an_override_the_configured_clip_folder_is_unchanged() {
        let buffer = Path::new(r"C:\Users\someone\AppData\Local\Liveback\buffer");
        let configured = r"D:\Apps\Liveback\Clips";

        assert_eq!(
            resolve_clip_directory(Some(configured), None, buffer),
            PathBuf::from(configured)
        );
        assert_eq!(
            resolve_clip_directory(Some("   "), None, buffer),
            buffer.join("Clips"),
            "a blank clipDirectory is not a folder named after a space"
        );
        assert_eq!(
            resolve_clip_directory(None, None, buffer),
            buffer.join("Clips")
        );
    }
}
