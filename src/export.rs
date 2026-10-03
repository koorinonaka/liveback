use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    thread,
};

use crate::events::EventSink;
use crate::ui_state::export_failure as msg;
use crate::ui_state::locale::active as ui_locale;
use serde::{Deserialize, Serialize};
#[cfg(test)]
use windows::Win32::Media::MediaFoundation::{
    IMFSample, MFCreateMediaType, MFMediaType_Video, MFVideoFormat_H264, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE, MF_MT_MPEG_SEQUENCE_HEADER, MF_MT_SUBTYPE,
    MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::{capture, encoder, ring_buffer};

const CAPACITY_MARGIN_BYTES: u64 = 64 * 1024 * 1024;
const AAC_MAX_ERROR_100NS: i64 = 106_700;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub session_id: String,
    pub start_100ns: i64,
    pub end_100ns: i64,
    pub game_name: String,
    /// The file the user named in the save dialog (task1050). `None` means the
    /// automatic naming under the default directory; task1210 removed the
    /// `output_directory` that used to sit beside this once the save dialog
    /// left it without a single producer.
    #[serde(default)]
    pub output_file: Option<PathBuf>,
    /// How a session with two or more audio tracks is mixed (t261003-b713).
    /// Ignored for a single-track session, which is copied as it always was.
    #[serde(default)]
    pub audio_mix: AudioMix,
}

/// One audio track's level in the mixdown: its saved volume and mute, never
/// the review screen's master (decision 12).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TrackLevel {
    pub volume_percent: u8,
    pub muted: bool,
}

impl Default for TrackLevel {
    fn default() -> Self {
        Self {
            volume_percent: 100,
            muted: false,
        }
    }
}

impl TrackLevel {
    fn scale(self) -> f32 {
        if self.muted {
            0.0
        } else {
            f32::from(self.volume_percent.min(100)) / 100.0
        }
    }
}

/// The audio half of an export request (t261003-b713).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AudioMix {
    /// In track order. A track past the end plays at 100%, unmuted.
    pub levels: Vec<TrackLevel>,
    /// A clip: the mix alone. An export writes the mix first and every
    /// original track after it (decisions 12 and 13).
    pub mix_only: bool,
}

/// Each recorded track's saved level, in track order (t261003-b713): track 0
/// from the target's own volume, the rest matched to the registration's added
/// sounds -- an application by executable name, a microphone by device id. A
/// track with no saved entry is 100%, unmuted. The master volume and mute in
/// `settings` are deliberately not read (decision 12).
pub fn track_levels(
    settings: &crate::settings::AppSettings,
    key: Option<&str>,
    tracks: &[ring_buffer::container::AudioTrackInfo],
) -> Vec<TrackLevel> {
    let Some(target) = key.and_then(|key| settings.target_audio.get(key)) else {
        return Vec::new();
    };
    let level = |volume_percent, muted| TrackLevel {
        volume_percent,
        muted,
    };
    let mut levels = vec![level(target.volume_percent, target.muted)];
    for track in tracks.iter().skip(1) {
        let saved = target
            .extras
            .iter()
            .find(|extra| extra.source.records(track));
        levels.push(saved.map_or_else(TrackLevel::default, |extra| {
            level(extra.volume_percent, extra.muted)
        }));
    }
    levels
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExportProgress {
    pub job_id: String,
    pub part: usize,
    pub part_count: usize,
    pub processed_100ns: i64,
    pub total_100ns: i64,
    pub ratio_millionths: u32,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExportStatus {
    pub job_id: String,
    pub state: ExportState,
    pub progress: ExportProgress,
    pub paths: Vec<PathBuf>,
    pub error: Option<String>,
    pub passthrough_video_samples: u64,
    pub reencoded_video_frames: u64,
    pub passthrough_audio_samples: u64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ExportState {
    Running,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug)]
struct ExportFailure {
    message: String,
    cancelled: bool,
}

impl ExportFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            cancelled: false,
        }
    }
    fn cancelled() -> Self {
        Self {
            message: crate::ui_state::export::export_cancelled(ui_locale()).to_owned(),
            cancelled: true,
        }
    }
}

impl From<encoder::EncoderStartError> for ExportFailure {
    fn from(value: encoder::EncoderStartError) -> Self {
        use encoder::EncoderStartErrorKind as Kind;
        let (locale, detail) = (ui_locale(), value.diagnostics);
        Self::new(match value.kind {
            Kind::NoHardwareEncoder => msg::encoder_no_hardware(locale, detail),
            Kind::UnsupportedFormat => msg::encoder_unsupported_format(locale, detail),
            Kind::Device => msg::encoder_device_failed(locale, detail),
            Kind::Io => msg::encoder_io_failed(locale, detail),
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct ExportPart {
    start_100ns: i64,
    end_100ns: i64,
    segments: Vec<ring_buffer::SegmentRecord>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportDiagnostics {
    passthrough_video_samples: u64,
    reencoded_video_frames: u64,
    passthrough_audio_samples: u64,
}

struct ExportLease {
    capture: capture::CaptureController,
    token: String,
}

impl Drop for ExportLease {
    fn drop(&mut self) {
        self.capture.release_review_lease(&self.token);
    }
}

/// The segments of a container session, written out as files for the duration
/// of one export (task450), and deleted with this guard.
struct StagedSegments {
    directory: PathBuf,
}

impl Drop for StagedSegments {
    fn drop(&mut self) {
        // Best effort: a leftover here is a few megabytes in TEMP, not a
        // corrupted export, and the job may well be unwinding from a failure.
        let _ = fs::remove_dir_all(&self.directory);
    }
}

/// Reads every segment the plan needs out of the container and rewrites the
/// plan to point at the copies.
///
/// Reading goes through the export's own review lease, which is the same
/// authorization the rest of the pipeline runs under and is already held by the
/// time this is called -- so the segments cannot be pruned out from under it.
/// One progress reading, with its ratio derived from the same two numbers it
/// reports -- the pair used to be computed twice at the call site, which is one
/// edit away from a ratio that disagrees with the bytes.
fn progress_at(
    job_id: &str,
    part: usize,
    part_count: usize,
    processed_100ns: i64,
    total_100ns: i64,
) -> ExportProgress {
    ExportProgress {
        job_id: job_id.into(),
        part,
        part_count,
        processed_100ns,
        total_100ns,
        ratio_millionths: ratio_millionths(processed_100ns, total_100ns),
    }
}

fn stage_container_segments(
    capture: &capture::CaptureController,
    lease: &str,
    parts: &mut [ExportPart],
    cancel: &AtomicBool,
) -> Result<StagedSegments, ExportFailure> {
    let directory = std::env::temp_dir().join(format!("livia-export-{}", std::process::id()));
    fs::create_dir_all(&directory)
        .map_err(|error| ExportFailure::new(msg::stage_dir_failed(ui_locale(), error)))?;
    let staged = StagedSegments { directory };
    for segment in parts.iter_mut().flat_map(|part| part.segments.iter_mut()) {
        // A long range off an HDD buffer stages for minutes, and 中止 must
        // not wait for all of it.
        check_cancel(cancel)?;
        let path = staged.directory.join(format!("{}.mp4", segment.index));
        // Several parts can name the same segment; write it once.
        if !path.is_file() {
            let bytes = capture
                .read_review_segment(lease, segment.index)
                .map_err(ExportFailure::new)?;
            fs::write(&path, bytes).map_err(|error| {
                ExportFailure::new(msg::stage_segment_failed(ui_locale(), error))
            })?;
        }
        segment.path = path;
    }
    Ok(staged)
}

struct PublishGuard {
    partials: Vec<PathBuf>,
    finals: Vec<PathBuf>,
    committed: bool,
}

impl PublishGuard {
    fn new() -> Self {
        Self {
            partials: Vec::new(),
            finals: Vec::new(),
            committed: false,
        }
    }
}

impl Drop for PublishGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for path in self.partials.iter().chain(self.finals.iter()) {
            let _ = fs::remove_file(path);
        }
    }
}

struct ControllerState {
    status: Option<ExportStatus>,
    cancel: Option<Arc<AtomicBool>>,
}

#[derive(Clone)]
pub struct ExportController {
    sink: Arc<dyn EventSink>,
    capture: capture::CaptureController,
    state: Arc<Mutex<ControllerState>>,
    next_job: Arc<AtomicU64>,
}

impl ExportController {
    pub fn new(sink: Arc<dyn EventSink>, capture: capture::CaptureController) -> Self {
        Self {
            sink,
            capture,
            state: Arc::new(Mutex::new(ControllerState {
                status: None,
                cancel: None,
            })),
            next_job: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn start(&self, request: ExportRequest) -> Result<String, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| msg::state_poisoned(ui_locale()).to_owned())?;
        if state.status.as_ref().is_some_and(|status| {
            matches!(status.state, ExportState::Running | ExportState::Cancelling)
        }) {
            return Err(msg::already_running(ui_locale()).to_owned());
        }
        let job_id = format!("export-{}", self.next_job.fetch_add(1, Ordering::Relaxed));
        let cancel = Arc::new(AtomicBool::new(false));
        let progress = ExportProgress {
            job_id: job_id.clone(),
            part: 0,
            part_count: 0,
            processed_100ns: 0,
            total_100ns: request.end_100ns.saturating_sub(request.start_100ns),
            ratio_millionths: 0,
        };
        state.status = Some(ExportStatus {
            job_id: job_id.clone(),
            state: ExportState::Running,
            progress,
            paths: Vec::new(),
            error: None,
            passthrough_video_samples: 0,
            reencoded_video_frames: 0,
            passthrough_audio_samples: 0,
        });
        state.cancel = Some(cancel.clone());
        let running = state.status.clone();
        drop(state);

        // Announce the job before the worker has anything to say about it
        // (task1050). Nothing did, and that is why the export button looked
        // dead: the UI folds `export_progress` into the status it is already
        // holding, and the only thing that ever seeded that status was
        // `finish_job` -- so every progress packet of a running export was
        // dropped on the floor and 書き出し中 never appeared.
        if let Some(running) = running.as_ref() {
            self.sink.export_status(running);
        }

        let controller = self.clone();
        let worker_job = job_id.clone();
        let spawned = thread::Builder::new()
            .name("livia-export".into())
            .spawn(move || {
                let result = controller.run_job(&worker_job, request, &cancel);
                controller.finish_job(&worker_job, result);
            });
        if let Err(error) = spawned {
            // Roll the Running status back, or every later start() is refused
            // with "already running" for a worker that never existed.
            if let Ok(mut state) = self.state.lock() {
                state.status = None;
                state.cancel = None;
            }
            // ...and take back the Running just announced, or the UI sits at
            // 書き出し中 for a job that never existed.
            if let Some(mut running) = running {
                running.state = ExportState::Failed;
                running.error = Some(msg::worker_start_failed(ui_locale(), &error));
                self.sink.export_status(&running);
            }
            return Err(msg::worker_start_failed(ui_locale(), &error));
        }
        Ok(job_id)
    }

    pub fn cancel(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| msg::state_poisoned(ui_locale()).to_owned())?;
        let Some(cancel) = state.cancel.as_ref() else {
            return Err(msg::none_running(ui_locale()).to_owned());
        };
        cancel.store(true, Ordering::Release);
        if let Some(status) = state.status.as_mut() {
            if status.state == ExportState::Running {
                status.state = ExportState::Cancelling;
            }
        }
        Ok(())
    }

    /// The session's audio tracks as its manifest names them, for
    /// [`track_levels`]: the review screen's snapshot keeps only the names and
    /// loses which device a microphone track records. Empty when the session
    /// cannot be read or has one track.
    pub fn session_audio_tracks(
        &self,
        session_id: &str,
    ) -> Vec<ring_buffer::container::AudioTrackInfo> {
        self.capture
            .session_manifest(session_id)
            .map(|manifest| manifest.audio_tracks)
            .unwrap_or_default()
    }

    pub fn status(&self) -> Option<ExportStatus> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.status.clone())
    }

    fn finish_job(
        &self,
        job_id: &str,
        result: Result<(Vec<PathBuf>, ExportDiagnostics), ExportFailure>,
    ) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let snapshot = {
            let Some(status) = state
                .status
                .as_mut()
                .filter(|status| status.job_id == job_id)
            else {
                return;
            };
            match result {
                Ok((paths, diagnostics)) => {
                    status.state = ExportState::Completed;
                    status.paths = paths;
                    status.progress.processed_100ns = status.progress.total_100ns;
                    status.progress.ratio_millionths = 1_000_000;
                    status.passthrough_video_samples = diagnostics.passthrough_video_samples;
                    status.reencoded_video_frames = diagnostics.reencoded_video_frames;
                    status.passthrough_audio_samples = diagnostics.passthrough_audio_samples;
                }
                Err(error) => {
                    status.state = if error.cancelled {
                        ExportState::Cancelled
                    } else {
                        ExportState::Failed
                    };
                    status.error = Some(error.message);
                }
            }
            status.clone()
        };
        state.cancel = None;
        drop(state);
        self.sink.export_status(&snapshot);
    }

    fn update_progress(&self, progress: ExportProgress) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(status) = state.status.as_mut() {
                status.progress = progress.clone();
            }
        }
        self.sink.export_progress(&progress);
    }

    fn run_job(
        &self,
        job_id: &str,
        request: ExportRequest,
        cancel: &AtomicBool,
    ) -> Result<(Vec<PathBuf>, ExportDiagnostics), ExportFailure> {
        validate_request(&request)?;
        let manifest = self
            .capture
            .session_manifest(&request.session_id)
            .map_err(ExportFailure::new)?;
        let mut parts = plan_parts(&manifest, request.start_100ns, request.end_100ns)?;
        let indexes = parts
            .iter()
            .flat_map(|part| part.segments.iter().map(|segment| segment.index))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let token = self
            .capture
            .acquire_review_lease(Some(request.session_id.clone()), indexes)
            .map_err(ExportFailure::new)?;
        let _lease = ExportLease {
            capture: self.capture.clone(),
            token,
        };
        // A container session has no per-segment files, and this pipeline reads
        // segments by opening paths (`open_reader`). Rather than teach every
        // reader about containers -- range-based export is task450's explicitly
        // out-of-scope successor -- the segments this export needs are written
        // out once, and the plan is pointed at them. The guard deletes them
        // when the job ends, however it ends.
        let _staged = if self.capture.session_is_container(&request.session_id) {
            Some(stage_container_segments(
                &self.capture,
                &_lease.token,
                &mut parts,
                cancel,
            )?)
        } else {
            let session_root = self
                .capture
                .session_root(&request.session_id)
                .map_err(ExportFailure::new)?;
            validate_segment_paths(&parts, &session_root)?;
            None
        };
        let output_directory =
            resolve_output_directory(request.output_file.as_deref().and_then(Path::parent))?;
        fs::create_dir_all(&output_directory)
            .map_err(|error| ExportFailure::new(msg::output_dir_failed(ui_locale(), error)))?;
        ensure_capacity(&parts, &output_directory)?;
        check_cancel(cancel)?;

        let final_paths = match request.output_file.as_deref() {
            // The user already answered "where and what name" in the dialog,
            // and overwriting is the dialog's own question to ask -- so no
            // collision counter here (task1050).
            Some(chosen) => named_output_paths(chosen, parts.len()),
            None => allocate_output_paths(&output_directory, &request.game_name, parts.len()),
        };
        let total_100ns: i64 = parts
            .iter()
            .map(|part| part.end_100ns.saturating_sub(part.start_100ns))
            .sum();
        let mut processed = 0;
        let mut guard = PublishGuard::new();
        let mut diagnostics = ExportDiagnostics::default();
        for (part_index, (part, final_path)) in parts.iter().zip(&final_paths).enumerate() {
            check_cancel(cancel)?;
            let partial = partial_path(final_path);
            guard.partials.push(partial.clone());
            let part_span = part.end_100ns - part.start_100ns;
            let part_diagnostics =
                export_part_in_helper(part, &partial, &request.audio_mix, cancel, &|into_part| {
                    self.update_progress(progress_at(
                        job_id,
                        part_index + 1,
                        parts.len(),
                        processed_with_part(processed, part_span, into_part),
                        total_100ns,
                    ));
                })?;
            diagnostics.passthrough_video_samples += part_diagnostics.passthrough_video_samples;
            diagnostics.reencoded_video_frames += part_diagnostics.reencoded_video_frames;
            diagnostics.passthrough_audio_samples += part_diagnostics.passthrough_audio_samples;
            processed += part.end_100ns - part.start_100ns;
            self.update_progress(progress_at(
                job_id,
                part_index + 1,
                parts.len(),
                processed,
                total_100ns,
            ));
        }

        // Commit starts only after every MP4 is finalized. Cancellation from here loses
        // to successful publication, so callers never see a partial multi-file result.
        for (partial, final_path) in guard.partials.iter().zip(&final_paths) {
            fs::rename(partial, final_path)
                .map_err(|error| ExportFailure::new(msg::finalize_failed(ui_locale(), error)))?;
            guard.finals.push(final_path.clone());
        }
        guard.committed = true;
        Ok((final_paths, diagnostics))
    }
}

mod plan;
#[cfg(test)]
use plan::default_output_directory_from;
#[cfg(test)]
use plan::sanitize_filename;
use plan::{
    allocate_output_paths, allocate_screenshot_path, check_cancel, ensure_capacity,
    named_output_paths, partial_path, plan_parts, processed_with_part, ratio_millionths,
    resolve_output_directory, validate_request, validate_segment_paths,
};

mod helper;
use helper::export_part_in_helper;
pub use helper::run_helper_if_requested;

mod probe;
#[cfg(test)]
use probe::{
    classify_frame_rate, open_reader, video_fingerprint, VideoFingerprint,
    BOUNDARY_FRAME_RATE_CANDIDATES,
};

mod mixdown;
mod part_writer;
#[cfg(test)]
use part_writer::audio_window;
#[cfg(test)]
use part_writer::{export_part, export_part_mixed, nearest_boundary};

pub fn resolve_export_directory(requested: Option<&Path>) -> Result<PathBuf, String> {
    resolve_output_directory(requested).map_err(|failure| failure.message)
}

/// The name the save dialog opens with (task1050): the same
/// `{game}_{YYYYMMDD-HHMMSS}.mp4` the automatic naming would have produced, so
/// a user who just presses 保存 gets exactly what they used to get.
pub fn suggested_export_filename(game: &str) -> String {
    plan::suggested_export_filename(game)
}

/// The next free `{game}_{YYYYMMDD-HHMMSS}.mp4` under `directory` (task2970).
///
/// The automatic route allocates this inside the job; a clip has to name its
/// file up front, because `output_file: Some(..)` is the only way to point a
/// job somewhere other than the default directory -- and that route overwrites
/// without asking, on the assumption that the save dialog already asked. A clip
/// never opens one, so it allocates here instead. Same counter the automatic
/// route uses, `.partial` files included, so two saves in the same second land
/// as two files.
pub fn allocate_export_path(directory: &Path, game: &str) -> PathBuf {
    let mut paths = allocate_output_paths(directory, game, 1);
    paths.remove(0)
}

/// First 8 bytes of every PNG file. Checked because the bytes arrive over IPC
/// from the webview and this writes them to disk under a `.png` name.
const PNG_SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

/// Writes an already-encoded PNG beside the video exports (task102).
///
/// Deliberately does not go through `ExportController`: that permits one job at
/// a time, and a still should be capturable while an export is being written.
pub fn save_screenshot_png(bytes: &[u8], game: &str) -> Result<PathBuf, String> {
    if !bytes.starts_with(&PNG_SIGNATURE) {
        return Err(msg::png_malformed(ui_locale()).to_owned());
    }
    let directory = resolve_export_directory(None)?;
    // The video route creates its own output directory (`run_job`), and this
    // one used to ride on that: `Videos\Liveback` already existed by the time
    // anyone pressed `s`. With `LIVEBACK_BUFFER_ROOT` set the default is
    // `<root>\Exports`, which a fresh isolated root has never had -- without
    // this the write just fails and the still lands nowhere.
    fs::create_dir_all(&directory).map_err(|error| msg::destination_failed(ui_locale(), error))?;
    let path = allocate_screenshot_path(&directory, game);
    fs::write(&path, bytes).map_err(|error| msg::screenshot_save_failed(ui_locale(), error))?;
    Ok(path)
}

#[cfg(test)]
mod track_level_tests {
    use super::*;
    use crate::ring_buffer::container::AudioTrackInfo;
    use crate::settings::{AppSettings, ExtraAudio, ExtraAudioSource, TargetAudio};

    fn extra(source: ExtraAudioSource, volume_percent: u8, muted: bool) -> ExtraAudio {
        ExtraAudio {
            source,
            enabled: true,
            muted,
            volume_percent,
        }
    }

    fn level(volume_percent: u8, muted: bool) -> TrackLevel {
        TrackLevel {
            volume_percent,
            muted,
        }
    }

    /// Each recorded track takes its own saved level, matched by what it
    /// records rather than by position in the registration; the master
    /// volume and mute are never read (decision 12).
    #[test]
    fn levels_follow_the_saved_registration_and_ignore_the_master() {
        let mut settings = AppSettings {
            muted: true,
            volume_percent: 0,
            ..AppSettings::default()
        };
        settings.target_audio.insert(
            "game.exe".into(),
            TargetAudio {
                extras: vec![
                    extra(
                        ExtraAudioSource::Mic {
                            device_id: None,
                            device_name: Some("Mic".into()),
                        },
                        30,
                        false,
                    ),
                    extra(
                        ExtraAudioSource::App {
                            name: "Discord.exe".into(),
                            display_name: None,
                            path: None,
                        },
                        100,
                        true,
                    ),
                ],
                muted: false,
                volume_percent: 70,
            },
        );
        // The recording's order: the app was added first, then the default
        // mic, then an app the registration no longer has.
        let tracks = vec![
            AudioTrackInfo {
                executable_name: "Game.exe".into(),
                microphone: None,
            },
            AudioTrackInfo {
                executable_name: "discord.exe".into(),
                microphone: None,
            },
            AudioTrackInfo {
                executable_name: "Mic".into(),
                microphone: Some(String::new()),
            },
            AudioTrackInfo {
                executable_name: "Gone.exe".into(),
                microphone: None,
            },
        ];
        assert_eq!(
            track_levels(&settings, Some("game.exe"), &tracks),
            vec![
                level(70, false),
                level(100, true),
                level(30, false),
                level(100, false),
            ]
        );
        // No registration for the target: nothing to apply.
        assert!(track_levels(&settings, Some("other.exe"), &tracks).is_empty());
    }
}

#[cfg(test)]
mod screenshot_tests {
    use super::*;

    fn png_bytes() -> Vec<u8> {
        // Signature plus a token chunk header -- enough to exercise the guard
        // without pulling in an encoder.
        let mut bytes = PNG_SIGNATURE.to_vec();
        bytes.extend_from_slice(b"\x00\x00\x00\x0dIHDR");
        bytes
    }

    #[test]
    fn task102_rejects_bytes_that_are_not_a_png() {
        assert!(save_screenshot_png(b"", "Replay").is_err());
        assert!(save_screenshot_png(b"not a png at all", "Replay").is_err());
        // A truncated signature must not slip through on a prefix match.
        assert!(save_screenshot_png(&PNG_SIGNATURE[..4], "Replay").is_err());
    }

    #[test]
    fn task102_burst_within_one_second_allocates_distinct_paths() {
        let root = std::env::temp_dir().join(format!(
            "livia-screenshot-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();

        // `timestamp_label` only has second resolution, so three shots taken
        // back to back share a base name and must diverge on the suffix.
        let mut paths = Vec::new();
        for _ in 0..3 {
            let path = allocate_screenshot_path(&root, "Live Back");
            fs::write(&path, png_bytes()).unwrap();
            paths.push(path);
        }
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), 3, "each screenshot needs its own file");
        for path in &paths {
            assert_eq!(path.extension().unwrap(), "png");
            assert!(path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("Live Back_"));
        }

        // Characters Windows refuses in a filename come back sanitized.
        let sanitized = allocate_screenshot_path(&root, "a/b:c*d");
        assert!(!sanitized
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(['/', ':', '*']));

        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
pub(crate) mod tests;
