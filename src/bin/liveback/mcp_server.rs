//! The settings switch behind the MCP server (t261009-5ebd). The server itself
//! is `livia::mcp`; this starts it at launch when the switch is on, starts and
//! stops it with the switch, and puts the outcome on the row's second line --
//! the URL, or the port another app already holds (a second Liveback, an
//! isolated test instance) without failing the app.

use std::cell::{Cell, RefCell};
use std::sync::{Arc, Mutex};

use livia::capture::targets::{list_capture_targets, CaptureTarget, CaptureTargetKind};
use livia::capture::CaptureController;
use livia::clips::{ClipDetails, ClipEntry, FrameReader};
use livia::export::{AudioMix, ExportController, ExportRequest, ExportedPart};
use livia::mcp::{video_size_of, Backend, Frame, RecordTarget, Server};
use livia::ring_buffer::{MarkerRecord, SessionManifest, SessionSummary};
use livia::settings::AppSettings;
use livia::ui_state::export_failure;
use livia::ui_state::settings as settings_ui;

use super::settings_page::{commit_settings, settings_snapshot};
use super::{tr_locale, AppWindow, Cmd, Msg, SettingsLabels, SettingsVm};
use crossbeam_channel::Sender;
use slint::ComponentHandle;

/// The running app: its one `CaptureController`, its one `ExportController`
/// (a second would race the first over the staging folder), the settings
/// the audio mix is read from, and the two ways back to the screens a write
/// leaves stale (t261103-d3d4): the worker that re-lists the history and the
/// clips, and the window, for the review screen's own copy of the markers.
struct Live {
    capture: CaptureController,
    exporter: ExportController,
    settings: Arc<Mutex<AppSettings>>,
    cmd_tx: Sender<Cmd>,
    ui: slint::Weak<AppWindow>,
    /// To the UI thread, for the start and stop (t261103-a803): the `starting`
    /// guard and the stop requests live there.
    tx: Sender<Msg>,
}

/// What a start may take before the connection gives up on the answer.
const START_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// The windows the hotkey would record: a window with an executable id that
/// the picker would list, none of this process's own.
fn recordable(target: &CaptureTarget) -> bool {
    target.kind == CaptureTargetKind::Window
        && target.selectable
        && target.executable_id.is_some()
        && target.process_id != std::process::id()
}

impl Backend for Live {
    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        self.capture.list_sessions().map_err(|e| e.to_string())
    }

    fn manifest(&self, session_id: &str) -> Result<SessionManifest, String> {
        self.capture.session_manifest(session_id)
    }

    fn export(&self, mut request: ExportRequest) -> Result<Vec<ExportedPart>, String> {
        // The same mix the review screen's export builds: each track's saved
        // level, never the master. `mix_only` is the caller's.
        let settings = self.settings_now();
        let executable = self
            .capture
            .session_manifest(&request.session_id)
            .ok()
            .and_then(|manifest| manifest.target_executable);
        let key = livia::settings::target_audio_key(executable.as_deref(), executable.is_none());
        request.audio_mix = AudioMix {
            levels: livia::export::track_levels(
                &settings,
                key.as_deref(),
                &self.exporter.session_audio_tracks(&request.session_id),
            ),
            mix_only: request.audio_mix.mix_only,
        };
        self.exporter
            .run(request)
            .map_err(|failure| export_failure::text(&failure, tr_locale()))
    }

    fn queued_markers(&self, session_id: &str) -> Vec<MarkerRecord> {
        self.capture.queued_markers_for(session_id)
    }

    fn segment_sizes(
        &self,
        session_id: &str,
        indexes: &[u64],
    ) -> Result<Vec<Option<(u32, u32)>>, String> {
        self.with_segments(session_id, indexes, |bytes| Ok(video_size_of(bytes)))
    }

    fn frame(
        &self,
        session_id: &str,
        index: u64,
        local_100ns: i64,
        max_width: u32,
    ) -> Result<Option<Frame>, String> {
        let bytes = self
            .with_segments(session_id, &[index], |bytes| Ok(bytes.to_vec()))?
            .remove(0);
        livia::com::init_mta();
        let _runtime = livia::encoder::MfRuntime::start().map_err(|e| format!("{e:?}"))?;
        let stream = livia::playback::segment_byte_stream(&bytes)?;
        let mut reader = FrameReader::from_byte_stream(&stream)?;
        let max = (max_width, u32::MAX);
        // Segment timestamps can be absolute (hours in), as the hover preview
        // found: the first frame is the segment's zero. ponytail: decodes from
        // the segment's start every call, fine for 1-2s segments.
        let first = reader
            .next_frame(i64::MIN, max)
            .ok_or("the segment has no frame")?;
        let base = first.0;
        // 1ms early, as `FrameReader::frame_at` asks.
        let from = base + local_100ns - 10_000;
        let found = if first.0 >= from {
            Some(first)
        } else {
            reader.next_frame(from, max)
        };
        Ok(found.map(|(at, width, height, rgba)| (at - base, width, height, rgba)))
    }

    fn clips(&self) -> Result<Vec<(ClipEntry, ClipDetails)>, String> {
        let settings = self.settings_now();
        let directory = livia::clips::clip_directory(&settings)?;
        // The apartment the clips pane reads details in.
        livia::com::init_sta();
        Ok(livia::clips::list(&directory)?
            .into_iter()
            .map(|entry| {
                let details = livia::clips::details(&entry.path);
                (entry, details)
            })
            .collect())
    }

    fn add_marker(
        &self,
        session_id: &str,
        time_100ns: i64,
        label: &str,
        comment: Option<&str>,
    ) -> Result<(), String> {
        // Two calls, as the review screen's add-then-name: `add_marker`
        // always adds an unnamed one.
        self.capture.add_marker(session_id, time_100ns)?;
        if !label.is_empty() || comment.is_some() {
            self.capture.update_marker(
                session_id,
                time_100ns,
                label.to_owned(),
                comment.map(str::to_owned),
            )?;
        }
        let _ = self.cmd_tx.send(Cmd::ListSessions);
        let (session_id, label) = (session_id.to_owned(), label.to_owned());
        let comment = comment.unwrap_or_default().to_owned();
        let _ = self.ui.upgrade_in_event_loop(move |ui| {
            super::review_wiring::marker_added_elsewhere(
                &ui,
                &session_id,
                time_100ns,
                label,
                comment,
            )
        });
        Ok(())
    }

    fn set_session_comment(&self, session_id: &str, comment: &str) -> Result<(), String> {
        // The review screen shows no session comment, so only the history
        // is left stale.
        self.capture.set_session_comment(session_id, comment)?;
        let _ = self.cmd_tx.send(Cmd::ListSessions);
        Ok(())
    }

    fn protect_session(&self, session_id: &str) -> Result<(), String> {
        self.capture.set_session_protected(session_id, true)?;
        let _ = self.cmd_tx.send(Cmd::ListSessions);
        Ok(())
    }

    /// The review screen's clip save (`review_stage::wire_clip_save` and
    /// `write_clip_metadata`), run to the end on this thread. The review
    /// screen holds no `clip_pending` for this job, so nothing is written twice.
    fn save_clip(
        &self,
        mut request: ExportRequest,
        comment: Option<&str>,
    ) -> Result<(Vec<ExportedPart>, Option<String>), String> {
        let settings = self.settings_now();
        let directory = livia::clips::clip_directory(&settings)?;
        // The pipeline refuses an output path whose parent does not exist.
        let _ = std::fs::create_dir_all(&directory);
        request.output_file = Some(livia::clips::allocate_clip_path(
            &directory,
            &request.game_name,
        ));
        let manifest = self.capture.session_manifest(&request.session_id).ok();
        let parts = self.export(request)?;
        // The shell property store, in the apartment the UI thread writes it from.
        livia::com::init_sta();
        let app = manifest.and_then(|manifest| {
            super::app_meta::recorded_icon_path(
                manifest.target_executable.as_deref(),
                manifest.target_executable_path.as_deref(),
                &settings.auto_capture_executables,
            )
        });
        let mut warning = None;
        for part in &parts {
            let shown = part.path.display().to_string();
            if let Some(app) = app.as_deref() {
                // Quietly, as the review screen does: only the monogram falls back.
                if let Err(error) = livia::clips::write_app_path(&part.path, app) {
                    let error = error.replace(&shown, "<clip>");
                    tracing::warn!(%error, "mcp clip app path");
                }
            }
            if let Some(comment) = comment {
                if let Err(error) = livia::clips::write_comment(&part.path, comment) {
                    let detail = error.detail.replace(&shown, "<clip>");
                    tracing::warn!(%detail, "mcp clip comment");
                    warning = Some(error.detail);
                }
            }
        }
        let _ = self.cmd_tx.send(Cmd::ListClips);
        Ok((parts, warning))
    }

    fn recordings(&self) -> Vec<(String, String)> {
        self.capture.active_targets()
    }

    fn stopping(&self) -> bool {
        self.capture.is_stopping()
    }

    fn targets(&self) -> Result<Vec<RecordTarget>, String> {
        Ok(list_capture_targets()?
            .into_iter()
            .filter(recordable)
            .map(|target| RecordTarget {
                hwnd: target.window_handle,
                title: target.title,
                executable: target.executable_name.unwrap_or_default(),
            })
            .collect())
    }

    fn start_recording(&self, hwnd: &str) -> Result<String, String> {
        let target = list_capture_targets()?
            .into_iter()
            .find(|target| recordable(target) && target.window_handle.eq_ignore_ascii_case(hwnd))
            .ok_or_else(|| format!("`{hwnd}` is not a window that can be recorded"))?;
        let executable_id = target.executable_id.clone().unwrap_or_default();
        tracing::info!(window = %hwnd, "mcp start_recording");
        let (reply, answer) = std::sync::mpsc::channel();
        self.tx
            .send(Msg::McpStart {
                target,
                executable_id,
                reply,
            })
            .map_err(|_| "the app is closing".to_owned())?;
        answer
            .recv_timeout(START_WAIT)
            .map_err(|_| "the recording did not start in time".to_owned())?
    }

    fn stop_recording(&self, session_id: &str) {
        let _ = self.tx.send(Msg::McpStop(session_id.to_owned()));
    }
}

impl Live {
    fn settings_now(&self) -> AppSettings {
        self.settings
            .lock()
            .map(|settings| settings.clone())
            .unwrap_or_default()
    }

    /// Reads the named segments under one review lease, held only for the
    /// reads (a lease makes `discard_session` refuse the session), and hands
    /// each one's bytes to `each`.
    fn with_segments<T>(
        &self,
        session_id: &str,
        indexes: &[u64],
        each: impl Fn(&[u8]) -> Result<T, String>,
    ) -> Result<Vec<T>, String> {
        let lease = self
            .capture
            .acquire_review_lease(Some(session_id.to_owned()), indexes.to_vec())?;
        let read = indexes
            .iter()
            .map(|index| each(&self.capture.read_review_segment(&lease, *index)?))
            .collect();
        self.capture.release_review_lease(&lease);
        read
    }
}

/// What the row's second line says.
#[derive(Clone, Copy)]
enum Line {
    Off,
    Listening(u16),
    Taken(u16),
}

thread_local! {
    /// Dropping it closes the port.
    static SERVER: RefCell<Option<Server>> = const { RefCell::new(None) };
    static LINE: Cell<Line> = const { Cell::new(Line::Off) };
}

/// The row's labels, re-pushed on a language change like the rest.
pub(super) fn push_labels(ui: &AppWindow) {
    let labels = ui.global::<SettingsLabels>();
    let locale = tr_locale();
    labels.set_mcp_server(settings_ui::mcp_server(locale).into());
    let line = match LINE.get() {
        Line::Off => settings_ui::mcp_server_sub(locale).to_owned(),
        Line::Listening(port) => settings_ui::mcp_listening(locale, port),
        Line::Taken(port) => settings_ui::mcp_port_taken(locale, port),
    };
    labels.set_mcp_server_sub(line.into());
}

/// Brings the server in line with `settings`. Always stops first, so an
/// off-and-on retry after a taken port binds afresh.
fn apply(ui: &AppWindow, settings: &AppSettings, backend: &Arc<Live>) {
    SERVER.with(|server| server.borrow_mut().take());
    let port = settings.mcp_port;
    let line = if !settings.mcp_enabled {
        Line::Off
    } else {
        match Server::start(port, backend.clone()) {
            Ok(server) => {
                tracing::info!(port, "mcp server listening");
                SERVER.with(|slot| *slot.borrow_mut() = Some(server));
                Line::Listening(port)
            }
            Err(error) => {
                tracing::warn!(port, %error, "mcp server could not bind");
                Line::Taken(port)
            }
        }
    };
    LINE.set(line);
    push_labels(ui);
}

pub(super) fn wire(
    ui: &AppWindow,
    settings: &Arc<Mutex<AppSettings>>,
    controller: &CaptureController,
    exporter: &ExportController,
    (cmd_tx, tx): (&Sender<Cmd>, &Sender<Msg>),
) {
    // The app's own controllers: a second `ExportController` would run beside
    // the review screen's and delete its staging folder.
    let backend = Arc::new(Live {
        capture: controller.clone(),
        exporter: exporter.clone(),
        settings: Arc::clone(settings),
        cmd_tx: cmd_tx.clone(),
        ui: ui.as_weak(),
        tx: tx.clone(),
    });
    let current = settings_snapshot(settings);
    let vm = ui.global::<SettingsVm>();
    vm.set_mcp_enabled(current.mcp_enabled);
    apply(ui, &current, &backend);

    let weak = ui.as_weak();
    let settings = Arc::clone(settings);
    vm.on_mcp_toggled(move |enabled| {
        let Some(ui) = weak.upgrade() else { return };
        let next = commit_settings(&settings, |current| {
            current.mcp_enabled = enabled;
        });
        ui.global::<SettingsVm>().set_mcp_enabled(next.mcp_enabled);
        apply(&ui, &next, &backend);
    });
}
