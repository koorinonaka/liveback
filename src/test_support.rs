//! Test fixtures more than one module's tests need (t261025-5982). Before this
//! each suite kept its own copy, and the copies had drifted apart in small ways
//! (one guard knew `child()`, one created the directory, one did not).
//!
//! `ring_buffer/container/tests.rs` keeps its own `TmpDir` on purpose:
//! `crates/livia-thumb` compiles `container.rs` -- and with it that test module
//! -- through `#[path]`, where `crate::test_support` does not exist.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A temp directory that deletes itself when its binding goes out of scope
/// (task3120). Explicit cleanup at the end of a test is not enough: an
/// assertion failure unwinds past it, so exactly the runs worth keeping small
/// -- the failing ones -- were the ones leaving fixtures behind. `Drop` runs on
/// the unwind too.
///
/// `root` is what gets removed; `path` is what the test sees. They differ only
/// for [`TmpDir::child`], so that a named subdirectory still takes its parent
/// with it. Deliberately not `Clone`: two guards over one directory would each
/// try to remove it.
pub(crate) struct TmpDir {
    root: PathBuf,
    path: PathBuf,
    kept: bool,
}

impl TmpDir {
    /// A fresh, unique path under the system temp directory, named after
    /// `prefix`. Nothing is created there: several fixtures (recovery, reclaim)
    /// need the directory not to exist yet. Chain [`TmpDir::create`] for one
    /// that does.
    pub(crate) fn new(prefix: &str) -> Self {
        // The sequence number, not the clock, is what makes this unique
        // (task3230): two parallel test threads in one process can read the
        // same 100ns tick and end up sharing -- and then removing -- one
        // directory.
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let root =
            std::env::temp_dir().join(format!("{prefix}-{}-{seq}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        Self {
            path: root.clone(),
            root,
            kept: false,
        }
    }

    /// Creates the directory the test sees (and its parents).
    pub(crate) fn create(self) -> Self {
        std::fs::create_dir_all(&self.path).expect("temp dir");
        self
    }

    /// A named child of the guarded root, for fixtures that want the name in
    /// the path. The guard still removes the whole root.
    pub(crate) fn child(mut self, name: &str) -> Self {
        self.path = self.root.join(name);
        self
    }

    /// Leaves the files on disk when `LIVIA_KEEP_ACCEPTANCE_OUTPUT` is set --
    /// the acceptance tests' switch for inspecting what they wrote. Decided
    /// here, at construction, so a test that panics half way still keeps its
    /// output: those are the runs worth inspecting.
    pub(crate) fn keep_on_request(mut self) -> Self {
        self.kept = std::env::var_os("LIVIA_KEEP_ACCEPTANCE_OUTPUT").is_some();
        self
    }
}

impl std::ops::Deref for TmpDir {
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
        if self.kept {
            eprintln!("kept acceptance output: {}", self.path.display());
            return;
        }
        // Never panic here. Recovery and reclaim tests legitimately end with
        // the directory already gone, and a `.lvb` a writer still holds open
        // (no `FILE_SHARE_DELETE`, see `container.rs`) makes the removal a
        // sharing violation. Neither is a reason to turn a green test red.
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Owned copies of a list of literals -- the expected ids, names and stamps
/// several `ui_state` suites compare against.
pub(crate) fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

/// Hex text to bytes. Spaces and newlines are ignored, so a fixture can be
/// wrapped across lines.
pub(crate) fn hex_bytes(hex: &str) -> Vec<u8> {
    let hex = hex.replace([' ', '\n'], "");
    (0..hex.len() / 2)
        .map(|index| u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).unwrap())
        .collect()
}

/// COM on this thread, multithreaded apartment -- what every WASAPI and Media
/// Foundation test needs first.
pub(crate) fn com_init_mta() {
    unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
        .ok()
        .expect("COM");
    }
}
