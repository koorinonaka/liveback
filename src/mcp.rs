//! The MCP server (t261009-5ebd): with the settings switch on, a running
//! Liveback answers MCP on `http://127.0.0.1:<port>/mcp`, so an editor or an
//! assistant on the same machine can read sessions and markers and export a
//! range as mp4. Three tools, none of which deletes or edits anything.
//!
//! Transport is hand-written HTTP/1.1 over `std::net` plus JSON-RPC, answered
//! with a single `application/json` body -- no SSE, no keep-alive. There is no
//! async runtime in this crate and three tools do not need one.
//!
//! One thread per connection: `export_range` blocks until its job ends, which
//! can be minutes, and `list_sessions` must still answer meanwhile.
//!
//! Trust boundary: bound to loopback only, and a request whose `Origin` is not
//! loopback is refused with 403 (DNS rebinding). Every tool argument is checked
//! and a bad one comes back as an `isError` result.

use std::collections::HashSet;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::capture::CaptureController;
use crate::export::{AudioMix, ExportController, ExportRequest, ExportedPart};
use crate::ring_buffer::{self, SessionManifest, SessionSummary};
use crate::settings::AppSettings;
use crate::ui_state::sessions::session_epoch_millis;
use crate::ui_state::timeline::{min_frame_100ns, TimelineSnapshot, HNS_PER_SECOND};

/// MCP protocol versions this server speaks, newest first. An unknown one is
/// answered with the first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Caps on one request's head and body.
const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 8 << 20;

/// What the tools read and run. [`Live`] is the app's; the tests put a
/// buffer folder of their own behind it.
pub trait Backend: Send + Sync {
    /// Every session in the buffer, as the history lists them.
    fn sessions(&self) -> Result<Vec<SessionSummary>, String>;
    /// One session's manifest. `Err` when it is not in the catalog.
    fn manifest(&self, session_id: &str) -> Result<SessionManifest, String>;
    /// Runs one export to the end.
    fn export(&self, request: ExportRequest) -> Result<Vec<ExportedPart>, String>;
}

/// The running app: its one `CaptureController`, its one `ExportController`
/// (a second would race the first over the staging folder), and the settings
/// the audio mix is read from.
pub struct Live {
    pub capture: CaptureController,
    pub exporter: ExportController,
    pub settings: Arc<Mutex<AppSettings>>,
}

impl Backend for Live {
    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        ring_buffer::list_sessions(&CaptureController::buffer_root()).map_err(|e| e.to_string())
    }

    fn manifest(&self, session_id: &str) -> Result<SessionManifest, String> {
        self.capture.session_manifest(session_id)
    }

    fn export(&self, mut request: ExportRequest) -> Result<Vec<ExportedPart>, String> {
        // The same mix the review screen's export builds: each track's saved
        // level, never the master. `mix_only` is the caller's.
        let settings = self
            .settings
            .lock()
            .map(|settings| settings.clone())
            .unwrap_or_default();
        let executable = self
            .capture
            .session_manifest(&request.session_id)
            .ok()
            .and_then(|manifest| manifest.target_executable);
        let key = crate::settings::target_audio_key(executable.as_deref(), executable.is_none());
        request.audio_mix = AudioMix {
            levels: crate::export::track_levels(
                &settings,
                key.as_deref(),
                &self.exporter.session_audio_tracks(&request.session_id),
            ),
            mix_only: request.audio_mix.mix_only,
        };
        self.exporter.run(request)
    }
}

/// A listening server. Dropping it stops accepting and frees the port;
/// requests already being answered finish on their own threads.
pub struct Server {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    /// Listens on `127.0.0.1:port`. A port already taken is the `Err`.
    pub fn start(port: u16, backend: Arc<dyn Backend>) -> io::Result<Self> {
        Self::serve(TcpListener::bind(("127.0.0.1", port))?, backend)
    }

    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    fn serve(listener: TcpListener, backend: Arc<dyn Backend>) -> io::Result<Self> {
        let addr = listener.local_addr()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let accept = std::thread::Builder::new()
            .name("mcp-server".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    if stopping.load(Ordering::Acquire) {
                        break;
                    }
                    let Ok(stream) = stream else { continue };
                    let backend = backend.clone();
                    let _ = std::thread::Builder::new()
                        .name("mcp-connection".into())
                        .spawn(move || connection(stream, &*backend));
                }
            })?;
        Ok(Self {
            addr,
            stop,
            accept: Some(accept),
        })
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // `accept` blocks; one connection of our own wakes it to see the flag.
        // Joined only when that connection landed, so the port is free by the
        // time this returns (an off-and-on must not find it still taken) and
        // a wake that failed cannot hang the caller.
        if TcpStream::connect_timeout(&self.addr, Duration::from_secs(1)).is_ok() {
            if let Some(accept) = self.accept.take() {
                let _ = accept.join();
            }
        }
    }
}

fn connection(stream: TcpStream, backend: &dyn Backend) -> io::Result<()> {
    // A client that sends nothing must not hold a thread forever.
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let (status, body) = route(&mut reader, backend);
    respond(stream, status, &body)
}

/// Reads one request and answers (HTTP status, body).
fn route(reader: &mut impl BufRead, backend: &dyn Backend) -> (u16, String) {
    let Some((method, path, headers)) = read_head(reader) else {
        return (400, String::new());
    };
    let header = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let len = header("content-length")
        .and_then(|v| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if len > MAX_BODY {
        return (413, String::new());
    }
    // Read the body before deciding: closing with it unread makes Windows
    // answer RST, and the client never sees the status.
    let mut body = vec![0; len];
    if reader.read_exact(&mut body).is_err() {
        return (400, String::new());
    }
    if path.split('?').next() != Some("/mcp") {
        return (404, String::new());
    }
    if !origin_allowed(header("origin")) {
        return (403, String::new());
    }
    if method != "POST" {
        // No GET (an SSE stream) and no DELETE (session end): 405 is allowed.
        return (405, String::new());
    }
    let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
        let err = json!({"jsonrpc": "2.0", "id": null,
            "error": {"code": -32700, "message": "parse error"}});
        return (400, err.to_string());
    };
    // Notifications (no id) and responses are only acknowledged.
    if !msg.is_object()
        || msg.get("id").is_none()
        || msg.get("result").is_some()
        || msg.get("error").is_some()
    {
        return (202, String::new());
    }
    (200, dispatch(&msg, backend).to_string())
}

type Head = (String, String, Vec<(String, String)>);

/// The request line and headers; `None` when malformed or too long.
fn read_head(reader: &mut impl BufRead) -> Option<Head> {
    let mut total = 0;
    let mut line = String::new();
    let mut next = |line: &mut String| -> Option<()> {
        line.clear();
        total += reader.read_line(line).ok()?;
        (total <= MAX_HEAD && !line.is_empty()).then_some(())
    };
    next(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut headers = Vec::new();
    loop {
        next(&mut line)?;
        let l = line.trim_end();
        if l.is_empty() {
            return Some((method, path, headers));
        }
        let (k, v) = l.split_once(':')?;
        headers.push((k.trim().to_owned(), v.trim().to_owned()));
    }
}

fn respond(mut stream: TcpStream, status: u16, body: &str) -> io::Result<()> {
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Payload Too Large",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if status == 405 {
        head.push_str("Allow: POST\r\n");
    }
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

/// No `Origin` (not a browser), or a loopback http one.
pub fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    ["http://127.0.0.1", "http://localhost"]
        .iter()
        .any(|host| match origin.strip_prefix(host) {
            Some("") => true,
            Some(rest) => rest
                .strip_prefix(':')
                .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())),
            None => false,
        })
}

// --- JSON-RPC ---------------------------------------------------------------

/// Answers one JSON-RPC request.
pub fn dispatch(msg: &Value, backend: &dyn Backend) -> Value {
    let id = msg.get("id").cloned().unwrap_or(Value::Null);
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Ok(initialize(msg)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => Ok(call(msg.get("params"), backend)),
        _ => Err(format!("method not found: {method}")),
    };
    match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err(message) => json!({"jsonrpc": "2.0", "id": id,
            "error": {"code": -32601, "message": message}}),
    }
}

fn initialize(msg: &Value) -> Value {
    let asked = msg["params"]["protocolVersion"].as_str();
    let version = asked
        .filter(|v| VERSIONS.contains(v))
        .unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "liveback", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Liveback keeps a rolling recording of the screen as sessions. These \
             tools read the sessions and their markers and export a range of a session as mp4 \
             into a folder you name. Times are seconds (fractions allowed) from the start of \
             the session's oldest remaining segment -- the same origin the review timeline \
             uses. Nothing here deletes or edits a recording.",
    })
}

fn call(params: Option<&Value>, backend: &dyn Backend) -> Value {
    let name = params
        .and_then(|p| p.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let args = params
        .and_then(|p| p.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let out = match name {
        "list_sessions" => list_sessions(backend),
        "list_markers" => list_markers(backend, &args),
        "export_range" => export_range(backend, &args),
        _ => Err(format!("unknown tool: {name}")),
    };
    let (content, is_error) = match out {
        Ok(value) => (
            json!([{ "type": "text", "text": value.to_string() }]),
            false,
        ),
        Err(message) => (json!([{ "type": "text", "text": message }]), true),
    };
    json!({ "content": content, "isError": is_error })
}

fn seconds(span_100ns: i64) -> f64 {
    span_100ns as f64 / HNS_PER_SECOND as f64
}

fn to_100ns(seconds: f64) -> i64 {
    (seconds * HNS_PER_SECOND as f64).round() as i64
}

fn text_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, String> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("`{key}` (a string) is required"))
}

fn seconds_arg(args: &Value, key: &str) -> Result<f64, String> {
    args.get(key)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("`{key}` (a number of seconds) is required"))
}

fn manifest_of(backend: &dyn Backend, session: &str) -> Result<SessionManifest, String> {
    backend.manifest(session).map_err(|error| {
        format!(
            "session `{session}` cannot be read ({error}). A session whose `closed` is false \
             is still recording and cannot be read until the recording stops."
        )
    })
}

/// Newest first; a session that cannot be read is left out.
fn list_sessions(backend: &dyn Backend) -> Result<Value, String> {
    let mut rows: Vec<SessionSummary> = backend
        .sessions()?
        .into_iter()
        .filter(|session| session.readable)
        .collect();
    rows.sort_by(|a, b| {
        (session_epoch_millis(&b.session_id), &b.session_id)
            .cmp(&(session_epoch_millis(&a.session_id), &a.session_id))
    });
    let sessions: Vec<Value> = rows
        .iter()
        .map(|s| {
            let duration = match (s.start_100ns, s.end_100ns) {
                (Some(start), Some(end)) => seconds((end - start).max(0)),
                _ => 0.0,
            };
            json!({
                "id": s.session_id,
                "title": s.target_title,
                "game": s.target_executable,
                "started_at_ms": session_epoch_millis(&s.session_id),
                "duration_s": duration,
                "closed": s.closed,
                "marker_count": s.marker_count,
                "note": s.note,
            })
        })
        .collect();
    Ok(json!({ "sessions": sessions }))
}

/// The markers the review screen shows: only those on recorded time, in time
/// order, with the review timeline's origin.
fn list_markers(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let manifest = manifest_of(backend, session)?;
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let origin = snapshot.start_100ns();
    let times: Vec<i64> = manifest.markers.iter().map(|m| m.time_100ns).collect();
    let kept: HashSet<i64> = snapshot.recorded_markers(&times).into_iter().collect();
    let mut markers: Vec<_> = manifest
        .markers
        .iter()
        .filter(|m| kept.contains(&m.time_100ns))
        .collect();
    markers.sort_by_key(|m| m.time_100ns);
    let markers: Vec<Value> = markers
        .iter()
        .map(|m| {
            json!({
                "time_s": seconds(m.time_100ns - origin),
                "time_100ns": m.time_100ns,
                "label": m.label,
                "comment": m.comment,
            })
        })
        .collect();
    Ok(json!({ "session": session, "markers": markers }))
}

/// The name a session's files are given: its window title, else its
/// executable, else the app's own name. The export refuses an empty one.
fn game_name(manifest: &SessionManifest) -> String {
    [&manifest.target_title, &manifest.target_executable]
        .into_iter()
        .flatten()
        .map(|name| name.trim())
        .find(|name| !name.is_empty())
        .unwrap_or("Liveback")
        .to_owned()
}

/// Checks the arguments and builds the request: seconds from the review
/// origin, clamped onto the recording the way the review range is.
fn export_request(
    manifest: &SessionManifest,
    args: &Value,
) -> Result<(ExportRequest, i64), String> {
    let start_s = seconds_arg(args, "start_s")?;
    let end_s = seconds_arg(args, "end_s")?;
    let output_dir = PathBuf::from(text_arg(args, "output_dir")?);
    let mix_only = match args.get("mix_only") {
        None | Some(Value::Null) => false,
        Some(value) => value.as_bool().ok_or("`mix_only` must be a boolean")?,
    };
    if !output_dir.is_absolute() {
        return Err("`output_dir` must be an absolute path".into());
    }
    if start_s >= end_s {
        return Err("`start_s` must be less than `end_s`".into());
    }
    let snapshot = TimelineSnapshot::from_manifest(manifest);
    let origin = snapshot.start_100ns();
    // Past the end is pulled back to the live edge, as the review range's
    // own end is; `clamp_range` then snaps both edges onto recorded time.
    let end = (origin.saturating_add(to_100ns(end_s))).min(snapshot.live_edge_100ns - 1);
    let (start, end) = snapshot
        .clamp_range(
            origin.saturating_add(to_100ns(start_s)),
            end,
            min_frame_100ns(),
        )
        .ok_or("the range does not overlap the recording")?;
    Ok((
        ExportRequest {
            session_id: manifest.session_id.clone(),
            start_100ns: start,
            end_100ns: end,
            game_name: game_name(manifest),
            output_file: None,
            output_directory: Some(output_dir),
            audio_mix: AudioMix {
                levels: Vec::new(),
                mix_only,
            },
        },
        origin,
    ))
}

fn export_range(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let manifest = manifest_of(backend, session)?;
    let (request, origin) = export_request(&manifest, args)?;
    let parts: Vec<Value> = backend
        .export(request)?
        .iter()
        .map(|part| {
            json!({
                "path": part.path.display().to_string(),
                "start_s": seconds(part.start_100ns - origin),
                "end_s": seconds(part.end_100ns - origin),
            })
        })
        .collect();
    Ok(json!({ "parts": parts }))
}

fn tools() -> Value {
    json!([
        {
            "name": "list_sessions",
            "description": "The recorded sessions, newest first: id, window title, executable \
                 (game), start time (Unix ms), duration in seconds, whether recording has \
                 stopped (closed), marker count and the user's note.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "list_markers",
            "description": "A session's markers in time order: time_s (seconds from the \
                 session's oldest remaining segment, as the review timeline shows it), the \
                 exact time_100ns, the label and the comment (multi-line, empty when none). Markers outside the recorded time are left out. \
                 A session still recording (closed: false) cannot be read until it stops.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." }
                },
                "required": ["session"],
            },
        },
        {
            "name": "export_range",
            "description": "Export a range of a session as mp4 into output_dir and wait until \
                 it is written. Existing files are never overwritten. A range that crosses a \
                 gap or a resolution change comes out as several parts; each part reports the \
                 start_s and end_s it actually holds, clamped onto the recording. A session \
                 with one audio track keeps it as is. With two or more, audio track 0 is a mix \
                 at each track's saved volume, followed by every original track unless \
                 mix_only is true. Fails at once \
                 while another export is running, and for a session still recording \
                 (closed: false).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." },
                    "start_s": { "type": "number", "description": "Range start, seconds on the list_markers origin." },
                    "end_s": { "type": "number", "description": "Range end, seconds on the same origin." },
                    "output_dir": { "type": "string", "description": "Absolute folder path. Created if missing; its parent must exist." },
                    "mix_only": { "type": "boolean", "description": "Write only the mixed audio track, without the original tracks. Default false. No effect on a session with one audio track." }
                },
                "required": ["session", "start_s", "end_s", "output_dir"],
            },
        },
    ])
}

#[cfg(test)]
mod tests;
