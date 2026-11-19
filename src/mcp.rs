//! The MCP server (t261009-5ebd): with the settings switch on, a running
//! Liveback answers MCP on `http://127.0.0.1:<port>/mcp`, so an editor or an
//! assistant on the same machine can read sessions and markers and export a
//! range as mp4. t261103-7cac added `get_session`, `get_frame` and
//! `list_clips`; t261103-d3d4 added four that write: `add_marker`,
//! `set_session_comment`, `protect_session` and `save_clip`. None deletes
//! anything, none renames or overwrites an existing marker, and none lifts a
//! protection -- that one is left to the app, since it puts a session back in
//! line for the automatic delete. t261103-a803 added `get_status`,
//! `start_recording` and `stop_recording`; a recording started here leaves the
//! review screen where the user has it.
//!
//! Transport is hand-written HTTP/1.1 over `std::net` plus JSON-RPC, answered
//! with a single `application/json` body -- no SSE, no keep-alive. There is no
//! async runtime in this crate and these tools do not need one.
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
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde_json::{json, Value};

use crate::clips::{ClipDetails, ClipEntry};
use crate::export::{AudioMix, ExportRequest, ExportedPart};
use crate::ring_buffer::session_epoch_millis;
use crate::ring_buffer::{MarkerRecord, SegmentRecord, SessionManifest, SessionSummary};
use crate::timeline::{min_frame_100ns, TimelineSnapshot, HNS_PER_SECOND};

/// MCP protocol versions this server speaks, newest first. An unknown one is
/// answered with the first.
const VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];

/// Caps on one request's head and body.
const MAX_HEAD: usize = 16 << 10;
const MAX_BODY: usize = 8 << 20;

/// One decoded frame: `(local_100ns, width, height, rgba)`.
pub type Frame = (i64, u32, u32, Vec<u8>);

/// What the tools read and run. The app's is the binary's `Live` (`mcp_server`); the tests put a
/// buffer folder of their own behind it.
pub trait Backend: Send + Sync {
    /// Every session in the buffer, as the history lists them.
    fn sessions(&self) -> Result<Vec<SessionSummary>, String>;
    /// One session's manifest. `Err` when it is not in the catalog.
    fn manifest(&self, session_id: &str) -> Result<SessionManifest, String>;
    /// Runs one export to the end.
    fn export(&self, request: ExportRequest) -> Result<Vec<ExportedPart>, String>;
    /// Markers queued for a recording session that its index writer has not
    /// folded into the manifest yet. Empty for anything else.
    fn queued_markers(&self, session_id: &str) -> Vec<MarkerRecord>;
    /// Each named segment's video size (`tkhd`, see [`video_size_of`]), in
    /// the order asked; `None` where it cannot be read.
    fn segment_sizes(
        &self,
        session_id: &str,
        indexes: &[u64],
    ) -> Result<Vec<Option<(u32, u32)>>, String>;
    /// The first frame of one segment at or after `local_100ns` (from that
    /// segment's first frame), as `(local_100ns, width, height, rgba)` scaled
    /// down to at most `max_width` wide. `None` when the segment has no frame
    /// that late: its manifest end runs past its last frame.
    fn frame(
        &self,
        session_id: &str,
        index: u64,
        local_100ns: i64,
        max_width: u32,
    ) -> Result<Option<Frame>, String>;
    /// The saved clips with their details, in any order.
    fn clips(&self) -> Result<Vec<(ClipEntry, ClipDetails)>, String>;
    /// Adds a marker at `time_100ns` with this label and, when `Some`, this
    /// comment. The caller has checked that none is there yet.
    fn add_marker(
        &self,
        session_id: &str,
        time_100ns: i64,
        label: &str,
        comment: Option<&str>,
    ) -> Result<(), String>;
    fn set_session_comment(&self, session_id: &str, comment: &str) -> Result<(), String>;
    /// Protects the session. There is no way to lift it from here.
    fn protect_session(&self, session_id: &str) -> Result<(), String>;
    /// Runs `request` into a new file in the clip folder, as the review
    /// screen's clip save does, and writes `comment` into every part. The
    /// second value is why the comment could not be written, if it was not;
    /// the clip is saved all the same.
    fn save_clip(
        &self,
        request: ExportRequest,
        comment: Option<&str>,
    ) -> Result<(Vec<ExportedPart>, Option<String>), String>;
    /// Every capture running right now as `(session id, window handle)`.
    fn recordings(&self) -> Vec<(String, String)>;
    /// Some capture's stop is still being flushed.
    fn stopping(&self) -> bool;
    /// The windows that can be recorded, Liveback's own left out.
    fn targets(&self) -> Result<Vec<RecordTarget>, String>;
    /// Starts recording the window with this handle, with the app's settings
    /// and without moving the review screen. Answers the new session's id.
    fn start_recording(&self, hwnd: &str) -> Result<String, String>;
    /// Asks for the stop, as the app's own stop buttons do; it lands later.
    fn stop_recording(&self, session_id: &str);
}

/// A window `start_recording` can take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordTarget {
    /// `0x…`, the way `get_status` and the capture name it.
    pub hwnd: String,
    pub title: String,
    pub executable: String,
}

/// How long `stop_recording` waits for the session to close.
const STOP_WAIT: Duration = Duration::from_secs(10);
const STOP_POLL: Duration = Duration::from_millis(200);

fn hwnd_value(text: &str) -> Option<usize> {
    let hex = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))?;
    usize::from_str_radix(hex, 16).ok()
}

/// The one target `window` names: `0x…` is a handle, anything else a
/// case-blind part of the title or the executable name. None or several is an
/// error, the several listed.
pub fn match_window<'a>(
    targets: &'a [RecordTarget],
    window: &str,
) -> Result<&'a RecordTarget, String> {
    let window = window.trim();
    if window.is_empty() {
        return Err("`window` must not be empty".into());
    }
    let found: Vec<&RecordTarget> = match hwnd_value(window) {
        Some(handle) => targets
            .iter()
            .filter(|t| hwnd_value(&t.hwnd) == Some(handle))
            .collect(),
        None => {
            let needle = window.to_lowercase();
            targets
                .iter()
                .filter(|t| {
                    t.title.to_lowercase().contains(&needle)
                        || t.executable.to_lowercase().contains(&needle)
                })
                .collect()
        }
    };
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no recordable window matches `{window}`; get_status lists them"
        )),
        many => Err(format!(
            "`{window}` matches {} windows; pass one hwnd: {}",
            many.len(),
            many.iter()
                .map(|t| json!({ "hwnd": t.hwnd, "title": t.title, "executable": t.executable }))
                .collect::<Value>()
        )),
    }
}

/// Polls `closed` until it is true or `deadline` has passed.
pub fn wait_closed(mut closed: impl FnMut() -> bool, deadline: Duration, step: Duration) -> bool {
    let until = std::time::Instant::now() + deadline;
    loop {
        if closed() {
            return true;
        }
        if std::time::Instant::now() >= until {
            return false;
        }
        std::thread::sleep(step);
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
             uses. You can add markers, set a session's comment, protect a session and save a \
             range as a clip, and start and stop recording a window. Nothing here deletes anything, changes a marker that is already \
             there or lifts a protection.",
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
    let text = |value: Value| json!([{ "type": "text", "text": value.to_string() }]);
    let out = match name {
        "list_sessions" => list_sessions(backend).map(text),
        "list_markers" => list_markers(backend, &args).map(text),
        "export_range" => export_range(backend, &args).map(text),
        "get_session" => get_session(backend, &args).map(text),
        "get_frame" => get_frame(backend, &args),
        "list_clips" => list_clips(backend).map(text),
        "add_marker" => add_marker(backend, &args).map(text),
        "set_session_comment" => set_session_comment(backend, &args).map(text),
        "protect_session" => protect_session(backend, &args).map(text),
        "save_clip" => save_clip(backend, &args).map(text),
        "get_status" => get_status(backend).map(text),
        "start_recording" => start_recording(backend, &args).map(text),
        "stop_recording" => stop_recording(backend, &args).map(text),
        _ => Err(format!("unknown tool: {name}")),
    };
    let (content, is_error) = match out {
        Ok(content) => (content, false),
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
    backend
        .manifest(session)
        .map_err(|error| format!("session `{session}` cannot be read ({error})."))
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
                "note": s.comment,
            })
        })
        .collect();
    Ok(json!({ "sessions": sessions }))
}

/// The markers the review screen shows: only those on recorded time, in time
/// order, with the review timeline's origin.
///
/// A recording session (t261103-7cac) also gets the markers still queued for
/// its index writer, and keeps those past its last indexed segment: a marker
/// pressed just now lies ahead of the segment that will cover it.
fn list_markers(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    // The queue before the manifest: a marker folded in between is then in
    // both, never in neither.
    let queued = backend.queued_markers(session);
    let mut manifest = manifest_of(backend, session)?;
    merge_queued(&mut manifest.markers, queued);
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let origin = snapshot.start_100ns();
    let times: Vec<i64> = manifest.markers.iter().map(|m| m.time_100ns).collect();
    let kept: HashSet<i64> = snapshot.recorded_markers(&times).into_iter().collect();
    let live_edge = snapshot.live_edge_100ns;
    let mut markers: Vec<_> = manifest
        .markers
        .iter()
        .filter(|m| {
            kept.contains(&m.time_100ns)
                || (!manifest.closed && !snapshot.segments.is_empty() && m.time_100ns >= live_edge)
        })
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

/// Adds the queued markers the manifest does not have yet. `time_100ns` is a
/// marker's identity, as in `RingBuffer::merge_markers`.
fn merge_queued(markers: &mut Vec<MarkerRecord>, queued: Vec<MarkerRecord>) {
    for marker in queued {
        if !markers.iter().any(|m| m.time_100ns == marker.time_100ns) {
            markers.push(marker);
        }
    }
}

/// The recording split where a gap lies between two segments -- the cut
/// `export_range` makes -- as each span's first and last segment.
fn spans(manifest: &SessionManifest) -> Vec<(&SegmentRecord, &SegmentRecord)> {
    let mut segments: Vec<&SegmentRecord> = manifest.segments.iter().collect();
    segments.sort_by_key(|s| s.start_100ns);
    let mut spans: Vec<(&SegmentRecord, &SegmentRecord)> = Vec::new();
    for segment in segments {
        match spans.last_mut() {
            Some((_, last))
                if !manifest.gaps.iter().any(|gap| {
                    gap.start_100ns >= last.end_100ns && gap.end_100ns <= segment.start_100ns
                }) =>
            {
                *last = segment
            }
            _ => spans.push((segment, segment)),
        }
    }
    spans
}

fn get_session(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let manifest = manifest_of(backend, session)?;
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let origin = snapshot.start_100ns();
    // The history's own numbers: `total_bytes` is the container's allocated size.
    let summary = backend
        .sessions()?
        .into_iter()
        .find(|s| s.session_id == session);
    let spans = spans(&manifest);
    // Only each span's two ends: on an HDD buffer a segment read is ~180ms,
    // so reading every segment of a long session would take minutes.
    let mut indexes: Vec<u64> = spans
        .iter()
        .flat_map(|(first, last)| [first.index, last.index])
        .collect();
    indexes.dedup();
    let sizes: std::collections::HashMap<u64, Option<(u32, u32)>> = indexes
        .iter()
        .copied()
        .zip(backend.segment_sizes(session, &indexes)?)
        .collect();
    let size = |index: u64| sizes.get(&index).copied().flatten();
    let spans: Vec<Value> = spans
        .iter()
        .map(|(first, last)| {
            let (width, height) =
                size(first.index).map_or((Value::Null, Value::Null), |(w, h)| (json!(w), json!(h)));
            json!({
                "start_s": seconds(first.start_100ns - origin),
                "end_s": seconds(last.end_100ns - origin),
                "width": width,
                "height": height,
                "size_changes": size(first.index) != size(last.index),
            })
        })
        .collect();
    let gaps: Vec<Value> = manifest
        .gaps
        .iter()
        .map(|gap| {
            json!({
                "start_s": seconds(gap.start_100ns - origin),
                "end_s": seconds(gap.end_100ns - origin),
                "reason": gap.reason,
            })
        })
        .collect();
    let audio_tracks: Vec<Value> = manifest
        .audio_tracks
        .iter()
        .map(|track| json!({ "name": track.executable_name, "microphone": track.microphone.is_some() }))
        .collect();
    Ok(json!({
        "id": manifest.session_id,
        "title": manifest.target_title,
        "game": manifest.target_executable,
        "started_at_ms": session_epoch_millis(&manifest.session_id),
        "duration_s": seconds(snapshot.full_duration_100ns()),
        "closed": manifest.closed,
        "protected": manifest.protected,
        "comment": manifest.comment,
        "total_bytes": summary.as_ref().map_or(0, |s| s.total_bytes),
        "segment_count": summary.as_ref().map_or(manifest.segments.len(), |s| s.segment_count),
        "gaps": gaps,
        "audio_tracks": audio_tracks,
        "spans": spans,
    }))
}

/// `max_width` when nothing is asked: a 1080p PNG in base64 runs to megabytes.
const FRAME_MAX_WIDTH: u64 = 1280;

fn get_frame(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let time_s = seconds_arg(args, "time_s")?;
    if time_s < 0.0 {
        return Err("`time_s` must not be negative".into());
    }
    let max_width = match args.get("max_width") {
        None | Some(Value::Null) => FRAME_MAX_WIDTH,
        Some(value) => value
            .as_u64()
            .filter(|width| (1..=u32::MAX as u64).contains(width))
            .ok_or("`max_width` must be a positive whole number of pixels")?,
    } as u32;
    let manifest = manifest_of(backend, session)?;
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let origin = snapshot.start_100ns();
    let at = origin.saturating_add(to_100ns(time_s));
    let refuse = || not_recorded(time_s);
    if !on_recorded_time(&manifest, &snapshot, at) {
        return Err(refuse());
    }
    let at = snapshot.snap_to_recorded_time(at).ok_or_else(refuse)?;
    let segment = snapshot.find_covering_segment(at).ok_or_else(refuse)?;
    // A segment's manifest end can run past its last frame; then the frame is
    // the next segment's first, unless a gap lies between.
    let next = snapshot
        .segments
        .iter()
        .find(|s| s.start_100ns >= segment.end_100ns)
        .filter(|next| {
            !manifest.gaps.iter().any(|gap| {
                gap.start_100ns >= segment.end_100ns && gap.end_100ns <= next.start_100ns
            })
        });
    let (segment, (local, width, height, rgba)) =
        match backend.frame(session, segment.index, at - segment.start_100ns, max_width)? {
            Some(frame) => (segment, frame),
            None => {
                let next = next.ok_or("no frame at that time")?;
                let frame = backend
                    .frame(session, next.index, 0, max_width)?
                    .ok_or("no frame at that time")?;
                (next, frame)
            }
        };
    let png = png_of(width, height, rgba)?;
    let info = json!({
        "time_s": seconds(segment.start_100ns + local - origin),
        "width": width,
        "height": height,
    });
    Ok(json!([
        { "type": "image", "data": base64::engine::general_purpose::STANDARD.encode(&png), "mimeType": "image/png" },
        { "type": "text", "text": info.to_string() },
    ]))
}

fn not_recorded(time_s: f64) -> String {
    format!("time_s {time_s} is not on recorded time (a gap, or past the end)")
}

/// Whether `at` lies on the recording: from its start up to the live edge,
/// in no gap. Segments do not quite touch: a hole shorter than a gap (one
/// frame, at a segment boundary) is recorded time and snaps forward onto the
/// next segment, as the review range does. A gap is not.
fn on_recorded_time(manifest: &SessionManifest, snapshot: &TimelineSnapshot, at: i64) -> bool {
    at >= snapshot.start_100ns()
        && at < snapshot.live_edge_100ns
        && !manifest
            .gaps
            .iter()
            .any(|gap| gap.start_100ns <= at && at < gap.end_100ns)
        && snapshot.snap_to_recorded_time(at).is_some()
}

fn png_of(width: u32, height: u32, rgba: Vec<u8>) -> Result<Vec<u8>, String> {
    let image = image::RgbaImage::from_raw(width, height, rgba).ok_or("frame size mismatch")?;
    let mut png = Vec::new();
    image
        .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(png)
}

/// A segment's video size from its `moov`'s `tkhd`, through
/// `mp4_boxes::video_track_size` -- the size `export_range` splits on. A
/// container session's segment has no file of its own, so its bytes go to a
/// temporary one first, as the export stages them. ponytail: writes the whole
/// segment (1-8MB) to %TEMP%; a bytes-level reader if that ever shows.
pub fn video_size_of(bytes: &[u8]) -> Option<(u32, u32)> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "liveback-mcp-size-{}-{}.mp4",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, bytes).ok()?;
    let size = crate::encoder::mp4_boxes::video_track_size(&path)
        .ok()
        .flatten();
    let _ = std::fs::remove_file(&path);
    size
}

/// Newest first, by when the clip was cut (the clips pane's order).
fn list_clips(backend: &dyn Backend) -> Result<Value, String> {
    let mut clips = backend.clips()?;
    clips.sort_by(|(a, _), (b, _)| b.created.cmp(&a.created).then_with(|| a.name.cmp(&b.name)));
    let clips: Vec<Value> = clips
        .iter()
        .map(|(entry, details)| {
            let created_ms = entry
                .created
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |since| since.as_millis() as i64);
            let app = details.app_path.as_deref().and_then(|path| {
                std::path::Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
            json!({
                "path": entry.path.display().to_string(),
                "name": entry.name,
                "created_ms": created_ms,
                "bytes": entry.bytes,
                "duration_s": details.duration_100ns.map(seconds),
                "comment": details.comment,
                "app": app,
            })
        })
        .collect();
    Ok(json!({ "clips": clips }))
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
    let output_dir = PathBuf::from(text_arg(args, "output_dir")?);
    let mix_only = match args.get("mix_only") {
        None | Some(Value::Null) => false,
        Some(value) => value.as_bool().ok_or("`mix_only` must be a boolean")?,
    };
    if !output_dir.is_absolute() {
        return Err("`output_dir` must be an absolute path".into());
    }
    range_request(manifest, args, Some(output_dir), mix_only)
}

/// `start_s` / `end_s` checked and clamped into a request, and the origin
/// they were read on. Shared by `export_range` and `save_clip`.
fn range_request(
    manifest: &SessionManifest,
    args: &Value,
    output_directory: Option<PathBuf>,
    mix_only: bool,
) -> Result<(ExportRequest, i64), String> {
    let start_s = seconds_arg(args, "start_s")?;
    let end_s = seconds_arg(args, "end_s")?;
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
            output_directory,
            audio_mix: AudioMix {
                levels: Vec::new(),
                mix_only,
            },
        },
        origin,
    ))
}

fn parts_json(parts: &[ExportedPart], origin: i64) -> Vec<Value> {
    parts
        .iter()
        .map(|part| {
            json!({
                "path": part.path.display().to_string(),
                "start_s": seconds(part.start_100ns - origin),
                "end_s": seconds(part.end_100ns - origin),
            })
        })
        .collect()
}

fn export_range(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let manifest = manifest_of(backend, session)?;
    let (request, origin) = export_request(&manifest, args)?;
    Ok(json!({ "parts": parts_json(&backend.export(request)?, origin) }))
}

/// An optional string argument: absent or null is `None`.
fn optional_text<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(Some)
            .ok_or_else(|| format!("`{key}` must be a string")),
    }
}

/// Only on recorded time, and never onto a marker that is already there:
/// re-adding an instant would otherwise rename it, and this tool only adds.
fn add_marker(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let time_s = seconds_arg(args, "time_s")?;
    let label = optional_text(args, "label")?.unwrap_or("").trim();
    let comment = optional_text(args, "comment")?.and_then(crate::ring_buffer::normalize_comment);
    // The queue too, so a marker pressed a moment ago counts as there.
    let queued = backend.queued_markers(session);
    let mut manifest = manifest_of(backend, session)?;
    merge_queued(&mut manifest.markers, queued);
    let snapshot = TimelineSnapshot::from_manifest(&manifest);
    let origin = snapshot.start_100ns();
    let at = origin.saturating_add(to_100ns(time_s));
    if time_s < 0.0 || !on_recorded_time(&manifest, &snapshot, at) {
        return Err(not_recorded(time_s));
    }
    if manifest.markers.iter().any(|m| m.time_100ns == at) {
        return Err(format!(
            "a marker is already at time_s {time_s}; this tool does not change existing markers"
        ));
    }
    backend.add_marker(session, at, label, comment.as_deref())?;
    Ok(json!({
        "time_s": seconds(at - origin),
        "time_100ns": at,
        "label": label,
        "comment": comment.unwrap_or_default(),
    }))
}

fn set_session_comment(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let comment = text_arg(args, "comment")?;
    let comment = crate::ring_buffer::normalize_session_comment(comment).unwrap_or_default();
    manifest_of(backend, session)?;
    backend.set_session_comment(session, &comment)?;
    Ok(json!({ "session": session, "comment": comment }))
}

fn protect_session(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    manifest_of(backend, session)?;
    backend.protect_session(session)?;
    Ok(json!({ "session": session, "protected": true }))
}

/// A clip is the mix alone, as the review screen's clip save makes it.
fn save_clip(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    let comment = optional_text(args, "comment")?
        .map(str::trim)
        .filter(|comment| !comment.is_empty());
    let manifest = manifest_of(backend, session)?;
    let (request, origin) = range_request(&manifest, args, None, true)?;
    let (parts, warning) = backend.save_clip(request, comment)?;
    let mut out = json!({
        "parts": parts_json(&parts, origin),
        "comment_written": comment.is_some() && warning.is_none(),
    });
    if let Some(warning) = warning {
        out["warning"] = json!(format!(
            "the clip is saved, but its comment was not: {warning}"
        ));
    }
    Ok(out)
}

fn target_json(target: &RecordTarget) -> Value {
    json!({ "hwnd": target.hwnd, "title": target.title, "executable": target.executable })
}

/// Elapsed is wall time since the session id's own start instant.
fn get_status(backend: &dyn Backend) -> Result<Value, String> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let recordings: Vec<Value> = backend
        .recordings()
        .into_iter()
        .map(|(session, hwnd)| {
            let manifest = backend.manifest(&session).ok();
            let elapsed =
                session_epoch_millis(&session).map(|start| (now_ms - start).max(0) as f64 / 1000.0);
            json!({
                "session": session,
                "hwnd": hwnd,
                "title": manifest.as_ref().and_then(|m| m.target_title.clone()),
                "executable": manifest.and_then(|m| m.target_executable),
                "elapsed_s": elapsed,
            })
        })
        .collect();
    let targets: Vec<Value> = backend.targets()?.iter().map(target_json).collect();
    Ok(json!({
        "recordings": recordings,
        "stopping": backend.stopping(),
        "targets": targets,
    }))
}

fn start_recording(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let window = text_arg(args, "window")?;
    let targets = backend.targets()?;
    let target = match_window(&targets, window)?;
    let handle = hwnd_value(&target.hwnd);
    if let Some((session, _)) = backend
        .recordings()
        .into_iter()
        .find(|(_, hwnd)| hwnd_value(hwnd) == handle)
    {
        return Err(format!(
            "`{}` is already recording as session `{session}`",
            target.hwnd
        ));
    }
    let session = backend.start_recording(&target.hwnd)?;
    let mut out = target_json(target);
    out["session"] = json!(session);
    Ok(out)
}

fn stop_recording(backend: &dyn Backend, args: &Value) -> Result<Value, String> {
    let session = text_arg(args, "session")?;
    if !backend.recordings().iter().any(|(id, _)| id == session) {
        return Err(format!("session `{session}` is not recording"));
    }
    backend.stop_recording(session);
    let closed = wait_closed(|| recording_closed(backend, session), STOP_WAIT, STOP_POLL);
    Ok(if closed {
        json!({ "session": session, "closed": true })
    } else {
        json!({ "session": session, "closed": false, "stopping": true })
    })
}

/// Out of the running set and its manifest says closed.
fn recording_closed(backend: &dyn Backend, session: &str) -> bool {
    !backend.recordings().iter().any(|(id, _)| id == session)
        && backend.manifest(session).is_ok_and(|m| m.closed)
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
                 A session still recording (closed: false) also lists the markers pressed in the \
                 last moments, ahead of the recorded time.",
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
        {
            "name": "get_session",
            "description": "One session in detail: id, title, game, started_at_ms, duration_s, \
                 closed, protected, comment, total_bytes (on disk), segment_count, the gaps \
                 (start_s, end_s, reason), the audio tracks (name, and microphone: true for a \
                 microphone) and spans: the recording cut at each gap, each with start_s, end_s \
                 and the video width and height at its start. size_changes is true when a \
                 span ends at another size than it starts; export_range then splits it where \
                 it changes. Seconds are on the list_markers origin. Works while recording.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." }
                },
                "required": ["session"],
            },
        },
        {
            "name": "get_frame",
            "description": "The picture of a session at a time, as a PNG image: the first frame \
                 at or after time_s, scaled down (never up) to at most max_width pixels wide. \
                 A text part gives that frame's own time_s, width and height. A time in a gap \
                 or past the end is an error. Works while recording, up to the last segment \
                 written.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." },
                    "time_s": { "type": "number", "description": "Seconds on the list_markers origin, 0 or more." },
                    "max_width": { "type": "integer", "description": "Largest width in pixels. Default 1280." }
                },
                "required": ["session", "time_s"],
            },
        },
        {
            "name": "list_clips",
            "description": "The clips saved in the clip folder, newest first: path, file name, \
                 created_ms (Unix ms), bytes, duration_s, the user's comment and app (the \
                 recorded executable's file name). duration_s, comment and app are null when \
                 the file does not carry them.",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "add_marker",
            "description": "Add a marker to a session at time_s, with an optional label and \
                 comment. The time must be on recorded time (not in a gap, not past the end), \
                 and no marker may be there already: this never changes an existing marker. \
                 Returns the new marker in list_markers' shape. Works while recording.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." },
                    "time_s": { "type": "number", "description": "Seconds on the list_markers origin, 0 or more." },
                    "label": { "type": "string", "description": "The marker's name. Default empty." },
                    "comment": { "type": "string", "description": "The marker's comment, multi-line allowed. Default none." }
                },
                "required": ["session", "time_s"],
            },
        },
        {
            "name": "set_session_comment",
            "description": "Set a session's comment (the note list_sessions shows), replacing \
                 the one it has. Works while recording.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." },
                    "comment": { "type": "string", "description": "The comment, not empty." }
                },
                "required": ["session", "comment"],
            },
        },
        {
            "name": "protect_session",
            "description": "Protect a session, so the automatic delete that keeps the buffer \
                 inside its size and age limits leaves it alone. A protection can only be \
                 lifted in the app. Works while recording.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." }
                },
                "required": ["session"],
            },
        },
        {
            "name": "save_clip",
            "description": "Save a range of a session as a clip in the app's clip folder (it \
                 then shows in list_clips and in the app's clip list) and wait until it is \
                 written. The clip holds the mixed audio only. A range that crosses a gap or \
                 a resolution change comes out as several parts, each with the start_s and \
                 end_s it actually holds; every part gets the comment. comment_written is \
                 false when no comment was given or it could not be written (then warning \
                 says why; the clip is saved all the same). Fails at once while another \
                 export or clip is running, and for a session still recording (closed: false).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from list_sessions." },
                    "start_s": { "type": "number", "description": "Range start, seconds on the list_markers origin." },
                    "end_s": { "type": "number", "description": "Range end, seconds on the same origin." },
                    "comment": { "type": "string", "description": "The clip's comment. Default none." }
                },
                "required": ["session", "start_s", "end_s"],
            },
        },
        {
            "name": "get_status",
            "description": "What is recording now and what can be recorded. recordings: each \
                 running capture's session id, hwnd, title, executable and elapsed_s (seconds \
                 since it started). stopping is true while a stop is still being written out. \
                 targets: the windows start_recording can take (hwnd, title, executable).",
            "inputSchema": { "type": "object", "properties": {} },
        },
        {
            "name": "start_recording",
            "description": "Start recording one window with the app's recording settings. \
                 window is an hwnd (0x...) from get_status, or a part of the window's title or \
                 executable name (case ignored). No match or more than one match is an error \
                 that lists the matches, and nothing starts; so is a window already \
                 recording. The app's screen does not change. Returns the new session id \
                 with the window's hwnd, title and executable.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "window": { "type": "string", "description": "An hwnd from get_status targets, or part of a title or executable name." }
                },
                "required": ["window"],
            },
        },
        {
            "name": "stop_recording",
            "description": "Stop a recording and wait up to 10 seconds for the session to \
                 close. Returns closed: true once it is closed, or closed: false with \
                 stopping: true when it is still being written out (list_sessions shows it \
                 closed later). A session that is not recording is an error.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "session": { "type": "string", "description": "Session id from get_status recordings." }
                },
                "required": ["session"],
            },
        },
    ])
}

#[cfg(test)]
mod tests;
