// `capture` is public because the UI binary calls `CaptureController` directly
// instead of through IPC command wrappers (task124).
pub mod capture;
// Comments read from and written into an exported mp4's own metadata (task2960).
pub mod clips;
pub mod encoder;
pub mod events;
// Public because the UI binary drives `ExportController` directly (task129), the
// same way it uses `capture`.
pub mod export;
// The settings screen resolves the default export directory through this
// (task126).
pub use export::resolve_export_directory;
// Native review playback (task128).
pub mod playback;
pub mod ring_buffer;
pub mod settings;
pub mod update;
// The path a second launch hands to the running instance (task580).
pub mod handoff;
pub mod ui_state;

// Public since task131: the file logger used to be installed from Tauri's
// `setup()`, and is now the binary's own first act.
pub mod logging;

// The panic hook and unhandled exception filter, installed right after the
// logger so a crash leaves a line behind rather than a gap (task750).
pub mod crash;

// Scope timing, `--insight` runs only (task205). Gated at the module so a
// build without the feature -- which is every build the installer makes --
// does not compile it at all. `cargo test --features insight` is what runs its
// tests; the default `cargo test` cannot see them.
#[cfg(feature = "insight")]
pub mod insight;

/// Times the enclosing block and files it under `name`, for runs started with
/// `--insight`. Expands to nothing without the `insight` feature, so a call
/// costs a shipped build neither bytes nor cycles.
///
/// The guard it binds is hygienic -- invisible to the surrounding code -- but
/// still drops at the end of the enclosing block, which is what ends the
/// measurement. Put it on the first line of the scope being measured.
#[cfg(feature = "insight")]
#[macro_export]
macro_rules! insight_scope {
    ($name:literal) => {
        let _insight_scope =
            ::tracing::debug_span!(target: $crate::insight::TARGET, $name).entered();
    };
}

#[cfg(not(feature = "insight"))]
#[macro_export]
macro_rules! insight_scope {
    ($name:literal) => {};
}

/// Reports how many decoded images one UI owner is holding, for the
/// `--insight` heartbeat's memory line (task212). Same shape as
/// [`insight_scope!`]: nothing at all without the feature, so the call site
/// reads the same in both builds.
#[cfg(feature = "insight")]
#[macro_export]
macro_rules! insight_images {
    ($owner:ident, $count:expr) => {
        $crate::insight::set_images($crate::insight::Images::$owner, $count);
    };
}

#[cfg(not(feature = "insight"))]
#[macro_export]
macro_rules! insight_images {
    ($owner:ident, $count:expr) => {};
}

/// How many colours the review panel cycles markers through (round5 §4-B).
/// It lives here rather than in `ui_state` because `ring_buffer` is what does
/// the assignment, and the engine must not depend on the UI layer for it --
/// only the *count* is shared; the colours themselves stay in the UI.
pub const MARKER_PALETTE_LEN: usize = 6;

pub const DEFAULT_HOTKEY: &str = "Ctrl+Shift+R";
pub const TRAY_TOOLTIP_IDLE: &str = "Liveback - 待機中";
pub const TRAY_TOOLTIP_RECORDING: &str = "Liveback - 録画中";

pub fn run_export_helper_if_requested() -> bool {
    export::run_helper_if_requested()
}

/// `liveback.exe --liveback-inspect <path.lvb> [--full]`: prints what a session
/// container says about itself, then exits (task401, plan R13; two modes since
/// task2890).
///
/// A format whose whole job is surviving a crash needs a way to look at one
/// that did -- and the safety rule "check the target before deleting it" needs
/// a way to put a session id next to its title. Those are different readings:
///
/// - Default is `container::inspect_summary`: the 4 KiB header plus the one
///   checkpoint record a slot names. It answers title / note / protected /
///   segments / closed on a 26 GB container in milliseconds, and when the
///   checkpoint is unusable it says so rather than quietly reading 26 GB.
/// - `--full` is the original: the whole file into memory, then
///   `container::dump`'s record-by-record scan. That is the damaged-file
///   reading, where a scan that stops early is output rather than an error.
///
/// Both take bytes or a path and return a string, so both are testable without
/// a process; this is only the arguments and the file access.
pub fn run_container_inspect_if_requested() -> bool {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    let Some(index) = args.iter().position(|arg| arg == "--liveback-inspect") else {
        return false;
    };
    let Some(path) = args.get(index + 1) else {
        eprintln!("--liveback-inspect needs a path to a .lvb file");
        return true;
    };
    let display = std::path::Path::new(path).display();
    if args.iter().any(|arg| arg == "--full") {
        match std::fs::read(path) {
            Ok(bytes) => print!("{}", ring_buffer::container::dump(&bytes)),
            Err(error) => eprintln!("{display}: {error}"),
        }
    } else {
        match ring_buffer::container::inspect_summary(std::path::Path::new(path)) {
            Ok(summary) => print!("{summary}"),
            Err(error) => eprintln!("{display}: {error}; rerun with --full to scan"),
        }
    }
    true
}

#[cfg(test)]
mod tests;
