//! The read paths, run against `.lvb` fixtures (task440).

use super::super::container::{ContainerBuilder, ContainerReader, Damage};
use super::*;

/// Commits a container's pending writes so its allocated size can be read.
fn flush(path: &Path) {
    fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .expect("flush");
}

fn container_root(name: &str) -> TmpDir {
    let root = tmp().child(name);
    fs::create_dir_all(&root).expect("root");
    root
}

/// A two-segment session with one thumbnail, closed cleanly. `body` is
/// recognisable per segment so a read that returns the wrong record is visible
/// rather than merely the wrong length.
fn fixture(root: &Path, session_id: &str) -> PathBuf {
    let path = root.join(format!("{session_id}.lvb"));
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, &[0xA0; 512])
        .thumbnail(0, &[0x4A; 32])
        .segment(1, 20_000_000, 40_000_000, &[0xB1; 512])
        .checkpoint()
        .closed()
        .build(&path)
        .expect("fixture builds");
    path
}

#[test]
fn container_sessions_list_newest_first() {
    let root = container_root("listing");
    fixture(&root, "capture-0000000000000000000000000000000b");
    fixture(&root, "capture-0000000000000000000000000000000a");

    let listed = list_sessions(&root).expect("list");
    let ids: Vec<&str> = listed
        .iter()
        .map(|summary| summary.session_id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![
            "capture-0000000000000000000000000000000b",
            "capture-0000000000000000000000000000000a"
        ],
        "newest id first"
    );
    let container = &listed[0];
    assert_eq!(container.segment_count, 2);
    assert_eq!(container.start_100ns, Some(0));
    assert_eq!(container.end_100ns, Some(40_000_000));
    assert!(container.closed);
    assert!(!container.recoverable);
    assert!(container.readable);
    assert_eq!(container.thumbnail_index, Some(0));
    assert!(container.total_bytes > 0, "allocated size is reported");
}

#[test]
fn a_truncated_container_still_lists_and_recovers() {
    let root = container_root("truncated");
    let session_id = "capture-00000000000000000000000000000002";
    let path = root.join(format!("{session_id}.lvb"));
    // Checkpoint after the first segment, then cut the file mid-second-record:
    // the checkpoint is what lets the scan keep segment 0 while the tail goes.
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, &[0xA0; 512])
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &[0xB1; 4096])
        .build(&path)
        .expect("fixture");
    let full = fs::metadata(&path).expect("meta").len();
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, &[0xA0; 512])
        .checkpoint()
        .segment(1, 20_000_000, 40_000_000, &[0xB1; 4096])
        .damaged(Damage::TruncateTo(full - 2048))
        .build(&path)
        .expect("damaged fixture");

    let listed = list_sessions(&root).expect("list");
    assert_eq!(listed.len(), 1, "a cut container is still a session");
    assert!(
        listed[0].recoverable,
        "never told it was finished, so it is a recovery candidate"
    );

    let manifest = recover_session(&root, session_id).expect("recovers");
    assert_eq!(
        manifest
            .segments
            .iter()
            .map(|s| s.index)
            .collect::<Vec<_>>(),
        vec![0],
        "the survivor is kept and the torn record dropped"
    );
    let ring = RingBuffer::open_container(root.join(format!("{session_id}.lvb"))).expect("reopen");
    assert_eq!(
        read_target(&ring.segment_target(0).expect("target"), false).expect("reads"),
        vec![0xA0; 512],
        "the surviving segment still reads"
    );
    assert!(
        !list_sessions(&root).expect("list")[0].recoverable,
        "recovery closed it"
    );
}

#[test]
fn edits_land_as_meta_records_and_survive_a_reopen() {
    let root = container_root("edits");
    let session_id = "capture-00000000000000000000000000000003";
    fixture(&root, session_id);

    set_session_title(&root, session_id, Some("配信".into())).expect("title");
    set_session_comment(&root, session_id, Some(" 後で見る ".into())).expect("comment");
    set_session_protected(&root, session_id, true).expect("protect");
    add_marker(&root, session_id, 12_345).expect("marker");
    update_marker(&root, session_id, 12_345, "ここ".into(), None).expect("rename");

    let summary = &list_sessions(&root).expect("list")[0];
    assert_eq!(summary.target_title.as_deref(), Some("配信"));
    assert_eq!(summary.comment.as_deref(), Some("後で見る"));
    assert!(summary.protected);
    assert!(summary.closed, "an edit does not reopen a closed session");

    let ring = RingBuffer::open_container(root.join(format!("{session_id}.lvb"))).expect("open");
    assert_eq!(
        ring.manifest()
            .markers
            .iter()
            .map(|marker| (marker.time_100ns, marker.label.as_str()))
            .collect::<Vec<_>>(),
        vec![(12_345, "ここ")]
    );

    // t261009-0251, the disk arm: a comment lands, survives a reopen, and a
    // later plain rename leaves it.
    update_marker(
        &root,
        session_id,
        12_345,
        "ここ".into(),
        Some("1行目\n2行目".into()),
    )
    .expect("comment");
    update_marker(&root, session_id, 12_345, "そこ".into(), None).expect("rename again");
    let ring = RingBuffer::open_container(root.join(format!("{session_id}.lvb"))).expect("open");
    assert_eq!(
        (
            ring.manifest().markers[0].label.as_str(),
            ring.manifest().markers[0].comment.as_str()
        ),
        ("そこ", "1行目\n2行目")
    );

    delete_marker(&root, session_id, 12_345).expect("delete");
    let ring = RingBuffer::open_container(root.join(format!("{session_id}.lvb"))).expect("open");
    assert!(ring.manifest().markers.is_empty());
}

/// The door the app actually uses. `CaptureController::set_session_title` only
/// falls back to `sessions::set_session_title` for a session it does *not* hold
/// in memory -- and it holds every container the running process has listed or
/// opened. That in-memory door went through `RingBuffer::set_target_title` ->
/// `persist()`, which writes nothing for a container, so renaming from the
/// history list was a silent no-op (found on the real app during task560's
/// verification: the row reverted, no toast, `.lvb` mtime unchanged).
#[test]
fn renaming_a_container_held_in_memory_reaches_the_file() {
    let root = container_root("rename-in-memory");
    let session_id = "capture-00000000000000000000000000000009";
    let path = fixture(&root, session_id);

    let mut ring = RingBuffer::open_container(path.clone()).expect("open");
    ring.set_target_title(Some("  改名  ".into()))
        .expect("rename");
    assert_eq!(
        ring.manifest().target_title.as_deref(),
        Some("改名"),
        "the ring keeps the normalized name, not the raw input"
    );
    drop(ring);

    // Reopened from the file, which is the only thing that survives a restart.
    assert_eq!(
        list_sessions(&root).expect("list")[0]
            .target_title
            .as_deref(),
        Some("改名")
    );

    // Clearing it has to reach the file too, not just the ring.
    let mut ring = RingBuffer::open_container(path).expect("reopen");
    ring.set_target_title(None).expect("clear");
    drop(ring);
    assert_eq!(list_sessions(&root).expect("list")[0].target_title, None);
}

/// The closed-container twin of task1630's
/// `a_marker_pressed_while_recording_reaches_the_container`, and the same door
/// `renaming_a_container_held_in_memory_reaches_the_file` guards for titles:
/// `CaptureController::edit_marker` edits through the cached ring for every
/// session the process holds -- i.e. every closed session, since
/// `load_closed_sessions` inserts them all at startup -- and that path went
/// through `persist()`, which writes nothing for a container. All three edits
/// showed in the review pane and none of them survived a launch (task1670).
/// Reopening from the file is the assert.
#[test]
fn marker_edits_on_a_container_held_in_memory_reach_the_file() {
    let root = container_root("marker-in-memory");
    let session_id = "capture-00000000000000000000000000000010";
    let path = fixture(&root, session_id);
    let markers = |path: &Path| {
        RingBuffer::open_container(path.to_path_buf())
            .expect("reopen")
            .manifest()
            .markers
            .iter()
            .map(|marker| (marker.time_100ns, marker.label.clone()))
            .collect::<Vec<_>>()
    };

    let mut ring = RingBuffer::open_container(path.clone()).expect("open");
    ring.add_marker(12_345).expect("add");
    ring.add_marker(23_456).expect("add another");
    drop(ring);
    assert_eq!(
        markers(&path),
        vec![(12_345, String::new()), (23_456, String::new())],
        "an add from the review pane is in the file, not just the ring"
    );

    let mut ring = RingBuffer::open_container(path.clone()).expect("reopen");
    ring.update_marker(12_345, "ここ".into(), None)
        .expect("rename");
    ring.delete_marker(23_456).expect("delete");
    drop(ring);
    assert_eq!(
        markers(&path),
        vec![(12_345, "ここ".to_owned())],
        "the rename landed and the deleted marker does not come back"
    );

    // A miss is still an error rather than an empty record on the file.
    let mut ring = RingBuffer::open_container(path.clone()).expect("reopen");
    assert!(ring.update_marker(99_999, "x".into(), None).is_err());
    assert!(ring.delete_marker(99_999).is_err());
    drop(ring);
    assert_eq!(markers(&path), vec![(12_345, "ここ".to_owned())]);
}

#[test]
fn discarding_a_container_removes_the_one_file() {
    let root = container_root("discard");
    let session_id = "capture-00000000000000000000000000000004";
    let path = fixture(&root, session_id);
    assert!(session_disk_bytes(&root, session_id) > 0);

    discard_session(&root, session_id, Disposal::Permanent).expect("discard");
    assert!(!path.exists());
    assert!(!still_listed(&path));
    assert!(list_sessions(&root).expect("list").is_empty());
}

/// Task1520's three outcomes, on the judgement alone: the delete went through,
/// the delete was accepted but the name is still there (a pending delete, which
/// must not read as success), and there was nothing to delete.
#[test]
fn a_removal_only_counts_when_the_name_actually_leaves_the_directory() {
    removal_verdict(Ok(()), false).expect("gone is gone");

    let pending = removal_verdict(Ok(()), true)
        .expect_err("a file still in the listing has not been removed");
    // The wording is for the log, not the screen: `history::worker`'s bulk
    // discard drops this error (`Err(_) => failed += 1`) and words the toast
    // itself through `tr!`. What the verdict still owes is a reason that names
    // *why* the name is still there.
    assert!(
        pending.to_string().contains("pending"),
        "the reason has to name the cause: {pending}"
    );

    let missing = removal_verdict(
        Err(std::io::Error::from(std::io::ErrorKind::NotFound)),
        false,
    )
    .expect_err("nothing to remove");
    assert_eq!(missing.kind(), std::io::ErrorKind::NotFound);
}

/// And the half that reads the disk: enumeration, not `exists()`. A
/// delete-pending file answers `false` to `exists()` while its name is still in
/// the listing the history pane is rebuilt from, so only the directory can say
/// whether the row is about to come back.
#[test]
fn a_containers_presence_is_read_from_the_directory_listing() {
    let root = container_root("discard-listing");
    let session_id = "capture-0000000000000000000000000000152a";
    let path = fixture(&root, session_id);
    assert!(still_listed(&path));
    assert!(!still_listed(
        &root.join("capture-000000000000000000000000000000ff.lvb")
    ));

    remove_container(&path, Disposal::Permanent).expect("discard");
    assert!(!still_listed(&path));
    assert!(list_sessions(&root).expect("list").is_empty());
    assert_eq!(
        remove_container(&path, Disposal::Permanent)
            .expect_err("nothing left to remove")
            .kind(),
        std::io::ErrorKind::NotFound
    );
}

/// 計画書 R3, and the reason `total_bytes` is not `metadata().len()`: a prune
/// punches holes and leaves every surviving offset where it was, so the file
/// stays exactly as long as it was and only the *allocated* size moves.
#[test]
fn a_pruned_container_reports_less_space_than_before() {
    let root = container_root("prune");
    let session_id = "capture-00000000000000000000000000000005";
    let path = root.join(format!("{session_id}.lvb"));
    // Large enough bodies that a freed range spans whole clusters -- a hole
    // punch gives back allocation units, not bytes.
    // Not a constant byte: `GetCompressedFileSizeW` reports the *compressed*
    // size on a compressed volume (this machine's temp is one), and a run of
    // one value squashes to a single 64KB unit -- which would make the number
    // this test is about meaningless. A cheap LCG is incompressible enough.
    let body: Vec<u8> = (0..512u32 * 1024)
        .scan(0x1234_5678u32, |state, _| {
            *state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            Some((*state >> 16) as u8)
        })
        .collect();
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, &body)
        .segment(1, 20_000_000, 40_000_000, &body)
        .segment(2, 40_000_000, 60_000_000, &body)
        .checkpoint()
        .closed()
        .build(&path)
        .expect("fixture");

    // Sparse files report their allocation lazily: without this the number
    // below is whatever the lazy writer has committed so far, not what the
    // container occupies.
    flush(&path);
    let logical_before = fs::metadata(&path).expect("meta").len();
    let before = session_disk_bytes(&root, session_id);
    assert!(before > 1_000_000, "three half-megabyte segments: {before}");

    let reader = super::super::container::ContainerReader::open(&path).expect("open");
    let valid_end = reader.valid_end();
    let snapshot = reader.snapshot().clone();
    drop(reader);
    let mut writer = super::super::container::ContainerWriter::reopen(&path, valid_end, snapshot)
        .expect("reopen");
    writer.prune(2).expect("prune");
    drop(writer);

    flush(&path);
    let after = session_disk_bytes(&root, session_id);
    assert!(
        after < before,
        "hole punch must reduce the reported size: {before} -> {after}"
    );
    assert!(
        fs::metadata(&path).expect("meta").len() >= logical_before,
        "and it must do so without shortening the file, which is what keeps          every surviving offset valid"
    );

    // The survivor still reads, from the offset it always had.
    let ring = RingBuffer::open_container(path).expect("reopen");
    assert_eq!(
        ring.manifest()
            .segments
            .iter()
            .map(|segment| segment.index)
            .collect::<Vec<_>>(),
        vec![2]
    );
    assert_eq!(
        read_target(&ring.segment_target(2).expect("target"), false)
            .expect("reads")
            .len(),
        body.len()
    );
    assert_eq!(
        list_sessions(&root).expect("list")[0].total_bytes,
        after,
        "the listing reports the same allocated number"
    );
}

/// The light read open (task450) resolves a container from its checkpoint
/// instead of replaying the file. This is the case where there is no checkpoint
/// to resolve -- a recording that crashed before its first one -- which must
/// fall back to the full scan rather than report an unreadable session.
#[test]
fn a_container_with_no_usable_checkpoint_falls_back_to_the_scan() {
    let root = container_root("no-checkpoint");
    let session_id = "capture-00000000000000000000000000000007";
    let path = root.join(format!("{session_id}.lvb"));
    // Segments, never a checkpoint: both header slots stay empty, so the
    // checkpoint path has nothing to point at.
    ContainerBuilder::new(session_id)
        .segment(0, 0, 20_000_000, &[0xC3; 512])
        .segment(1, 20_000_000, 40_000_000, &[0xD4; 512])
        .build(&path)
        .expect("fixture");

    // Both doors agree, which is the property that matters: the fallback is
    // invisible to callers.
    let scanned = super::super::container::ContainerReader::open(&path).expect("scan open");
    let light =
        super::super::container::ContainerReader::open_for_reading(&path).expect("light open");
    assert_eq!(
        light.snapshot().segments.len(),
        scanned.snapshot().segments.len(),
        "the fallback sees the same segments the scan does"
    );

    // And the session is usable end to end through the normal read path.
    let listed = list_sessions(&root).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].segment_count, 2);
    let ring = RingBuffer::open_container(path).expect("open");
    let target = ring.segment_target(1).expect("segment 1 is addressable");
    assert_eq!(
        super::super::read_target(&target, false).expect("read"),
        vec![0xD4; 512]
    );
}

/// Task560: a container has to carry the capture target's name *in the file*.
///
/// It used to live only in the in-memory `SessionManifest` that
/// `create_container` builds, so the name was correct for exactly as long as
/// the recording process stayed alive. After a restart every session came back
/// nameless: the history list showed timestamps, and export refused with
/// 「ゲーム名がありません」 because `game_name` is `target_title` (found
/// verifying task240). The only channel a container has for this is a
/// `TitleSet` meta record, and the only thing that used to write one was a
/// user rename.
///
/// So this closes the file and reads it back through the same paths the app
/// uses on a cold start -- the light checkpoint open and `list_sessions` --
/// rather than asserting on the writer's own snapshot, which would have passed
/// even while the bug was live.
#[test]
fn a_containers_target_title_survives_being_reopened() {
    let root = container_root("container-title");
    let path = root.join("capture-title.lvb");
    let mut writer = super::super::container::ContainerWriter::create(&path, "capture-title", 15)
        .expect("create");
    writer
        .append_meta(&super::super::container::MetaEvent::TitleSet {
            title: Some("Sample Game".to_owned()),
        })
        .expect("title");
    writer
        .append_segment(0, 0, 20_000_000, &[0], &[0xA0; 512])
        .expect("segment");
    writer.close().expect("close");

    let light =
        super::super::container::ContainerReader::open_for_reading(&path).expect("light open");
    assert_eq!(
        light.snapshot().title.as_deref(),
        Some("Sample Game"),
        "the checkpoint open is what the app uses on a cold start"
    );

    let listed = list_sessions(&root).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(
        listed[0].target_title.as_deref(),
        Some("Sample Game"),
        "the history list reads the name from the file, not from a live recording"
    );
}

/// task2350. Both wires at once, because only one of them is enough to look
/// right: the live `RingBuffer` the recording holds, and the manifest
/// `open_container` rebuilds from the records after a restart. The auto-record
/// toggle reads whichever of the two the review screen was handed.
#[test]
fn a_containers_target_executable_survives_being_reopened() {
    let root = container_root("container-executable");
    let path = root.join("capture-exe.lvb");
    let mut writer =
        super::super::container::ContainerWriter::create(&path, "capture-exe", 15).expect("create");
    writer
        .append_meta(&super::super::container::MetaEvent::TargetExecutableSet {
            executable: Some("mspaint.exe".to_owned()),
            path: None,
        })
        .expect("executable");
    writer
        .append_segment(0, 0, 20_000_000, &[0], &[0xA0; 512])
        .expect("segment");
    writer.close().expect("close");

    assert_eq!(
        RingBuffer::open_container(path.clone())
            .expect("open")
            .manifest()
            .target_executable
            .as_deref(),
        Some("mspaint.exe"),
        "a reopened container names the executable it recorded"
    );

    let live = RingBuffer::create_container(
        path,
        "capture-exe".to_owned(),
        15,
        None,
        Some("mspaint.exe".to_owned()),
    );
    assert_eq!(
        live.manifest().target_executable.as_deref(),
        Some("mspaint.exe"),
        "and so does the manifest the running recording holds"
    );
}

/// Task720, and the same disease task560 found on the rename: the catalog holds
/// a `RingBuffer` for every container the process has listed or opened, so a
/// note or a protection toggle from the history list went through
/// `RingBuffer::set_comment`/`set_protected` -> `persist()`, which writes nothing
/// for a container. The row looked edited until the next read.
///
/// Protection is the one that costs data: `sessions_to_reclaim` reads
/// `protected` off disk, so a protection that never reached the file left the
/// session fully exposed to the capacity sweep (task700).
#[test]
fn a_comment_on_a_container_held_in_memory_reaches_the_file() {
    let root = container_root("comment-in-memory");
    let session_id = "capture-00000000000000000000000000000011";
    let path = fixture(&root, session_id);

    let mut ring = RingBuffer::open_container(path.clone()).expect("open");
    ring.set_comment(Some("  後で切り出す  ".into()))
        .expect("comment");
    assert_eq!(
        ring.manifest().comment.as_deref(),
        Some("後で切り出す"),
        "the ring keeps the normalized comment, as the disk door stores it"
    );
    drop(ring);

    assert_eq!(
        list_sessions(&root).expect("list")[0].comment.as_deref(),
        Some("後で切り出す")
    );

    // Clearing has to reach the file too.
    let mut ring = RingBuffer::open_container(path).expect("reopen");
    ring.set_comment(None).expect("clear");
    drop(ring);
    assert_eq!(list_sessions(&root).expect("list")[0].comment, None);
}

#[test]
fn protecting_a_container_held_in_memory_reaches_the_file() {
    let root = container_root("protect-in-memory");
    let session_id = "capture-00000000000000000000000000000012";
    let path = fixture(&root, session_id);

    let mut ring = RingBuffer::open_container(path.clone()).expect("open");
    ring.set_protected(true).expect("protect");
    drop(ring);
    assert!(
        list_sessions(&root).expect("list")[0].protected,
        "the sweep reads this off disk; in memory alone protects nothing"
    );

    let mut ring = RingBuffer::open_container(path).expect("reopen");
    ring.set_protected(false).expect("unprotect");
    drop(ring);
    assert!(!list_sessions(&root).expect("list")[0].protected);
}

/// task3140: an edit used to read the whole container to find `valid_end` and
/// the snapshot, so one marker on a 26 GB session froze the UI thread for as
/// long as the disk took. `append_meta` now opens at the checkpoint when the
/// checkpoint is provably the last thing in the file.
///
/// The round trip alone would pass against the old code, so the assert that
/// actually guards the fix is on the predicate itself: a cleanly closed
/// container ends exactly where its newest checkpoint ends, both before the
/// edit and after it -- the second one is what keeps the *next* edit cheap.
#[test]
fn an_edit_on_a_cleanly_closed_container_takes_the_checkpoint_path() {
    let root = container_root("edit-fast-path");
    let session_id = "capture-00000000000000000000000000000013";
    let path = fixture(&root, session_id);

    let tail_is_the_checkpoint = |path: &Path| {
        let len = fs::metadata(path).expect("metadata").len();
        ContainerReader::open_at_checkpoint(path)
            .expect("a closed container has a usable checkpoint")
            .checkpoint_end()
            == Some(len)
    };

    assert!(
        tail_is_the_checkpoint(&path),
        "closing writes the checkpoint last, so nothing sits behind it"
    );

    add_marker(&root, session_id, 12_345).expect("marker");

    let ring = RingBuffer::open_container(path.clone()).expect("open");
    assert_eq!(
        ring.manifest()
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![12_345]
    );
    drop(ring);

    assert!(
        tail_is_the_checkpoint(&path),
        "the edit checkpoints last too, so the next one stays on the cheap path"
    );
}

/// The fallback half of task3140. Bytes behind the newest checkpoint mean the
/// cheap open's `valid_end` (the file's length) would leave that debris in
/// front of the appended record, where the next full scan stops -- the edit
/// would vanish. So the edit has to fall back to the full scan, whose
/// `valid_end` cuts the debris off first.
#[test]
fn an_edit_falls_back_to_the_full_scan_when_bytes_sit_behind_the_checkpoint() {
    let root = container_root("edit-fallback");
    let session_id = "capture-00000000000000000000000000000014";
    let path = fixture(&root, session_id);

    // Not a record: byte 0 is not a known kind, so a scan stops here and
    // `read_record_at` refuses it -- exactly the shape a torn write leaves.
    let debris = [0xFFu8; 32];
    use std::io::Write;
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .expect("open for append")
        .write_all(&debris)
        .expect("append debris");
    flush(&path);

    let len = fs::metadata(&path).expect("metadata").len();
    assert_ne!(
        ContainerReader::open_at_checkpoint(&path)
            .expect("the checkpoint still resolves")
            .checkpoint_end(),
        Some(len),
        "the debris is what the predicate has to notice"
    );

    add_marker(&root, session_id, 67_890).expect("marker");

    let ring = RingBuffer::open_container(path).expect("open");
    assert_eq!(
        ring.manifest()
            .markers
            .iter()
            .map(|marker| marker.time_100ns)
            .collect::<Vec<_>>(),
        vec![67_890],
        "the appended meta is visible, so it landed in front of no debris"
    );
}

// ---------- task t260913-66a6: reads by resolved span ----------

fn open_for_reading_calls() -> usize {
    super::super::container::open_for_reading_calls()
}

/// The same container target with its span replaced.
fn with_span(target: &ReadTarget, span: Option<super::super::container::Located>) -> ReadTarget {
    let ReadTarget { path, index, .. } = target;
    ReadTarget {
        path: path.clone(),
        index: *index,
        span,
    }
}

/// An opened container resolves every segment's span, and reading by it never
/// goes back through `open_for_reading` -- counted, and then shown a second way
/// that needs no counter: with the header wiped, the span read still works and
/// the checkpoint path cannot.
#[test]
fn a_resolved_span_reads_without_reopening_the_container() {
    let root = container_root("span-fast");
    let path = fixture(&root, "capture-00000000000000000000000000000066");
    let ring = RingBuffer::open_container(path.clone()).expect("opens");
    let segment = ring.segment_target(1).expect("target");
    let thumbnail = ring.thumbnail_target(0).expect("target");
    assert!(
        matches!(segment, ReadTarget { span: Some(_), .. }),
        "an opened container resolves its segment spans"
    );
    assert!(
        matches!(thumbnail, ReadTarget { span: Some(_), .. }),
        "and its thumbnail spans"
    );
    assert!(
        ring.thumbnail_target(1).is_none(),
        "a segment without a thumbnail says so rather than reading one"
    );
    assert!(ring.segment_target(9).is_none(), "an unknown index is None");

    let before = open_for_reading_calls();
    for _ in 0..2 {
        assert_eq!(
            read_target(&segment, false).expect("segment"),
            vec![0xB1; 512]
        );
        assert_eq!(
            read_target(&thumbnail, true).expect("thumbnail"),
            vec![0x4A; 32]
        );
    }
    assert_eq!(
        open_for_reading_calls() - before,
        0,
        "four reads by span, no reopen"
    );
    // Control: the counter does see the checkpoint path.
    let unresolved = with_span(&segment, None);
    assert_eq!(
        read_target(&unresolved, false).expect("index path"),
        vec![0xB1; 512]
    );
    assert_eq!(open_for_reading_calls() - before, 1);

    let mut bytes = fs::read(&path).expect("read");
    bytes[..super::super::container::HEADER_LEN as usize].fill(0);
    fs::write(&path, &bytes).expect("wipe the header");
    assert_eq!(
        read_target(&segment, false).expect("a span read needs no header"),
        vec![0xB1; 512]
    );
    assert!(
        read_target(&unresolved, false).is_err(),
        "the checkpoint path does need it"
    );
}

/// Checkpoints are written once per 30 s of media now (t261012-fe57), so a live
/// recording's newest segments sit behind the last one. A read that has no
/// span, or whose span failed, has to find them anyway -- and still serves
/// what the checkpoint does know.
#[test]
fn a_read_without_a_span_finds_a_segment_newer_than_the_checkpoint() {
    use super::super::container::ContainerWriter;

    let root = container_root("behind-checkpoint");
    let path = root.join("capture-00000000000000000000000000000068.lvb");
    let mut writer = ContainerWriter::create(&path, "capture-68", 15).expect("create");
    writer
        .append_segment(0, 0, 20_000_000, &[0], &[0xA0; 512])
        .expect("segment 0");
    writer.checkpoint().expect("checkpoint");
    writer
        .append_segment(1, 20_000_000, 40_000_000, &[0], &[0xB1; 512])
        .expect("segment 1");
    writer
        .append_thumbnail(1, &[0x4A; 32])
        .expect("thumbnail 1");
    // The writer stays open: this is the live recording.
    let target = |index| ReadTarget {
        path: path.clone(),
        index,
        span: None,
    };
    assert_eq!(
        read_target(&target(0), false).expect("in the checkpoint"),
        vec![0xA0; 512]
    );
    assert_eq!(
        read_target(&target(1), false).expect("behind the checkpoint"),
        vec![0xB1; 512]
    );
    assert_eq!(
        read_target(&target(1), true).expect("its thumbnail"),
        vec![0x4A; 32]
    );
    assert!(
        read_target(&target(2), false).is_err(),
        "an index nobody wrote is still an error"
    );
    drop(writer);
}

// ---------- t261012-fe57: v2 files read as they are, upgrade on an edit ----------

/// The same real v2 file `container/tests.rs` reads: three segments, two
/// audio tracks, a thumbnail on segment 0, a title, markers "first" (commented)
/// and "second", protected, closed; JSON checkpoints in both slots.
const V2_FIXTURE: &[u8] = include_bytes!("../container/fixtures/v2-closed.lvb");

fn v2_session(root: &Path, session_id: &str) -> PathBuf {
    let path = root.join(format!("{session_id}.lvb"));
    fs::write(&path, V2_FIXTURE).expect("v2 fixture");
    path
}

/// Each slot's checkpoint, decoded as the header's version says.
fn slot_snapshots(bytes: &[u8]) -> Vec<Option<super::super::container::Snapshot>> {
    use super::super::container::{decode_checkpoint, decode_header, read_record_at};
    let (header, slot_a, slot_b) = decode_header(bytes).expect("header");
    [slot_a, slot_b]
        .into_iter()
        .map(|slot| {
            let slot = slot?;
            let offset = slot.offset - super::super::container::RECORD_HEADER_LEN as u64;
            let (_, payload) = read_record_at(bytes, offset)?;
            decode_checkpoint(header.format_version, payload)
        })
        .collect()
}

/// Listing, opening and playing an old session never write to it
/// (t261012-fe57): only an edit upgrades a file.
#[test]
fn a_v2_container_lists_and_opens_without_a_byte_changing() {
    let root = container_root("v2-read-only");
    let id = "capture-00000000000000000000000000000069";
    let path = v2_session(&root, id);

    let listed = list_sessions(&root).expect("lists");
    let summary = listed
        .iter()
        .find(|summary| summary.session_id == id)
        .expect("listed");
    assert_eq!(summary.segment_count, 3);
    let ring = RingBuffer::open_container(path.clone()).expect("opens");
    let segment = ring.segment_target(1).expect("target");
    assert_eq!(read_target(&segment, false).expect("by span").len(), 64);
    assert_eq!(
        read_target(&with_span(&segment, None), false)
            .expect("by index")
            .len(),
        64
    );
    drop(ring);
    assert_eq!(fs::read(&path).expect("read"), V2_FIXTURE, "untouched");
}

/// The first edit to a v2 file makes it v3 (t261012-fe57): both slots name
/// MessagePack checkpoints, nothing it held is lost, and with the newer slot
/// damaged the older one -- the upgrade's own -- still opens without a replay.
#[test]
fn editing_a_v2_container_upgrades_it_and_keeps_everything() {
    let root = container_root("v2-edit");
    let id = "capture-00000000000000000000000000000070";
    let path = v2_session(&root, id);
    add_marker(&root, id, 50_000_000).expect("the edit lands");

    let mut bytes = fs::read(&path).expect("read");
    assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), 3);
    let slots = slot_snapshots(&bytes);
    assert!(
        slots.iter().all(Option::is_some),
        "both slots decode as v3: {slots:?}"
    );
    let reader = ContainerReader::open_for_reading(&path).expect("opens");
    let snapshot = reader.snapshot();
    let indexes: Vec<u64> = snapshot.segments.iter().map(|s| s.index).collect();
    assert_eq!(indexes, vec![0, 1, 2]);
    assert!(snapshot.segments[0].thumbnail.is_some());
    assert!(snapshot
        .segments
        .iter()
        .all(|segment| segment.audio_offsets_100ns == vec![0, 1_234]));
    assert_eq!(snapshot.title.as_deref(), Some("v2 fixture"));
    assert!(snapshot.protected && snapshot.closed);
    assert_eq!(snapshot.audio_tracks.len(), 2);
    assert_eq!(snapshot.audio_tracks[1].microphone.as_deref(), Some(""));
    let markers: Vec<(i64, &str)> = snapshot
        .markers
        .iter()
        .map(|marker| (marker.time_100ns, marker.comment.as_str()))
        .collect();
    assert_eq!(
        markers,
        vec![
            (10_000_000, "commented"),
            (30_000_000, ""),
            (50_000_000, "")
        ]
    );
    drop(reader);

    // Damage the newer slot: the older is the upgrade's checkpoint, the file
    // as it was before the edit.
    let (_, slot_a, slot_b) = super::super::container::decode_header(&bytes).expect("header");
    let generation = |slot: Option<super::super::container::Slot>| slot.map_or(0, |s| s.generation);
    let at = if generation(slot_a) > generation(slot_b) {
        256
    } else {
        320
    };
    bytes[at] ^= 0xFF;
    fs::write(&path, &bytes).expect("damage");
    let reader = ContainerReader::open_for_reading(&path).expect("opens");
    assert!(
        reader.checkpoint_end().is_some(),
        "the older slot opened, not a replay"
    );
    assert_eq!(reader.snapshot().markers.len(), 2);
    // The replay still sees the edit behind it.
    let full = ContainerReader::open(&path).expect("replays");
    assert_eq!(full.snapshot().markers.len(), 3);
}

/// An edit to a v1 file is refused and writes nothing (t261012-fe57) -- the
/// `Err` the history and review screens turn into their save-failed toast.
/// The refusal reads the header alone, so the v2 fixture relabelled v1 stands
/// in for one here; `container/tests.rs` covers a real v1 layout.
#[test]
fn an_edit_to_a_v1_container_is_an_error_and_writes_nothing() {
    let root = container_root("v1-edit");
    let id = "capture-00000000000000000000000000000071";
    let path = root.join(format!("{id}.lvb"));
    let mut bytes = V2_FIXTURE.to_vec();
    bytes[4..8].copy_from_slice(&1u32.to_le_bytes());
    fs::write(&path, &bytes).expect("v1 fixture");

    assert!(add_marker(&root, id, 50_000_000).is_err());
    assert!(set_session_protected(&root, id, false).is_err());
    assert!(set_session_comment(&root, id, Some("comment".into())).is_err());
    assert_eq!(fs::read(&path).expect("read"), bytes, "untouched");
}

/// A span that does not verify -- wrong CRC, or pointing past the end -- is
/// not served and not an error: the read asks the container's index instead.
/// So does a session whose table simply has no span for the index.
#[test]
fn a_span_that_does_not_verify_falls_back_to_the_index() {
    use super::super::container::{Located, Span};

    let root = container_root("span-fallback");
    let path = fixture(&root, "capture-00000000000000000000000000000067");
    let ring = RingBuffer::open_container(path.clone()).expect("opens");
    let segment = ring.segment_target(1).expect("target");
    let ReadTarget {
        span: Some(good), ..
    } = segment
    else {
        panic!("an opened container resolves its spans");
    };
    let wrong_crc = Located {
        crc: good.crc ^ 1,
        ..good
    };
    let past_end = Located {
        record: Span {
            offset: u64::MAX / 2,
            len: good.record.len,
        },
        ..good
    };

    let before = open_for_reading_calls();
    for (label, span) in [("wrong crc", wrong_crc), ("past the end", past_end)] {
        assert_eq!(
            read_target(&with_span(&segment, Some(span)), false)
                .unwrap_or_else(|error| panic!("{label}: falls back, got {error}")),
            vec![0xB1; 512],
            "{label}: the index path serves the right record"
        );
    }
    let thumbnail = ring.thumbnail_target(0).expect("target");
    let ReadTarget {
        span: Some(thumbnail_span),
        ..
    } = thumbnail
    else {
        panic!("thumbnail span resolved");
    };
    assert_eq!(
        read_target(
            &with_span(
                &thumbnail,
                Some(Located {
                    crc: thumbnail_span.crc ^ 1,
                    ..thumbnail_span
                })
            ),
            true
        )
        .expect("thumbnail falls back"),
        vec![0x4A; 32]
    );
    assert_eq!(
        open_for_reading_calls() - before,
        3,
        "each of the three went through the index"
    );

    // No span in the table at all: a container view that was never told any.
    let mut untold = RingBuffer::create_container(
        path,
        "capture-00000000000000000000000000000067".into(),
        15,
        None,
        None,
    );
    untold
        .add(SegmentRecord {
            index: 1,
            path: PathBuf::new(),
            start_100ns: 20_000_000,
            end_100ns: 40_000_000,
            bytes: 512,
            thumbnail_path: None,
            audio_offsets_100ns: vec![0],
        })
        .expect("add");
    let target = untold.segment_target(1).expect("target");
    assert!(matches!(target, ReadTarget { span: None, .. }));
    assert_eq!(
        read_target(&target, false).expect("index path"),
        vec![0xB1; 512]
    );
    assert_eq!(open_for_reading_calls() - before, 4);
}

/// Rot in the record itself is an error by either door -- the fallback exists
/// for a stale span, not to find another way to serve bad bytes. Segment 0,
/// untouched in the same file, is the control that the reads work at all.
#[test]
fn a_rotten_record_is_an_error_by_span_and_by_index() {
    let root = container_root("span-rot");
    let path = fixture(&root, "capture-00000000000000000000000000000068");
    let body = ContainerReader::open(&path)
        .expect("scan")
        .snapshot()
        .segments
        .iter()
        .find(|segment| segment.index == 1)
        .expect("segment 1")
        .data
        .body;
    let ring = RingBuffer::open_container(path.clone()).expect("opens");
    let segment = ring.segment_target(1).expect("target");
    let control = ring.segment_target(0).expect("target");

    let mut bytes = fs::read(&path).expect("read");
    bytes[(body.offset + 7) as usize] ^= 0x01;
    fs::write(&path, &bytes).expect("rot one byte");

    assert!(
        matches!(segment, ReadTarget { span: Some(_), .. }),
        "the span path is the one under test"
    );
    assert!(
        read_target(&segment, false).is_err(),
        "a span whose bytes rotted is not served"
    );
    assert!(
        read_target(&with_span(&segment, None), false).is_err(),
        "nor does the index path serve them"
    );
    assert_eq!(
        read_target(&control, false).expect("segment 0 is untouched"),
        vec![0xA0; 512]
    );
}

/// Every `cut=false` read line says why it read whole (t261018-fc75): one read
/// per path through `read_target_from` / `read_target`, captured from the real
/// subscriber. The cut read is the control -- its line carries no reason, so a
/// reason printed on every line regardless would fail here.
///
/// The table is hand-built over plain bytes: the cut read checks tiling and
/// CRCs, not MP4 boxes, so no muxer is needed to reach every branch.
#[test]
fn a_whole_read_logs_why_it_was_not_cut() {
    use super::super::container::{crc32, fragments::FragmentRow, ContainerWriter, FragmentTable};
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Lines(Arc<Mutex<Vec<u8>>>);
    impl Write for Lines {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let root = container_root("cut-skip");
    let path = root.join("cut-skip.lvb");
    let (head, first, second) = ([0x11u8; 64], [0x22u8; 100], [0x33u8; 100]);
    let body = [&head[..], &first, &second].concat();
    let fragment = |offset: u32, bytes: &[u8], time: i64| FragmentRow {
        offset,
        len: bytes.len() as u32,
        crc: crc32(bytes),
        keyframe_100ns: Some(time),
        audio_100ns: Some(time),
    };
    let table = FragmentTable {
        head_len: 64,
        head_crc: crc32(&head),
        rows: vec![fragment(64, &first, 0), fragment(164, &second, 5_000_000)],
    };
    let mut writer = ContainerWriter::create(&path, "cut-skip", 15).expect("create");
    let appended = writer
        .append_segment(0, 0, 20_000_000, &[0], &body)
        .expect("append");
    writer.close().expect("close");
    let target = ReadTarget {
        path: path.clone(),
        index: 0,
        span: Some(appended.located),
    };
    // Audio that starts after the second GOP: only row 0 is in time, and a
    // cut there would be the head.
    let mut late_audio = table.clone();
    late_audio.rows[1].audio_100ns = Some(6_000_000);
    // The kept fragment fails its check; the whole record still verifies.
    let mut stale = table.clone();
    stale.rows[1].crc ^= 1;

    // tracing-core caches a callsite's interest from the *registering
    // thread's* default while only one dispatcher exists, so a parallel test
    // that reached `read_target` first cached its lines as never-enabled and
    // the module run kept only the cut line (alone the test passed, 3 of 5
    // module runs failed). A second live dispatcher makes registration ask
    // every dispatcher, ours included.
    let _second = tracing::Dispatch::new(tracing_subscriber::registry());
    let lines = Lines::default();
    let sink = lines.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || sink.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        for (fragments, from) in [
            (Some(&table), 7_500_000),
            (None, 7_500_000),
            (Some(&table), 1_000_000),
            (Some(&late_audio), 7_500_000),
            (Some(&stale), 7_500_000),
        ] {
            read_target_from(&target, fragments, from).expect("served");
        }
        read_target(&target, false).expect("served");
    });
    let logged = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
    let field = |line: &str, key: &str| {
        line.split_whitespace()
            .find_map(|word| word.strip_prefix(key))
            .map(str::to_owned)
    };
    let reads: Vec<(Option<String>, Option<String>)> = logged
        .lines()
        .filter(|line| line.contains("task128_read_span"))
        .map(|line| (field(line, "cut="), field(line, "cut_skip=")))
        .collect();
    let line = |cut: &str, skip: Option<&str>| (Some(cut.to_owned()), skip.map(str::to_owned));
    assert_eq!(
        reads,
        [
            line("true", None),
            line("false", Some("no_table")),
            line("false", Some("first_gop")),
            line("false", Some("no_audio")),
            line("false", Some("crc")),
            line("false", Some("whole")),
        ],
        "{logged}"
    );
}

/// t261025-d59e: a relisting reopens only the containers that changed since
/// the last listing; the history relists every 5 s while recording.
#[test]
fn a_relisting_opens_only_the_containers_that_changed() {
    use super::super::container::open_for_reading_calls;
    let root = container_root("listing-cache");
    let edited = "capture-000000000000000000000000000000c1";
    fixture(&root, edited);
    fixture(&root, "capture-000000000000000000000000000000c2");

    let opens = open_for_reading_calls();
    let first = list_sessions(&root).expect("list");
    assert_eq!(open_for_reading_calls() - opens, 2);

    let opens = open_for_reading_calls();
    assert_eq!(list_sessions(&root).expect("list"), first);
    assert_eq!(
        open_for_reading_calls() - opens,
        0,
        "unchanged containers are not reopened"
    );

    set_session_title(&root, edited, Some("renamed".into())).expect("title");
    let opens = open_for_reading_calls();
    let third = list_sessions(&root).expect("list");
    assert_eq!(open_for_reading_calls() - opens, 1, "only the edited one");
    let row = third
        .iter()
        .find(|row| row.session_id == edited)
        .expect("row");
    assert_eq!(row.target_title.as_deref(), Some("renamed"));
}

/// t261101-27d6: a recording session's row comes from the caller, so the 5 s
/// relist neither opens nor caches the container it is appending to. The
/// other container in the same listing is the control that opens.
#[test]
fn a_live_row_is_listed_without_opening_its_container() {
    use super::super::container::open_for_reading_calls;
    let root = container_root("listing-live");
    let live_id = "capture-000000000000000000000000000000e1";
    let live_path = fixture(&root, live_id);
    fixture(&root, "capture-000000000000000000000000000000e2");

    let mut live_row = list_sessions(&root)
        .expect("list")
        .into_iter()
        .find(|row| row.session_id == live_id)
        .expect("row");
    live_row.segment_count = 7;
    live_row.end_100ns = Some(99);
    // A new key on both, so neither is served from the cache.
    for path in [
        &live_path,
        &root.join("capture-000000000000000000000000000000e2.lvb"),
    ] {
        fs::File::options()
            .write(true)
            .open(path)
            .and_then(|file| file.set_modified(std::time::SystemTime::UNIX_EPOCH))
            .expect("touch");
    }

    let live = HashMap::from([(live_path.clone(), live_row.clone())]);
    let opens = open_for_reading_calls();
    let listed = list_sessions_with_live(&root, &live).expect("list");
    assert_eq!(open_for_reading_calls() - opens, 1, "only the closed one");
    assert_eq!(
        listed.iter().find(|row| row.session_id == live_id),
        Some(&live_row)
    );

    // Not cached: once the overlay is gone the file is opened, once.
    let opens = open_for_reading_calls();
    let after = list_sessions(&root).expect("list");
    assert_eq!(open_for_reading_calls() - opens, 1, "the stopped one opens");
    assert_eq!(
        after
            .iter()
            .find(|row| row.session_id == live_id)
            .map(|row| row.segment_count),
        Some(2)
    );
    let opens = open_for_reading_calls();
    list_sessions(&root).expect("list");
    assert_eq!(
        open_for_reading_calls() - opens,
        0,
        "then the cache answers"
    );
}
