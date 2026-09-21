//! `ring_buffer::store` (task400). These pin the parts of the session
//! directory's layout that other modules used to know for themselves: the
//! names, the partial→final boundary, the traversal guard and what each of
//! the two byte sums counts.

use super::*;
use crate::ring_buffer::store::{self, SessionStore};

/// The guard comes back with the store: dropping it removes the whole
/// `rs-ring-*` root, so callers bind it (`let (_root, store) = ...`) rather
/// than discard it.
fn store_at(name: &str) -> (TmpDir, SessionStore) {
    let root = tmp().child(name);
    fs::create_dir_all(&root).unwrap();
    let store = SessionStore::new(root.to_path_buf());
    (root, store)
}

/// The names are an on-disk contract: sessions already recorded have to keep
/// resolving, and recovery relies on the zero padding being wide enough that
/// sorting the filenames sorts the segments. 20 digits is what every existing
/// buffer directory holds.
#[test]
fn segment_and_thumbnail_names_are_the_ones_already_on_disk() {
    let root = Path::new("buffer");
    let (partial, final_path) = store::segment_paths(root, 7);
    assert_eq!(
        partial.file_name().unwrap(),
        "segment-00000000000000000007.partial.mp4"
    );
    assert_eq!(
        final_path.file_name().unwrap(),
        "segment-00000000000000000007.mp4"
    );

    let (thumb_partial, thumb_final) = store::thumbnail_paths(root, 7);
    assert_eq!(
        thumb_partial.file_name().unwrap(),
        "segment-00000000000000000007.partial.jpg"
    );
    assert_eq!(
        thumb_final.file_name().unwrap(),
        "segment-00000000000000000007.jpg"
    );

    // Same stem for both, which is what lets recovery find a sidecar by
    // swapping the extension.
    assert_eq!(
        final_path.file_stem().unwrap(),
        thumb_final.file_stem().unwrap()
    );
    // Zero padded wide enough that lexicographic order is numeric order.
    let (_, tenth) = store::segment_paths(root, 10);
    let (_, ninth) = store::segment_paths(root, 9);
    assert!(ninth < tenth);
}

#[test]
fn publishing_moves_the_partial_onto_the_real_name() {
    let (_root, store) = store_at("publish");
    let (partial, final_path) = store.segment_paths(3);
    fs::write(&partial, b"segment").unwrap();

    let published = store.publish_segment(&partial, 3).unwrap();

    assert_eq!(published, final_path);
    assert!(
        !partial.exists(),
        "the partial must not survive the publish"
    );
    assert_eq!(fs::read(&final_path).unwrap(), b"segment");
}

/// A publish that cannot happen must leave the partial where it is: that
/// debris is exactly what recovery and quarantine look for. Here the rename
/// fails because there is nothing to rename.
#[test]
fn a_failed_publish_leaves_the_partial_alone() {
    let (_root, store) = store_at("publish-fail");
    let (partial, final_path) = store.thumbnail_paths(1);

    assert!(store.publish_thumbnail(&partial, 1).is_err());
    assert!(!final_path.exists());

    // And with the partial present but the destination unwritable-by-absence
    // of its directory, the source still survives.
    let orphan = SessionStore::new(store.root().join("gone"));
    fs::write(&partial, b"jpeg").unwrap();
    assert!(orphan.publish_thumbnail(&partial, 1).is_err());
    assert!(
        partial.exists(),
        "the partial must survive a failed publish"
    );
}

/// Regression for the traversal guard the three readers used to keep a copy
/// of each. `starts_with` is lexical, so a manifest record carrying `..` can
/// resolve outside the session while still passing it -- the explicit
/// `ParentDir` check is what stops that.
#[test]
fn resolving_rejects_anything_that_leaves_the_session() {
    let (_root, store) = store_at("resolve");
    let (_, inside) = store.segment_paths(0);
    fs::write(&inside, b"segment").unwrap();

    // The ordinary case: a session-relative record (task067).
    let name = Path::new(inside.file_name().unwrap());
    assert_eq!(store.resolve_recorded(name, "mp4").unwrap(), inside);

    // An escaping record, even though the file it names really exists.
    let outside = store.root().parent().unwrap().join("outside.mp4");
    fs::write(&outside, b"not mine").unwrap();
    assert!(store
        .resolve_recorded(Path::new("..\\outside.mp4"), "mp4")
        .is_err());
    assert!(store
        .resolve_recorded(Path::new("../outside.mp4"), "mp4")
        .is_err());
    // An absolute path outside the session is refused by `starts_with`.
    assert!(store.resolve_recorded(&outside, "mp4").is_err());

    // The extension is part of the guard: the segment route must not be
    // talked into serving a sidecar, or vice versa.
    assert!(store.resolve_recorded(name, "jpg").is_err());

    // And a record naming a file that is not there is refused rather than
    // handed back for a read that would fail anyway.
    assert!(store
        .resolve_recorded(Path::new("segment-00000000000000000009.mp4"), "mp4")
        .is_err());

    let _ = fs::remove_file(&outside);
}

/// The two sums are not interchangeable, which is the reason they are two
/// named functions rather than one. `session_bytes` is what the history
/// reports as freed, so it counts everything in the directory;
/// `finished_segment_bytes` backs a summary's `segment_count`, so it counts
/// only the segments that count -- no thumbnails, no partials.
#[test]
fn the_two_byte_sums_count_different_things() {
    let (_root, store) = store_at("bytes");
    let (partial, final_path) = store.segment_paths(0);
    fs::write(&final_path, vec![0u8; 100]).unwrap();
    fs::write(&partial, vec![0u8; 7]).unwrap();
    let (_, thumbnail) = store.thumbnail_paths(0);
    fs::write(&thumbnail, vec![0u8; 3]).unwrap();

    assert_eq!(store.finished_segment_bytes(), 100);
    assert_eq!(store.session_bytes(), 110);
    assert_eq!(store.finished_segments().unwrap(), vec![final_path]);
    assert!(store.has_partial_work());

    // A directory that does not exist reads as empty rather than failing --
    // the history worker's version behaved that way and its caller has no
    // error path.
    let missing = SessionStore::new(store.root().join("nope"));
    assert_eq!(missing.session_bytes(), 0);
    assert_eq!(missing.finished_segment_bytes(), 0);
}

#[test]
fn the_manifest_round_trips_and_a_missing_one_is_not_a_panic() {
    let (_root, store) = store_at("manifest");
    assert!(!store.has_manifest());
    assert!(store.read_manifest_opt().is_none());
    assert!(store.read_manifest().is_err());

    let mut manifest = SessionManifest {
        audio_tracks: Vec::new(),
        version: MANIFEST_VERSION,
        session_id: "capture-store".into(),
        closed: false,
        retention_minutes: DEFAULT_RETENTION_MINUTES,
        segments: vec![seg(0)],
        gaps: vec![],
        target_title: None,
        target_executable: None,
        target_executable_path: None,
        markers: vec![],
        note: None,
        protected: false,
    };
    store.write_manifest(&manifest).unwrap();

    assert!(store.has_manifest());
    assert_eq!(store.read_manifest().unwrap(), manifest);
    // The tmp file the write goes through must not be left behind.
    assert!(!store.root().join("manifest.tmp").exists());

    manifest.closed = true;
    store.write_manifest(&manifest).unwrap();
    assert!(store.read_manifest_opt().unwrap().closed);
}

#[test]
fn the_recording_marker_is_planted_and_cleared() {
    // The guard is its own binding: `tmp().join(..)` would drop it at the end
    // of the statement, leaving the `rs-ring-*` root behind once
    // `begin_recording` creates it.
    let root = tmp().child("marker");
    let store = SessionStore::new(root.to_path_buf());
    assert!(!store.has_recording_marker());

    store.begin_recording().unwrap();
    assert!(store.root().is_dir(), "begin_recording makes the directory");
    assert!(store.has_recording_marker());

    store.clear_recording_marker();
    assert!(!store.has_recording_marker());
    // Clearing one that is already gone is the normal case, not a failure.
    store.clear_recording_marker();
}
