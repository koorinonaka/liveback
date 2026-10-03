use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

use windows::Win32::System::SystemInformation::GetLocalTime;

use crate::ring_buffer;
use crate::ui_state::export_failure as msg;
use crate::ui_state::locale::active as ui_locale;

use super::{ExportFailure, ExportPart, ExportRequest, CAPACITY_MARGIN_BYTES};

pub(super) fn validate_request(request: &ExportRequest) -> Result<(), ExportFailure> {
    if request.session_id.trim().is_empty() {
        return Err(ExportFailure::new(msg::no_session_id(ui_locale())));
    }
    if request.start_100ns < 0 || request.end_100ns <= request.start_100ns {
        return Err(ExportFailure::new(msg::range_invalid(ui_locale())));
    }
    if request.game_name.trim().is_empty() {
        return Err(ExportFailure::new(msg::no_game_name(ui_locale())));
    }
    // Checked on `output_file`, which is the field that actually reaches the
    // filesystem (task1210). It used to check `output_directory`, and when
    // task1050 moved every real caller onto `output_file` the guard kept
    // passing while looking at a field nobody filled -- so deleting that field
    // without moving the check would have removed the guard in silence.
    if request.output_file.as_ref().is_some_and(|path| {
        path.components()
            .any(|component| component == Component::ParentDir)
    }) {
        return Err(ExportFailure::new(msg::output_relative_traversal(
            ui_locale(),
        )));
    }
    Ok(())
}

pub(super) fn plan_parts(
    manifest: &ring_buffer::SessionManifest,
    start_100ns: i64,
    end_100ns: i64,
) -> Result<Vec<ExportPart>, ExportFailure> {
    let mut selected = manifest
        .segments
        .iter()
        .filter(|segment| segment.end_100ns > start_100ns && segment.start_100ns < end_100ns)
        .cloned()
        .collect::<Vec<_>>();
    selected.sort_by_key(|segment| segment.start_100ns);
    if selected.is_empty() {
        return Err(ExportFailure::new(msg::range_has_no_segment(ui_locale())));
    }
    let mut groups: Vec<Vec<ring_buffer::SegmentRecord>> = Vec::new();
    for segment in selected {
        let split = groups
            .last()
            .and_then(|group| group.last())
            .is_some_and(|previous| {
                manifest.gaps.iter().any(|gap| {
                    gap.start_100ns >= previous.end_100ns
                        && gap.end_100ns <= segment.start_100ns
                        && gap.end_100ns > start_100ns
                        && gap.start_100ns < end_100ns
                })
            });
        if split || groups.is_empty() {
            groups.push(Vec::new());
        }
        groups.last_mut().unwrap().push(segment);
    }
    Ok(groups
        .into_iter()
        .map(|segments| ExportPart {
            start_100ns: start_100ns.max(segments.first().unwrap().start_100ns),
            end_100ns: end_100ns.min(segments.last().unwrap().end_100ns),
            segments,
        })
        .collect())
}

/// Validates every segment path that will actually be read for this export.
/// `root` is the session's own directory (`RingBuffer::root`/`CaptureController
/// ::session_root`) — not derived from any segment's own path, since a
/// session-relative `SegmentRecord.path` (Task067) has no parent to derive it
/// from. Callers must pass segment paths already resolved to absolute (see
/// `CaptureController::session_manifest`); this only re-validates them.
pub(super) fn validate_segment_paths(
    parts: &[ExportPart],
    root: &Path,
) -> Result<(), ExportFailure> {
    // The guard itself is `ring_buffer::store`'s now (task400) -- the same one
    // `read_review_segment` and `read_review_thumbnail` use, rather than a
    // third copy of it here. Passing an already-absolute path through
    // `resolve_recorded` is exactly what it did before: joining an absolute
    // path onto any base returns it unchanged, and the checks that follow are
    // the ones this function used to spell out (`..` rejected explicitly,
    // because `starts_with` is lexical and a tampered relative
    // `SegmentRecord.path` could otherwise resolve outside `root`).
    let store = ring_buffer::SessionStore::new(root.to_path_buf());
    for segment in parts.iter().flat_map(|part| &part.segments) {
        store.resolve_recorded(&segment.path, "mp4").map_err(|_| {
            ExportFailure::new(msg::segment_path_invalid(
                ui_locale(),
                segment.path.display(),
            ))
        })?;
    }
    Ok(())
}

fn default_output_directory() -> Result<PathBuf, ExportFailure> {
    default_output_directory_from(
        crate::capture::buffer_root_override(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

/// The rule, as a pure function so the override can be passed in rather than
/// read from `buffer_root_override()`'s process-wide `OnceLock` (same shape as
/// `clips::resolve_clip_directory`, task3910).
///
/// With `LIVEBACK_BUFFER_ROOT` set, the default lands in `<root>\Exports`
/// instead of the user's `Videos\Liveback`: an isolated agent run would
/// otherwise write its stills -- the review screen's `s` -- straight into the
/// user's own folder, which is what happened on task t260913-3842. `Exports`
/// rather than `Screenshots` because the automatic export route takes the same
/// default when no file was named.
///
/// A path the user picked themselves (`requested = Some`) is untouched; that
/// is `resolve_output_directory`'s job, not this one.
pub(super) fn default_output_directory_from(
    override_root: Option<&Path>,
    user_profile: Option<&OsStr>,
) -> Result<PathBuf, ExportFailure> {
    if let Some(root) = override_root {
        return Ok(root.join("Exports"));
    }
    user_profile
        .map(PathBuf::from)
        .map(|path| path.join("Videos").join("Liveback"))
        .ok_or_else(|| ExportFailure::new(msg::videos_folder_unresolved(ui_locale())))
}

// Policy change (2026-07-24, user decision on task 024): output_directory is no
// longer confined to Videos\Liveback. This is a personal, single-user local
// tool, and the only source of a custom path is a native folder-picker dialog
// the user drives themselves, so the risk of an attacker-controlled path is
// negligible. What's still enforced: the path must be absolute (no relative
// traversal), and it must resolve to a plausible directory (either it already
// exists and is a directory, or its immediate parent exists) rather than an
// arbitrary nonsense path.
pub(super) fn resolve_output_directory(requested: Option<&Path>) -> Result<PathBuf, ExportFailure> {
    let Some(path) = requested else {
        return default_output_directory();
    };
    if !path.is_absolute()
        || path
            .components()
            .any(|component| component == Component::ParentDir)
    {
        return Err(ExportFailure::new(
            msg::output_must_be_absolute(ui_locale()),
        ));
    }
    if path.is_file() {
        return Err(ExportFailure::new(msg::output_is_a_file(ui_locale())));
    }
    if !path.exists() {
        let parent_exists = path.parent().is_some_and(Path::exists);
        if !parent_exists {
            return Err(ExportFailure::new(msg::output_parent_missing(ui_locale())));
        }
    }
    Ok(path.to_path_buf())
}

pub(super) fn ensure_capacity(parts: &[ExportPart], output: &Path) -> Result<(), ExportFailure> {
    let selected_duration: i64 = parts
        .iter()
        .map(|part| part.end_100ns - part.start_100ns)
        .sum();
    let source_duration: i64 = parts
        .iter()
        .flat_map(|part| &part.segments)
        .map(|segment| segment.end_100ns - segment.start_100ns)
        .sum();
    let source_bytes: u64 = parts
        .iter()
        .flat_map(|part| &part.segments)
        .map(|segment| segment.bytes)
        .sum();
    let proportional = if source_duration > 0 {
        (u128::from(source_bytes) * selected_duration.max(0) as u128 / source_duration as u128)
            as u64
    } else {
        source_bytes
    };
    let estimate = proportional.saturating_mul(120) / 100 + CAPACITY_MARGIN_BYTES;
    let free = ring_buffer::free_bytes(output)
        .map_err(|error| ExportFailure::new(msg::free_space_unreadable(ui_locale(), error)))?;
    if free < ring_buffer::MIN_FREE_BYTES.saturating_add(estimate) {
        return Err(ExportFailure::new(msg::not_enough_free_space(ui_locale())));
    }
    Ok(())
}

pub(super) fn sanitize_filename(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) || character.is_control()
            {
                '_'
            } else {
                character
            }
        })
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    if sanitized.is_empty() {
        "Replay".into()
    } else {
        sanitized.chars().take(80).collect()
    }
}

/// Local wall-clock `YYYYMMDD-HHMMSS` (design round18 §2/§4-1): unix seconds
/// used to be unreadable in a filename, and the file list has no separate
/// date column to fall back on. Seconds resolution is what keeps a burst of
/// saves inside the same minute from colliding on name alone -- the
/// `allocate_*_path` callers still add a numeric suffix on top for the same
/// second.
fn timestamp_label() -> String {
    let now = unsafe { GetLocalTime() };
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
    )
}

pub(super) fn allocate_output_paths(
    directory: &Path,
    game: &str,
    part_count: usize,
) -> Vec<PathBuf> {
    let base = format!("{}_{}", sanitize_filename(game), timestamp_label());
    let mut counter = 0u32;
    loop {
        let suffix = if counter == 0 {
            String::new()
        } else {
            format!("_{counter:02}")
        };
        let paths = (0..part_count)
            .map(|index| {
                let part = if part_count > 1 {
                    format!("_part{:02}", index + 1)
                } else {
                    String::new()
                };
                directory.join(format!("{base}{suffix}{part}.mp4"))
            })
            .collect::<Vec<_>>();
        if paths
            .iter()
            .all(|path| !path.exists() && !partial_path(path).exists())
        {
            return paths;
        }
        counter = counter.saturating_add(1);
    }
}

/// What the save dialog opens with (task1050): the name the automatic route
/// would have chosen, so pressing 保存 straight away gives the old behaviour.
pub(super) fn suggested_export_filename(game: &str) -> String {
    format!("{}_{}.mp4", sanitize_filename(game), timestamp_label())
}

/// The files a job writes when the user named one in the save dialog
/// (task1050). One part is exactly the file they picked. A selection that
/// spans a gap is written as several, so the rest take `_partNN` off the same
/// stem -- the alternative is silently dropping all but one of them.
pub(super) fn named_output_paths(chosen: &Path, part_count: usize) -> Vec<PathBuf> {
    if part_count <= 1 {
        return vec![chosen.to_path_buf()];
    }
    let directory = chosen.parent().unwrap_or(Path::new("."));
    let stem = chosen
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".to_owned());
    let extension = chosen
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .unwrap_or_else(|| "mp4".to_owned());
    (0..part_count)
        .map(|index| directory.join(format!("{stem}_part{:02}.{extension}", index + 1)))
        .collect()
}

/// Same `{game}_{YYYYMMDD-HHMMSS}` base as the video exports, with a numeric
/// suffix on collision (task102). The suffix is what makes a burst of
/// screenshots inside one second land as separate files rather than
/// overwriting each other -- `timestamp_label` only has second resolution.
pub(super) fn allocate_screenshot_path(directory: &Path, game: &str) -> PathBuf {
    let base = format!("{}_{}", sanitize_filename(game), timestamp_label());
    let mut counter = 0u32;
    loop {
        let suffix = if counter == 0 {
            String::new()
        } else {
            format!("_{counter:02}")
        };
        let path = directory.join(format!("{base}{suffix}.png"));
        if !path.exists() {
            return path;
        }
        counter = counter.saturating_add(1);
    }
}

pub(super) fn partial_path(final_path: &Path) -> PathBuf {
    final_path.with_extension("partial")
}

/// How much of the whole export is done, counting the part being written
/// (task1350): everything finished before it, plus how far into this one the
/// writer has got.
///
/// Clamped to the part's own span. The writer reports a sample's timestamp,
/// and the last samples of a part can sit a frame past its nominal end -- an
/// export must never report more than it has, and must never go backwards.
pub(super) fn processed_with_part(
    completed_100ns: i64,
    part_span_100ns: i64,
    into_part_100ns: i64,
) -> i64 {
    completed_100ns + into_part_100ns.clamp(0, part_span_100ns.max(0))
}

pub(super) fn ratio_millionths(processed: i64, total: i64) -> u32 {
    if total <= 0 {
        return 0;
    }
    ((processed.clamp(0, total) as i128 * 1_000_000 / total as i128) as u32).min(1_000_000)
}

pub(super) fn check_cancel(cancel: &AtomicBool) -> Result<(), ExportFailure> {
    if cancel.load(Ordering::Acquire) {
        Err(ExportFailure::cancelled())
    } else {
        Ok(())
    }
}
