//! Session listing and thumbnail-index tests.
use super::*;
use crate::ring_buffer::container::{ContainerBuilder, MetaEvent};

/// A closed one-segment container at `root/<session_id>.lvb`.
fn closed_container(root: &Path, session_id: &str) -> PathBuf {
    let path = root.join(format!("{session_id}.{CONTAINER_EXTENSION}"));
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, b"abcd")
        .checkpoint()
        .closed()
        .build(&path)
        .unwrap();
    path
}

/// Task3100. `<buffer_root>\Clips` is where clips are saved, not a session, and
/// the listing scan used to call every directory a session. That gave the
/// history a "Clips / 読み込めません" row *and* fed `repair_sessions` a target
/// that eventually deleted the folder.
///
/// Both post-defect states are covered: the folder as the user has it (mp4s,
/// no manifest) and the folder after one launch already "recovered" it
/// (`manifest.json` present). The second is the one a name-plus-manifest
/// predicate would still get wrong.
#[test]
fn a_clips_folder_is_not_a_session_even_after_a_manifest_was_written_into_it() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();

    let clips = root.join("Clips");
    fs::create_dir_all(&clips).unwrap();
    for n in 0..4 {
        fs::write(
            clips.join(format!("verify-{n}.mp4")),
            b"a clip the user kept",
        )
        .unwrap();
    }

    // A real session beside it, so this pins "Clips is skipped", not "the scan
    // returned nothing".
    let session_id = "capture-00000000000000000000000000000001";
    closed_container(&root, session_id);

    let ids: Vec<_> = list_sessions(&root)
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    assert_eq!(ids, vec![session_id.to_owned()], "no Clips row");

    // Now the state a machine that already hit the defect is in: the clip
    // folder carries a manifest the old scan's "recovery" wrote into it.
    fs::write(
        clips.join("manifest.json"),
        format!(
            r#"{{"version":{MANIFEST_VERSION},"sessionId":"Clips","closed":true,
                 "retentionMinutes":15,"segments":[],"gaps":[]}}"#
        ),
    )
    .unwrap();

    let ids: Vec<_> = list_sessions(&root)
        .unwrap()
        .into_iter()
        .map(|s| s.session_id)
        .collect();
    assert_eq!(
        ids,
        vec![session_id.to_owned()],
        "a manifest inside Clips must not buy it a row back"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn list_sessions_never_mutates_the_buffer_directory() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();
    // A crashed recording -- no `Closed` record -- is what a listing must not
    // "helpfully" repair, plus a file that is not a session at all.
    let session_id = "capture-side-effect-check";
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, b"video-bytes")
        .checkpoint()
        .build(&root.join(format!("{session_id}.{CONTAINER_EXTENSION}")))
        .unwrap();
    fs::write(root.join("stray.partial.mp4"), b"bad").unwrap();

    // Only files are compared, not directories: NTFS can lazily flush a
    // directory's own Last Write Time after a create+write burst, so a
    // directory's reported mtime can still visibly change between the "before"
    // and "after" snapshots below with zero actual filesystem mutation in
    // between -- that's a metadata-coherency artifact of the test setup, not
    // something list_sessions did. File contents/sizes/mtimes are exact.
    fn snapshot(dir: &Path) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut out = vec![];
        for entry in fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            let path = entry.path();
            if path.is_dir() {
                out.extend(snapshot(&path));
                continue;
            }
            let metadata = entry.metadata().unwrap();
            out.push((path.clone(), metadata.len(), metadata.modified().unwrap()));
        }
        out.sort();
        out
    }

    let before = snapshot(&root);
    let listed = list_sessions(&root).unwrap();
    let after = snapshot(&root);
    // Control: the listing did open the crashed container.
    assert_eq!(listed.len(), 1);
    assert!(listed[0].recoverable);
    assert_eq!(
        before, after,
        "list_sessions must not create, rename, or remove anything on disk"
    );
}

#[test]
fn thumbnail_index_uses_the_segment_index_field_not_vec_position_after_prune() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();
    // Simulates a retention-pruned session: segments 0-2 (and their
    // thumbnails) were already pruned, so the oldest surviving segment has
    // `SegmentRecord.index == 3` while sitting at vec position 0. Picking
    // the vec position instead of the `index` field here would wrongly
    // report thumbnail_index 0, a segment that no longer exists.
    let mut builder = ContainerBuilder::new("x");
    for i in 3..6u64 {
        let start = i as i64 * 20_000_000;
        builder = builder
            .segment(i, start, start + 20_000_000, b"x")
            .thumbnail(i, b"jpg");
    }
    builder
        .checkpoint()
        .closed()
        .build(&root.join(format!("x.{CONTAINER_EXTENSION}")))
        .unwrap();
    let summary = list_sessions(&root).unwrap().remove(0);
    assert_eq!(summary.thumbnail_index, Some(3));
}

/// Task1520 follow-up. A `.lvb` that will not open keeps its row instead of
/// disappearing from the list.
///
/// The row that vanishes is not merely missing -- every row below it moves up
/// one. The history is a list you right-click, so a row sliding into the
/// position the pointer just aimed at hands the discard entry a different
/// session. That is how a user's 16 GB recording was destroyed.
#[test]
fn an_unopenable_container_keeps_its_row_so_the_rows_below_it_do_not_move() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();

    // Newer id sorts first, so this is the row a pointer aimed at row 0 hits.
    let broken_id = "capture-00000000000000000000000000000009";
    let broken = root.join(format!("{broken_id}.lvb"));
    // Not a container at all: `ContainerReader::open_for_reading` refuses it,
    // which is the same `None` a locked file produces without needing a second
    // process to hold one open.
    fs::write(&broken, b"not a container").unwrap();

    let intact_id = "capture-00000000000000000000000000000001";
    closed_container(&root, intact_id);

    let listed = list_sessions(&root).unwrap();
    assert_eq!(listed.len(), 2, "the unreadable session is still listed");

    // Position is the whole point: the intact session must not have moved up
    // into the unreadable one's place.
    assert_eq!(listed[0].session_id, broken_id);
    assert_eq!(listed[1].session_id, intact_id);

    // And it is marked so the UI can refuse to act on it.
    assert!(!listed[0].readable);
    assert_eq!(listed[0].segment_count, 0);
    assert_eq!(listed[0].total_bytes, 0);
    assert!(!listed[0].protected);
    assert!(listed[1].readable);
}

/// t260927-9a17: the history's app column and marker flag read these two off
/// the summary, copied from the container -- and one with neither (a monitor
/// recording, or one made before either existed) reports none.
#[test]
fn a_summary_carries_the_recorded_app_and_the_marker_count() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();
    let write = |session_id: &str, executable: Option<&str>, markers: usize| {
        let mut builder = ContainerBuilder::new(session_id)
            .segment(0, 0, 20_000_000, b"x")
            .meta(MetaEvent::TitleSet {
                title: Some("Notepad".into()),
            });
        if let Some(exe) = executable {
            builder = builder.meta(MetaEvent::TargetExecutableSet {
                executable: Some(exe.to_owned()),
                path: Some(format!("C:\\Apps\\{exe}")),
            });
        }
        for index in 0..markers {
            builder = builder.meta(MetaEvent::MarkerAdded {
                time_100ns: index as i64 * 10_000_000,
                label: String::new(),
                palette_index: None,
                comment: None,
            });
        }
        let path = root.join(format!("{session_id}.{CONTAINER_EXTENSION}"));
        builder.checkpoint().closed().build(&path).unwrap();
        list_sessions(&root)
            .unwrap()
            .into_iter()
            .find(|summary| summary.session_id == session_id)
            .unwrap()
    };

    let app = write("with-app", Some("notepad.exe"), 2);
    assert_eq!(app.target_executable.as_deref(), Some("notepad.exe"));
    assert_eq!(
        app.target_executable_path.as_deref(),
        Some("C:\\Apps\\notepad.exe")
    );
    assert_eq!(app.marker_count, 2);

    let monitor = write("monitor", None, 0);
    assert_eq!(monitor.target_executable, None);
    assert_eq!(monitor.target_executable_path, None);
    assert_eq!(monitor.marker_count, 0);
}
