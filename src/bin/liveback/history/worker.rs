//! The session worker thread: listing, thumbnails, discard/load and
//! the retention sweep, all off the UI thread. Split from `history.rs`.

use std::sync::{Arc, Mutex};
use std::thread;

use crossbeam_channel::{Receiver, Sender};
use livia::capture::CaptureController;
use livia::ring_buffer;
use livia::settings::AppSettings;
use livia::ui_state::sessions;

use super::super::desktop::{open_folder, reveal_items};
use super::super::tr_locale;
use super::super::{Cmd, HistoryMsg, LoadKind, Msg};

/// Both load doors answer the same way: the session on success (the screen
/// that opens is the report), the one generic message on failure.
fn report_loaded(
    tx: &Sender<Msg>,
    loaded: Result<ring_buffer::SessionManifest, String>,
    kind: LoadKind,
) {
    match loaded {
        Ok(manifest) => {
            let _ = tx.send(Msg::History(HistoryMsg::Loaded {
                session_id: manifest.session_id,
                kind,
            }));
        }
        Err(error) => {
            // The UI only ever gets the one generic message, so without this the
            // reason a load failed leaves no trace anywhere (task3700's sweep
            // hit exactly that: "load failure, no log reason").
            tracing::warn!(%error, ?kind, "could not load the session manifest");
            let _ = tx.send(Msg::Failed((
                sessions::load_error(tr_locale()).into(),
                String::new(),
            )));
        }
    }
}

/// Discards each id in turn and reports what it actually freed.
///
/// One `discard_session` per id, sequentially: there is no bulk command, and
/// a partial run still has to report what it freed (task116).
fn discard_sessions(controller: &CaptureController, tx: &Sender<Msg>, ids: &[String]) {
    let mut freed = 0u64;
    let mut removed = 0usize;
    let mut failed = 0usize;
    for id in ids {
        // Measured straight off the buffer root, not through
        // `session_root`: that resolves against the in-memory
        // catalog and reports nothing for a session the
        // controller never loaded (every crash-recovery
        // candidate), which made the freed figure read 0 B.
        // Whichever shape the session has, and **allocated**
        // rather than logical for a container (task440, 計画書
        // R3): a container's length does not shrink when prune
        // punches its holes, so the logical size would report
        // space that was given back long ago.
        let bytes = ring_buffer::session_disk_bytes(&CaptureController::buffer_root(), id);
        // The recycle bin, not `remove_file` (task1520
        // follow-up): this is the one deletion a person aims
        // at a single recording by hand, and the mis-aimed one
        // that prompted this took 16 GB with no way back.
        match controller.discard_session(id, ring_buffer::Disposal::Recycle) {
            Ok(()) => {
                removed += 1;
                freed += bytes;
            }
            Err(_) => failed += 1,
        }
    }
    let freed_text = sessions::format_bytes(freed);
    if failed > 0 {
        // A partial run still reports what it freed: the point
        // of the whole screen is reclaiming disk (task116).
        let _ = tx.send(Msg::Failed(sessions::bulk_discard_partial_error(
            tr_locale(),
            ids.len(),
            failed,
            &freed_text,
        )));
    } else {
        let _ = tx.send(Msg::Notice(sessions::discarded_text(
            tr_locale(),
            removed,
            &freed_text,
        )));
    }
}

/// Opens the folder a session lives in, reporting why when it cannot.
fn open_session_directory(controller: &CaptureController, tx: &Sender<Msg>, session_id: &str) {
    // A container session is one file, so "show in folder"
    // opens the buffer root with that file selected rather than
    // opening a folder of segments the user no longer has
    // (計画書 R6). `SHOpenFolderAndSelectItems` already takes an
    // item; only the caller had never had one to give.
    let opened = match controller.session_container_path(session_id) {
        Some(container) => container
            .parent()
            .and_then(|folder| reveal_items(folder, &[&container]).ok())
            .is_some(),
        None => controller
            .session_root(session_id)
            .ok()
            .and_then(|root| open_folder(&root).ok())
            .is_some(),
    };
    if !opened {
        let _ = tx.send(Msg::Failed((
            sessions::open_directory_error(tr_locale()).into(),
            String::new(),
        )));
    }
}

/// Session-side worker: listing walks every directory in the buffer and reads
/// each manifest, discarding deletes files, and both would freeze the window.
/// Commands are handled one at a time, in order.
pub(crate) fn spawn_session_worker(
    controller: CaptureController,
    settings: Arc<Mutex<AppSettings>>,
    rx: Receiver<Cmd>,
    tx: Sender<Msg>,
) {
    thread::spawn(move || {
        let list = |tx: &Sender<Msg>| {
            let listed = controller
                .list_sessions()
                .map_err(|error| error.to_string());
            let _ = tx.send(Msg::History(HistoryMsg::Sessions(listed)));
        };
        while let Ok(command) = rx.recv() {
            // t261018-34fe: the settings screen's confirmed sweep is the same
            // sweep, told apart only by what it reports back.
            let confirmed = matches!(command, Cmd::ReclaimConfirmed(_));
            match command {
                Cmd::ListSessions => list(&tx),
                // Task2980: the clip folder's own scan. Here rather than on the
                // clip worker because the folder is a setting this thread
                // already holds, and reading a directory's metadata is the same
                // class of work as the session listing above.
                Cmd::ListClips => super::super::clips::scan(&settings, &tx),
                // An app icon for the history's app column (t260927-9a17).
                Cmd::AppIcon(path) => {
                    let pixels = std::path::Path::new(&path)
                        .is_file()
                        .then(|| super::super::app_meta::file_icon_rgba(&path))
                        .flatten();
                    let _ = tx.send(Msg::History(HistoryMsg::AppIcon(path, pixels)));
                }
                Cmd::Thumbnail(session_id, index) => {
                    let pixels = controller
                        .read_review_thumbnail(&session_id, index)
                        .ok()
                        .and_then(|jpeg| decode_rgba(&jpeg));
                    let _ = tx.send(Msg::History(HistoryMsg::SessionThumbnail(
                        session_id, pixels,
                    )));
                }
                Cmd::ReviewThumbnail(session_id, index) => {
                    let pixels = controller
                        .read_review_thumbnail(&session_id, index)
                        .ok()
                        .and_then(|jpeg| decode_rgba(&jpeg));
                    let _ = tx.send(Msg::ReviewThumbnail(index, pixels));
                }
                Cmd::Discard(ids) => {
                    discard_sessions(&controller, &tx, &ids);
                    list(&tx);
                }
                Cmd::Load(session_id) => {
                    report_loaded(&tx, controller.load_session(&session_id), LoadKind::Closed)
                }
                // Not `report_loaded`'s error path: that one files the failure
                // in the 履歴 pane, which nobody is looking at when the load
                // came from a double-click in Explorer. The reason travels
                // intact instead, to a toast (task570).
                // The catalog's reason is English and internal ("not a Liveback
                // session file (.lvb)"), so it goes to the log and the toast says
                // it plainly (t260928-e309 F9).
                Cmd::LoadFile(path) => match controller.load_session_file(&path) {
                    Ok(manifest) => report_loaded(&tx, Ok(manifest), LoadKind::Closed),
                    Err(error) => {
                        tracing::warn!(%error, "could not open the session file");
                        let _ = tx.send(Msg::Failed(sessions::file_open_failed(tr_locale())));
                    }
                },
                // LiveReview (task162): the recording session's own door, which
                // is the only load `load_session` would refuse mid-recording.
                Cmd::LoadActive(session_id) => report_loaded(
                    &tx,
                    match session_id {
                        // A history LIVE row names the recording it is (task2030).
                        Some(session_id) => controller.load_active_session_by_id(&session_id),
                        None => controller.load_active_session(),
                    },
                    LoadKind::Live,
                ),
                Cmd::LoadStarted => {
                    report_loaded(&tx, controller.load_active_session(), LoadKind::Fresh)
                }
                Cmd::OpenDirectory(session_id) => {
                    open_session_directory(&controller, &tx, &session_id);
                }
                Cmd::Reclaim(loaded_session_id) | Cmd::ReclaimConfirmed(loaded_session_id) => {
                    // Auto-repair before reclaim (task164): a crash-interrupted
                    // session becomes an ordinary recording and unplayable
                    // debris is discarded, so the history has only two states
                    // left to show. Never while capturing -- the live session
                    // looks exactly like a recoverable one, and "recovering" it
                    // would close a manifest the capture worker still owns.
                    let mut relist = false;
                    if !controller.is_active() {
                        match ring_buffer::repair_sessions(&CaptureController::buffer_root()) {
                            Ok(changed) => relist = changed > 0,
                            Err(error) => {
                                tracing::warn!(?error, "startup repair sweep failed")
                            }
                        }
                    }
                    // Skip the sweep outright if the settings mutex is
                    // poisoned: a (0, 0) fallback would read as "0 GB budget,
                    // 0-day lifetime" and doom every unprotected session.
                    let Ok((capacity_bytes, lifetime_days)) =
                        settings.lock().map(|current| current.retention_limits())
                    else {
                        continue;
                    };
                    // Housekeeping never takes the UI over with an error the
                    // user cannot act on, and a no-op sweep must not turn into
                    // a second listing on every launch.
                    let report = controller
                        .reclaim_sessions(capacity_bytes, lifetime_days, loaded_session_id)
                        .unwrap_or_default();
                    // Task700: the report used to be flattened to a bool here
                    // and the rest thrown away, which is why two sessions could
                    // disappear with the history quietly one shorter and
                    // nothing said. Only the capacity share raises a toast --
                    // aging out is the setting working as configured.
                    // The user just confirmed this one on the settings screen,
                    // so it answers there rather than with the toast.
                    if confirmed {
                        let _ = tx.send(Msg::RetentionApplied(report.removed_sessions));
                    } else if let Some(notice) = sessions::capacity_reclaim_notice(
                        tr_locale(),
                        report.over_capacity_sessions,
                        report.over_capacity_bytes,
                    ) {
                        let _ = tx.send(Msg::CapacityReclaimed(notice.0, notice.1));
                    }
                    if report.removed_sessions > 0 || relist {
                        list(&tx);
                    }
                }
            }
        }
    });
}

/// Session thumbnails come off disk as JPEG (the same sidecar the WebView build
/// fetched over its custom protocol); decoding is the expensive part and stays
/// on the worker.
fn decode_rgba(jpeg: &[u8]) -> Option<super::super::Pixels> {
    let decoded = image::load_from_memory(jpeg).ok()?.to_rgba8();
    Some((decoded.width(), decoded.height(), decoded.into_raw()))
}

// `directory_bytes` lived here, summing `metadata().len()` over a session
// directory by hand. A session is one `.lvb` now (t261025-d118), and its size
// is `ring_buffer::container::allocated_bytes`, which `list_sessions` reads
// into `SessionSummary::total_bytes`.
