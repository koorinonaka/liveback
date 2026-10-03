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

use std::{
    fs, io,
    path::{Component, Path, PathBuf},
};

use super::SessionManifest;

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

/// `(partial, final)` for a segment's thumbnail sidecar (task066), same stem
/// and the same `.partial` → rename discipline as the mp4.
pub fn thumbnail_paths(directory: &Path, index: u64) -> (PathBuf, PathBuf) {
    let stem = segment_stem(index);
    (
        directory.join(format!("{stem}.partial.jpg")),
        directory.join(format!("{stem}.jpg")),
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

/// Whether a directory entry is a segment still being written. Recovery and
/// the history summary both ask this, and both used to spell it out.
pub fn is_partial_segment(file_name: &str) -> bool {
    file_name.ends_with(".partial.mp4")
}

/// One session's directory.
///
/// Cheap to make and cheap to clone: it is a path and nothing else, holding no
/// handle and no lock. Constructing one neither creates nor validates the
/// directory -- callers that need it to exist say so themselves, as they did
/// before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionStore {
    root: PathBuf,
}

impl SessionStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// The file whose presence means "a capture was writing here and never
    /// finished" -- what makes a session a recovery candidate.
    pub fn recording_marker_path(&self) -> PathBuf {
        self.root.join("recording.marker")
    }

    /// Makes the directory and plants the "a capture is writing here" marker.
    /// The pair a fresh session starts with.
    pub fn begin_recording(&self) -> io::Result<()> {
        fs::create_dir_all(&self.root)?;
        fs::write(self.recording_marker_path(), b"recording")
    }

    pub fn has_manifest(&self) -> bool {
        self.manifest_path().exists()
    }

    pub fn has_recording_marker(&self) -> bool {
        self.recording_marker_path().exists()
    }

    /// Drops the "still open" signal. Ignoring the error is deliberate and
    /// pre-existing: every caller reaches here having already decided nothing
    /// will write to this session again, and a marker that was never created
    /// is the normal case, not a failure.
    pub fn clear_recording_marker(&self) {
        let _ = fs::remove_file(self.recording_marker_path());
    }

    pub fn read_manifest(&self) -> io::Result<SessionManifest> {
        serde_json::from_slice(&fs::read(self.manifest_path())?).map_err(io::Error::other)
    }

    /// The same read, but `None` for *any* reason it did not work -- a missing
    /// file, unreadable bytes, or JSON that will not parse. The session list
    /// and the thumbnail fallback both want "is there a usable manifest here"
    /// rather than which of those went wrong.
    pub fn read_manifest_opt(&self) -> Option<SessionManifest> {
        let path = self.manifest_path();
        path.is_file()
            .then(|| fs::read(&path).ok())
            .flatten()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
    }

    /// Write through a temporary and rename onto the real name, so a crash
    /// mid-write leaves the previous manifest intact rather than a truncated
    /// one. Both former writers did exactly this; now there is one of them.
    pub fn write_manifest(&self, manifest: &SessionManifest) -> io::Result<()> {
        let tmp = self.root.join("manifest.tmp");
        let bytes = serde_json::to_vec_pretty(manifest).map_err(io::Error::other)?;
        fs::write(&tmp, bytes)?;
        fs::rename(tmp, self.manifest_path())
    }

    pub fn segment_paths(&self, index: u64) -> (PathBuf, PathBuf) {
        segment_paths(&self.root, index)
    }

    pub fn thumbnail_paths(&self, index: u64) -> (PathBuf, PathBuf) {
        thumbnail_paths(&self.root, index)
    }

    /// The rename that makes a finished segment visible. Returns the name it
    /// now has. On failure the partial is left exactly where it was -- that is
    /// what quarantine and recovery expect to find.
    pub fn publish_segment(&self, partial: &Path, index: u64) -> io::Result<PathBuf> {
        let (_, final_path) = self.segment_paths(index);
        publish(partial, &final_path)?;
        Ok(final_path)
    }

    pub fn publish_thumbnail(&self, partial: &Path, index: u64) -> io::Result<PathBuf> {
        let (_, final_path) = self.thumbnail_paths(index);
        publish(partial, &final_path)?;
        Ok(final_path)
    }

    /// Where a manifest record's stored path actually lands, once it is
    /// checked to be inside this session.
    ///
    /// Records hold a relative path (task067), so this is a `join` -- but the
    /// manifest is a local JSON file, not something to trust unconditionally,
    /// and `starts_with` is purely lexical: a record carrying `..` resolves
    /// outside `root` while still passing a naive `starts_with(root)`. Hence
    /// the explicit `ParentDir` component check, which is the guard
    /// `read_review_segment`, `read_review_thumbnail` and
    /// `export::plan::validate_segment_paths` each carried a copy of.
    ///
    /// `extension` is the one the caller will accept (`"mp4"`, `"jpg"`); the
    /// file has to exist, because every caller is about to read it.
    pub fn resolve_recorded(&self, relative: &Path, extension: &str) -> Result<PathBuf, String> {
        let path = self.root.join(relative);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| component == Component::ParentDir)
            || path.extension().and_then(|value| value.to_str()) != Some(extension)
            || !path.starts_with(&self.root)
            || !path.is_file()
        {
            return Err(format!("{extension} path is invalid"));
        }
        Ok(path)
    }

    /// Deletes one file a manifest record points at. Absent is not an error:
    /// a thumbnail whose write failed, or a segment from before task066, has
    /// nothing to delete -- prune has always treated it that way.
    pub fn remove_recorded(&self, relative: &Path) {
        let _ = fs::remove_file(self.root.join(relative));
    }

    /// The whole session, directory and all.
    pub fn remove_session(&self) -> io::Result<()> {
        fs::remove_dir_all(&self.root)
    }

    /// What this session occupies, summed over the files directly in its
    /// directory.
    ///
    /// Still `metadata().len()` -- the logical size, not the allocated one.
    /// Reconciling those is the plan's R3 and belongs to Phase 3; moving the
    /// sum here without changing it is what makes that a one-line change.
    /// Unreadable directory reads as 0, as the history worker's version did.
    pub fn session_bytes(&self) -> u64 {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return 0;
        };
        entries
            .flatten()
            .filter_map(|entry| entry.metadata().ok())
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
            .sum()
    }

    /// What the finished segments occupy, when there is no manifest to ask.
    ///
    /// Deliberately *not* `session_bytes`: this is the fallback a session
    /// without a readable manifest gets, and it counts what the summary also
    /// counts as its `segment_count` -- finished mp4s only, no thumbnails, no
    /// partials. Keeping the two sums separate and named is the point; they
    /// were two open-coded loops that looked interchangeable and are not.
    /// A file whose metadata will not read counts as 0, as it did.
    pub fn finished_segment_bytes(&self) -> u64 {
        self.finished_segments()
            .unwrap_or_default()
            .iter()
            .map(|path| path.metadata().map(|meta| meta.len()).unwrap_or_default())
            .sum()
    }

    /// Every finished segment file in the directory, in recording order.
    ///
    /// Partials are excluded: they are what a crash left behind, and recovery
    /// reconstructs from the ones that completed. The sort is lexicographic on
    /// the whole path, which is also chronological because `segment_stem` is
    /// fixed width and zero padded -- `read_dir` guarantees no order of its
    /// own, and recovery accumulates durations in sequence.
    pub fn finished_segments(&self) -> io::Result<Vec<PathBuf>> {
        let mut paths = fs::read_dir(&self.root)?
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().is_some_and(|ext| ext == "mp4")
                    && !is_partial_segment(&entry.file_name().to_string_lossy())
            })
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    /// Every `.partial.mp4` still sitting in this session.
    ///
    /// These are what a crash left mid-write. Nothing ever reads one: recovery
    /// rebuilds from `finished_segments`, which excludes them, and the README
    /// says outright that a partial is not used for export.
    pub fn partial_segments(&self) -> Vec<PathBuf> {
        let Ok(entries) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| is_partial_segment(&entry.file_name().to_string_lossy()))
            .map(|entry| entry.path())
            .collect()
    }

    /// Deletes them, and says how many went (task470).
    ///
    /// Not fatal if one will not go -- a partial can still be held open by the
    /// process that was writing it, and a session that recovered fine except
    /// for some debris has still recovered. Same tolerance `remove_recorded`
    /// has for an absent file.
    pub fn remove_partial_segments(&self) -> usize {
        self.partial_segments()
            .iter()
            .filter(|path| fs::remove_file(path).is_ok())
            .count()
    }

    /// Whether a crash left work behind here: either a segment still marked
    /// partial, or a quarantine directory a previous recovery made.
    pub fn has_partial_work(&self) -> bool {
        if self.root.join("quarantine").exists() {
            return true;
        }
        let Ok(entries) = fs::read_dir(&self.root) else {
            return false;
        };
        entries
            .flatten()
            .any(|entry| is_partial_segment(&entry.file_name().to_string_lossy()))
    }
}
