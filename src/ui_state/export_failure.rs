//! What a failed export says on screen -- and, since t260926-97dd, what it
//! says in the log instead.
//!
//! `ui_state::export::outcome` hands `ExportStatus.error` to the screen
//! verbatim as `Outcome::Failed(..)` (the toast's detail), `review_stage` does
//! the same with what `ExportController::start`/`cancel` return, and
//! `screenshot_toast` uses the string as the toast's own title. So every
//! string returned here is UI text.
//!
//! Those strings used to carry the pipeline's own vocabulary -- segment,
//! codec, frame rate, SPS, sample -- and the raw `{error}` / `{path}` behind
//! it. A user can act on none of that. So the entries below keep the
//! signatures their callers already use, but return one of a handful of plain
//! sentences ([`Kind`]), grouped by what the user can do about it, and send
//! the technical detail (in English, with every argument) to `tracing::warn!`.
//! The export helper is a separate process with no log file of its own; it
//! writes its warnings to stderr, and the parent logs what it captured there
//! (`export::helper`).
//!
//! The pipeline runs off the UI thread -- and the part writer runs in a
//! different *process* (`--livia-export-helper`) -- so callers pass the locale
//! rather than hold one. Inside the helper it comes over the request; on this
//! side it is `locale::active()`, the same pair `capture::start`'s
//! `disk_refusal` already uses.

use crate::ui_state::locale::Locale;

crate::tr! {
    // ---------- ExportController::start / cancel ----------
    state_poisoned { ja: "書き出しの状態が異常です", en: "The export state is corrupt" }
    already_running { ja: "書き出しはすでに実行中です", en: "An export is already running" }
    none_running { ja: "実行中の書き出しがありません", en: "No export is running" }

    // ---------- plan ----------
    range_invalid { ja: "書き出し範囲が無効です", en: "The export range is invalid" }
    no_game_name { ja: "ゲーム名がありません", en: "The game name is missing" }
    output_relative_traversal {
        ja: "出力先に相対移動は使えません",
        en: "The output path cannot use relative traversal"
    }
    output_must_be_absolute {
        ja: "出力先は絶対パスで指定してください",
        en: "The output path has to be absolute (a relative path is not allowed)"
    }
    output_is_a_file {
        ja: "出力先はフォルダではなくファイルです",
        en: "The output path is a file, not a folder"
    }
    output_parent_missing {
        ja: "出力先の親フォルダが存在しません",
        en: "The output path's parent folder does not exist"
    }
    videos_folder_unresolved {
        ja: "ユーザーのビデオフォルダが見つかりません",
        en: "The user's Videos folder could not be resolved"
    }
    not_enough_free_space {
        ja: "空き容量が不足しているため書き出しを開始できません",
        en: "There is not enough free space to start the export"
    }
}

/// The plain sentences a failure is told in. Grouped by what the user can do
/// about it, not by where in the pipeline it happened -- that is the log's job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
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
}

impl Kind {
    #[cfg(test)]
    const ALL: [Kind; 7] = [
        Kind::NoRecording,
        Kind::Unreadable,
        Kind::Unsupported,
        Kind::MixedSettings,
        Kind::Folder,
        Kind::Failed,
        Kind::Screenshot,
    ];

    fn text(self, locale: Locale) -> &'static str {
        match (self, locale) {
            (Kind::NoRecording, Locale::Ja) => "この範囲には録画がありません",
            (Kind::NoRecording, Locale::En) => "There is no recording in this range",
            (Kind::Unreadable, Locale::Ja) => "録画データを読めませんでした",
            (Kind::Unreadable, Locale::En) => "The recording could not be read",
            (Kind::Unsupported, Locale::Ja) => "この録画の形式には対応していません",
            (Kind::Unsupported, Locale::En) => "This recording's format is not supported",
            (Kind::MixedSettings, Locale::Ja) => {
                "範囲の途中で録画の設定が変わっているため書き出せません"
            }
            (Kind::MixedSettings, Locale::En) => {
                "The recording settings change within this range, so it cannot be exported"
            }
            (Kind::Folder, Locale::Ja) => "保存先のフォルダを作成できません",
            (Kind::Folder, Locale::En) => "The destination folder could not be created",
            (Kind::Failed, Locale::Ja) => "書き出しに失敗しました",
            (Kind::Failed, Locale::En) => "The export failed",
            (Kind::Screenshot, Locale::Ja) => "静止画を保存できませんでした",
            (Kind::Screenshot, Locale::En) => "The screenshot could not be saved",
        }
    }
}

/// Logs `detail` under `key` and returns the plain sentence for `kind`.
fn report(key: &'static str, kind: Kind, locale: Locale, detail: String) -> String {
    tracing::warn!(key, %detail, "export failure");
    #[cfg(test)]
    LAST_DETAIL.with(|last| *last.borrow_mut() = detail);
    kind.text(locale).to_owned()
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

/// `name(args) => Kind, "detail template"`: an entry with the signature the
/// old `trf!` / `tr!` one had (`String` now for the argument-less ones too),
/// returning `Kind`'s sentence and logging the detail. The template has to
/// name every argument or `format!` refuses it -- the guarantee `trf!` gave.
macro_rules! failure {
    ($(
        $(#[$meta:meta])*
        $name:ident ( $($arg:ident),* $(,)? ) => $kind:ident, $detail:literal;
    )*) => {
        $(
            $(#[$meta])*
            pub fn $name(locale: Locale, $($arg: impl ::std::fmt::Display),*) -> String {
                report(stringify!($name), Kind::$kind, locale, format!($detail $(, $arg = $arg)*))
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

    // ---------- plan ----------
    no_session_id() => Failed, "The session id is missing";
    range_has_no_segment() => NoRecording, "The selected range holds no recorded segment";
    segment_path_invalid(path) => Unreadable, "The segment path is invalid: {path}";
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
    video_frame_rate_unsupported(rate) => Unsupported,
        "Unsupported video frame rate: {rate:.3} fps";

    // ---------- part writer ----------
    aac_start_boundary_missing() => Failed, "The AAC start boundary is missing";
    aac_end_boundary_missing() => Failed, "The AAC end boundary is missing";
    aac_boundary_drift() => Failed, "The AAC boundary error is over 10.67 ms";
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
/// `incompatible_codec_config`'s detail. Log text only: no locale, and nothing
/// logged here -- the outer entry logs the whole line once.
pub fn boundary_attempt_failed(
    _locale: Locale,
    rate: impl std::fmt::Display,
    error: impl std::fmt::Display,
) -> String {
    format!("boundary({rate}fps)=could not create: {error}")
}

/// Stands in for the exit status in `helper_exited_without_result`'s detail.
pub fn helper_wait_failed(_locale: Locale, error: impl std::fmt::Display) -> String {
    format!("the helper could not be waited on: {error}")
}

/// The helper's own failure, already worded for the user in the parent's
/// locale by the entries above (the helper takes the locale off the request).
/// Passed through rather than wrapped, so the plain sentence survives; the
/// helper's detail reaches the log through its stderr (`export::helper`).
pub fn helper_failed(_locale: Locale, error: impl std::fmt::Display) -> String {
    let message = error.to_string();
    tracing::warn!(key = "helper_failed", %message, "export failure");
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn japanese(text: &str) -> bool {
        text.chars().any(|c| {
            matches!(c, '\u{3040}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
        })
    }

    /// A copy-paste that leaves the English text in the `ja` slot compiles
    /// perfectly well, and that is the exact mistake a bulk edit made on
    /// 2026-09-21. Every plain sentence is checked, both ways.
    #[test]
    fn both_languages_are_actually_written() {
        for kind in Kind::ALL {
            assert!(japanese(kind.text(Locale::Ja)), "{kind:?}");
            assert!(!japanese(kind.text(Locale::En)), "{kind:?}");
        }
        assert!(japanese(already_running(Locale::Ja)));
        assert!(!japanese(already_running(Locale::En)));
    }

    /// t260926-97dd: the screen says none of the pipeline's vocabulary. The
    /// same word list run over a detail line is the control -- it proves the
    /// check sees the words when they are there.
    #[test]
    fn the_japanese_sentences_carry_no_pipeline_vocabulary() {
        let words = [
            "segment",
            "codec",
            "frame",
            "header",
            "SPS",
            "sample",
            "session ID",
        ];
        let contains_any = |text: &str| words.iter().any(|word| text.contains(word));
        for kind in Kind::ALL {
            assert!(!contains_any(kind.text(Locale::Ja)), "{kind:?}");
        }
        let shown = segment_open_failed(Locale::Ja, r"C:\a\b.mp4", "denied");
        assert!(!contains_any(&shown), "{shown}");
        assert!(contains_any(&last_detail()), "{}", last_detail());
    }

    /// The arguments moved from the screen to the log (t260926-97dd, Goal 1:
    /// 「`{error}` `{path}` の生値は本文に出さず `tracing::warn!` へ」). Each has to
    /// reach the detail, and none may reach the sentence -- the detail
    /// containing it is what makes the sentence's absence meaningful.
    #[test]
    fn arguments_reach_the_log_and_not_the_screen() {
        for locale in [Locale::Ja, Locale::En] {
            let shown = pts_before_origin(locale, 120, 340);
            let detail = last_detail();
            assert!(detail.contains("120") && detail.contains("340"), "{detail}");
            assert!(!shown.contains("120") && !shown.contains("340"), "{shown}");

            let shown = segment_open_failed(locale, r"C:\a\b.mp4", "denied");
            let detail = last_detail();
            assert!(
                detail.contains("b.mp4") && detail.contains("denied"),
                "{detail}"
            );
            assert!(
                !shown.contains("b.mp4") && !shown.contains("denied"),
                "{shown}"
            );
        }
    }

    /// The frame-rate detail is the only one with a precision spec, which is
    /// the part an edit is most likely to drop.
    #[test]
    fn the_frame_rate_keeps_three_decimals_in_the_log() {
        video_frame_rate_unsupported(Locale::Ja, 23.976_023);
        assert!(last_detail().contains("23.976"), "{}", last_detail());
    }

    /// The helper already worded its failure in the user's language; the
    /// parent must not bury it under a second, vaguer sentence.
    #[test]
    fn a_helper_failure_passes_through() {
        let from_helper = audio_codec_not_aac(Locale::Ja);
        assert_eq!(helper_failed(Locale::Ja, &from_helper), from_helper);
    }
}
