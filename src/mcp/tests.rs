//! MCP server tests (t261009-5ebd). The sessions are real `.lvb` containers in
//! a temp folder, read through the same `list_sessions` the history uses; the
//! export is a stand-in, since a real one cannot run under `cargo test` (the
//! helper process is the test binary).

use super::*;
use crate::ring_buffer;
use crate::ring_buffer::container::{ContainerBuilder, MetaEvent};
use std::io::Read;
use std::sync::{mpsc, Mutex};

const S: i64 = HNS_PER_SECOND;
/// Older, with markers; its first remaining segment starts at 5s.
const OLD: &str = "capture-00000000000000000000000000000001";
/// Newer, no markers.
const NEW: &str = "capture-00000000000000000000000000000002";

struct TmpRoot(PathBuf);

impl Drop for TmpRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    root: TmpRoot,
    asked: Mutex<Vec<ExportRequest>>,
    answer: Result<Vec<ExportedPart>, String>,
    /// When set, `export` waits for one message on it before answering.
    gate: Option<Mutex<mpsc::Receiver<()>>>,
}

impl Backend for Fixture {
    fn sessions(&self) -> Result<Vec<SessionSummary>, String> {
        ring_buffer::list_sessions(&self.root.0).map_err(|e| e.to_string())
    }

    fn manifest(&self, session_id: &str) -> Result<SessionManifest, String> {
        let path = self.root.0.join(format!("{session_id}.lvb"));
        if !path.is_file() {
            return Err("no such session".into());
        }
        let ring = ring_buffer::RingBuffer::open_container(path).map_err(|e| e.to_string())?;
        Ok(ring.manifest().clone())
    }

    fn export(&self, request: ExportRequest) -> Result<Vec<ExportedPart>, String> {
        self.asked.lock().unwrap().push(request);
        if let Some(gate) = &self.gate {
            let _ = gate.lock().unwrap().recv();
        }
        self.answer.clone()
    }
}

fn marker(time_100ns: i64, label: &str) -> MetaEvent {
    MetaEvent::MarkerAdded {
        time_100ns,
        label: label.into(),
        palette_index: None,
        comment: None,
    }
}

fn fixture(name: &str, answer: Result<Vec<ExportedPart>, String>) -> Fixture {
    let root = std::env::temp_dir().join(format!("livia-mcp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    ContainerBuilder::new(OLD)
        .segment(0, 5 * S, 20 * S, &[0xA0; 64])
        .segment(1, 20 * S, 40 * S, &[0xA1; 64])
        .meta(marker(30 * S, ""))
        .meta(marker(10 * S, "boss"))
        // t261009-0251: list_markers carries the comment.
        .meta(MetaEvent::MarkerRenamed {
            time_100ns: 10 * S,
            label: "boss".into(),
            comment: Some("slow here\nthen cut".into()),
        })
        .meta(marker(25 * S, ""))
        // Past the recording's end: the review screen does not show it.
        .meta(marker(100 * S, "gone"))
        .checkpoint()
        .closed()
        .build(&root.join(format!("{OLD}.lvb")))
        .unwrap();
    ContainerBuilder::new(NEW)
        .segment(0, 0, 3 * S, &[0xA2; 64])
        .checkpoint()
        .closed()
        .build(&root.join(format!("{NEW}.lvb")))
        .unwrap();
    Fixture {
        root: TmpRoot(root),
        asked: Mutex::new(Vec::new()),
        answer,
        gate: None,
    }
}

fn rpc(method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })
}

/// `tools/call` -> (the tool's JSON, or its error text, and isError).
fn call_tool(backend: &dyn Backend, name: &str, args: Value) -> (Value, bool) {
    let reply = dispatch(
        &rpc("tools/call", json!({ "name": name, "arguments": args })),
        backend,
    );
    let result = &reply["result"];
    let text = result["content"][0]["text"].as_str().unwrap().to_owned();
    let is_error = result["isError"].as_bool().unwrap();
    let value = if is_error {
        Value::String(text)
    } else {
        serde_json::from_str(&text).unwrap()
    };
    (value, is_error)
}

#[test]
fn initialize_and_tools_list_and_unknown_names() {
    let f = fixture("init", Ok(vec![]));
    let reply = dispatch(
        &rpc("initialize", json!({ "protocolVersion": "2025-03-26" })),
        &f,
    );
    assert_eq!(reply["id"], 1);
    assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(reply["result"]["serverInfo"]["name"], "liveback");
    assert!(reply["result"]["capabilities"]["tools"].is_object());
    // An unknown version is answered with the newest.
    let reply = dispatch(
        &rpc("initialize", json!({ "protocolVersion": "1999-01-01" })),
        &f,
    );
    assert_eq!(reply["result"]["protocolVersion"], VERSIONS[0]);
    assert_eq!(dispatch(&rpc("ping", json!({})), &f)["result"], json!({}));

    let reply = dispatch(&rpc("tools/list", json!({})), &f);
    let mut names: Vec<&str> = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(names, ["export_range", "list_markers", "list_sessions"]);

    let reply = dispatch(&rpc("resources/list", json!({})), &f);
    assert_eq!(reply["error"]["code"], -32601);
    for name in ["delete_session", "add_marker", ""] {
        let (text, err) = call_tool(&f, name, json!({}));
        assert!(err, "{name}");
        assert!(text.as_str().unwrap().contains("unknown tool"), "{text}");
    }
}

#[test]
fn sessions_come_newest_first_and_markers_on_the_review_origin() {
    let f = fixture("lists", Ok(vec![]));
    let (listed, err) = call_tool(&f, "list_sessions", json!({}));
    assert!(!err, "{listed}");
    let sessions = listed["sessions"].as_array().unwrap();
    let ids: Vec<&str> = sessions.iter().map(|s| s["id"].as_str().unwrap()).collect();
    assert_eq!(ids, [NEW, OLD]);
    let old = &sessions[1];
    assert_eq!(old["duration_s"], 35.0);
    assert_eq!(old["closed"], true);
    assert_eq!(old["started_at_ms"], json!(session_epoch_millis(OLD)));
    assert!(old["started_at_ms"].is_i64());

    let (listed, err) = call_tool(&f, "list_markers", json!({ "session": OLD }));
    assert!(!err, "{listed}");
    assert_eq!(listed["session"], OLD);
    // Origin is the first remaining segment (5s), the one past the end is
    // dropped, and the rest are in time order.
    assert_eq!(
        listed["markers"],
        json!([
            { "time_s": 5.0, "time_100ns": 10 * S, "label": "boss", "comment": "slow here\nthen cut" },
            { "time_s": 20.0, "time_100ns": 25 * S, "label": "", "comment": "" },
            { "time_s": 25.0, "time_100ns": 30 * S, "label": "", "comment": "" },
        ])
    );
    let (listed, err) = call_tool(&f, "list_markers", json!({ "session": NEW }));
    assert!(!err);
    assert_eq!(listed["markers"], json!([]));

    for args in [
        json!({}),
        json!({ "session": 5 }),
        json!({ "session": "capture-nope" }),
    ] {
        let (_, err) = call_tool(&f, "list_markers", args.clone());
        assert!(err, "{args}");
    }
}

#[test]
fn export_range_checks_its_arguments_before_exporting() {
    let f = fixture("refuse", Ok(vec![]));
    let dir = f.root.0.join("out").display().to_string();
    for args in [
        json!({ "session": OLD, "start_s": 1, "end_s": 2, "output_dir": "relative\\out" }),
        json!({ "session": OLD, "start_s": 2, "end_s": 2, "output_dir": dir }),
        json!({ "session": OLD, "start_s": 3, "end_s": 2, "output_dir": dir }),
        json!({ "session": "capture-nope", "start_s": 1, "end_s": 2, "output_dir": dir }),
        json!({ "session": OLD, "end_s": 2, "output_dir": dir }),
        json!({ "session": OLD, "start_s": "1", "end_s": 2, "output_dir": dir }),
        json!({ "session": OLD, "start_s": 1, "end_s": 2 }),
        json!({ "session": OLD, "start_s": 1, "end_s": 2, "output_dir": dir, "mix_only": "yes" }),
        // Wholly after the recording (35s long): nothing to clamp onto.
        json!({ "session": OLD, "start_s": 500, "end_s": 600, "output_dir": dir }),
    ] {
        let (_, err) = call_tool(&f, "export_range", args.clone());
        assert!(err, "{args}");
    }
    assert!(
        f.asked.lock().unwrap().is_empty(),
        "nothing reached the export"
    );
}

#[test]
fn export_range_converts_seconds_clamps_and_reports_each_part() {
    let out = PathBuf::from(r"C:\out");
    let parts = vec![
        ExportedPart {
            path: out.join("a_part01.mp4"),
            start_100ns: 5 * S,
            end_100ns: 20 * S,
        },
        ExportedPart {
            path: out.join("a_part02.mp4"),
            start_100ns: 22 * S,
            end_100ns: 40 * S - 1,
        },
    ];
    let f = fixture("export", Ok(parts));
    let dir = out.display().to_string();

    let args = json!({ "session": OLD, "start_s": 1.5, "end_s": 2.25, "output_dir": dir });
    let (answer, err) = call_tool(&f, "export_range", args);
    assert!(!err, "{answer}");
    // Out past both ends: pulled onto the recording.
    let args = json!({ "session": OLD, "start_s": -3, "end_s": 1000, "output_dir": dir,
                       "mix_only": true });
    let (_, err) = call_tool(&f, "export_range", args);
    assert!(!err);

    let asked = f.asked.lock().unwrap();
    let first = &asked[0];
    assert_eq!(first.session_id, OLD);
    assert_eq!(
        (first.start_100ns, first.end_100ns),
        (5 * S + 15_000_000, 5 * S + 22_500_000)
    );
    assert_eq!(first.output_file, None);
    assert_eq!(first.output_directory.as_deref(), Some(out.as_path()));
    // No title and no executable in the fixture: the fixed name.
    assert_eq!(first.game_name, "Liveback");
    assert_eq!(
        (asked[1].start_100ns, asked[1].end_100ns),
        (5 * S, 40 * S - 1)
    );
    // `mix_only` defaults to the export's layout: mix plus originals.
    assert!(!first.audio_mix.mix_only);
    assert!(asked[1].audio_mix.mix_only);

    assert_eq!(
        answer["parts"],
        json!([
            { "path": out.join("a_part01.mp4").display().to_string(), "start_s": 0.0, "end_s": 15.0 },
            { "path": out.join("a_part02.mp4").display().to_string(), "start_s": 17.0,
              "end_s": seconds(35 * S - 1) },
        ])
    );
}

#[test]
fn an_export_failure_comes_back_as_a_tool_error() {
    let f = fixture("fail", Err("An export is already running".into()));
    let args = json!({ "session": OLD, "start_s": 1, "end_s": 2, "output_dir": r"C:\out" });
    let (text, err) = call_tool(&f, "export_range", args);
    assert!(err);
    assert_eq!(text, "An export is already running");
}

#[test]
fn the_file_name_falls_back_from_title_to_executable_to_the_app() {
    let f = fixture("name", Ok(vec![]));
    let mut manifest = f.manifest(OLD).unwrap();
    manifest.target_title = Some("Game Window".into());
    manifest.target_executable = Some("game.exe".into());
    assert_eq!(game_name(&manifest), "Game Window");
    manifest.target_title = Some("  ".into());
    assert_eq!(game_name(&manifest), "game.exe");
    manifest.target_executable = None;
    assert_eq!(game_name(&manifest), "Liveback");
}

#[test]
fn origin_is_loopback_only() {
    assert!(origin_allowed(None));
    assert!(origin_allowed(Some("http://127.0.0.1")));
    assert!(origin_allowed(Some("http://localhost:3000")));
    assert!(!origin_allowed(Some("http://evil.example")));
    assert!(!origin_allowed(Some("http://localhost.evil.example")));
    assert!(!origin_allowed(Some("http://127.0.0.1.evil.example")));
    assert!(!origin_allowed(Some("http://localhost:")));
    assert!(!origin_allowed(Some("null")));
}

fn send(addr: SocketAddr, method: &str, path: &str, extra: &str, body: &str) -> (u16, String) {
    let mut s = TcpStream::connect(addr).unwrap();
    write!(
        s,
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}\
        Content-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    (status, out.split_once("\r\n\r\n").unwrap().1.to_owned())
}

/// A raw HTTP client against a real socket, then the port closing on drop.
#[test]
fn http_round_trip_over_a_real_socket_and_close_on_drop() {
    let server = Server::start(0, Arc::new(fixture("http", Ok(vec![])))).unwrap();
    let addr = SocketAddr::from(([127, 0, 0, 1], server.port()));
    assert_eq!(server.addr, addr, "bound to loopback only");

    let init = rpc("initialize", json!({ "protocolVersion": "2025-06-18" })).to_string();
    let (status, body) = send(addr, "POST", "/mcp", "", &init);
    assert_eq!(status, 200);
    let reply: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(reply["result"]["serverInfo"]["name"], "liveback");

    let initialized =
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    assert_eq!(
        send(addr, "POST", "/mcp", "", &initialized),
        (202, String::new())
    );

    let call = rpc(
        "tools/call",
        json!({ "name": "list_sessions", "arguments": {} }),
    )
    .to_string();
    let (status, body) = send(
        addr,
        "POST",
        "/mcp",
        "Origin: http://localhost:1234\r\n",
        &call,
    );
    assert_eq!(status, 200);
    assert!(body.contains(OLD), "{body}");

    let evil = "Origin: http://evil.example\r\n";
    assert_eq!(send(addr, "POST", "/mcp", evil, &call).0, 403);
    assert_eq!(send(addr, "GET", "/mcp", "", "").0, 405);
    assert_eq!(send(addr, "POST", "/other", "", &call).0, 404);

    // Closed once the drop returns, and free to bind again -- the switch's
    // off-and-on. (Passed 6/6 with the drop's `join` hollowed out too: the
    // join is a guard against a slow accept thread, not what this observes.)
    drop(server);
    assert!(
        TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_err(),
        "the port still accepts after the server was dropped"
    );
    let again = Server::start(addr.port(), Arc::new(fixture("rebind", Ok(vec![])))).unwrap();
    assert_eq!(again.port(), addr.port());
}

/// One thread per connection: a list answers while an export is still
/// writing.
#[test]
fn a_list_answers_while_an_export_is_running() {
    let (release, gate) = mpsc::channel();
    let mut f = fixture("parallel", Ok(vec![]));
    f.gate = Some(Mutex::new(gate));
    let server = Server::start(0, Arc::new(f)).unwrap();
    let addr = SocketAddr::from(([127, 0, 0, 1], server.port()));

    let export = rpc(
        "tools/call",
        json!({ "name": "export_range",
            "arguments": { "session": OLD, "start_s": 1, "end_s": 2, "output_dir": r"C:\out" } }),
    )
    .to_string();
    let (done_tx, done) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = done_tx.send(send(addr, "POST", "/mcp", "", &export));
    });
    // Let the export reach the backend and block there.
    std::thread::sleep(Duration::from_millis(200));
    assert!(done.try_recv().is_err(), "the export has not finished yet");

    let list = rpc("tools/call", json!({ "name": "list_sessions" })).to_string();
    let (status, body) = send(addr, "POST", "/mcp", "", &list);
    assert_eq!(status, 200);
    assert!(body.contains(NEW), "{body}");
    assert!(
        done.try_recv().is_err(),
        "the list did not wait for the export"
    );

    release.send(()).unwrap();
    let (status, body) = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(status, 200);
    assert!(body.contains(r#"\"parts\":[]"#), "{body}");
}
