//! Every read and write that happens inside one session directory (task400).
//!
//! Before this, the layout of a session directory was knowledge spread across
//! six modules: `encoder` built the segment stems, `capture::worker::setup`
//! built the thumbnail ones, `ring_buffer` and `ring_buffer::sessions` each
//! wrote `manifest.json` through their own tmp-and-rename, `capture::review`
//! and `export::plan` each re-derived the traversal guard, and the history
//! worker summed `metadata().len()` by hand. Nothing was wrong with any of
//! them individually -- the problem is that a session is about to stop being a
//! directory at all (`.agents/docs/ring-buffer-single-container-plan.md`), and
//! six places that each know what a segment file is called are six places that
//! have to change together.
//!
//! So this module owns the names, the atomic boundaries and the guards, and
//! the callers ask for them. Phase 0 of the plan: **behaviour is unchanged**,
//! deliberately, down to which errors are ignored.
//!
//! The plan's §6 calls for a trait here. There is one implementation, so this
//! is a concrete type instead -- an interface with a single implementor buys
//! nothing but diff. The container backend in Phase 2 is the second one, and
//! the seam it needs is this module's surface either way.
//!
//! Once every session was a container, the directory's manifest, marker and
//! sidecar handling went (t261025-d118), and with it the traversal guard
//! export checked a manifest's segment paths with (t261029-b266). What is left
//! is the encoder's segment names and its partial→final rename, and the atomic
//! write the settings and update files share.

use std::{
    fs, io,
    path::{Path, PathBuf},
};

/// The stem both a segment and its thumbnail sidecar are named from, so the
/// two stay trivially associable on disk (task066). Fixed-width and zero
/// padded, which is also what lets recovery sort a directory listing
/// lexicographically and get chronological order (`sessions.rs`).
fn segment_stem(index: u64) -> String {
    format!("segment-{index:020}")
}

/// `(partial, final)` for a segment's mp4. Written to the first and renamed
/// onto the second, so a reader never sees a half-written file.
pub fn segment_paths(directory: &Path, index: u64) -> (PathBuf, PathBuf) {
    let stem = segment_stem(index);
    (
        directory.join(format!("{stem}.partial.mp4")),
        directory.join(format!("{stem}.mp4")),
    )
}

/// The rename that makes a finished file visible, and the only place it
/// happens. Writers build `.partial.*` and land it here, so a reader that
/// sees the real name is guaranteed a complete file. On failure the partial
/// is left exactly where it was, which is what recovery and quarantine expect
/// to find.
pub fn publish(partial: &Path, final_path: &Path) -> io::Result<()> {
    fs::rename(partial, final_path)
}

/// Writes `path` all at once or not at all (t261025-00ad): `write` fills
/// `temp`, which is flushed to disk and only then [`publish`]ed over `path`.
/// On any failure `temp` is removed and `path` is left as it was -- a crash
/// mid-write cannot leave an empty or half file where the old one stood.
/// `temp` belongs next to `path`, so the rename stays on one volume; its name
/// is the caller's, which keeps the leftovers older builds looked for.
pub(crate) fn write_atomic(
    path: &Path,
    temp: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let written = (|| {
        let mut file = fs::File::create(temp)?;
        write(&mut file)?;
        file.sync_all()?;
        drop(file);
        publish(temp, path)
    })();
    if written.is_err() {
        let _ = fs::remove_file(temp);
    }
    written
}
