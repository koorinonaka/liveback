//! `ring_buffer::store` (task400). These pin what other modules used to know
//! for themselves: the segment names and the partial→final boundary.

use super::*;
use crate::ring_buffer::store;

/// Dropping the guard removes the whole `rs-ring-*` root, so callers bind it
/// rather than discard it.
fn dir_at(name: &str) -> TmpDir {
    let root = tmp().child(name);
    fs::create_dir_all(&root).unwrap();
    root
}

/// The names are an on-disk contract: sessions already recorded have to keep
/// resolving, and recovery relies on the zero padding being wide enough that
/// sorting the filenames sorts the segments. 20 digits is what every existing
/// buffer directory holds.
#[test]
fn segment_names_are_the_ones_already_on_disk() {
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

    // Zero padded wide enough that lexicographic order is numeric order.
    let (_, tenth) = store::segment_paths(root, 10);
    let (_, ninth) = store::segment_paths(root, 9);
    assert!(ninth < tenth);
}

#[test]
fn publishing_moves_the_partial_onto_the_real_name() {
    let root = dir_at("publish");
    let (partial, final_path) = store::segment_paths(&root, 3);
    fs::write(&partial, b"segment").unwrap();

    store::publish(&partial, &final_path).unwrap();

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
    let root = dir_at("publish-fail");
    let (partial, final_path) = store::segment_paths(&root, 1);

    assert!(store::publish(&partial, &final_path).is_err());
    assert!(!final_path.exists());

    // And with the partial present but the destination unwritable-by-absence
    // of its directory, the source still survives.
    let (_, orphan) = store::segment_paths(&root.join("gone"), 1);
    fs::write(&partial, b"mp4").unwrap();
    assert!(store::publish(&partial, &orphan).is_err());
    assert!(
        partial.exists(),
        "the partial must survive a failed publish"
    );
}
