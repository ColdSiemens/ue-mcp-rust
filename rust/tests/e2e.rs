//! End to end: the real binary against a fake editor bridge.
//!
//! The fake behaves like a stock UE_MCP_Bridge plugin where it matters here:
//! one thread per socket, requests on a socket answered strictly in order,
//! the capabilities handshake, the lock registry, and the 64 MiB inbound cap.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

const PLUGIN_MAX_INBOUND: usize = 64 * 1024 * 1024;

// ── Fake editor bridge ──────────────────────────────────────────────────────

#[derive(Default)]
struct FakeState {
    current: AtomicUsize,
    max: AtomicUsize,
    locks: Mutex<HashMap<String, String>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

struct FakeBridge {
    port: u16,
    state: Arc<FakeState>,
}

/// Every bridge method in the embedded surface except `read_blueprint` and
/// `epic_call_tool`, so the hub's unregistered-action filtering is visible.
fn registered_methods() -> Vec<String> {
    let surface: Value = serde_json::from_str(include_str!("../surface/surface.json")).unwrap();
    let mut out: Vec<String> = surface["tools"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|t| t["actions"].as_object().unwrap().values())
        .filter(|a| a["kind"] == "bridge")
        .filter_map(|a| a["bridge"].as_str().map(str::to_string))
        .filter(|m| m != "read_blueprint" && m != "epic_call_tool")
        .collect();
    out.extend(["acquire_lock", "release_lock", "release_session_locks"].map(String::from));
    out.sort();
    out.dedup();
    out
}

impl FakeBridge {
    async fn start(port: u16) -> FakeBridge {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(FakeState::default());
        let methods = Arc::new(registered_methods());
        let st = state.clone();
        let accept = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let task = tokio::spawn(serve_socket(stream, st.clone(), methods.clone()));
                st.tasks.lock().unwrap().push(task);
            }
        });
        state.tasks.lock().unwrap().push(accept);
        FakeBridge { port, state }
    }

    /// Editor quits: listener and every socket go away.
    fn stop(&self) {
        for t in self.state.tasks.lock().unwrap().drain(..) {
            t.abort();
        }
    }
}

async fn serve_socket(stream: TcpStream, st: Arc<FakeState>, methods: Arc<Vec<String>>) {
    let config = WebSocketConfig { max_message_size: Some(PLUGIN_MAX_INBOUND), max_frame_size: Some(PLUGIN_MAX_INBOUND), ..Default::default() };
    let Ok(ws) = tokio_tungstenite::accept_async_with_config(stream, Some(config)).await else { return };
    let (mut sink, mut source) = ws.split();
    // Strictly one at a time, like a stock plugin's connection thread.
    while let Some(Ok(msg)) = source.next().await {
        let Message::Text(text) = msg else { continue };
        let req: Value = serde_json::from_str(&text).unwrap();
        let params = req["params"].clone();
        let result = match req["method"].as_str().unwrap() {
            "get_bridge_capabilities" => json!({ "protocolVersion": 3, "features": [], "actionCount": methods.len(), "actions": *methods }),
            "get_engine_state" => json!({ "success": true, "gameThread": "idle", "servedWithoutGameThread": true }),
            "acquire_lock" => {
                let path = params["assetPath"].as_str().unwrap().to_lowercase();
                let who = params["sessionId"].as_str().unwrap().to_string();
                let mut locks = st.locks.lock().unwrap();
                match locks.get(&path) {
                    Some(holder) if *holder != who => json!({ "success": true, "acquired": false, "holder": { "sessionId": holder } }),
                    _ => {
                        locks.insert(path, who);
                        json!({ "success": true, "acquired": true })
                    }
                }
            }
            "release_lock" => {
                let path = params["assetPath"].as_str().unwrap().to_lowercase();
                let who = params["sessionId"].as_str().unwrap();
                let mut locks = st.locks.lock().unwrap();
                let released = locks.get(&path).is_some_and(|h| h == who);
                if released {
                    locks.remove(&path);
                }
                json!({ "success": true, "released": released })
            }
            "get_world_outliner" if params.get("_bytes").is_some() => {
                json!({ "success": true, "blob": "x".repeat(params["_bytes"].as_u64().unwrap() as usize) })
            }
            method => {
                let now = st.current.fetch_add(1, Ordering::SeqCst) + 1;
                st.max.fetch_max(now, Ordering::SeqCst);
                if let Some(ms) = params.get("_sleepMs").and_then(Value::as_u64) {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                st.current.fetch_sub(1, Ordering::SeqCst);
                let size = params.to_string().len();
                json!({ "success": true, "method": method, "paramBytes": size, "echo": if size < 4096 { params } else { Value::Null } })
            }
        };
        let reply = json!({ "jsonrpc": "2.0", "id": req["id"], "result": result }).to_string();
        if sink.send(Message::Text(reply)).await.is_err() {
            return;
        }
    }
}

// ── The hub under test ──────────────────────────────────────────────────────

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Hub {
    port: u16,
    _child: Child,
}

impl Hub {
    async fn start(bridge_port: u16) -> Hub {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_ue-mcp-rs"))
            .args(["--http", &format!("127.0.0.1:{port}"), "--bridge-port", &bridge_port.to_string(), "--connections", "4", "--connect-wait", "3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).await.is_err() {
            assert!(Instant::now() < deadline, "hub did not start");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Hub { port, _child: child }
    }

    async fn client(&self, query: &str) -> Client {
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e", "version": "1" } } });
        let (status, session, body) = post(self.port, query, None, &init).await;
        assert_eq!(status, 200, "initialize failed: {body}");
        assert_eq!(body["result"]["protocolVersion"], "2025-06-18");
        let session = session.expect("session header");
        let (status, _, _) = post(self.port, "", Some(&session), &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).await;
        assert_eq!(status, 202);
        Client { port: self.port, session }
    }
}

async fn post(port: u16, query: &str, session: Option<&str>, body: &Value) -> (u16, Option<String>, Value) {
    let body = body.to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let session_header = session.map(|s| format!("Mcp-Session-Id: {s}\r\n")).unwrap_or_default();
    let request = format!(
        "POST /mcp{query} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\n{session_header}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.unwrap();
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&raw[..split]).to_string();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let session = head
        .lines()
        .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("mcp-session-id")).map(|(_, v)| v.trim().to_string()));
    let payload = &raw[split + 4..];
    let value = if payload.is_empty() { Value::Null } else { serde_json::from_slice(payload).unwrap_or(Value::Null) };
    (status, session, value)
}

#[derive(Clone)]
struct Client {
    port: u16,
    session: String,
}

impl Client {
    async fn rpc(&self, method: &str, params: Value) -> Value {
        let (status, _, body) = post(self.port, "", Some(&self.session), &json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).await;
        assert_eq!(status, 200);
        body["result"].clone()
    }

    /// (isError, text)
    async fn call(&self, tool: &str, action: &str, args: Value) -> (bool, String) {
        let r = self.rpc("tools/call", json!({ "name": tool, "arguments": { "action": action, "args": args } })).await;
        (r["isError"].as_bool().unwrap(), r["content"][0]["text"].as_str().unwrap().to_string())
    }
}

fn action_names(tools: &Value, tool: &str) -> Vec<String> {
    let t = tools["tools"].as_array().unwrap().iter().find(|t| t["name"] == tool).unwrap();
    t["inputSchema"]["properties"]["action"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn lists_tools_and_hides_what_the_plugin_lacks() {
    let fake = FakeBridge::start(0).await;
    // The full list is ~200 KB: over stdio, for the reason given at StdioHub.
    let mut stdio = StdioHub::start(fake.port).await;
    stdio.call("project", "get_status", json!({})).await; // handshake done, capabilities known
    let tools = stdio.request("tools/list", json!({})).await;
    let blueprint = action_names(&tools, "blueprint");
    assert!(blueprint.contains(&"add_variable".to_string()));
    assert!(!blueprint.contains(&"read".to_string()), "read_blueprint is not registered by the plugin");
    assert!(!action_names(&tools, "level").iter().any(|a| a.starts_with("epic_")), "no epic_call_tool, no epic actions");
    assert!(action_names(&tools, "project").contains(&"describe_action".to_string()));

    let full = tools.to_string().len();
    let hub = Hub::start(fake.port).await;
    let compact = hub.client("?compact=1&tools=level,blueprint").await.rpc("tools/list", json!({})).await;
    assert_eq!(compact["tools"].as_array().unwrap().len(), 3);
    assert!(compact.to_string().len() * 5 < full);

    let (err, text) = stdio.call("project", "describe_action", json!({ "name": "level.place_actor" })).await;
    assert!(!err);
    let described: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(described["bridgeMethod"], "place_actor");
    assert!(described["params"].as_array().is_some_and(|p| !p.is_empty()));
    fake.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn pool_runs_calls_in_parallel_against_a_serial_plugin() {
    let fake = FakeBridge::start(0).await;
    let hub = Hub::start(fake.port).await;
    let client = hub.client("").await;
    client.call("project", "get_status", json!({})).await; // connected and warmed up

    let started = Instant::now();
    let calls = (0..8).map(|i| {
        let c = client.clone();
        tokio::spawn(async move { c.call("level", "place_actor", json!({ "actorClass": "PointLight", "label": format!("L{i}"), "_sleepMs": 400 })).await })
    });
    for r in futures_util::future::join_all(calls).await {
        let (err, text) = r.unwrap();
        assert!(!err, "{text}");
    }
    let elapsed = started.elapsed();
    // 8 calls x 400ms over 4 sockets = 2 waves (0.8s plus HTTP overhead).
    // One shared socket, as upstream uses, cannot finish under 3.2s.
    assert!(elapsed < Duration::from_millis(2800), "took {elapsed:?}");
    assert_eq!(fake.state.max.load(Ordering::SeqCst), 4);
    fake.stop();
}

/// The hub over stdio, one request at a time.
struct StdioHub {
    _child: Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: i64,
}

impl StdioHub {
    async fn start(bridge_port: u16) -> StdioHub {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ue-mcp-rs"))
            .args(["--bridge-port", &bridge_port.to_string(), "--connections", "4"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut hub = StdioHub { _child: child, stdin, lines, next_id: 1 };
        hub.request("initialize", json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "e2e" } })).await;
        hub
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = format!("{}\n", json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        let reply: Value = serde_json::from_str(&self.lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["id"], id);
        reply["result"].clone()
    }

    async fn call(&mut self, tool: &str, action: &str, args: Value) -> (bool, String) {
        let r = self.request("tools/call", json!({ "name": tool, "arguments": { "action": action, "args": args } })).await;
        (r["isError"].as_bool().unwrap(), r["content"][0]["text"].as_str().unwrap().to_string())
    }
}

// Over stdio: some antivirus web shields reset localhost HTTP responses of a
// few MB, which says nothing about the hub. The HTTP path gets a smaller case.
#[tokio::test(flavor = "multi_thread")]
async fn large_payloads_round_trip_and_oversize_is_refused_cleanly() {
    let fake = FakeBridge::start(0).await;
    let mut hub = StdioHub::start(fake.port).await;

    let (err, text) = hub.call("level", "get_outliner", json!({ "_bytes": 40_000_000 })).await;
    assert!(!err);
    assert!(text.len() > 40_000_000);

    let big = "y".repeat(20_000_000);
    let (err, text) = hub.call("level", "place_actor", json!({ "actorClass": "A", "blob": big })).await;
    assert!(!err, "{}", &text[..text.len().min(300)]);
    let answer: Value = serde_json::from_str(&text).unwrap();
    assert!(answer["paramBytes"].as_u64().unwrap() > 20_000_000);

    let too_big = "z".repeat(PLUGIN_MAX_INBOUND + 1);
    let (err, text) = hub.call("level", "place_actor", json!({ "actorClass": "A", "blob": too_big })).await;
    assert!(err);
    assert!(text.contains("Split it into smaller batches"), "{text}");

    // Refused before it reached the editor, so every socket is still fine.
    let (err, _) = hub.call("level", "place_actor", json!({ "actorClass": "A" })).await;
    assert!(!err);
    fake.stop();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "an antivirus web shield on localhost HTTP can reset multi-MB responses; run with --ignored on a clean machine"]
async fn http_carries_megabyte_answers() {
    let fake = FakeBridge::start(0).await;
    let hub = Hub::start(fake.port).await;
    let client = hub.client("").await;
    let (err, text) = client.call("level", "get_outliner", json!({ "_bytes": 2_000_000 })).await;
    assert!(!err);
    assert!(text.len() > 2_000_000);
    fake.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn locks_keep_agents_off_each_others_assets() {
    let fake = FakeBridge::start(0).await;
    let hub = Hub::start(fake.port).await;
    let alice = hub.client("?agent=alice").await;
    let bob = hub.client("?agent=bob").await;
    let door = "/Game/Doors/BP_Door";

    let (err, text) = alice.call("asset", "lock", json!({ "assetPath": door })).await;
    assert!(!err && text.contains("\"acquired\":true"), "{text}");

    let (err, text) = bob.call("blueprint", "add_variable", json!({ "path": format!("{door}.BP_Door"), "name": "bOpen", "varType": "bool" })).await;
    assert!(err, "bob must be refused: {text}");
    assert!(text.contains("alice"), "{text}");

    let (err, text) = bob.call("asset", "lock", json!({ "assetPath": door })).await;
    assert!(!err && text.contains("\"acquired\":false"), "{text}");

    let (err, _) = alice.call("blueprint", "add_variable", json!({ "path": door, "name": "bOpen", "varType": "bool" })).await;
    assert!(!err, "the holder is not blocked by its own lock");

    let (err, _) = alice.call("asset", "unlock", json!({ "assetPath": door })).await;
    assert!(!err);
    let (err, text) = bob.call("blueprint", "add_variable", json!({ "path": door, "name": "bOpen", "varType": "bool" })).await;
    assert!(!err, "{text}");
    fake.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn survives_an_editor_restart() {
    let fake = FakeBridge::start(0).await;
    let port = fake.port;
    let hub = Hub::start(port).await;
    let client = hub.client("").await;
    assert!(!client.call("level", "place_actor", json!({ "actorClass": "A" })).await.0);

    fake.stop();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (err, text) = client.call("level", "place_actor", json!({ "actorClass": "A" })).await;
    assert!(err && text.contains("not connected"), "{text}");

    let fake = FakeBridge::start(port).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (err, text) = client.call("level", "place_actor", json!({ "actorClass": "A" })).await;
        if !err {
            break;
        }
        assert!(Instant::now() < deadline, "never reconnected: {text}");
    }
    fake.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn stdio_serves_subagents_concurrently() {
    let fake = FakeBridge::start(0).await;
    let mut child = Command::new(env!("CARGO_BIN_EXE_ue-mcp-rs"))
        .args(["--bridge-port", &fake.port.to_string(), "--connections", "4"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    let send = |v: Value| format!("{v}\n");
    stdin.write_all(send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": { "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "e2e" } } })).as_bytes()).await.unwrap();
    let init: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    stdin.write_all(send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).as_bytes()).await.unwrap();
    // Warm up: the first call waits for the sockets.
    stdin.write_all(send(json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/call", "params": { "name": "project", "arguments": { "action": "get_status" } } })).as_bytes()).await.unwrap();
    lines.next_line().await.unwrap().unwrap();

    let started = Instant::now();
    for i in 1..=4 {
        let call = json!({ "jsonrpc": "2.0", "id": i, "method": "tools/call", "params": { "name": "level", "arguments": { "action": "place_actor", "args": { "actorClass": "A", "_sleepMs": 500 } } } });
        stdin.write_all(send(call).as_bytes()).await.unwrap();
    }
    let mut ids = Vec::new();
    for _ in 0..4 {
        let reply: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(reply["result"]["isError"], false, "{reply}");
        ids.push(reply["id"].as_i64().unwrap());
    }
    ids.sort();
    assert_eq!(ids, vec![1, 2, 3, 4]);
    assert!(started.elapsed() < Duration::from_millis(1200), "took {:?}", started.elapsed());
    fake.stop();
}

#[tokio::test(flavor = "multi_thread")]
async fn subagents_sharing_one_stdio_session_keep_separate_locks() {
    let fake = FakeBridge::start(0).await;
    let mut hub = StdioHub::start(fake.port).await;
    let level = "/Game/Maps/Arena";

    let (err, text) = hub.call("asset", "lock", json!({ "assetPath": level, "agent": "lighting" })).await;
    assert!(!err && text.contains("\"acquired\":true"), "{text}");
    let (err, text) = hub.call("level", "place_actor", json!({ "assetPath": level, "actorClass": "A", "agent": "props" })).await;
    assert!(err && text.contains("lighting"), "{text}");
    let (err, text) = hub.call("level", "place_actor", json!({ "assetPath": level, "actorClass": "A", "agent": "lighting" })).await;
    assert!(!err, "{text}");
    assert!(!text.contains("\"agent\""), "routing arg must not reach the editor: {text}");
    fake.stop();
}
