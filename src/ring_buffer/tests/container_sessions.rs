//! The read paths, run against `.lvb` fixtures instead of session directories
//! (task440). Every one of these is a case the directory tests already cover
//! for the other shape, so the pair is what says "the seam holds".

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
fn a_container_session_lists_beside_the_directory_ones() {
    let root = container_root("listing");
    // One of each shape, so the listing has to handle both in one pass.
    fixture(&root, "capture-0000000000000000000000000000000b");
    let directory = root.join("capture-0000000000000000000000000000000a");
    fs::create_dir_all(&directory).expect("dir");
    RingBuffer::create(
        directory,
        "capture-0000000000000000000000000000000a".into(),
        15,
        None,
    )
    .expect("ring")
    .close()
    .expect("close");

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
        "both shapes list, newest id first"
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
        read_target(&ring.segment_target(0).expect("target"), "mp4", false).expect("reads"),
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
    set_session_note(&root, session_id, Some(" 後で見る ".into())).expect("note");
    set_session_protected(&root, session_id, true).expect("protect");
    add_marker(&root, session_id, 12_345).expect("marker");
    update_marker(&root, session_id, 12_345, "ここ".into()).expect("rename");

    let summary = &list_sessions(&root).expect("list")[0];
    assert_eq!(summary.target_title.as_deref(), Some("配信"));
    assert_eq!(summary.note.as_deref(), Some("後で見る"));
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
    ring.update_marker(12_345, "ここ".into()).expect("rename");
    ring.delete_marker(23_456).expect("delete");
    drop(ring);
    assert_eq!(
        markers(&path),
        vec![(12_345, "ここ".to_owned())],
        "the rename landed and the deleted marker does not come back"
    );

    // A miss is still an error rather than an empty record on the file.
    let mut ring = RingBuffer::open_container(path.clone()).expect("reopen");
    assert!(ring.update_marker(99_999, "x".into()).is_err());
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
        read_target(&ring.segment_target(2).expect("target"), "mp4", false)
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
        super::super::read_target(&target, "mp4", false).expect("read"),
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
/// `RingBuffer::set_note`/`set_protected` -> `persist()`, which writes nothing
/// for a container. The row looked edited until the next read.
///
/// Protection is the one that costs data: `sessions_to_reclaim` reads
/// `protected` off disk, so a protection that never reached the file left the
/// session fully exposed to the capacity sweep (task700).
#[test]
fn a_note_on_a_container_held_in_memory_reaches_the_file() {
    let root = container_root("note-in-memory");
    let session_id = "capture-00000000000000000000000000000011";
    let path = fixture(&root, session_id);

    let mut ring = RingBuffer::open_container(path.clone()).expect("open");
    ring.set_note(Some("  後で切り出す  ".into()))
        .expect("note");
    assert_eq!(
        ring.manifest().note.as_deref(),
        Some("後で切り出す"),
        "the ring keeps the normalized note, as the disk door stores it"
    );
    drop(ring);

    assert_eq!(
        list_sessions(&root).expect("list")[0].note.as_deref(),
        Some("後で切り出す")
    );

    // Clearing has to reach the file too.
    let mut ring = RingBuffer::open_container(path).expect("reopen");
    ring.set_note(None).expect("clear");
    drop(ring);
    assert_eq!(list_sessions(&root).expect("list")[0].note, None);
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
    let ReadTarget::Container { path, index, .. } = target else {
        panic!("a container target");
    };
    ReadTarget::Container {
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
        matches!(segment, ReadTarget::Container { span: Some(_), .. }),
        "an opened container resolves its segment spans"
    );
    assert!(
        matches!(thumbnail, ReadTarget::Container { span: Some(_), .. }),
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
            read_target(&segment, "mp4", false).expect("segment"),
            vec![0xB1; 512]
        );
        assert_eq!(
            read_target(&thumbnail, "jpg", true).expect("thumbnail"),
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
        read_target(&unresolved, "mp4", false).expect("index path"),
        vec![0xB1; 512]
    );
    assert_eq!(open_for_reading_calls() - before, 1);

    let mut bytes = fs::read(&path).expect("read");
    bytes[..super::super::container::HEADER_LEN as usize].fill(0);
    fs::write(&path, &bytes).expect("wipe the header");
    assert_eq!(
        read_target(&segment, "mp4", false).expect("a span read needs no header"),
        vec![0xB1; 512]
    );
    assert!(
        read_target(&unresolved, "mp4", false).is_err(),
        "the checkpoint path does need it"
    );
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
    let ReadTarget::Container {
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
            read_target(&with_span(&segment, Some(span)), "mp4", false)
                .unwrap_or_else(|error| panic!("{label}: falls back, got {error}")),
            vec![0xB1; 512],
            "{label}: the index path serves the right record"
        );
    }
    let thumbnail = ring.thumbnail_target(0).expect("target");
    let ReadTarget::Container {
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
            "jpg",
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
    assert!(matches!(target, ReadTarget::Container { span: None, .. }));
    assert_eq!(
        read_target(&target, "mp4", false).expect("index path"),
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
        matches!(segment, ReadTarget::Container { span: Some(_), .. }),
        "the span path is the one under test"
    );
    assert!(
        read_target(&segment, "mp4", false).is_err(),
        "a span whose bytes rotted is not served"
    );
    assert!(
        read_target(&with_span(&segment, None), "mp4", false).is_err(),
        "nor does the index path serve them"
    );
    assert_eq!(
        read_target(&control, "mp4", false).expect("segment 0 is untouched"),
        vec![0xA0; 512]
    );
}

/// A directory session has no container to reopen: its target is a path and
/// the read never touches `open_for_reading`.
#[test]
fn a_directory_session_reads_by_path_without_a_container() {
    let root = container_root("span-directory");
    let session_id = "capture-0000000000000000000000000000006a";
    let directory = root.join(session_id);
    fs::create_dir_all(&directory).expect("dir");
    let mut ring =
        RingBuffer::create(directory.clone(), session_id.into(), 15, None).expect("ring");
    fs::write(directory.join("segment-0.mp4"), [0xE5; 128]).expect("segment");
    ring.add(SegmentRecord {
        index: 0,
        path: "segment-0.mp4".into(),
        start_100ns: 0,
        end_100ns: 20_000_000,
        bytes: 128,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    })
    .expect("add");
    let target = ring.segment_target(0).expect("target");
    assert!(matches!(target, ReadTarget::Directory { .. }));
    let before = open_for_reading_calls();
    assert_eq!(
        read_target(&target, "mp4", false).expect("reads"),
        vec![0xE5; 128]
    );
    assert_eq!(open_for_reading_calls() - before, 0);
}
