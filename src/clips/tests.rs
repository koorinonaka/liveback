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

/// Each test gets its own copy: writing a comment mutates the file.
pub(crate) fn fixture(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("livia-clips-{}-{}", std::process::id(), name));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let path = dir.join("clip.mp4");
    std::fs::write(
        &path,
        crate::test_support::hex_bytes(&FIXTURE_MP4_HEX.concat()),
    )
    .expect("fixture mp4");
    path
}

pub(crate) fn cleanup(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().expect("fixture parent"));
}

/// task3540's storyboard reader against the real 160x120 two-frame export:
/// both frames decode and come back fitted inside the box asked for.
#[test]
fn the_storyboard_reader_seeks_and_fits_a_frame() {
    let path = fixture("storyboard");
    crate::com::init_sta();
    let _runtime = crate::encoder::MfRuntime::start().expect("MFStartup");
    let mut reader = FrameReader::open(&path).expect("open the fixture");
    for at in [0, 333_333] {
        let (width, height, rgba) = reader.frame_at(at, (80, 80)).expect("a frame");
        assert_eq!((width, height), (80, 60), "at {at}");
        assert_eq!(rgba.len(), 80 * 60 * 4);
        assert!(rgba.chunks(4).all(|pixel| pixel[3] == 255));
    }
    drop(reader);
    cleanup(&path);
}

#[test]
fn fit_keeps_the_aspect_and_never_enlarges() {
    assert_eq!(fit((1920, 1080), (384, 216)), (384, 216));
    assert_eq!(fit((1920, 1200), (384, 216)), (346, 216));
    assert_eq!(fit((160, 120), (384, 216)), (160, 120));
}

#[test]
fn shrink_reads_bottom_up_rows_upside_down() {
    // 1x2 BGRA: top row red, bottom row blue, stored bottom-up.
    let bottom_up = [255, 0, 0, 255, 0, 0, 255, 255];
    let rgba = shrink_bgra(&bottom_up, (1, 2), -4, (1, 2)).unwrap();
    assert_eq!(rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    let top_down = shrink_bgra(&bottom_up, (1, 2), 4, (1, 2)).unwrap();
    assert_eq!(top_down, vec![0, 0, 255, 255, 255, 0, 0, 255]);
    assert!(shrink_bgra(&bottom_up, (1, 3), 4, (1, 1)).is_none());
}

#[test]
fn a_japanese_comment_survives_the_round_trip() {
    let path = fixture("roundtrip");
    let comment = "テスト録画🎬 2026-09-05 の検証";
    write_comment(&path, comment).expect("write");
    assert_eq!(read_comment(&path).as_deref(), Some(comment));
    cleanup(&path);
}

/// Task2990's edit is a *re*-write, not a first write: the list screen
/// commits over whatever the file already says. Each step is read back
/// through `details`, which is the call the row actually renders from.
#[test]
fn a_comment_can_be_rewritten_from_the_list() {
    let path = fixture("rewrite");
    // The app path is written first, as the clip save does, and every
    // comment write below must leave it standing.
    let app = r"D:\Games\テスト\game.exe";
    write_app_path(&path, app).expect("app path");
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
    assert_eq!(details(&path).app_path.as_deref(), Some(app));
    cleanup(&path);
}

/// Task2970's first bold warning still holds -- the default must not
/// follow the save dialog's memory around -- but round18 (task3030) moved
/// what it follows instead: the buffer root, not the export directory.
///
/// Holds `BUFFER_ROOT_LOCK` too, though it never moves the root: it reads
/// `buffer_root()` once for `expected` and again inside each
/// `clip_directory`, and the sibling below moving the root in between made
/// the two reads disagree (t260926-8f98: 9 red in 1500 runs of `clips::tests`).
#[test]
fn the_default_clip_folder_tracks_the_buffer_root_not_the_export_directory() {
    let _root_lock = crate::capture::tests::BUFFER_ROOT_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
    let moved = std::env::temp_dir().join(format!("livia-clip-buffer-root-{}", std::process::id()));

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

    let scanned = list(&dir).expect("scan");
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
    assert!(list(&missing).expect("created").is_empty());
    assert!(
        missing.is_dir(),
        "list() should create the folder it just scanned"
    );
    let _ = std::fs::remove_dir_all(&missing);

    // A file handed in where a directory was expected is a folder that
    // will not read: an error, not the empty folder's nothing
    // (t260928-6080 F9, overriding task3030's silence).
    let file = fixture("as-directory");
    assert!(list(&file).is_err());
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
    assert_eq!(
        crate::ui_state::clips::comment_write_error_text(locale, &error).0,
        crate::ui_state::clips::comment_write_failed(locale),
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
    let over = Path::new(r"C:\Temp\livia-buffer-3910");
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
