//! Pure UI state for the slint bin. No slint types anywhere below this module:
//! it compiles in the default build, so plain `cargo test` covers it, and the
//! `.slint` side stays a view that only maps these values onto elements.
//! (There is no `slint-ui` feature -- an earlier version of this comment named
//! one. The crate's features are `insight` and `control`; the slint bin is a
//! target, not a feature.)
//!
//! One submodule per screen, matching the task that ported it.

pub mod auto_capture;
pub mod clip_player;
pub mod clips;
// The four operations a verification sweep cannot drive without the GUI
// (task3750). Parsing and judgement only -- the wiring that acts on them is
// feature-gated in the bin, which is why this half is not.
pub mod control;
pub mod export;
/// What a failed export says. Separate from `export` because the producers are
/// `crate::export`'s -- including one in another process -- while the reader is
/// the review screen.
pub mod export_failure;
pub mod hotkeys;
pub mod lifecycle;
pub mod locale;
pub mod playback;
pub mod sessions;
pub mod settings;
pub mod shell;
pub mod targets;
pub mod timeline;
pub mod toast;
