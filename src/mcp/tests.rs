//! MCP server tests (t261009-5ebd). The sessions are real `.lvb` containers in
//! a temp folder, read through the same `list_sessions` the history uses; the
//! export is a stand-in, since a real one cannot run under `cargo test` (the
//! helper process is the test binary).

use super::*;
use crate::ring_buffer;
use crate::ring_buffer::container::{AudioTrackInfo, ContainerBuilder, MetaEvent};
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

/// Recording (no `.closed()`), one marker folded in.
const LIVE: &str = "capture-00000000000000000000000000000003";
/// Closed, a gap from 15s to 25s and a 0.5s hole (not a gap) at 10s; its
/// first segment starts at 5s.
const GAP: &str = "capture-00000000000000000000000000000004";

struct Fixture {
    root: TmpRoot,
    asked: Mutex<Vec<ExportRequest>>,
    answer: Result<Vec<ExportedPart>, String>,
    /// When set, `export` waits for one message on it before answering.
    gate: Option<Mutex<mpsc::Receiver<()>>>,
    /// The markers queued for `LIVE`'s index writer.
    queued: Vec<MarkerRecord>,
    /// Segment index -> video size, for every session.
    sizes: std::collections::HashMap<u64, (u32, u32)>,
    sizes_asked: Mutex<Vec<(String, Vec<u64>)>>,
    /// (session, index, local_100ns, max_width) of every `frame`.
    frames_asked: Mutex<Vec<(String, u64, i64, u32)>>,
    clips: Vec<(ClipEntry, ClipDetails)>,
    /// (request, comment) of every `save_clip`; it answers with `answer`.
    clips_asked: Mutex<Vec<(ExportRequest, Option<String>)>>,
    /// What `save_clip` says about the comment write.
    clip_warning: Option<String>,
    /// `(session, hwnd)` running; `stop_recording` takes its row out.
    recording: Mutex<Vec<(String, String)>>,
    targets: Vec<RecordTarget>,
    /// The hwnd of every `start_recording`, and the id of every stop asked.
    started: Mutex<Vec<String>>,
    stopped: Mutex<Vec<String>>,
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

    fn queued_markers(&self, session_id: &str) -> Vec<MarkerRecord> {
        if session_id == LIVE {
            self.queued.clone()
        } else {
            Vec::new()
        }
    }

    fn segment_sizes(
        &self,
        session_id: &str,
        indexes: &[u64],
    ) -> Result<Vec<Option<(u32, u32)>>, String> {
        self.sizes_asked
            .lock()
            .unwrap()
            .push((session_id.into(), indexes.to_vec()));
        Ok(indexes.iter().map(|i| self.sizes.get(i).copied()).collect())
    }

    /// A frame 1/60s after the asked time, `max_width` (capped at 40) by 10;
    /// none past 4.9s into a segment (each ends 0.1s after its last frame).
    fn frame(
        &self,
        session_id: &str,
        index: u64,
        local_100ns: i64,
        max_width: u32,
    ) -> Result<Option<Frame>, String> {
        self.frames_asked
            .lock()
            .unwrap()
            .push((session_id.into(), index, local_100ns, max_width));
        if local_100ns > 49 * S / 10 {
            return Ok(None);
        }
        let width = max_width.min(40);
        Ok(Some((
            local_100ns + S / 60,
            width,
            10,
            vec![0x80; (width * 10 * 4) as usize],
        )))
    }

    fn clips(&self) -> Result<Vec<(ClipEntry, ClipDetails)>, String> {
        Ok(self.clips.clone())
    }

    /// The writes go into the fixture's real containers, through the
    /// buffer's own on-disk edits, so the tools read them back.
    fn add_marker(
        &self,
        session_id: &str,
        time_100ns: i64,
        label: &str,
        comment: Option<&str>,
    ) -> Result<(), String> {
        let root = &self.root.0;
        ring_buffer::add_marker(root, session_id, time_100ns).map_err(|e| e.to_string())?;
        ring_buffer::update_marker(
            root,
            session_id,
            time_100ns,
            label.into(),
            comment.map(str::to_owned),
        )
        .map_err(|e| e.to_string())
    }

    fn set_session_comment(&self, session_id: &str, comment: &str) -> Result<(), String> {
        ring_buffer::set_session_comment(&self.root.0, session_id, Some(comment.into()))
            .map_err(|e| e.to_string())
    }

    fn protect_session(&self, session_id: &str) -> Result<(), String> {
        ring_buffer::set_session_protected(&self.root.0, session_id, true)
            .map_err(|e| e.to_string())
    }

    fn save_clip(
        &self,
        request: ExportRequest,
        comment: Option<&str>,
    ) -> Result<(Vec<ExportedPart>, Option<String>), String> {
        self.clips_asked
            .lock()
            .unwrap()
            .push((request, comment.map(str::to_owned)));
        Ok((self.answer.clone()?, self.clip_warning.clone()))
    }

    fn recordings(&self) -> Vec<(String, String)> {
        self.recording.lock().unwrap().clone()
    }

    fn stopping(&self) -> bool {
        false
    }

    fn targets(&self) -> Result<Vec<RecordTarget>, String> {
        Ok(self.targets.clone())
    }

    fn start_recording(&self, hwnd: &str) -> Result<String, String> {
        self.started.lock().unwrap().push(hwnd.to_owned());
        Ok("capture-started".into())
    }

    fn stop_recording(&self, session_id: &str) {
        self.stopped.lock().unwrap().push(session_id.to_owned());
        self.recording
            .lock()
            .unwrap()
            .retain(|(id, _)| id != session_id);
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
        queued: Vec::new(),
        sizes: Default::default(),
        sizes_asked: Mutex::new(Vec::new()),
        frames_asked: Mutex::new(Vec::new()),
        clips: Vec::new(),
        clips_asked: Mutex::new(Vec::new()),
        clip_warning: None,
        recording: Mutex::new(Vec::new()),
        targets: Vec::new(),
        started: Mutex::new(Vec::new()),
        stopped: Mutex::new(Vec::new()),
    }
}

/// Adds `LIVE` and `GAP` to the fixture's buffer, for the tools that read them.
fn add_live_and_gap(f: &Fixture) {
    let root = &f.root.0;
    ContainerBuilder::new(LIVE)
        .segment(0, 0, 5 * S, &[0xA3; 64])
        .segment(1, 5 * S, 10 * S, &[0xA4; 64])
        .meta(marker(3 * S, "folded"))
        .checkpoint()
        .build(&root.join(format!("{LIVE}.lvb")))
        .unwrap();
    ContainerBuilder::new(GAP)
        .segment(0, 5 * S, 10 * S, &[0xA5; 64])
        .segment(1, 21 * S / 2, 15 * S, &[0xA6; 64])
        .segment(2, 25 * S, 30 * S, &[0xA7; 64])
        .meta(MetaEvent::TitleSet {
            title: Some("Game Window".into()),
        })
        .meta(MetaEvent::TargetExecutableSet {
            executable: Some("game.exe".into()),
            path: None,
        })
        .meta(MetaEvent::CommentSet {
            comment: Some("a note".into()),
        })
        .meta(MetaEvent::ProtectedSet { protected: true })
        .meta(MetaEvent::AudioTracksSet {
            tracks: vec![
                AudioTrackInfo {
                    executable_name: "game.exe".into(),
                    microphone: None,
                },
                AudioTrackInfo {
                    executable_name: "Headset Mic".into(),
                    microphone: Some(String::new()),
                },
            ],
        })
        .checkpoint()
        .closed()
        .build(&root.join(format!("{GAP}.lvb")))
        .unwrap();
}

/// `tools/call` -> the result's whole `content` array, and isError.
fn call_content(backend: &dyn Backend, name: &str, args: Value) -> (Vec<Value>, bool) {
    let reply = dispatch(
        &rpc("tools/call", json!({ "name": name, "arguments": args })),
        backend,
    );
    let result = &reply["result"];
    (
        result["content"].as_array().unwrap().clone(),
        result["isError"].as_bool().unwrap(),
    )
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
    // t261103-7cac added three readers, t261103-d3d4 four writers,
    // t261103-a803 the three recording controls.
    assert_eq!(
        names,
        [
            "add_marker",
            "export_range",
            "get_frame",
            "get_session",
            "get_status",
            "list_clips",
            "list_markers",
            "list_sessions",
            "protect_session",
            "save_clip",
            "set_session_comment",
            "start_recording",
            "stop_recording"
        ]
    );

    let reply = dispatch(&rpc("resources/list", json!({})), &f);
    assert_eq!(reply["error"]["code"], -32601);
    // t261103-d3d4: still no tool that deletes, renames or lifts a protection.
    for name in [
        "delete_session",
        "delete_marker",
        "rename_marker",
        "unprotect_session",
        "",
    ] {
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

/// The three tools a client already calls keep their arguments (t261103-7cac
/// only added tools).
#[test]
fn the_first_three_tools_keep_their_input_schemas() {
    let tools = tools();
    let schema = |name: &str| {
        tools
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()["inputSchema"]
            .clone()
    };
    let session = json!({ "type": "string", "description": "Session id from list_sessions." });
    assert_eq!(
        schema("list_sessions"),
        json!({ "type": "object", "properties": {} })
    );
    assert_eq!(
        schema("list_markers"),
        json!({ "type": "object", "properties": { "session": session }, "required": ["session"] })
    );
    assert_eq!(
        schema("export_range"),
        json!({
            "type": "object",
            "properties": {
                "session": session,
                "start_s": { "type": "number", "description": "Range start, seconds on the list_markers origin." },
                "end_s": { "type": "number", "description": "Range end, seconds on the same origin." },
                "output_dir": { "type": "string", "description": "Absolute folder path. Created if missing; its parent must exist." },
                "mix_only": { "type": "boolean", "description": "Write only the mixed audio track, without the original tracks. Default false. No effect on a session with one audio track." }
            },
            "required": ["session", "start_s", "end_s", "output_dir"],
        })
    );
}

/// A recording session answers, with the markers its index writer has not
/// folded in yet: one inside the recorded time, one pressed past the last
/// segment, and one the manifest already has (kept once, as folded).
#[test]
fn list_markers_reads_a_recording_session_with_its_queued_markers() {
    let mut f = fixture("live", Ok(vec![]));
    add_live_and_gap(&f);
    let mut renamed = MarkerRecord::at(3 * S);
    renamed.label = "queued twice".into();
    f.queued = vec![MarkerRecord::at(12 * S), MarkerRecord::at(7 * S), renamed];
    let (listed, err) = call_tool(&f, "list_markers", json!({ "session": LIVE }));
    assert!(!err, "{listed}");
    let times: Vec<(f64, &str)> = listed["markers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["time_s"].as_f64().unwrap(), m["label"].as_str().unwrap()))
        .collect();
    assert_eq!(times, [(3.0, "folded"), (7.0, ""), (12.0, "")]);

    // A closed session still drops a marker past its end (OLD's 100s one).
    let (listed, _) = call_tool(&f, "list_markers", json!({ "session": OLD }));
    assert_eq!(listed["markers"].as_array().unwrap().len(), 3);
}

#[test]
fn get_session_reports_gaps_tracks_and_a_size_per_span() {
    let mut f = fixture("session", Ok(vec![]));
    add_live_and_gap(&f);
    f.sizes = [(0, (1920, 1080)), (1, (1280, 720)), (2, (1920, 1080))].into();
    let (got, err) = call_tool(&f, "get_session", json!({ "session": GAP }));
    assert!(!err, "{got}");
    assert_eq!(got["id"], GAP);
    assert_eq!(got["title"], "Game Window");
    assert_eq!(got["game"], "game.exe");
    assert_eq!(got["started_at_ms"], json!(session_epoch_millis(GAP)));
    assert_eq!(got["duration_s"], 25.0);
    assert_eq!(got["closed"], true);
    assert_eq!(got["protected"], true);
    assert_eq!(got["comment"], "a note");
    assert_eq!(got["segment_count"], 3);
    assert!(got["total_bytes"].as_u64().unwrap() > 0, "{got}");
    // Seconds from the first segment (5s).
    assert_eq!(
        got["gaps"],
        json!([{ "start_s": 10.0, "end_s": 20.0, "reason": "video frame interruption" }])
    );
    assert_eq!(
        got["audio_tracks"],
        json!([
            { "name": "game.exe", "microphone": false },
            { "name": "Headset Mic", "microphone": true },
        ])
    );
    assert_eq!(
        got["spans"],
        json!([
            { "start_s": 0.0, "end_s": 10.0, "width": 1920, "height": 1080, "size_changes": true },
            { "start_s": 20.0, "end_s": 25.0, "width": 1920, "height": 1080, "size_changes": false },
        ])
    );
    // Only each span's ends were read.
    assert_eq!(
        *f.sizes_asked.lock().unwrap(),
        [(GAP.to_owned(), vec![0, 1, 2])]
    );

    // A recording session answers too, and an unreadable size is null.
    let (got, err) = call_tool(&f, "get_session", json!({ "session": LIVE }));
    assert!(!err, "{got}");
    assert_eq!(got["closed"], false);
    f.sizes.clear();
    let (got, _) = call_tool(&f, "get_session", json!({ "session": LIVE }));
    assert_eq!(
        got["spans"],
        json!([{ "start_s": 0.0, "end_s": 10.0, "width": null, "height": null, "size_changes": false }])
    );

    for args in [json!({}), json!({ "session": "capture-nope" })] {
        let (_, err) = call_tool(&f, "get_session", args.clone());
        assert!(err, "{args}");
    }
}

#[test]
fn get_frame_returns_a_png_image_and_refuses_gaps_and_bad_arguments() {
    let f = fixture("frame", Ok(vec![]));
    add_live_and_gap(&f);
    let (content, err) = call_content(&f, "get_frame", json!({ "session": GAP, "time_s": 7.5 }));
    assert!(!err, "{content:?}");
    assert_eq!(content[0]["type"], "image");
    assert_eq!(content[0]["mimeType"], "image/png");
    let png = image::load_from_memory_with_format(
        &base64::engine::general_purpose::STANDARD
            .decode(content[0]["data"].as_str().unwrap())
            .unwrap(),
        image::ImageFormat::Png,
    )
    .unwrap();
    assert_eq!((png.width(), png.height()), (40, 10));
    let info: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    // 7.5s from the 5s origin is 12.5s: segment 1 (10.5-15s), 2s in; the
    // frame came 1/60s later.
    assert_eq!(info["width"], 40);
    assert_eq!(info["height"], 10);
    assert_eq!(info["time_s"], seconds(75_000_000 + S / 60));
    // max_width given, after the default.
    let (content, _) = call_content(
        &f,
        "get_frame",
        json!({ "session": GAP, "time_s": 0, "max_width": 8 }),
    );
    let info: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(info["width"], 8);
    assert_eq!(
        *f.frames_asked.lock().unwrap(),
        [
            (GAP.to_owned(), 1, 20_000_000, 1280),
            (GAP.to_owned(), 0, 0, 8)
        ]
    );

    // Past segment 0's last frame (9.9s), and in the 0.5s hole after it
    // (10-10.5s): both land on segment 1's first frame, at 10.5s.
    let first_of_1 = seconds(55 * S / 10 + S / 60);
    for time_s in [4.95, 5.2] {
        let (content, err) =
            call_content(&f, "get_frame", json!({ "session": GAP, "time_s": time_s }));
        assert!(!err, "{time_s}: {content:?}");
        let info: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
        assert_eq!(info["time_s"], first_of_1, "{time_s}");
    }

    f.frames_asked.lock().unwrap().clear();
    // Past the last segment's last frame: nothing after it to fall back to.
    let (_, err) = call_content(&f, "get_frame", json!({ "session": GAP, "time_s": 24.95 }));
    assert!(err);
    assert_eq!(f.frames_asked.lock().unwrap().len(), 1);

    f.frames_asked.lock().unwrap().clear();
    for args in [
        // In the gap (15-25s, so 10-20s on the origin), and past the end.
        json!({ "session": GAP, "time_s": 12 }),
        json!({ "session": GAP, "time_s": 25 }),
        json!({ "session": GAP, "time_s": -1 }),
        json!({ "session": GAP, "time_s": "1" }),
        json!({ "session": GAP }),
        json!({ "session": GAP, "time_s": 1, "max_width": "big" }),
        json!({ "session": GAP, "time_s": 1, "max_width": 0 }),
        json!({ "session": GAP, "time_s": 1, "max_width": 12.5 }),
        json!({ "session": "capture-nope", "time_s": 1 }),
    ] {
        let (_, err) = call_content(&f, "get_frame", args.clone());
        assert!(err, "{args}");
    }
    assert!(
        f.frames_asked.lock().unwrap().is_empty(),
        "nothing was decoded"
    );
}

#[test]
fn list_clips_comes_newest_first_with_its_details() {
    let mut f = fixture("clips", Ok(vec![]));
    let at = |s: u64| std::time::UNIX_EPOCH + Duration::from_secs(s);
    let entry = |name: &str, created: u64| ClipEntry {
        path: PathBuf::from(r"C:\clips").join(name),
        name: name.into(),
        created: at(created),
        modified: at(created + 100),
        bytes: 1234,
    };
    f.clips = vec![
        (entry("old.mp4", 1_000), ClipDetails::default()),
        (
            entry("new.mp4", 2_000),
            ClipDetails {
                comment: Some("good one".into()),
                duration_100ns: Some(15 * S / 2),
                app_path: Some(r"C:\Games\game.exe".into()),
            },
        ),
    ];
    let (got, err) = call_tool(&f, "list_clips", json!({}));
    assert!(!err, "{got}");
    assert_eq!(
        got["clips"],
        json!([
            { "path": r"C:\clips\new.mp4", "name": "new.mp4", "created_ms": 2_000_000,
              "bytes": 1234, "duration_s": 7.5, "comment": "good one", "app": "game.exe" },
            { "path": r"C:\clips\old.mp4", "name": "old.mp4", "created_ms": 1_000_000,
              "bytes": 1234, "duration_s": null, "comment": null, "app": null },
        ])
    );
}

/// Markers on `session` as `(time_100ns, label, comment)`, through `list_markers`.
fn markers_of(backend: &dyn Backend, session: &str) -> Vec<(i64, String, String)> {
    let (listed, err) = call_tool(backend, "list_markers", json!({ "session": session }));
    assert!(!err, "{listed}");
    listed["markers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            (
                m["time_100ns"].as_i64().unwrap(),
                m["label"].as_str().unwrap().to_owned(),
                m["comment"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn add_marker_writes_a_named_marker_on_the_review_origin() {
    let f = fixture("add-marker", Ok(vec![]));
    let args = json!({ "session": OLD, "time_s": 12.5, "label": " clutch ",
                       "comment": "line one\r\nline two" });
    let (added, err) = call_tool(&f, "add_marker", args);
    assert!(!err, "{added}");
    // Origin 5s: 12.5s on the timeline is 17.5s in the recording.
    assert_eq!(
        added,
        json!({ "time_s": 12.5, "time_100ns": 175_000_000, "label": "clutch",
                "comment": "line one\nline two" })
    );
    // An unnamed one stays unnamed and comment-less.
    let (_, err) = call_tool(&f, "add_marker", json!({ "session": OLD, "time_s": 1 }));
    assert!(!err);
    let markers = markers_of(&f, OLD);
    assert!(markers.contains(&(175_000_000, "clutch".into(), "line one\nline two".into())));
    assert!(markers.contains(&(6 * S, String::new(), String::new())));
    assert_eq!(markers.len(), 5, "{markers:?}");
}

#[test]
fn add_marker_refuses_off_the_recording_onto_a_marker_and_bad_arguments() {
    let f = fixture("add-marker-refuse", Ok(vec![]));
    add_live_and_gap(&f);
    let before = (markers_of(&f, OLD), markers_of(&f, GAP));
    for args in [
        json!({ "time_s": 1 }),
        json!({ "session": "capture-nope", "time_s": 1 }),
        json!({ "session": OLD }),
        json!({ "session": OLD, "time_s": "1" }),
        json!({ "session": OLD, "time_s": -1 }),
        // The recording is 35s long.
        json!({ "session": OLD, "time_s": 35 }),
        // 10s already holds "boss" (origin 5s): adding would rename it.
        json!({ "session": OLD, "time_s": 5, "label": "other" }),
        json!({ "session": OLD, "time_s": 1, "label": 7 }),
        json!({ "session": OLD, "time_s": 1, "comment": ["x"] }),
        // GAP's gap runs 15s-25s; origin 5s.
        json!({ "session": GAP, "time_s": 12 }),
    ] {
        let (text, err) = call_tool(&f, "add_marker", args.clone());
        assert!(err, "{args} -> {text}");
    }
    assert_eq!((markers_of(&f, OLD), markers_of(&f, GAP)), before);
    // The control: the same session takes one on recorded time.
    let (_, err) = call_tool(&f, "add_marker", json!({ "session": GAP, "time_s": 21 }));
    assert!(!err);
    assert_eq!(markers_of(&f, GAP).len(), before.1.len() + 1);
}

#[test]
fn comment_and_protection_are_written_and_cannot_be_lifted() {
    let f = fixture("comment-protect", Ok(vec![]));
    let read = |f: &Fixture| {
        let (session, err) = call_tool(f, "get_session", json!({ "session": OLD }));
        assert!(!err, "{session}");
        (session["comment"].clone(), session["protected"].clone())
    };
    assert_eq!(read(&f), (Value::Null, json!(false)));
    for (name, args) in [
        ("set_session_comment", json!({ "session": OLD })),
        (
            "set_session_comment",
            json!({ "session": OLD, "comment": "  " }),
        ),
        (
            "set_session_comment",
            json!({ "session": "capture-nope", "comment": "x" }),
        ),
        ("protect_session", json!({})),
        ("protect_session", json!({ "session": "capture-nope" })),
    ] {
        let (_, err) = call_tool(&f, name, args.clone());
        assert!(err, "{name} {args}");
    }
    assert_eq!(read(&f), (Value::Null, json!(false)));

    let args = json!({ "session": OLD, "comment": "good run" });
    let (set, err) = call_tool(&f, "set_session_comment", args);
    assert!(!err, "{set}");
    let (_, err) = call_tool(&f, "protect_session", json!({ "session": OLD }));
    assert!(!err);
    assert_eq!(read(&f), (json!("good run"), json!(true)));
    // An extra argument asking for the opposite is not one the tool reads.
    let args = json!({ "session": OLD, "protected": false });
    let (_, err) = call_tool(&f, "protect_session", args);
    assert!(!err);
    assert_eq!(read(&f).1, json!(true));

    let reply = dispatch(&rpc("tools/list", json!({})), &f);
    let tool = reply["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "protect_session")
        .unwrap()
        .clone();
    let keys: Vec<&String> = tool["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(keys, ["session"]);
}

#[test]
fn save_clip_asks_for_a_mixed_clip_on_the_review_origin() {
    let out = PathBuf::from(r"C:\clips");
    let parts = vec![ExportedPart {
        path: out.join("a.mp4"),
        start_100ns: 6 * S,
        end_100ns: 9 * S,
    }];
    let mut f = fixture("save-clip", Ok(parts));
    let args = json!({ "session": OLD, "start_s": 1, "end_s": 4, "comment": "  nice  " });
    let (saved, err) = call_tool(&f, "save_clip", args);
    assert!(!err, "{saved}");
    assert_eq!(
        saved,
        json!({ "parts": [{ "path": out.join("a.mp4").display().to_string(),
                            "start_s": 1.0, "end_s": 4.0 }],
                "comment_written": true })
    );
    let args = json!({ "session": OLD, "start_s": 1, "end_s": 4, "comment": " " });
    let (_, err) = call_tool(&f, "save_clip", args);
    assert!(!err);
    {
        let asked = f.clips_asked.lock().unwrap();
        let (request, comment) = &asked[0];
        assert_eq!((request.start_100ns, request.end_100ns), (6 * S, 9 * S));
        // The clip folder is the backend's to name; a clip is the mix alone.
        assert_eq!(request.output_file, None);
        assert_eq!(request.output_directory, None);
        assert!(request.audio_mix.mix_only);
        assert_eq!(comment.as_deref(), Some("nice"));
        assert_eq!(asked[1].1, None);
    }
    assert!(
        f.asked.lock().unwrap().is_empty(),
        "not through export_range"
    );

    f.clip_warning = Some("locked".into());
    let args = json!({ "session": OLD, "start_s": 1, "end_s": 4, "comment": "x" });
    let (saved, err) = call_tool(&f, "save_clip", args);
    assert!(!err);
    assert_eq!(saved["comment_written"], false);
    assert!(
        saved["warning"].as_str().unwrap().contains("locked"),
        "{saved}"
    );

    for args in [
        json!({ "session": OLD, "start_s": 4, "end_s": 4 }),
        json!({ "session": OLD, "start_s": 500, "end_s": 600 }),
        json!({ "session": "capture-nope", "start_s": 1, "end_s": 4 }),
        json!({ "session": OLD, "start_s": 1, "end_s": 4, "comment": 3 }),
    ] {
        let (_, err) = call_tool(&f, "save_clip", args.clone());
        assert!(err, "{args}");
    }
    assert_eq!(f.clips_asked.lock().unwrap().len(), 3);

    let busy = fixture("save-clip-busy", Err("another export is running".into()));
    let args = json!({ "session": OLD, "start_s": 1, "end_s": 4 });
    let (text, err) = call_tool(&busy, "save_clip", args);
    assert!(err);
    assert_eq!(text, "another export is running");
}

fn target(hwnd: &str, title: &str, executable: &str) -> RecordTarget {
    RecordTarget {
        hwnd: hwnd.into(),
        title: title.into(),
        executable: executable.into(),
    }
}

#[test]
fn a_window_is_named_by_hwnd_or_by_part_of_its_title_or_executable() {
    let targets = [
        target("0xAB", "Notes - Editor", "editor.exe"),
        target("0x1F00", "Game Window", "game.exe"),
        target("0x2000", "Second Game Window", "game.exe"),
    ];
    let hwnd = |found: Result<&RecordTarget, String>| found.map(|t| t.hwnd.clone());
    // A handle is a number: case and leading zeros do not matter.
    assert_eq!(hwnd(match_window(&targets, "0x00ab")), Ok("0xAB".into()));
    assert_eq!(hwnd(match_window(&targets, "0X1f00")), Ok("0x1F00".into()));
    // A part of the title or of the executable, case ignored.
    assert_eq!(hwnd(match_window(&targets, "notes")), Ok("0xAB".into()));
    assert_eq!(
        hwnd(match_window(&targets, "EDITOR.EXE")),
        Ok("0xAB".into())
    );
    assert_eq!(hwnd(match_window(&targets, "second")), Ok("0x2000".into()));
    // None: an unknown handle or text, or nothing at all.
    for window in ["0x99", "browser", "  "] {
        assert!(match_window(&targets, window).is_err(), "{window}");
    }
    // Several: each candidate's hwnd is in the error.
    let several = match_window(&targets, "game").unwrap_err();
    assert!(several.contains("2 windows"), "{several}");
    assert!(
        several.contains("0x1F00") && several.contains("0x2000"),
        "{several}"
    );
    assert!(!several.contains("0xAB"), "{several}");
}

#[test]
fn start_recording_starts_only_one_unambiguous_idle_window() {
    let mut f = fixture("start", Ok(vec![]));
    f.targets = vec![
        target("0xAB", "Notes", "editor.exe"),
        target("0x1F00", "Game Window", "game.exe"),
        target("0x2000", "Other Game Window", "game.exe"),
    ];
    f.recording
        .lock()
        .unwrap()
        .push((NEW.into(), "0x2000".into()));

    let (status, err) = call_tool(&f, "get_status", json!({}));
    assert!(!err, "{status}");
    assert_eq!(status["targets"].as_array().unwrap().len(), 3);
    assert_eq!(status["recordings"][0]["session"], NEW);
    assert_eq!(status["recordings"][0]["hwnd"], "0x2000");
    assert_eq!(status["stopping"], false);

    let (started, err) = call_tool(&f, "start_recording", json!({ "window": "notes" }));
    assert!(!err, "{started}");
    assert_eq!(
        started,
        json!({ "session": "capture-started", "hwnd": "0xAB", "title": "Notes", "executable": "editor.exe" })
    );
    // Two matches, an already recording window, no match, a bad argument:
    // an error each, and nothing more reaches the backend.
    for args in [
        json!({ "window": "game" }),
        json!({ "window": "0x2000" }),
        json!({ "window": "browser" }),
        json!({ "window": 3 }),
        json!({}),
    ] {
        let (_, err) = call_tool(&f, "start_recording", args.clone());
        assert!(err, "{args}");
    }
    assert_eq!(*f.started.lock().unwrap(), ["0xAB"]);
}

#[test]
fn stop_recording_waits_for_the_session_to_close() {
    let f = fixture("stop", Ok(vec![]));
    f.recording
        .lock()
        .unwrap()
        .push((NEW.into(), "0x10".into()));
    let (stopped, err) = call_tool(&f, "stop_recording", json!({ "session": NEW }));
    assert!(!err, "{stopped}");
    assert_eq!(stopped, json!({ "session": NEW, "closed": true }));
    // Not recording any more, so a second stop is an error and asks nothing.
    let (_, err) = call_tool(&f, "stop_recording", json!({ "session": NEW }));
    assert!(err);
    assert_eq!(*f.stopped.lock().unwrap(), [NEW]);

    // The wait itself: true once the poll says closed, false at the deadline.
    let step = Duration::from_millis(1);
    let mut answers = [false, false, true].into_iter();
    assert!(wait_closed(
        || answers.next().unwrap(),
        Duration::from_secs(5),
        step
    ));
    let mut polls = 0;
    let never = wait_closed(
        || {
            polls += 1;
            false
        },
        Duration::from_millis(20),
        step,
    );
    assert!(!never);
    assert!(polls >= 2, "{polls}");
}
