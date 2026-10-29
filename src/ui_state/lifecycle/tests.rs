use super::*;

// `CaptureLifecycleStatus`'s constructors are `pub(super)` to `capture`, so
// the statuses are built field by field here. `seq` stands in for the real
// counter: any two distinct values are two distinct stop events.
fn stopped(code: &str, message: Option<&str>) -> CaptureLifecycleStatus {
    stopped_seq(code, message, 1)
}

fn stopped_seq(code: &str, message: Option<&str>, seq: u64) -> CaptureLifecycleStatus {
    CaptureLifecycleStatus {
        state: "stopped".into(),
        diagnostic_code: Some(code.into()),
        message: message.map(Into::into),
        failure_stage: None,
        seq,
    }
}

fn failed(stage: Option<&str>) -> CaptureLifecycleStatus {
    CaptureLifecycleStatus {
        failure_stage: stage.map(Into::into),
        ..stopped(CAPTURE_FAILED, Some("停止しました"))
    }
}

/// Task199: the OS toast and the in-app toast are two renderings of one
/// event, so they cannot disagree about what happened.
#[test]
fn the_os_toast_title_says_failed_only_when_the_capture_failed() {
    assert_eq!(
        auto_stop_title(Locale::Ja, &failed(Some(VIDEO_ENCODER_STAGE))),
        capture_failed_title(Locale::Ja)
    );
    assert_eq!(
        auto_stop_title(Locale::Ja, &failed(None)),
        capture_failed_title(Locale::Ja)
    );
    // A target that went away stopped the recording; it did not fail it.
    assert_eq!(
        auto_stop_title(
            Locale::Ja,
            &stopped("CAP-TGT-001", Some("対象が終了しました"))
        ),
        auto_stop_title_text(Locale::Ja)
    );
    assert_eq!(
        auto_stop_title(Locale::Ja, &recording()),
        auto_stop_title_text(Locale::Ja)
    );
}

fn recording() -> CaptureLifecycleStatus {
    CaptureLifecycleStatus {
        state: "recording".into(),
        diagnostic_code: None,
        message: None,
        failure_stage: None,
        seq: 0,
    }
}

fn idle() -> CaptureLifecycleStatus {
    CaptureLifecycleStatus {
        state: "idle".into(),
        diagnostic_code: None,
        message: None,
        failure_stage: None,
        seq: 0,
    }
}

/// Task2090, the reason the duplicate check moved off the code. The NVENC
/// wall takes captures down one after another and every one of them is
/// `CAP-DEV-001`; keyed on the code, the second death was silent -- no
/// banner, and a recording the user had running was simply gone.
#[test]
fn a_second_capture_dying_under_the_same_code_is_still_news() {
    let mut notified = None;
    let first = CaptureLifecycleStatus {
        failure_stage: Some(VIDEO_ENCODER_STAGE.into()),
        ..stopped_seq(CAPTURE_FAILED, Some("停止しました"), 7)
    };
    let second = CaptureLifecycleStatus {
        seq: 8,
        ..first.clone()
    };
    assert!(auto_stop_toast(Locale::Ja, &mut notified, &first).is_some());
    // The 1s poll re-reading the same stop is not a second stop.
    assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &first), None);
    // A different stop event under the identical code is.
    assert!(auto_stop_toast(Locale::Ja, &mut notified, &second).is_some());
    assert_eq!(notified, Some(8));
    assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &second), None);
}

/// Task1420: the disk guard stops the recording, it does not fail it -- the
/// session is closed and playable, so the toast says 「録画が停止しました」
/// over the body that tells the user to free some room.
#[test]
fn running_out_of_disk_is_a_stop_and_is_notified_once() {
    let status = stopped("CAP-DSK-001", Some("容量を空けてください"));
    assert!(!is_capture_failure(&status));
    assert_eq!(
        auto_stop_title(Locale::Ja, &status),
        auto_stop_title_text(Locale::Ja)
    );
    let mut notified = None;
    assert_eq!(
        auto_stop_toast(Locale::Ja, &mut notified, &status),
        Some("容量を空けてください".to_owned())
    );
    assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &status), None);
}

#[test]
fn a_user_requested_stop_is_not_news() {
    let mut notified = None;
    assert_eq!(
        auto_stop_toast(
            Locale::Ja,
            &mut notified,
            &stopped(REQUESTED, Some("録画を安全に停止しました。"))
        ),
        None
    );
    assert_eq!(notified, None);
}

#[test]
fn a_new_recording_makes_the_same_failure_news_again() {
    let mut notified = None;
    let status = stopped("CAP-TGT-001", Some("対象が終了しました"));
    assert!(auto_stop_toast(Locale::Ja, &mut notified, &status).is_some());
    assert_eq!(
        auto_stop_toast(Locale::Ja, &mut notified, &recording()),
        None
    );
    assert_eq!(notified, None);
    assert!(auto_stop_toast(Locale::Ja, &mut notified, &status).is_some());
}

#[test]
fn idle_says_nothing() {
    let mut notified = None;
    assert_eq!(auto_stop_toast(Locale::Ja, &mut notified, &idle()), None);
}

#[test]
fn a_body_less_stop_falls_back_to_the_code() {
    let mut notified = None;
    assert_eq!(
        auto_stop_toast(Locale::Ja, &mut notified, &stopped("CAP-TGT-001", None)),
        Some("CAP-TGT-001".to_owned())
    );
}

/// The two failures task193 came from: fps 120 rejected by the encoder, and
/// the mid-recording death -- which reported `start_capture` until
/// t260911-77c3 gave it `MID_RECORDING_STAGE`. The `start_capture` arm is
/// still checked here: a status an older build wrote still reaches this
/// function, and dropping the arm would send it to the generic GPU bucket.
///
/// Task2100: the encoder stage goes through the const the worker assigns
/// to `startup_stage`, never a literal -- task1760 renamed the stage, the
/// arm went unreachable, and every encoder failure fell into the generic
/// GPU bucket with the suite green. Do not rewrite it as a literal.
#[test]
fn each_stage_gets_its_own_reason() {
    assert!(capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)).contains("エンコーダ"));
    assert_ne!(
        capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)),
        capture_failure_detail(Locale::Ja, None),
        "the encoder failure must not fall through to the generic GPU bucket"
    );
    assert!(capture_failure_detail(Locale::Ja, Some("start_capture")).contains("続けられ"));
    assert!(capture_failure_detail(Locale::Ja, Some(MID_RECORDING_STAGE)).contains("続けられ"));
    assert!(capture_failure_detail(Locale::Ja, Some("wgc_item")).contains("対象"));
    assert_ne!(
        capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)),
        capture_failure_detail(Locale::Ja, Some("start_capture"))
    );
    assert_ne!(
        capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)),
        capture_failure_detail(Locale::Ja, Some(MID_RECORDING_STAGE))
    );
}

/// Task2150. A recording killed by the audio session dying reported
/// `start_capture`, so the toast told the user to pick a different target --
/// advice that is wrong, because the target was never the problem. Same
/// shape as the encoder test above: the stage comes from the const the
/// worker assigns, never a literal.
#[test]
fn an_audio_session_death_is_not_blamed_on_the_capture_target() {
    for locale in [Locale::Ja, Locale::En] {
        let detail = capture_failure_detail(locale, Some(AUDIO_SESSION_STAGE));
        assert_ne!(
            detail,
            capture_failure_detail(locale, None),
            "it must not fall through to the generic GPU bucket"
        );
        assert_ne!(
            detail,
            capture_failure_detail(locale, Some(MID_RECORDING_STAGE)),
            "the generic mid-recording bucket loses the one actionable cause: {detail}"
        );
        assert_ne!(
            detail,
            capture_failure_detail(locale, Some("start_capture")),
            "the pre-77c3 mid-recording bucket is the wrong advice: {detail}"
        );
        assert_ne!(detail, capture_failure_detail(locale, Some("wgc_item")));
    }
    let ja = capture_failure_detail(Locale::Ja, Some(AUDIO_SESSION_STAGE));
    assert!(ja.contains("音声"), "the body has to name the cause: {ja}");
    assert!(
        !ja.contains("対象"),
        "the target is fine; never tell the user to pick another: {ja}"
    );
    // `ui/controls.slint` renders the body on one elided line with no wrap,
    // which holds roughly 29 Japanese characters. Longer text loses its tail
    // -- and the tail here is the only advice the user gets.
    assert!(
        ja.chars().count() <= 29,
        "the body has to survive elision, got {} chars: {ja}",
        ja.chars().count()
    );
}

/// t260911-77c3. On 2026-09-11 a recording died mid-run because the fMP4
/// sink stopped taking video; the window was healthy the whole time, and the
/// toast said 「キャプチャを続けられなくなりました。対象を選び直して再開
/// してください。」 -- following it repairs nothing. Same shape as the audio
/// test above: the stage comes from the const the worker assigns, never a
/// literal (task2100).
#[test]
fn a_mid_recording_death_is_not_blamed_on_the_capture_target() {
    for locale in [Locale::Ja, Locale::En] {
        let detail = capture_failure_detail(locale, Some(MID_RECORDING_STAGE));
        assert_ne!(
            detail,
            capture_failure_detail(locale, None),
            "it must not fall through to the generic GPU bucket: {detail}"
        );
        assert_ne!(
            detail,
            capture_failure_detail(locale, Some("start_capture")),
            "the point of the new stage is that it is not the old advice: {detail}"
        );
        assert_ne!(detail, capture_failure_detail(locale, Some("wgc_item")));
        assert_ne!(
            detail,
            capture_failure_detail(locale, Some("window_handle"))
        );
    }
    let ja = capture_failure_detail(Locale::Ja, Some(MID_RECORDING_STAGE));
    let en = capture_failure_detail(Locale::En, Some(MID_RECORDING_STAGE));
    println!("mid_recording ja ({} chars): {ja}", ja.chars().count());
    println!("mid_recording en: {en}");
    assert!(
        !ja.contains("対象"),
        "the target is fine; never tell the user to pick another: {ja}"
    );
    assert!(
        !en.to_ascii_lowercase().contains("target"),
        "same in English: {en}"
    );
    // The same one-elided-line budget the audio wording is held to.
    assert!(
        ja.chars().count() <= 29,
        "the body has to survive elision, got {} chars: {ja}",
        ja.chars().count()
    );
}

#[test]
fn an_unknown_or_absent_stage_falls_back_to_the_generic_reason() {
    let generic = capture_failure_detail(Locale::Ja, None);
    assert_eq!(
        capture_failure_detail(Locale::Ja, Some("a_stage_added_later")),
        generic
    );
    // Every setup step shares the bucket the fallback uses.
    assert_eq!(
        capture_failure_detail(Locale::Ja, Some("d3d_device")),
        generic
    );
}

/// The toast has two lines and the user reads Japanese: no HRESULT, no MFT
/// diagnostics string.
#[test]
fn no_reason_leaks_a_diagnostic_string() {
    for stage in [
        None,
        Some("com"),
        Some("d3d_device"),
        Some("window_handle"),
        Some("wgc_item"),
        Some("wgc_frame_pool"),
        Some("wgc_session"),
        Some(VIDEO_ENCODER_STAGE),
        Some("transform_pipeline"),
        Some("nv12_converter"),
        Some("start_capture"),
        Some(AUDIO_SESSION_STAGE),
        Some(MID_RECORDING_STAGE),
    ] {
        let detail = capture_failure_detail(Locale::Ja, stage);
        assert!(!detail.contains("0x"), "{stage:?}: {detail}");
        assert!(
            !detail.is_ascii(),
            "{stage:?} should read as Japanese, not a diagnostic: {detail}"
        );
    }
}

/// Task2140. Every one of these used to be true of the shipped app: the
/// flag was set, nothing read it, and the recording came out silent with
/// only a log line to say so.
#[test]
fn a_recording_with_audio_says_nothing() {
    let mut notified = Vec::new();
    assert_eq!(
        audio_unavailable_toast(Locale::Ja, &mut notified, &[("s1", Some("メモ帳"), false)]),
        None
    );
    assert!(notified.is_empty());
    // Nothing recording at all is not news either.
    assert_eq!(
        audio_unavailable_toast(Locale::Ja, &mut notified, &[]),
        None
    );
}

#[test]
fn a_mute_recording_is_named_in_both_languages() {
    let mut notified = Vec::new();
    let running = [("s1", Some("ペイント"), true)];
    let body = audio_unavailable_toast(Locale::Ja, &mut notified, &running).unwrap();
    assert!(body.contains("ペイント"), "it has to say which one: {body}");
    assert!(body.contains("音声"));
    let mut notified = Vec::new();
    let body = audio_unavailable_toast(Locale::En, &mut notified, &running).unwrap();
    assert!(body.contains("ペイント"), "{body}");
    assert!(body.contains("Audio"), "{body}");
}

/// The 1s..10s poll re-reads the same running capture forever; one recording
/// says this once.
#[test]
fn the_same_mute_recording_is_only_announced_once() {
    let mut notified = Vec::new();
    let running = [("s1", Some("ペイント"), true)];
    assert!(audio_unavailable_toast(Locale::Ja, &mut notified, &running).is_some());
    assert_eq!(notified, vec!["s1".to_owned()]);
    assert_eq!(
        audio_unavailable_toast(Locale::Ja, &mut notified, &running),
        None
    );
    // The next recording is a different capture, so it is news again -- and
    // the finished one drops out of the memory rather than accumulating.
    let next = [("s2", Some("ペイント"), true)];
    assert!(audio_unavailable_toast(Locale::Ja, &mut notified, &next).is_some());
    assert_eq!(notified, vec!["s2".to_owned()]);
}

/// Concurrent captures are ordinary since task2080, and the corner holds one
/// toast: two separate ones would mean the first being overwritten unread.
#[test]
fn two_mute_recordings_at_once_are_one_toast_naming_both() {
    let mut notified = Vec::new();
    let body = audio_unavailable_toast(
        Locale::Ja,
        &mut notified,
        &[("s1", Some("ペイント"), true), ("s2", Some("メモ帳"), true)],
    )
    .unwrap();
    assert!(
        body.contains("ペイント") && body.contains("メモ帳"),
        "{body}"
    );
    assert_eq!(notified.len(), 2);
    // A third capture joining later is its own news; the two already told
    // are not repeated.
    let body = audio_unavailable_toast(
        Locale::Ja,
        &mut notified,
        &[
            ("s1", Some("ペイント"), true),
            ("s2", Some("メモ帳"), true),
            ("s3", Some("電卓"), true),
        ],
    )
    .unwrap();
    assert!(body.contains("電卓"), "{body}");
    assert!(!body.contains("ペイント"), "already said: {body}");
}

/// `targets::title_for_handle` answers `None` for a window with an empty
/// caption. Naming some of the mute captures would read as the whole list.
#[test]
fn an_unnamed_target_falls_back_to_wording_that_names_none_of_them() {
    let mut notified = Vec::new();
    assert_eq!(
        audio_unavailable_toast(Locale::Ja, &mut notified, &[("s1", None, true)]),
        Some(audio_unavailable_unnamed(Locale::Ja).to_owned())
    );
    let mut notified = Vec::new();
    assert_eq!(
        audio_unavailable_toast(
            Locale::En,
            &mut notified,
            &[("s1", Some("Paint"), true), ("s2", None, true)]
        ),
        Some(audio_unavailable_unnamed(Locale::En).to_owned())
    );
}

/// It reports a recording that is still running, so it must not borrow the
/// failure title -- task198 / task1430 keep the video.
#[test]
fn the_mute_notice_does_not_claim_the_recording_failed() {
    assert_ne!(
        audio_unavailable_title(Locale::Ja),
        capture_failed_title(Locale::Ja)
    );
    assert_ne!(
        audio_unavailable_title(Locale::En),
        capture_failed_title(Locale::En)
    );
}

#[test]
fn a_stop_in_flight_outranks_everything() {
    assert_eq!(live_chip(false, false), LiveChip::Hidden);
    // Pulled back into the past, or looking at an old session entirely:
    // the capture is still running and is still stoppable from here
    // (task670). round7 §2-3: hovering it does nothing at all now.
    assert_eq!(live_chip(true, false), LiveChip::Live);
    // The resting word is the picker tile's, not a second spelling of it.
    assert_eq!(
        live_chip_label(Locale::Ja, LiveChip::Live),
        crate::ui_state::targets::recording(Locale::Ja)
    );
    assert_eq!(live_chip(true, true), LiveChip::Stopping);
    // Even once the capture is gone: `stopping` clears on the same message
    // that clears `recording`, and until then the chip has to keep saying
    // what it is doing rather than blink out.
    assert_eq!(live_chip(false, true), LiveChip::Stopping);
    assert_eq!(
        live_chip_label(Locale::Ja, LiveChip::Stopping),
        live_chip_stopping(Locale::Ja)
    );
    assert_eq!(live_chip_label(Locale::Ja, LiveChip::Hidden), "");
}

/// Task2030 判断2, reworded by t260927-711d (DS CaptureScreen 「状態」):
/// a warning, not a gate. It appears at N and says the running count,
/// whatever that count is.
#[test]
fn the_warning_appears_at_three_captures_and_counts_them() {
    assert_eq!(CONCURRENCY_WARNING_MIN, 3, "task2070 measured N=3");
    for running in 0..CONCURRENCY_WARNING_MIN {
        assert_eq!(
            concurrency_warning(Locale::Ja, running),
            None,
            "{running} captures is below the line"
        );
    }
    assert_eq!(
        concurrency_warning(Locale::Ja, 3).as_deref(),
        Some("3 本を録画中です。同時に録る本数が多いと、録画が重くなることがあります。")
    );
    let four = concurrency_warning(Locale::Ja, 4).unwrap();
    assert!(four.starts_with("4 本を録画中です。"), "{four}");
    assert!(concurrency_warning(Locale::En, 4).unwrap().contains('4'));
}

/// The tray's stop entry is enabled off the tooltip alone (the sink carries
/// no count), so the two have to be read together. Task2030 shipped the
/// enable test as `tooltip == tray_tooltip_recording(..)`, which after the same
/// task's tooltip rewrite was true for exactly one shape -- one capture with
/// no caption -- and greyed out 停止 for every real recording. Found on the
/// machine in task2030's verification sweep.
#[test]
fn the_tray_stop_entry_is_enabled_for_every_recording_tooltip() {
    assert!(!tray_stop_enabled(&tray_tooltip(Locale::Ja, 0, None)));
    assert!(!tray_stop_enabled(&tray_tooltip(
        Locale::En,
        0,
        Some("Paint")
    )));
    for (running, title) in [
        (1, Some("ペイント")),
        (1, None),
        (2, Some("ペイント")),
        (3, None),
    ] {
        for locale in [Locale::Ja, Locale::En] {
            let tooltip = tray_tooltip(locale, running, title);
            assert!(
                tray_stop_enabled(&tooltip),
                "{running} running must leave the tray's stop clickable: {tooltip}"
            );
        }
    }
}

#[test]
fn the_tray_switches_to_24px_above_96dpi() {
    for (dpi, large) in [
        (0, false),
        (96, false),
        (120, true),
        (144, true),
        (192, true),
    ] {
        assert_eq!(tray_uses_24px(dpi), large, "dpi {dpi}");
    }
}

/// t260927-4c04 決めたこと 7: the DS TrayMenu README's words, one and
/// three captures, both locales. Sentence case in English; 「録画」, never
/// 「キャプチャ」, in Japanese.
#[test]
fn the_tray_speaks_the_ds_tray_menu_words() {
    assert_eq!(tray_stop_label(Locale::Ja, 1), "録画を停止");
    assert_eq!(tray_stop_label(Locale::Ja, 0), "録画を停止");
    assert_eq!(tray_stop_label(Locale::Ja, 3), "すべての録画を停止（3 本）");
    assert_eq!(tray_stop_label(Locale::En, 1), "Stop recording");
    assert_eq!(tray_stop_label(Locale::En, 3), "Stop all recordings (3)");

    assert_eq!(tray_tooltip(Locale::Ja, 0, None), "Liveback");
    assert_eq!(tray_tooltip(Locale::En, 0, None), "Liveback");
    assert_eq!(
        tray_tooltip(Locale::Ja, 1, Some("オーバーウォッチ 2")),
        "Liveback — 録画中: オーバーウォッチ 2"
    );
    assert_eq!(
        tray_tooltip(Locale::En, 1, Some("Overwatch 2")),
        "Liveback — Recording: Overwatch 2"
    );
    assert_eq!(tray_tooltip(Locale::Ja, 1, None), "Liveback — 録画中");
    assert_eq!(tray_tooltip(Locale::Ja, 3, None), "Liveback — 3 本を録画中");
    // One name fits a tooltip; with two or more it is only a count, and
    // with none recording a leftover name is dropped.
    assert_eq!(
        tray_tooltip(Locale::Ja, 2, Some("ペイント")),
        "Liveback — 2 本を録画中"
    );
    assert_eq!(tray_tooltip(Locale::Ja, 0, Some("ペイント")), "Liveback");
    assert_eq!(
        tray_tooltip(Locale::En, 3, None),
        "Liveback — Recording 3 targets"
    );
}

/// t260927-4c04 決めたこと 5: the title bar's chip words. The `Tally`
/// default is Japanese only, so the word comes from here.
#[test]
fn the_title_bar_chip_counts_in_both_locales() {
    assert_eq!(titlebar_tally_label(Locale::Ja, 1), "録画中");
    assert_eq!(titlebar_tally_label(Locale::Ja, 3), "3 本を録画中");
    assert_eq!(titlebar_tally_label(Locale::En, 1), "Recording");
    assert_eq!(titlebar_tally_label(Locale::En, 3), "Recording 3 targets");
}

/// The chip's buffer length, DS TitleBar preview `detail: '30:00'`.
/// No limit (the ring buffer switched off) says nothing.
#[test]
fn the_buffer_length_reads_as_a_clock() {
    assert_eq!(buffer_length_label(0), "");
    assert_eq!(buffer_length_label(5), "5:00");
    assert_eq!(buffer_length_label(30), "30:00");
    assert_eq!(buffer_length_label(60), "1:00:00");
    assert_eq!(buffer_length_label(125), "2:05:00");
}

/// t260928-b780 F13 (user ruling 「経過時間を出す」): with no retention
/// limit the chip shows the elapsed time instead, in the buffer length's
/// own clock and in whole minutes -- so the text changes once a minute.
#[test]
fn an_unlimited_capture_shows_its_elapsed_time() {
    const MINUTE: i64 = 60 * 10_000_000;
    assert_eq!(tally_detail(0, || 0), "0:00");
    assert_eq!(tally_detail(0, || MINUTE - 1), "0:00");
    assert_eq!(tally_detail(0, || 12 * MINUTE + 59 * 10_000_000), "12:00");
    assert_eq!(tally_detail(0, || 60 * MINUTE), "1:00:00");
    assert_eq!(tally_detail(0, || 125 * MINUTE), "2:05:00");
    // A clock that went backwards reads as nothing elapsed.
    assert_eq!(tally_detail(0, || -5), "0:00");
    // A limit keeps the buffer length, whatever has elapsed.
    assert_eq!(tally_detail(30, || 125 * MINUTE), "30:00");
}

/// Task2030 判断7. Every capture writing to the filled disk settles its own
/// lifecycle event, and the corner holds one toast: unconsolidated, the
/// user would see the last of N and never know how many stopped.
#[test]
fn a_disk_full_sweep_is_one_toast_naming_the_count() {
    let mut notified = None;
    let mut sweep = false;
    let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 4);
    let (body, sub) = stop_toast(Locale::Ja, &mut notified, &mut sweep, &first, 3).unwrap();
    assert!(body.contains('3'), "it has to say how many stopped: {body}");
    assert!(body.contains("すべて停止"), "{body}");
    assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
    // The other two captures of the same sweep are the same event, under
    // their own `seq`s.
    let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
    assert_eq!(
        stop_toast(Locale::Ja, &mut notified, &mut sweep, &second, 3),
        None
    );
    // A new recording makes the next sweep news again.
    stop_toast(Locale::Ja, &mut notified, &mut sweep, &recording(), 0);
    assert!(!sweep);
    let later = stopped_seq(DISK_FULL, Some("容量を空けてください"), 9);
    assert!(stop_toast(Locale::Ja, &mut notified, &mut sweep, &later, 2).is_some());
}

/// Task2200, the bug this file's `RunningCount` exists for. The guard's
/// 10s per-worker poll took the two captures 1.6s apart on the machine
/// (task2030: 16:47:10.29 / 16:47:11.89), and the picker re-reads the
/// active set before draining the death, so the live count is already 1
/// when the first `CAP-DSK-001` arrives -- which used to fall straight
/// through `stopped < 2` into the single-capture wording.
#[test]
fn a_staggered_disk_sweep_is_one_toast_naming_the_count() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(2);
    running.observe(1);
    let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 4);
    let (body, sub) = stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &first,
        running.count(),
    )
    .unwrap();
    assert!(body.contains('2'), "it has to say how many stopped: {body}");
    assert!(body.contains("すべて停止"), "{body}");
    assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
    // 1.6s later the second capture dies, with nothing left running.
    running.observe(0);
    let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &second,
            running.count()
        ),
        None,
        "no task1420 toast may trail the consolidated one"
    );
}

/// The other end of the same race: both deaths land inside one poll, so
/// the live count goes 2 -> 0 with nothing in between. The lifecycle is one
/// process-wide slot and the second death overwrites the first, but the
/// count is not read from it, so the number still adds up.
#[test]
fn a_disk_sweep_collapsed_into_one_poll_still_counts_two() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(2);
    running.observe(0);
    let status = stopped_seq(DISK_FULL, Some("容量を空けてください"), 7);
    let (body, sub) = stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &status,
        running.count(),
    )
    .unwrap();
    assert!(body.contains('2'), "{body}");
    assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
}

/// The user stopped one of two, then the disk took the other. Sticky
/// without `settled` would call that 「2 本の録画をすべて停止しました」,
/// which is a lie about a recording the user ended themselves.
#[test]
fn a_stop_the_user_asked_for_does_not_pad_the_sweep() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(2);
    running.settled(1);
    running.observe(1);
    let status = stopped(DISK_FULL, Some("容量を空けてください"));
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &status,
            running.count()
        ),
        Some(("容量を空けてください".to_owned(), None)),
        "one capture left means the task1420 wording, not a consolidation"
    );
    assert!(!sweep);
}

/// One capture on the disk is unchanged by the count: single wording, no
/// sweep remembered, and the next one rings again.
#[test]
fn a_lone_capture_out_of_disk_is_untouched_by_the_count() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(1);
    let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 1);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &first,
            running.count()
        ),
        Some(("容量を空けてください".to_owned(), None))
    );
    assert!(!sweep, "the single path never raises the sweep");
    running.observe(0);
    running.observe(1);
    let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 2);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &second,
            running.count()
        ),
        Some(("容量を空けてください".to_owned(), None)),
        "a later lone capture is its own event"
    );
}

/// Recording again ends the sweep for both states at once -- the count and
/// `stop_toast`'s `sweep` are cleared on the same status, which is why they
/// cannot disagree about whether the next disk hit is new.
#[test]
fn recording_again_rearms_the_consolidated_toast() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(2);
    let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 3);
    assert!(stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &first,
        running.count()
    )
    .is_some());
    running.observe(0);
    let live = recording();
    running.observe_lifecycle(&live);
    stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &live,
        running.count(),
    );
    assert!(!sweep);
    assert_eq!(running.count(), 0, "the old sweep's count does not survive");
    running.observe(2);
    let later = stopped_seq(DISK_FULL, Some("容量を空けてください"), 11);
    let (body, sub) = stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &later,
        running.count(),
    )
    .unwrap();
    assert!(body.contains('2'), "{body}");
    assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
}

/// Task2210, the lie task2200 left behind: one of two captures dies for its
/// own reason, the disk then takes the survivor, and the sticky count still
/// claims 「2 本の録画をすべて停止しました」 about a recording the disk never
/// touched.
///
/// The live count is dropped *before* the death's lifecycle status, not
/// after, and that is not an arbitrary ordering: `take_active` removes the
/// capture from the active set seconds ahead of `settle_lifecycle`, and the
/// picker sends `Msg::Capturing` before `Msg::Lifecycle` inside the same 1s
/// poll. Writing it the other way round would be testing a sequence the app
/// cannot produce.
#[test]
fn a_capture_that_died_on_its_own_is_not_padded_into_the_sweep() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(2);
    running.observe(1);
    let died = stopped_seq("CAP-TGT-001", Some("対象が終了しました"), 4);
    running.observe_lifecycle(&died);
    assert_eq!(running.count(), 1, "the closed target is out of the count");
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &died,
            running.count()
        ),
        Some(("対象が終了しました".to_owned(), None)),
        "the death is still reported on its own"
    );
    running.observe(0);
    let disk = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
    running.observe_lifecycle(&disk);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &disk,
            running.count()
        ),
        Some(("容量を空けてください".to_owned(), None)),
        "one capture was on the disk, so it is the task1420 wording"
    );
    assert!(!sweep, "nothing was consolidated, so nothing is remembered");
}

/// The shrink is one event, not one per poll. The lifecycle is a single
/// process-wide slot that keeps handing back the same stopped status every
/// second until something records again, so a shrink without the `seq`
/// guard would re-run forever and pin the count to `live` -- which is the
/// sticky count gone and task2200's bug back.
#[test]
fn a_stale_re_read_of_a_death_does_not_kill_the_sticky_count() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(3);
    running.observe(2);
    let died = stopped_seq("CAP-TGT-001", Some("対象が終了しました"), 4);
    running.observe_lifecycle(&died);
    assert_eq!(running.count(), 2);
    running.observe(2);
    running.observe_lifecycle(&died);
    // The disk takes the first of the two survivors: it leaves the active
    // set well before its own status is written, so this poll still reads
    // the *old* death while the live count is already 1.
    running.observe(1);
    running.observe_lifecycle(&died);
    assert_eq!(running.count(), 2, "the same death shrinks the count once");
    let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 5);
    running.observe_lifecycle(&first);
    let (body, sub) = stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &first,
        running.count(),
    )
    .unwrap();
    assert!(body.contains('2'), "it names the two the disk took: {body}");
    assert!(body.contains("すべて停止"), "{body}");
    assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
    running.observe(0);
    let second = stopped_seq(DISK_FULL, Some("容量を空けてください"), 6);
    running.observe_lifecycle(&second);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &second,
            running.count()
        ),
        None,
        "the rest of the sweep is the same event"
    );
}

/// Why `sweep` is read before `stopped < 2`. The count can now fall in the
/// middle of a sweep, and the guard's remaining captures would drop out of
/// the consolidated branch into the single-capture wording -- trailing a
/// task1420 toast behind the one that already named all three.
#[test]
fn a_death_inside_a_sweep_does_not_trail_a_single_toast() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(3);
    let first = stopped_seq(DISK_FULL, Some("容量を空けてください"), 7);
    running.observe_lifecycle(&first);
    let (body, sub) = stop_toast(
        Locale::Ja,
        &mut notified,
        &mut sweep,
        &first,
        running.count(),
    )
    .unwrap();
    assert!(body.contains('3'), "{body}");
    assert_eq!(sub, Some(disk_full_kept(Locale::Ja)));
    // A target window closes while the rest of the sweep is still landing.
    running.observe(1);
    let died = stopped_seq("CAP-TGT-001", Some("対象が終了しました"), 8);
    running.observe_lifecycle(&died);
    assert_eq!(running.count(), 1);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &died,
            running.count()
        ),
        Some(("対象が終了しました".to_owned(), None)),
        "an ordinary death is unchanged by the sweep"
    );
    running.observe(0);
    let last = stopped_seq(DISK_FULL, Some("容量を空けてください"), 9);
    running.observe_lifecycle(&last);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &last,
            running.count()
        ),
        None,
        "the sweep was already reported, however small the count has become"
    );
}

/// A stop the user asked for arrives twice -- once as the session leaving
/// the active set (`settled`), once as its own `CAP-EXIT-001` lifecycle --
/// and must be taken out of the count once. `count = live` is what makes
/// the second sighting a no-op; `count -= 1` would drop it to zero and the
/// disk's toast would then name nothing at all.
#[test]
fn a_user_stop_shrinks_the_count_once_not_twice() {
    let mut notified = None;
    let mut sweep = false;
    let mut running = RunningCount::default();
    running.observe(2);
    running.settled(1);
    running.observe(1);
    let asked = stopped_seq(REQUESTED, Some("録画を安全に停止しました。"), 12);
    running.observe_lifecycle(&asked);
    assert_eq!(running.count(), 1, "settled already took it out");
    let disk = stopped_seq(DISK_FULL, Some("容量を空けてください"), 13);
    running.observe_lifecycle(&disk);
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &disk,
            running.count()
        ),
        Some(("容量を空けてください".to_owned(), None)),
        "one capture left is the task1420 wording, and it names no count"
    );
    assert!(!sweep);
}

/// Every other stop is `auto_stop_toast` unchanged -- including the seq
/// bookkeeping, which has to keep running even on the polls whose answer
/// the disk-full branch throws away.
#[test]
fn an_ordinary_stop_passes_straight_through() {
    let mut notified = None;
    let mut sweep = false;
    let status = stopped("CAP-TGT-001", Some("対象が終了しました"));
    assert_eq!(
        stop_toast(Locale::Ja, &mut notified, &mut sweep, &status, 3),
        Some(("対象が終了しました".to_owned(), None))
    );
    assert_eq!(
        stop_toast(Locale::Ja, &mut notified, &mut sweep, &status, 3),
        None,
        "the same event polled again is not a second stop"
    );
    let mut notified = None;
    let mut sweep = false;
    assert_eq!(
        stop_toast(
            Locale::Ja,
            &mut notified,
            &mut sweep,
            &stopped(REQUESTED, Some("録画を安全に停止しました。")),
            3
        ),
        None,
        "a stop the user asked for is reported by settled_stops, not here"
    );
}

/// Task2030 判断7's first half: one toast per capture the user stopped, and
/// only once the capture is actually gone.
#[test]
fn a_requested_stop_is_reported_when_the_capture_leaves_the_active_set() {
    // Asking twice is one stop: the tile and the row stay clickable while
    // the stop is in flight, and the tray asks for everything running --
    // including whatever was already asked for on its own.
    let mut pending = Vec::new();
    request_stop(&mut pending, "s1");
    request_stop(&mut pending, "s1");
    request_stop(&mut pending, "s2");
    assert_eq!(pending, vec!["s1".to_owned(), "s2".to_owned()]);
    assert_eq!(settled_stops(&mut pending, &[]), 2);

    let mut pending = vec!["s1".to_owned(), "s2".to_owned()];
    let active = ["s1".to_owned(), "s2".to_owned(), "s3".to_owned()];
    assert_eq!(settled_stops(&mut pending, &active), 0);
    assert_eq!(pending.len(), 2);
    // s2 finished; s1 is still flushing.
    assert_eq!(
        settled_stops(&mut pending, &["s1".to_owned(), "s3".to_owned()]),
        1
    );
    assert_eq!(pending, vec!["s1".to_owned()]);
    // Stop-everything from the tray: the rest land together, one each.
    let mut pending = vec!["s1".to_owned(), "s3".to_owned()];
    assert_eq!(settled_stops(&mut pending, &[]), 2);
    assert!(pending.is_empty());
    // And nothing is reported twice.
    assert_eq!(settled_stops(&mut pending, &[]), 0);
}

/// The OS toast and the in-app toast say the same thing.
#[test]
fn the_os_toast_body_is_the_stage_reason() {
    let mut notified = None;
    assert_eq!(
        auto_stop_toast(
            Locale::Ja,
            &mut notified,
            &failed(Some(VIDEO_ENCODER_STAGE))
        ),
        Some(capture_failure_detail(Locale::Ja, Some(VIDEO_ENCODER_STAGE)).to_owned())
    );
}

/// t260928-e309 (F8, the user's ruling): one notice, one place. In front
/// the corner only, behind (or in the tray) Windows only -- for the
/// auto-stop too, which task3920 had sent to both.
#[test]
fn a_notice_goes_to_the_corner_in_front_and_to_windows_behind() {
    assert_eq!(
        notice_sinks(true),
        NoticeSinks {
            corner: true,
            os: false
        }
    );
    assert_eq!(
        notice_sinks(false),
        NoticeSinks {
            corner: false,
            os: true
        }
    );
}
