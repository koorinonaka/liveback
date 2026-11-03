//! What a failed export says on screen (t260926-97dd, t261101-5ff3).
//!
//! The engine (`crate::export::failure`) decides *which* failure it was and
//! logs the technical detail; this turns that kind into the sentence the
//! screen shows. `ui_state::export::outcome` and the corner toasts hand the
//! text to the screen verbatim, `review_stage` does the same with what
//! `ExportController::start`/`cancel` return, and `screenshot_toast` uses it as
//! the toast's own title. So every string returned here is UI text.
//!
//! The sentences are a handful of plain ones, grouped by what the user can do
//! about the failure, rather than the pipeline's own vocabulary -- segment,
//! codec, frame rate, SPS, sample -- that the log keeps.

pub use crate::export::failure::Failure;
use crate::ui_state::locale::Locale;

crate::tr! {
    // ---------- ExportController::start / cancel ----------
    state_poisoned { ja: "書き出しの状態が異常です", en: "The export state is corrupt" }
    already_running { ja: "書き出しはすでに実行中です", en: "An export is already running" }
    none_running { ja: "実行中の書き出しがありません", en: "No export is running" }
    /// `ExportController::run` (t261009-5ebd): the job ended without a word.
    worker_gone { ja: "書き出しが結果を返さずに終わりました", en: "The export ended without a result" }

    // ---------- plan ----------
    range_invalid { ja: "書き出し範囲が無効です", en: "The export range is invalid" }
    no_game_name { ja: "ゲーム名がありません", en: "The game name is missing" }
    output_relative_traversal {
        ja: "出力先のパスに「..」は使えません",
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

/// The sentence for `failure`.
pub fn text(failure: &Failure, locale: Locale) -> String {
    let sentence = match (failure, locale) {
        (Failure::StatePoisoned, _) => state_poisoned(locale),
        (Failure::AlreadyRunning, _) => already_running(locale),
        (Failure::NoneRunning, _) => none_running(locale),
        (Failure::WorkerGone, _) => worker_gone(locale),
        (Failure::RangeInvalid, _) => range_invalid(locale),
        (Failure::NoGameName, _) => no_game_name(locale),
        (Failure::OutputRelativeTraversal, _) => output_relative_traversal(locale),
        (Failure::OutputMustBeAbsolute, _) => output_must_be_absolute(locale),
        (Failure::OutputIsAFile, _) => output_is_a_file(locale),
        (Failure::OutputParentMissing, _) => output_parent_missing(locale),
        (Failure::VideosFolderUnresolved, _) => videos_folder_unresolved(locale),
        (Failure::NotEnoughFreeSpace, _) => not_enough_free_space(locale),
        (Failure::Cancelled, _) => crate::ui_state::export::export_cancelled(locale),
        (Failure::NoRecording, Locale::Ja) => "この範囲には録画がありません",
        (Failure::NoRecording, Locale::En) => "There is no recording in this range",
        (Failure::Unreadable, Locale::Ja) => "録画データを読めませんでした",
        (Failure::Unreadable, Locale::En) => "The recording could not be read",
        (Failure::Unsupported, Locale::Ja) => "この録画の形式には対応していません",
        (Failure::Unsupported, Locale::En) => "This recording's format is not supported",
        (Failure::MixedSettings, Locale::Ja) => {
            "範囲の途中で録画の設定が変わっているため書き出せません"
        }
        (Failure::MixedSettings, Locale::En) => {
            "The recording settings change within this range, so it cannot be exported"
        }
        (Failure::Folder, Locale::Ja) => "保存先のフォルダを作成できません",
        (Failure::Folder, Locale::En) => "The destination folder could not be created",
        (Failure::Failed, Locale::Ja) => "書き出しに失敗しました",
        (Failure::Failed, Locale::En) => "The export failed",
        (Failure::Screenshot, Locale::Ja) => "静止画を保存できませんでした",
        (Failure::Screenshot, Locale::En) => "The screenshot could not be saved",
    };
    sentence.to_owned()
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::failure::{self as lib, last_detail};

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
        for failure in Failure::ALL {
            assert!(japanese(&text(&failure, Locale::Ja)), "{failure:?}");
            assert!(!japanese(&text(&failure, Locale::En)), "{failure:?}");
        }
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
        for failure in Failure::ALL {
            assert!(!contains_any(&text(&failure, Locale::Ja)), "{failure:?}");
        }
        // The catalog's and the review lease's English (t261101-e9ab): each
        // reaches the log, none reaches the screen.
        let check = |failure: Failure, raw: &str| {
            let shown = text(&failure, Locale::Ja);
            assert!(last_detail().contains(raw), "{}", last_detail());
            assert!(!shown.contains(raw) && japanese(&shown), "{shown}");
        };
        let raw = "no such session";
        check(lib::session_unavailable(raw), raw);
        let raw = "the selected segment is not in that session";
        check(lib::session_unavailable(raw), raw);
        let raw = "segment read failed";
        check(lib::segment_unreadable(raw), raw);
        let shown = text(
            &lib::segment_open_failed(r"C:\a\b.mp4", "denied"),
            Locale::Ja,
        );
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
            let shown = text(&lib::pts_before_origin(120, 340), locale);
            let detail = last_detail();
            assert!(detail.contains("120") && detail.contains("340"), "{detail}");
            assert!(!shown.contains("120") && !shown.contains("340"), "{shown}");

            let shown = text(&lib::segment_open_failed(r"C:\a\b.mp4", "denied"), locale);
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

    /// The helper already chose its failure; the parent must not bury it
    /// under a second, vaguer one.
    #[test]
    fn a_helper_failure_passes_through() {
        let from_helper = lib::audio_codec_not_aac();
        assert_eq!(lib::helper_failed(from_helper), from_helper);
    }
}
