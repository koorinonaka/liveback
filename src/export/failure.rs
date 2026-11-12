//! Why an export (or a screenshot) failed, as a kind the screen can word
//! (t261101-5ff3). The engine decides *which* sentence; the words themselves
//! live in `ui_state::export_failure::text`, which the binary and the MCP
//! server call with the locale. The export helper is a separate process and
//! hands its [`Failure`] back over JSON, so it needs no locale at all.
//!
//! Each entry below logs the technical detail (in English, with every
//! argument) to `tracing::warn!` and returns the plain kind -- the pipeline's
//! own vocabulary and the raw `{error}` / `{path}` stay in the log
//! (t260926-97dd). The export helper has no log file of its own; it writes its
//! warnings to stderr, and the parent logs what it captured there
//! (`export::helper`).

use serde::{Deserialize, Serialize};

/// What a failure says on screen. The first group is one sentence each; the
/// second groups the pipeline's failures by what the user can do about it, not
/// by where in the pipeline they happened -- that is the log's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    // ---------- ExportController::start / cancel ----------
    StatePoisoned,
    AlreadyRunning,
    NoneRunning,
    /// `ExportController::run` (t261009-5ebd): the job ended without a word.
    WorkerGone,

    // ---------- plan ----------
    RangeInvalid,
    NoGameName,
    OutputRelativeTraversal,
    OutputMustBeAbsolute,
    OutputIsAFile,
    OutputParentMissing,
    VideosFolderUnresolved,
    NotEnoughFreeSpace,

    // ---------- grouped by what the user can do ----------
    /// Pick another range.
    NoRecording,
    /// The recording itself is damaged or unreadable.
    Unreadable,
    /// Recorded in a form the exporter does not handle.
    Unsupported,
    /// The recording settings changed partway through the range.
    MixedSettings,
    /// The destination folder could not be made.
    Folder,
    /// Anything else: retrying is all the user can do.
    Failed,
    /// The screenshot path.
    Screenshot,

    /// The user stopped the job.
    Cancelled,
}

impl Failure {
    #[cfg(test)]
    pub(crate) const ALL: [Failure; 20] = [
        Failure::StatePoisoned,
        Failure::AlreadyRunning,
        Failure::NoneRunning,
        Failure::WorkerGone,
        Failure::RangeInvalid,
        Failure::NoGameName,
        Failure::OutputRelativeTraversal,
        Failure::OutputMustBeAbsolute,
        Failure::OutputIsAFile,
        Failure::OutputParentMissing,
        Failure::VideosFolderUnresolved,
        Failure::NotEnoughFreeSpace,
        Failure::NoRecording,
        Failure::Unreadable,
        Failure::Unsupported,
        Failure::MixedSettings,
        Failure::Folder,
        Failure::Failed,
        Failure::Screenshot,
        Failure::Cancelled,
    ];
}

/// Logs `detail` under `key` and returns `failure`.
fn report(key: &'static str, failure: Failure, detail: String) -> Failure {
    tracing::warn!(key, %detail, "export failure");
    #[cfg(test)]
    LAST_DETAIL.with(|last| *last.borrow_mut() = detail);
    failure
}

#[cfg(test)]
thread_local! {
    /// What the last `report` on this thread logged, so a test can read the
    /// detail without installing a subscriber.
    static LAST_DETAIL: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

/// What the last entry called on this thread logged -- for the tests of the
/// callers, which moved their argument checks from the message to the log.
#[cfg(test)]
pub(crate) fn last_detail() -> String {
    LAST_DETAIL.with(|last| last.borrow().clone())
}

/// `name(args) => Kind, "detail template"`: an entry that logs the detail and
/// returns the kind. The template has to name every argument or `format!`
/// refuses it.
macro_rules! failure {
    ($(
        $(#[$meta:meta])*
        $name:ident ( $($arg:ident),* $(,)? ) => $kind:ident, $detail:literal;
    )*) => {
        $(
            $(#[$meta])*
            pub fn $name($($arg: impl ::std::fmt::Display),*) -> Failure {
                report(stringify!($name), Failure::$kind, format!($detail $(, $arg = $arg)*))
            }
        )*
    };
}

failure! {
    // ---------- ExportController ----------
    worker_start_failed(error) => Failed, "The export worker could not be started: {error}";
    stage_dir_failed(error) => Failed, "The export workspace could not be created: {error}";
    stage_segment_failed(error) => Failed,
        "The segment could not be staged into the export workspace: {error}";
    output_dir_failed(error) => Folder, "The output folder could not be created: {error}";
    finalize_failed(error) => Failed, "The export result could not be finalized: {error}";
    /// The catalog's own English (`no such session`, t261101-e9ab): the
    /// session was pruned or discarded while the review screen still showed it.
    session_unavailable(error) => NoRecording, "The session is not available: {error}";
    segment_unreadable(error) => Unreadable,
        "The segment could not be read from the recording: {error}";

    // ---------- plan ----------
    no_session_id() => Failed, "The session id is missing";
    range_has_no_segment() => NoRecording, "The selected range holds no recorded segment";
    free_space_unreadable(error) => Failed, "The free space could not be read: {error}";

    // ---------- probe ----------
    no_segment_to_export() => NoRecording, "There is no segment to export";
    segments_config_mismatch() => MixedSettings,
        "The segments' video/AAC settings do not match, so they cannot be exported";
    h264_sequence_header_missing() => Unreadable, "The H.264 sequence header is missing";
    h264_sps_unparsable() => Unreadable, "The H.264 SPS could not be parsed";
    av1_sequence_header_missing() => Unreadable, "The AV1 sequence header is missing";
    video_frame_size_missing() => Unreadable, "The video has no frame size";
    video_frame_rate_missing() => Unreadable, "The video has no frame rate";
    video_frame_rate_invalid() => Unreadable, "The video frame rate is invalid";
    audio_codec_not_aac() => Unsupported, "The audio codec is not AAC";
    aac_sample_rate_missing() => Unreadable, "The AAC sample rate is missing";
    aac_channels_missing() => Unreadable, "The AAC channel information is missing";
    video_info_unreadable(error) => Unreadable,
        "The video information could not be read: {error}";
    aac_info_unreadable(error) => Unreadable, "The AAC information could not be read: {error}";
    /// The path stays in the log: two segments of one export fail the same way
    /// for different files, and the name is what tells them apart.
    segment_open_failed(path, error) => Unreadable,
        "The segment could not be opened ({path}): {error}";
    /// `subtype` is a GUID the caller has already formatted -- the arguments
    /// are `Display`, and this one only exists as `Debug`.
    video_codec_unsupported(subtype) => Unsupported, "Unsupported video codec: {subtype}";

    // ---------- part writer ----------
    aac_start_boundary_missing() => Failed, "The AAC start boundary is missing";
    aac_end_boundary_missing() => Failed, "The AAC end boundary is missing";
    aac_boundary_drift(edge, requested, chosen, first, last) => Failed,
        "The AAC boundary error is over 10.67 ms: edge={edge} requested={requested} chosen={chosen} first={first} last={last}";
    boundary_drain_timeout() => Failed, "The boundary encoder's drain timed out";
    boundary_input_timeout() => Failed, "Feeding the boundary encoder timed out";
    pts_before_origin(timestamp, origin) => Failed,
        "The export PTS is before the origin: pts={timestamp}, origin={origin}";
    aac_discontinuity(expected, actual) => Failed,
        "The AAC discontinuity is over one AU: expected={expected}, actual={actual}";
    video_sample_read_failed(error) => Unreadable,
        "The video sample could not be read: {error}";
    video_timestamp_read_failed(error) => Unreadable,
        "The video timestamp could not be read: {error}";
    aac_sample_read_failed(error) => Unreadable, "The AAC sample could not be read: {error}";
    /// One entry for every Media Foundation step of the NV12 path; `step`
    /// names the MF call.
    nv12_step_failed(step, error) => Failed, "The NV12 {step} failed: {error}";
    boundary_device_failed(error) => Failed,
        "The D3D11 device for the boundary re-encode could not be created: {error}";
    boundary_encoder_unavailable(config, attempts) => Failed,
        "No encoder could be created for the boundary re-encode ({config}): {attempts}";
    incompatible_codec_config(source, attempts) => MixedSettings,
        "The video settings do not match, so the export stopped safely (IncompatibleCodecConfig): source={source}, {attempts}";

    // ---------- encoder start (`encoder::EncoderStartError`, one per kind) ----------
    // `diagnostics` is the Windows API text (`MFStartup failed: ...`,
    // t260927-11da): log only.
    encoder_no_hardware(diagnostics) => Failed,
        "No hardware encoder is available: {diagnostics}";
    encoder_unsupported_format(diagnostics) => Failed,
        "The encoder does not support this format: {diagnostics}";
    encoder_device_failed(diagnostics) => Failed, "The encoder device failed: {diagnostics}";
    encoder_io_failed(diagnostics) => Failed, "The encoder could not write: {diagnostics}";

    // ---------- helper process ----------
    helper_stdin_failed() => Failed, "The export helper's stdin could not be opened";
    helper_stdout_failed() => Failed, "The export helper's stdout could not be opened";
    helper_heartbeat_stopped() => Failed, "The export helper's heartbeat stopped";
    helper_request_malformed(error) => Failed, "The helper request is malformed: {error}";
    helper_exited_without_result(status, stderr) => Failed,
        "The export helper exited without a result (exit={status}){stderr}";
    helper_path_failed(error) => Failed,
        "The export helper's path could not be resolved: {error}";
    helper_spawn_failed(error) => Failed, "The export helper could not be started: {error}";
    helper_request_send_failed(error) => Failed,
        "The request could not be sent to the export helper: {error}";
    helper_status_failed(error) => Failed,
        "The export helper's state could not be read: {error}";

    // ---------- screenshot ----------
    png_malformed() => Screenshot, "The PNG data is malformed";
    destination_failed(error) => Folder,
        "The destination folder could not be created: {error}";
    screenshot_save_failed(error) => Screenshot, "The screenshot could not be saved: {error}";
}

/// One line per frame rate tried, joined into `boundary_encoder_unavailable` /
/// `incompatible_codec_config`'s detail. Log text only -- the outer entry logs
/// the whole line once.
pub fn boundary_attempt_failed(
    rate: impl std::fmt::Display,
    error: impl std::fmt::Display,
) -> String {
    format!("boundary({rate}fps)=could not create: {error}")
}

/// Stands in for the exit status in `helper_exited_without_result`'s detail.
pub fn helper_wait_failed(error: impl std::fmt::Display) -> String {
    format!("the helper could not be waited on: {error}")
}

/// The helper's own failure, already a kind. Passed through rather than
/// wrapped, so the plain sentence survives; the helper's detail reaches the log
/// through its stderr (`export::helper`).
pub fn helper_failed(failure: Failure) -> Failure {
    tracing::warn!(key = "helper_failed", ?failure, "export failure");
    failure
}
