//! Session listing and thumbnail-index tests.
use super::*;

#[test]
fn list_sessions_classifies_every_kind_and_sorts_newest_first() {
    let root = tmp();
    fs::create_dir_all(&root).unwrap();

    let closed_id = "capture-00000000000000000000000000000001";
    let closed_dir = root.join(closed_id);
    let mut closed = RingBuffer::create(closed_dir.clone(), closed_id.into(), 15, None).unwrap();
    fs::write(closed_dir.join("0.mp4"), b"abcd").unwrap();
    closed
        .add(SegmentRecord {
            index: 0,
            path: "0.mp4".into(),
            start_100ns: 0,
            end_100ns: 20_000_000,
            bytes: 4,
            thumbnail_path: None,
            audio_offsets_100ns: vec![0],
        })
        .unwrap();
    closed.close().unwrap();

    let open_id = "capture-00000000000000000000000000000002";
    let open_dir = root.join(open_id);
    let _open = RingBuffer::create(open_dir.clone(), open_id.into(), 15, None).unwrap();

    let partial_id = "capture-00000000000000000000000000000003";
    let partial_dir = root.join(partial_id);
    let mut partial = RingBuffer::create(partial_dir.clone(), partial_id.into(), 15, None).unwrap();
    partial.close().unwrap();
    fs::write(partial_dir.join("stray.partial.mp4"), b"bad").unwrap();

    let no_manifest_id = "capture-00000000000000000000000000000004";
    let no_manifest_dir = root.join(no_manifest_id);
    fs::create_dir_all(&no_manifest_dir).unwrap();
    fs::write(no_manifest_dir.join("recording.marker"), b"recording").unwrap();
    fs::write(no_manifest_dir.join("0.mp4"), b"12345").unwrap();
    fs::write(no_manifest_dir.join("1.mp4"), b"1234567890").unwrap();

    let old_version_id = "capture-00000000000000000000000000000005";
    let old_version_dir = root.join(old_version_id);
    let mut old_version =
        RingBuffer::create(old_version_dir.clone(), old_version_id.into(), 15, None).unwrap();
    old_version.close().unwrap();
    let mut manifest = old_version.manifest().clone();
    manifest.version = MANIFEST_VERSION - 1;
    fs::write(
        old_version_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let sessions = list_sessions(&root).unwrap();
    let ids: Vec<_> = sessions.iter().map(|s| s.session_id.clone()).collect();
    assert_eq!(
        ids,
        vec![
            old_version_id,
            no_manifest_id,
            partial_id,
            open_id,
            closed_id
        ],
        "sessions must sort newest (highest session id) first"
    );

    let by_id: HashMap<_, _> = sessions
        .into_iter()
        .map(|s| (s.session_id.clone(), s))
        .collect();

    let closed_summary = &by_id[closed_id];
    assert!(closed_summary.closed);
    assert!(closed_summary.readable);
    assert_eq!(closed_summary.manifest_version, MANIFEST_VERSION);
    assert!(!closed_summary.has_partial);
    assert!(!closed_summary.recoverable);
    assert_eq!(closed_summary.segment_count, 1);
    assert_eq!(closed_summary.start_100ns, Some(0));
    assert_eq!(closed_summary.end_100ns, Some(20_000_000));
    assert_eq!(
        closed_summary.thumbnail_index, None,
        "no segment carries a thumbnail"
    );

    let open_summary = &by_id[open_id];
    assert!(!open_summary.closed);
    assert!(open_summary.recoverable);
    assert!(open_summary.readable);

    let partial_summary = &by_id[partial_id];
    assert!(partial_summary.has_partial);
    assert!(
        partial_dir.join("stray.partial.mp4").exists(),
        "list_sessions must never quarantine or move partial files"
    );
    assert!(!partial_dir.join("quarantine").exists());

    let no_manifest_summary = &by_id[no_manifest_id];
    assert!(!no_manifest_summary.readable);
    assert_eq!(no_manifest_summary.manifest_version, 0);
    assert_eq!(no_manifest_summary.segment_count, 2);
    assert_eq!(no_manifest_summary.total_bytes, 15);
    assert!(!no_manifest_summary.closed);
    assert!(no_manifest_summary.recoverable);
    assert_eq!(no_manifest_summary.start_100ns, None);
    assert_eq!(no_manifest_summary.thumbnail_index, None);

    let old_version_summary = &by_id[old_version_id];
    assert!(!old_version_summary.readable);
    assert_eq!(old_version_summary.manifest_version, MANIFEST_VERSION - 1);
    assert!(old_version_summary.closed);
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
    let session_dir = root.join(session_id);
    let mut session = RingBuffer::create(session_dir.clone(), session_id.into(), 15, None).unwrap();
    fs::write(session_dir.join("0.mp4"), b"x").unwrap();
    session.add(seg(0)).unwrap();
    session.close().unwrap();

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
    let session_dir = root.join("capture-side-effect-check");
    fs::create_dir_all(&session_dir).unwrap();
    fs::write(session_dir.join("recording.marker"), b"recording").unwrap();
    fs::write(session_dir.join("stray.partial.mp4"), b"bad").unwrap();
    fs::write(session_dir.join("0.mp4"), b"video-bytes").unwrap();

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
    let _ = list_sessions(&root).unwrap();
    let after = snapshot(&root);
    assert_eq!(
        before, after,
        "list_sessions must not create, rename, or remove anything on disk"
    );
}

#[test]
fn thumbnail_index_uses_the_segment_index_field_not_vec_position_after_prune() {
    let p = tmp();
    // Simulates a retention-pruned session: segments 0-2 (and their .jpg
    // sidecars) were already pruned, so the oldest surviving segment has
    // `SegmentRecord.index == 3` while sitting at vec position 0. Picking
    // the vec position instead of the `index` field here would wrongly
    // report thumbnail_index 0, a segment that no longer exists.
    fs::create_dir_all(&p).unwrap();
    let mut segments = vec![];
    for i in 3..6u64 {
        fs::write(p.join(format!("{i}.mp4")), b"x").unwrap();
        fs::write(p.join(format!("{i}.jpg")), b"jpg").unwrap();
        let mut s = seg(i);
        s.thumbnail_path = Some(format!("{i}.jpg").into());
        segments.push(s);
    }
    let manifest = SessionManifest {
        audio_tracks: Vec::new(),
        version: MANIFEST_VERSION,
        session_id: "x".into(),
        closed: true,
        retention_minutes: DEFAULT_RETENTION_MINUTES,
        segments,
        gaps: vec![],
        target_title: None,
        target_executable: None,
        target_executable_path: None,
        markers: vec![],
        note: None,
        protected: false,
    };
    fs::write(
        p.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let summary = summarize_session_dir(&p).unwrap();
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
    let intact_dir = root.join(intact_id);
    let mut intact = RingBuffer::create(intact_dir.clone(), intact_id.into(), 15, None).unwrap();
    fs::write(intact_dir.join("0.mp4"), b"abcd").unwrap();
    intact
        .add(SegmentRecord {
            index: 0,
            path: "0.mp4".into(),
            start_100ns: 0,
            end_100ns: 20_000_000,
            bytes: 4,
            thumbnail_path: None,
            audio_offsets_100ns: vec![0],
        })
        .unwrap();
    intact.close().unwrap();

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
/// the summary, copied from the manifest -- and a manifest with neither (a
/// monitor recording, or one made before either existed) reports none.
#[test]
fn a_summary_carries_the_recorded_app_and_the_marker_count() {
    let write = |dir: &std::path::Path, executable: Option<&str>, markers: usize| {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("0.mp4"), b"x").unwrap();
        let manifest = SessionManifest {
            audio_tracks: Vec::new(),
            version: MANIFEST_VERSION,
            session_id: "x".into(),
            closed: true,
            retention_minutes: DEFAULT_RETENTION_MINUTES,
            segments: vec![seg(0)],
            gaps: vec![],
            target_title: Some("Notepad".into()),
            target_executable: executable.map(str::to_owned),
            target_executable_path: executable.map(|exe| format!("C:\\Apps\\{exe}")),
            markers: (0..markers)
                .map(|index| MarkerRecord {
                    time_100ns: index as i64 * 10_000_000,
                    label: String::new(),
                    color_index: None,
                })
                .collect(),
            note: None,
            protected: false,
        };
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        summarize_session_dir(dir).unwrap()
    };
    let root = tmp();

    let app = write(&root.join("with-app"), Some("notepad.exe"), 2);
    assert_eq!(app.target_executable.as_deref(), Some("notepad.exe"));
    assert_eq!(
        app.target_executable_path.as_deref(),
        Some("C:\\Apps\\notepad.exe")
    );
    assert_eq!(app.marker_count, 2);

    let monitor = write(&root.join("monitor"), None, 0);
    assert_eq!(monitor.target_executable, None);
    assert_eq!(monitor.target_executable_path, None);
    assert_eq!(monitor.marker_count, 0);
}
