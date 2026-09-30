use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crate::ui_state::export_failure as msg;
use crate::ui_state::locale::active as ui_locale;
use serde::{Deserialize, Serialize};

use super::part_writer::export_part_reporting;
use super::{ExportDiagnostics, ExportFailure, ExportPart};

// Export helper子プロセス契約:
// - 起動: 自分自身を `--livia-export-helper` 引数付きで再起動する(`run_helper_if_requested`)。
// - stdin: `ExportHelperRequest` を JSON 1 個 + 改行で 1 回だけ渡す。
// - stdout: NDJSON で `ExportHelperMessage` を流す。順序は
//   `Heartbeat`(`HELPER_HEARTBEAT`=250ms間隔で任意回数) → `Phase`(任意) → `Result`(必ず最後に1回)。
// - 親側タイムアウト: feed中は `HELPER_FEED_TIMEOUT`=15s、finalize中は `HELPER_FINALIZE_TIMEOUT`=60s、
//   いずれもハートビートが途切れたら親が異常終了とみなす。
// - 終了コード: `Result{ok:true}` → 0、それ以外(`Result{ok:false}` や `Phase` で終わる異常系) → 1。
// - 失敗時のクリーンアップ責任は `PublishGuard`(`committed` が立たない限り Drop で partial/final を削除)。
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportHelperRequest {
    part: ExportPart,
    partial: PathBuf,
    /// Which language the part writer's failures come back in. The child has
    /// no settings of its own, and the strings it produces are what the review
    /// screen shows, so the parent sends its own locale rather than let the
    /// child default to `Ja`.
    locale: crate::ui_state::locale::Locale,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum ExportHelperMessage {
    Heartbeat,
    Phase {
        finalize: bool,
    },
    /// How far into this part the writer has got, in 100ns from the part's
    /// start (task1350). Throttled by the helper -- see `HELPER_PROGRESS`.
    Progress {
        into_part_100ns: i64,
    },
    Result {
        ok: bool,
        diagnostics: Option<ExportDiagnostics>,
        error: Option<String>,
    },
}

const HELPER_HEARTBEAT: Duration = Duration::from_millis(250);
/// How often the helper is allowed to report progress inside a part. A sample
/// is a few milliseconds of media, so reporting each one would put thousands of
/// lines a second down a pipe whose reader also drives the UI; 150ms is well
/// under what a moving bar needs and well over what the loop costs.
const HELPER_PROGRESS: Duration = Duration::from_millis(150);
const HELPER_FEED_TIMEOUT: Duration = Duration::from_secs(15);
const HELPER_FINALIZE_TIMEOUT: Duration = Duration::from_secs(60);

pub fn run_helper_if_requested() -> bool {
    if !std::env::args_os().any(|arg| arg == "--livia-export-helper") {
        return false;
    }
    // The helper returns before `configure_logging`, so without this its
    // failure details (`ui_state::export_failure`) would go nowhere. stderr,
    // never stdout: stdout is the JSON channel. The parent logs what arrives.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .try_init();
    let result = (|| -> Result<ExportDiagnostics, ExportFailure> {
        let request: ExportHelperRequest =
            serde_json::from_reader(std::io::stdin()).map_err(|error| {
                ExportFailure::new(msg::helper_request_malformed(ui_locale(), error))
            })?;
        // Before anything else the child can fail at: from here on its own
        // `ui_locale()` answers the same as the parent's. The one message above
        // is the exception -- a request that will not parse cannot say which
        // language to refuse it in, so that one keeps the default.
        crate::ui_state::locale::set_active(request.locale);
        let heartbeat_stop = Arc::new(AtomicBool::new(false));
        let heartbeat_stop_worker = heartbeat_stop.clone();
        let output = Arc::new(Mutex::new(std::io::stdout()));
        let heartbeat_output = output.clone();
        let heartbeat = thread::spawn(move || {
            while !heartbeat_stop_worker.load(Ordering::Acquire) {
                let message = serde_json::to_string(&ExportHelperMessage::Heartbeat).unwrap();
                if let Ok(mut output) = heartbeat_output.lock() {
                    let _ = writeln!(output, "{message}");
                    let _ = output.flush();
                }
                thread::sleep(HELPER_HEARTBEAT);
            }
        });
        let mut diagnostics = ExportDiagnostics::default();
        let phase_output = output.clone();
        let phase = |finalize| {
            let message = serde_json::to_string(&ExportHelperMessage::Phase { finalize }).unwrap();
            if let Ok(mut output) = phase_output.lock() {
                let _ = writeln!(output, "{message}");
                let _ = output.flush();
            }
        };
        let progress_output = output.clone();
        let last_progress = Mutex::new(Instant::now() - HELPER_PROGRESS);
        let progress = |into_part_100ns: i64| {
            if let Ok(mut last) = last_progress.lock() {
                if last.elapsed() < HELPER_PROGRESS {
                    return;
                }
                *last = Instant::now();
            }
            let message =
                serde_json::to_string(&ExportHelperMessage::Progress { into_part_100ns }).unwrap();
            if let Ok(mut output) = progress_output.lock() {
                let _ = writeln!(output, "{message}");
                let _ = output.flush();
            }
        };
        let result = export_part_reporting(
            &request.part,
            &request.partial,
            Path::new("helper-does-not-publish"),
            &AtomicBool::new(false),
            &mut diagnostics,
            Some(&phase),
            Some(&progress),
        )
        .map(|_| diagnostics);
        heartbeat_stop.store(true, Ordering::Release);
        let _ = heartbeat.join();
        result
    })();
    let message = match result {
        Ok(diagnostics) => ExportHelperMessage::Result {
            ok: true,
            diagnostics: Some(diagnostics),
            error: None,
        },
        Err(error) => ExportHelperMessage::Result {
            ok: false,
            diagnostics: None,
            error: Some(error.message),
        },
    };
    let _ = writeln!(
        std::io::stdout(),
        "{}",
        serde_json::to_string(&message).unwrap()
    );
    let _ = std::io::stdout().flush();
    std::process::exit(match message {
        ExportHelperMessage::Result { ok: true, .. } => 0,
        _ => 1,
    });
}

/// Drains the helper's stderr on its own thread, so an OS-level crash (no
/// panic message, no `Result` message) still surfaces something to report.
fn capture_stderr(child: &mut std::process::Child) -> Arc<Mutex<String>> {
    let captured = Arc::new(Mutex::new(String::new()));
    if let Some(stderr) = child.stderr.take() {
        let captured = captured.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Ok(mut captured) = captured.lock() {
                    captured.push_str(&line);
                    captured.push('\n');
                }
            }
        });
    }
    captured
}

/// Kills the helper and removes the partial file it was writing. Cancellation
/// and the heartbeat timeout both end this way.
fn abort_helper(child: &mut std::process::Child, partial: &Path) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(partial);
}

/// What the helper's last message means for the caller. No `Result` message at
/// all means it died without going through its own error-reporting path, so the
/// exit status and whatever it wrote to stderr are all there is to report.
fn helper_outcome(
    received: &Arc<Mutex<Option<ExportHelperMessage>>>,
    child: &mut std::process::Child,
    stderr_captured: &Arc<Mutex<String>>,
) -> Result<ExportDiagnostics, ExportFailure> {
    match received.lock().ok().and_then(|mut message| message.take()) {
        Some(ExportHelperMessage::Result {
            ok: true,
            diagnostics: Some(diagnostics),
            ..
        }) => Ok(diagnostics),
        Some(ExportHelperMessage::Result {
            error: Some(error), ..
        }) => {
            // The detail behind the helper's plain sentence is on its stderr.
            // ponytail: waits for exit, not for the reader thread's last line;
            // join the thread if lines go missing.
            let _ = child.wait();
            let stderr = stderr_captured
                .lock()
                .map(|captured| captured.clone())
                .unwrap_or_default();
            tracing::warn!(stderr = %stderr.trim(), "export helper failed");
            Err(ExportFailure::new(msg::helper_failed(ui_locale(), error)))
        }
        _ => {
            // No ExportHelperMessage::Result arrived at all: the helper died
            // without going through its own error-reporting path (an OS-level
            // crash, not a Rust panic or a normal error return). Report the
            // exit status and any captured stderr, since previously this
            // case surfaced no information beyond a bare "結果を返さず終了
            // しました".
            let status = child
                .wait()
                .map(|status| status.to_string())
                .unwrap_or_else(|error| msg::helper_wait_failed(ui_locale(), error));
            let stderr = stderr_captured
                .lock()
                .map(|captured| captured.clone())
                .unwrap_or_default();
            let stderr_suffix = if stderr.trim().is_empty() {
                String::new()
            } else {
                format!(", stderr={}", stderr.trim())
            };
            Err(ExportFailure::new(msg::helper_exited_without_result(
                ui_locale(),
                status,
                stderr_suffix,
            )))
        }
    }
}

pub(super) fn export_part_in_helper(
    part: &ExportPart,
    partial: &Path,
    cancel: &AtomicBool,
    progress: &dyn Fn(i64),
) -> Result<ExportDiagnostics, ExportFailure> {
    let executable = std::env::current_exe()
        .map_err(|error| ExportFailure::new(msg::helper_path_failed(ui_locale(), error)))?;
    let mut child = Command::new(executable)
        .arg("--livia-export-helper")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| ExportFailure::new(msg::helper_spawn_failed(ui_locale(), error)))?;
    // Captures stderr so an OS-level crash (no panic message, no
    // ExportHelperMessage::Result) at least surfaces raw stderr output plus
    // the exit status, instead of the previous bare "結果を返さず終了しました"
    // with no diagnostic information at all.
    let stderr_captured = capture_stderr(&mut child);
    let request = ExportHelperRequest {
        part: part.clone(),
        partial: partial.to_path_buf(),
        locale: ui_locale(),
    };
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| ExportFailure::new(msg::helper_stdin_failed(ui_locale())))?;
    serde_json::to_writer(&mut stdin, &request)
        .map_err(|error| ExportFailure::new(msg::helper_request_send_failed(ui_locale(), error)))?;
    stdin
        .write_all(b"\n")
        .map_err(|error| ExportFailure::new(msg::helper_request_send_failed(ui_locale(), error)))?;
    drop(stdin);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ExportFailure::new(msg::helper_stdout_failed(ui_locale())))?;
    let last_heartbeat = Arc::new(Mutex::new(Instant::now()));
    let timeout = Arc::new(Mutex::new(HELPER_FEED_TIMEOUT));
    let received = Arc::new(Mutex::new(None));
    let reader_heartbeat = last_heartbeat.clone();
    let reader_timeout = timeout.clone();
    let reader_received = received.clone();
    // The reader thread cannot hold the caller's closure (it is not `Send`),
    // so progress arrives through a slot the main loop below drains.
    let reported = Arc::new(Mutex::new(None::<i64>));
    let reader_reported = reported.clone();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            match serde_json::from_str::<ExportHelperMessage>(&line) {
                Ok(ExportHelperMessage::Heartbeat) => {
                    if let Ok(mut latest) = reader_heartbeat.lock() {
                        *latest = Instant::now();
                    }
                }
                Ok(ExportHelperMessage::Phase { finalize }) => {
                    if let Ok(mut timeout) = reader_timeout.lock() {
                        *timeout = if finalize {
                            HELPER_FINALIZE_TIMEOUT
                        } else {
                            HELPER_FEED_TIMEOUT
                        };
                    }
                }
                Ok(ExportHelperMessage::Progress { into_part_100ns }) => {
                    if let Ok(mut slot) = reader_reported.lock() {
                        *slot = Some(into_part_100ns);
                    }
                }
                Ok(result @ ExportHelperMessage::Result { .. }) => {
                    if let Ok(mut slot) = reader_received.lock() {
                        *slot = Some(result);
                    }
                    break;
                }
                Err(_) => break,
            }
        }
    });
    loop {
        // Whatever the helper last said, at the loop's own pace rather than
        // the helper's: one publish per turn, not one per message.
        if let Some(into_part_100ns) = reported.lock().ok().and_then(|mut slot| slot.take()) {
            progress(into_part_100ns);
        }
        if cancel.load(Ordering::Acquire) {
            abort_helper(&mut child, partial);
            return Err(ExportFailure::cancelled());
        }
        let timeout_limit = timeout
            .lock()
            .map(|value| *value)
            .unwrap_or(HELPER_FEED_TIMEOUT);
        let elapsed = last_heartbeat
            .lock()
            .map(|latest| latest.elapsed())
            .unwrap_or(timeout_limit);
        if elapsed > timeout_limit {
            abort_helper(&mut child, partial);
            return Err(ExportFailure::new(msg::helper_heartbeat_stopped(
                ui_locale(),
            )));
        }
        if child
            .try_wait()
            .map_err(|error| ExportFailure::new(msg::helper_status_failed(ui_locale(), error)))?
            .is_some()
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    let _ = reader.join();
    helper_outcome(&received, &mut child, &stderr_captured)
}
