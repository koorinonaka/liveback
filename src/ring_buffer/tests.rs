use super::*;
// The fixtures here build files by hand, which is the exception the task400
// acceptance criteria allow. The module under test no longer touches `std::fs`
// itself -- `store` does -- so the import lives here.
use crate::test_support::TmpDir;
use std::fs;

/// The guarded temp directory every `ring_buffer` fixture builds in.
fn tmp() -> TmpDir {
    TmpDir::new("rs-ring")
}
/// A closed, readable one-segment session with nothing else set -- the base
/// the session-list fixtures here and in `ui_state::sessions` override.
pub(crate) fn summary(session_id: &str) -> SessionSummary {
    SessionSummary {
        session_id: session_id.into(),
        start_100ns: None,
        end_100ns: None,
        segment_count: 1,
        total_bytes: 0,
        closed: true,
        recoverable: false,
        manifest_version: MANIFEST_VERSION,
        readable: true,
        target_title: None,
        thumbnail_index: None,
        note: None,
        protected: false,
        target_executable: None,
        target_executable_path: None,
        marker_count: 0,
    }
}

/// The live index view of a recording at `root/<session_id>.lvb`, with nothing
/// on disk -- what `CaptureController` builds when a recording starts.
fn ring_at(root: &Path, session_id: &str, retention_minutes: u16) -> RingBuffer {
    RingBuffer::create_container(
        root.join(format!("{session_id}.{CONTAINER_EXTENSION}")),
        session_id.into(),
        retention_minutes,
        None,
        None,
    )
}

/// A closed, empty container on disk at `root/<session_id>.lvb`, opened -- for
/// the edits that append to the file.
fn stored_ring_at(root: &Path, session_id: &str) -> RingBuffer {
    fs::create_dir_all(root).unwrap();
    let path = root.join(format!("{session_id}.{CONTAINER_EXTENSION}"));
    container::ContainerBuilder::new(session_id)
        .checkpoint()
        .closed()
        .build(&path)
        .unwrap();
    RingBuffer::open_container(path).unwrap()
}

fn seg(i: u64) -> SegmentRecord {
    SegmentRecord {
        index: i,
        path: format!("{i}.mp4").into(),
        start_100ns: i as i64 * 20_000_000,
        end_100ns: (i as i64 + 1) * 20_000_000,
        bytes: 1,
        thumbnail_path: None,
        audio_offsets_100ns: vec![0],
    }
}

mod container_sessions;
mod listing;
mod markers;
mod reclaim;
mod recovery;
mod ring;
mod store;

/// Task1250 turned one audio offset into a list. A manifest written before
/// that -- every recording on disk today -- still has to load, and its single
/// number has to arrive as a list of one rather than as nothing.
#[test]
fn a_pre_multi_track_manifest_loads_its_offset_as_one_track() {
    let json = r#"{
        "version": 3,
        "sessionId": "capture-old",
        "closed": true,
        "retentionMinutes": 15,
        "segments": [
            {
                "index": 0,
                "path": "segment-000.mp4",
                "start100ns": 0,
                "end100ns": 20000000,
                "bytes": 4,
                "audioOffset100ns": 213333
            },
            {
                "index": 1,
                "path": "segment-001.mp4",
                "start100ns": 20000000,
                "end100ns": 40000000,
                "bytes": 4
            }
        ],
        "gaps": []
    }"#;
    let manifest: SessionManifest =
        serde_json::from_str(json).expect("an old manifest still loads");
    assert_eq!(manifest.segments[0].audio_offsets_100ns, vec![213_333]);
    // A segment written before priming existed had no key at all, and that
    // still means "no offset", not "no tracks".
    assert!(manifest.segments[1].audio_offsets_100ns.is_empty());
    // Written before thumbnails existed: no `thumbnailPath` key reads as none.
    assert_eq!(manifest.segments[0].thumbnail_path, None);
    assert!(manifest.audio_tracks.is_empty(), "single track by default");

    // And the new shape round-trips.
    let text = serde_json::to_string(&manifest).unwrap();
    assert!(text.contains("audioOffsets100ns"));
    let again: SessionManifest = serde_json::from_str(&text).unwrap();
    assert_eq!(again.segments[0].audio_offsets_100ns, vec![213_333]);
}
