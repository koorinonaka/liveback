use super::sessions::summarize_session_dir;
use super::*;
use crate::encoder;
// The fixtures here build session directories by hand, which is the exception
// the task400 acceptance criteria allow. The module under test no longer
// touches `std::fs` itself -- `store` does -- so the import lives here.
use std::fs;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp directory that deletes itself when its binding goes out of scope
/// (task3120). Explicit cleanup at the end of a test is not enough: an
/// assertion failure unwinds past it, so exactly the runs worth keeping small
/// -- the failing ones -- were the ones leaving `rs-ring-*` behind. `Drop` runs
/// on the unwind too.
///
/// `root` is what gets removed; `path` is what the test sees. They differ only
/// for [`TmpDir::child`], so that a named subdirectory still takes its parent
/// with it. Deliberately not `Clone`: two guards over one directory would each
/// try to remove it.
struct TmpDir {
    root: PathBuf,
    path: PathBuf,
}

impl TmpDir {
    /// A named child of the guarded root, for fixtures that want the name in
    /// the path. The guard still removes the whole root.
    fn child(mut self, name: &str) -> Self {
        self.path = self.root.join(name);
        self
    }
}

impl Deref for TmpDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        // Never panic here. Recovery and reclaim tests legitimately end with
        // the directory already gone, and a `.lvb` a writer still holds open
        // (no `FILE_SHARE_DELETE`, see `container.rs`) makes the removal a
        // sharing violation. Neither is a reason to turn a green test red.
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn tmp() -> TmpDir {
    // The sequence number, not the clock, is what makes this unique (task3230):
    // two parallel test threads in one process can read the same 100ns tick and
    // end up sharing -- and then removing -- one directory.
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let p = std::env::temp_dir().join(format!("rs-ring-{}-{seq}-{id}", std::process::id()));
    let _ = fs::remove_dir_all(&p);
    TmpDir {
        root: p.clone(),
        path: p,
    }
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
