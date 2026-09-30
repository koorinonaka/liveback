//! The one thing a second launch has to say to the first (task580): the path
//! of the `.lvb` it was started for.
//!
//! Single-instance is a named mutex plus an auto-reset event, and an event
//! carries no payload -- it can only say "come forward". So the path travels
//! through a file beside `settings.json`, written before the event is signalled
//! and taken (read *and* removed) by the running instance on its next drain
//! tick. Last write wins: two double-clicks in a row are one raise and one
//! load, which is what the user sees anyway.

use std::path::{Path, PathBuf};

/// Beside `settings.json`, so it lands in a folder the app already owns and
/// already creates.
const OPEN_REQUEST_FILE: &str = "open-request.txt";

fn open_request_path(dir: &Path) -> PathBuf {
    dir.join(OPEN_REQUEST_FILE)
}

/// Leaves `path` for the running instance to pick up. The caller signals the
/// activation event *after* this returns, so a request never arrives before the
/// file it names.
pub fn write_open_request(dir: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(open_request_path(dir), path.to_string_lossy().as_bytes())
}

/// The pending request, if any -- read and removed in one go, so a stale file
/// from a crashed launch can only ever be acted on once.
pub fn take_open_request(dir: &Path) -> Option<PathBuf> {
    let path = open_request_path(dir);
    let contents = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    let contents = contents?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// Beside `open-request.txt` and deliberately *not* it (task3750): a control
/// command must not be mistaken for a double-clicked `.lvb`, and the two are
/// drained differently -- the open request only when the running instance was
/// told to come forward, this one on every tick, because raising the window
/// for each command would wreck the very screen state a sweep is inspecting.
const CONTROL_REQUEST_FILE: &str = "control-request.txt";

/// Where a control request lands. Public so the sending half can print the
/// absolute path it wrote: sender and receiver have to be looking at the same
/// filesystem view, and a sandboxed shell whose `%APPDATA%` is shadowed cannot
/// tell from the inside which copy it hit (task3720).
pub fn control_request_path(dir: &Path) -> PathBuf {
    dir.join(CONTROL_REQUEST_FILE)
}

/// Leaves one command line for the running instance. Last write wins and no
/// event is signalled -- the receiver is already polling, and signalling is
/// what raises the window.
pub fn write_control_request(dir: &Path, command: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(control_request_path(dir), command.as_bytes())
}

/// The pending control command, read and removed in one go -- same shape as
/// [`take_open_request`], so a command left behind by a crash is acted on at
/// most once.
pub fn take_control_request(dir: &Path) -> Option<String> {
    let path = control_request_path(dir);
    let contents = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    let trimmed = contents?.trim().to_owned();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The folder the two halves agree on: the one holding `settings.json`.
pub fn request_directory() -> Option<PathBuf> {
    crate::settings::default_path()?
        .parent()
        .map(Path::to_path_buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_open_request_survives_the_trip_and_is_taken_exactly_once() {
        let dir = std::env::temp_dir().join(format!("livia-handoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            take_open_request(&dir).is_none(),
            "no request means nothing to open, not an empty path"
        );

        let wanted = Path::new(r"D:\somewhere else\capture-0000.lvb");
        write_open_request(&dir, wanted).expect("write");
        assert_eq!(take_open_request(&dir).as_deref(), Some(wanted));
        assert!(
            take_open_request(&dir).is_none(),
            "taking it removes it, so a raise-only launch never re-opens the last file"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_control_request_survives_the_trip_and_is_taken_exactly_once() {
        let dir = std::env::temp_dir().join(format!("livia-control-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);

        assert!(
            take_control_request(&dir).is_none(),
            "no request means nothing to do, not an empty command"
        );

        write_control_request(&dir, "range 12 34.5").expect("write");
        assert_eq!(take_control_request(&dir).as_deref(), Some("range 12 34.5"));
        assert!(
            take_control_request(&dir).is_none(),
            "taking it removes it, so one command is never run twice"
        );

        // The two requests share a folder and must not share a file: a control
        // command that arrived as an open request would raise the window, and
        // a double-clicked `.lvb` read as a command would be a refusal line.
        let wanted = Path::new(r"D:\somewhere else\capture-0000.lvb");
        write_open_request(&dir, wanted).expect("write");
        write_control_request(&dir, "clip かぶらないこと").expect("write");
        assert_eq!(take_open_request(&dir).as_deref(), Some(wanted));
        assert_eq!(
            take_control_request(&dir).as_deref(),
            Some("clip かぶらないこと"),
            "taking the open request must not consume the control one"
        );
        assert_ne!(
            control_request_path(&dir),
            open_request_path(&dir),
            "two requests, two files"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
