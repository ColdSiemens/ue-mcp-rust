//! WebSocket client for the UE-MCP editor plugin.
//!
//! The plugin runs one thread per socket. A stock plugin answers the requests
//! on a socket strictly one after another, so a single shared socket (what
//! the Node daemon uses) puts every agent in one queue behind the slowest
//! call. This client keeps a pool of sockets and sends each request on the
//! least loaded one. Against a stock plugin it only ever uses an idle socket,
//! so N sockets give N requests in flight; against a plugin with the
//! `concurrent-dispatch` patch every socket takes many at once.
//!
//! Each socket reconnects on its own, so an editor restart costs the requests
//! in flight at that moment and nothing else.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, Notify};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, info, warn};

use crate::config::PortResolver;

/// The plugin closes the socket on any inbound message larger than this.
pub const BRIDGE_MAX_INBOUND_BYTES: usize = 64 * 1024 * 1024;
/// Answers can be large (height maps, mesh data); the plugin caps its own at 32M chars.
const MAX_ANSWER_BYTES: usize = 512 * 1024 * 1024;
/// Feature the patched plugin advertises when one socket runs requests concurrently.
pub const FEATURE_CONCURRENT_DISPATCH: &str = "concurrent-dispatch";

#[derive(Debug, Clone)]
pub enum BridgeError {
    NotConnected(String),
    Timeout { method: String, after: Duration },
    ConnectionLost(String),
    Rpc { code: i64, message: String },
    TooLarge { bytes: usize },
}

impl fmt::Display for BridgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BridgeError::NotConnected(detail) => write!(f, "Unreal editor is not connected ({detail}). Start the editor with the UE_MCP_Bridge plugin enabled."),
            BridgeError::Timeout { method, after } => write!(
                f,
                "Bridge call '{method}' got no answer within {}s. The editor may still finish it: check the result before retrying a mutation. Pass timeoutMs for long operations.",
                after.as_secs()
            ),
            BridgeError::ConnectionLost(reason) => write!(f, "Connection to the editor was lost while the call was in flight ({reason}); its outcome is unknown."),
            BridgeError::Rpc { code, message } => write!(f, "{message} (bridge error {code})"),
            BridgeError::TooLarge { bytes } => write!(
                f,
                "Request is {bytes} bytes; the editor plugin refuses messages over {} bytes. Split it into smaller batches.",
                BRIDGE_MAX_INBOUND_BYTES
            ),
        }
    }
}

impl std::error::Error for BridgeError {}

struct Pending {
    tx: oneshot::Sender<Result<Value, BridgeError>>,
    method: String,
    started: Instant,
}

struct Conn {
    index: usize,
    /// Present while the socket is up. Locked before `pending`, never the other way round.
    outbox: Mutex<Option<mpsc::UnboundedSender<Message>>>,
    pending: Mutex<HashMap<u64, Pending>>,
    inflight: AtomicUsize,
    port: AtomicUsize,
    answered: AtomicU64,
}

#[derive(Default)]
struct Capabilities {
    raw: Option<Arc<Value>>,
    methods: Option<Arc<HashSet<String>>>,
    concurrent: bool,
}

pub struct Bridge {
    conns: Vec<Arc<Conn>>,
    next_id: AtomicU64,
    rotate: AtomicUsize,
    connected: AtomicUsize,
    /// Woken on every connect, disconnect and answer.
    changed: Notify,
    caps: Mutex<Capabilities>,
    resolver: PortResolver,
    connect_wait: Duration,
}

impl Bridge {
    pub fn start(resolver: PortResolver, pool_size: usize, connect_wait: Duration) -> Arc<Bridge> {
        let conns = (0..pool_size.max(1))
            .map(|index| {
                Arc::new(Conn {
                    index,
                    outbox: Mutex::new(None),
                    pending: Mutex::new(HashMap::new()),
                    inflight: AtomicUsize::new(0),
                    port: AtomicUsize::new(0),
                    answered: AtomicU64::new(0),
                })
            })
            .collect::<Vec<_>>();
        let bridge = Arc::new(Bridge {
            conns,
            next_id: AtomicU64::new(1),
            rotate: AtomicUsize::new(0),
            connected: AtomicUsize::new(0),
            changed: Notify::new(),
            caps: Mutex::new(Capabilities::default()),
            resolver,
            connect_wait,
        });
        for conn in &bridge.conns {
            tokio::spawn(bridge.clone().supervise(conn.clone()));
        }
        bridge
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire) > 0
    }

    pub fn capabilities(&self) -> Option<Arc<Value>> {
        self.caps.lock().unwrap().raw.clone()
    }

    /// The methods the running plugin registered, once a handshake has answered.
    pub fn registered_methods(&self) -> Option<Arc<HashSet<String>>> {
        self.caps.lock().unwrap().methods.clone()
    }

    pub fn concurrent_dispatch(&self) -> bool {
        self.caps.lock().unwrap().concurrent
    }

    /// The running editor's engine version, e.g. "5.5.4-40574608+++UE5+Release-5.5".
    pub fn engine_version(&self) -> Option<String> {
        self.capabilities().and_then(|c| c.get("engineVersion").and_then(Value::as_str).map(str::to_string))
    }

    /// Wait until at least one socket is up, or the timeout passes.
    pub async fn wait_connected(&self, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let notified = self.changed.notified();
            if self.is_connected() {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.is_connected();
            }
        }
    }

    pub fn status(&self) -> Value {
        let conns: Vec<Value> = self
            .conns
            .iter()
            .map(|c| {
                let port = c.port.load(Ordering::Relaxed);
                json!({
                    "index": c.index,
                    "connected": port != 0,
                    "port": if port == 0 { Value::Null } else { json!(port) },
                    "inFlight": c.inflight.load(Ordering::Relaxed),
                    "answered": c.answered.load(Ordering::Relaxed),
                })
            })
            .collect();
        let caps = self.capabilities();
        json!({
            "connected": self.is_connected(),
            "portCandidates": self.resolver.candidates(),
            "concurrentDispatch": self.concurrent_dispatch(),
            "engineVersion": caps.as_ref().and_then(|c| c.get("engineVersion").cloned()),
            "editorProject": caps.as_ref().and_then(|c| c.get("projectName").cloned()),
            "pluginProtocolVersion": caps.as_ref().and_then(|c| c.get("protocolVersion").cloned()),
            "pluginActionCount": caps.as_ref().and_then(|c| c.get("actionCount").cloned()),
            "pluginFeatures": caps.as_ref().and_then(|c| c.get("features").cloned()),
            "connections": conns,
        })
    }

    /// Send one request and wait up to `wait` for its answer, counted from the
    /// moment it was written. Time spent waiting for a free socket is extra.
    pub async fn call(&self, method: &str, params: Value, frame_timeout_ms: Option<u64>, wait: Duration) -> Result<Value, BridgeError> {
        self.call_checked(method, params, frame_timeout_ms, wait, None).await
    }

    /// `call`, plus a frame-level `lockCheck` the plugin tests against its
    /// shared lock registry right before it runs the handler.
    pub async fn call_checked(&self, method: &str, params: Value, frame_timeout_ms: Option<u64>, wait: Duration, lock_check: Option<Value>) -> Result<Value, BridgeError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut frame = json!({ "id": id.to_string(), "method": method, "params": params });
        if let Some(ms) = frame_timeout_ms {
            frame["timeoutMs"] = json!(ms);
        }
        if let Some(check) = lock_check {
            frame["lockCheck"] = check;
        }
        let text = serde_json::to_string(&frame).expect("a Value always serializes");
        drop(frame);
        if text.len() > BRIDGE_MAX_INBOUND_BYTES {
            return Err(BridgeError::TooLarge { bytes: text.len() });
        }

        let rx = self.enqueue(id, method, text, wait).await?;
        match tokio::time::timeout(wait, rx).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err(BridgeError::ConnectionLost("socket closed".into())),
            // The pending entry stays: the late answer still frees the socket's slot.
            Err(_) => Err(BridgeError::Timeout { method: method.to_string(), after: wait }),
        }
    }

    async fn enqueue(&self, id: u64, method: &str, mut text: String, queue_limit: Duration) -> Result<oneshot::Receiver<Result<Value, BridgeError>>, BridgeError> {
        let started = tokio::time::Instant::now();
        loop {
            let notified = self.changed.notified();
            if let Some(conn) = self.pick() {
                let (tx, rx) = oneshot::channel();
                let outbox = conn.outbox.lock().unwrap();
                if let Some(out) = outbox.as_ref() {
                    conn.pending.lock().unwrap().insert(id, Pending { tx, method: method.to_string(), started: Instant::now() });
                    conn.inflight.fetch_add(1, Ordering::AcqRel);
                    match out.send(Message::Text(text)) {
                        Ok(()) => return Ok(rx),
                        Err(mpsc::error::SendError(returned)) => {
                            conn.pending.lock().unwrap().remove(&id);
                            conn.inflight.fetch_sub(1, Ordering::AcqRel);
                            text = match returned {
                                Message::Text(t) => t,
                                _ => unreachable!("only text frames are queued"),
                            };
                        }
                    }
                }
                continue;
            }
            // Nothing usable: either no socket is up, or (stock plugin) every socket is busy.
            let elapsed = started.elapsed();
            let limit = if self.is_connected() { queue_limit } else { self.connect_wait };
            if elapsed >= limit {
                return Err(if self.is_connected() {
                    BridgeError::Timeout { method: method.to_string(), after: elapsed }
                } else {
                    BridgeError::NotConnected(format!("tried ports {:?}", self.resolver.candidates()))
                });
            }
            let _ = tokio::time::timeout(limit - elapsed, notified).await;
        }
    }

    /// The least loaded live socket. Against a stock plugin only idle sockets
    /// qualify, because a busy one would hold the request behind another.
    fn pick(&self) -> Option<Arc<Conn>> {
        let serial = !self.concurrent_dispatch();
        let n = self.conns.len();
        let start = self.rotate.fetch_add(1, Ordering::Relaxed);
        let mut best: Option<(&Arc<Conn>, usize)> = None;
        for k in 0..n {
            let conn = &self.conns[(start + k) % n];
            if conn.port.load(Ordering::Acquire) == 0 {
                continue;
            }
            let load = conn.inflight.load(Ordering::Acquire);
            if serial && load > 0 {
                continue;
            }
            if best.is_none_or(|(_, l)| load < l) {
                best = Some((conn, load));
            }
        }
        best.map(|(c, _)| c.clone())
    }

    async fn supervise(self: Arc<Self>, conn: Arc<Conn>) {
        // Stagger the pool so a cold start does not hit the editor with N handshakes at once.
        tokio::time::sleep(Duration::from_millis(40 * conn.index as u64)).await;
        let mut backoff = Duration::from_millis(250);
        loop {
            match self.connect_once(&conn).await {
                Ok(()) => backoff = Duration::from_millis(250),
                Err(e) => debug!(conn = conn.index, "bridge connect failed: {e}"),
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(2));
        }
    }

    async fn connect_once(&self, conn: &Arc<Conn>) -> anyhow::Result<()> {
        let config = WebSocketConfig {
            max_message_size: Some(MAX_ANSWER_BYTES),
            max_frame_size: Some(MAX_ANSWER_BYTES),
            ..Default::default()
        };
        let mut last_err = None;
        for port in self.resolver.candidates() {
            let url = format!("ws://127.0.0.1:{port}");
            match tokio::time::timeout(Duration::from_secs(3), tokio_tungstenite::connect_async_with_config(url.as_str(), Some(config), true)).await {
                Ok(Ok((ws, _))) => return self.run_session(conn, ws, port).await,
                Ok(Err(e)) => last_err = Some(anyhow::anyhow!("{url}: {e}")),
                Err(_) => last_err = Some(anyhow::anyhow!("{url}: connect timed out")),
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no port candidates")))
    }

    async fn run_session<S>(&self, conn: &Arc<Conn>, ws: tokio_tungstenite::WebSocketStream<S>, port: u16) -> anyhow::Result<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sink, mut stream) = ws.split();

        // Handshake: what the plugin is and which methods it registered. Served
        // off the game thread, so it answers even while the editor is busy.
        let hs_id = format!("hs-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        sink.send(Message::Text(json!({ "id": hs_id, "method": "get_bridge_capabilities", "params": {} }).to_string())).await?;
        let handshake = tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(msg) = stream.next().await {
                if let Message::Text(t) = msg? {
                    let mut v: Value = serde_json::from_str(&t)?;
                    if v.get("id").and_then(Value::as_str) == Some(hs_id.as_str()) {
                        return Ok::<_, anyhow::Error>(v.get_mut("result").map(Value::take));
                    }
                }
            }
            anyhow::bail!("closed during handshake")
        })
        .await;
        match handshake {
            Ok(Ok(Some(caps))) => self.store_caps(caps),
            Ok(Ok(None)) => warn!("plugin answered the handshake without a result; treating it as a legacy build"),
            Ok(Err(e)) => return Err(e),
            Err(_) => warn!(conn = conn.index, "handshake timed out; continuing without capabilities"),
        }

        let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
        *conn.outbox.lock().unwrap() = Some(tx);
        conn.port.store(port as usize, Ordering::Release);
        let up = self.connected.fetch_add(1, Ordering::AcqRel) + 1;
        if up == 1 || conn.index == 0 {
            info!(conn = conn.index, port, "connected to the editor bridge");
        }
        self.changed.notify_waiters();

        let writer = tokio::spawn(async move {
            while let Some(m) = rx.recv().await {
                if sink.send(m).await.is_err() {
                    break;
                }
            }
            let _ = sink.close().await;
        });

        let reason = loop {
            match stream.next().await {
                Some(Ok(Message::Text(t))) => self.on_answer(conn, &t),
                Some(Ok(Message::Binary(b))) => match String::from_utf8(b) {
                    Ok(t) => self.on_answer(conn, &t),
                    Err(_) => warn!("ignoring a non-UTF-8 binary frame from the editor"),
                },
                Some(Ok(Message::Close(frame))) => {
                    break match frame {
                        Some(f) => format!("editor closed the socket ({}: {})", u16::from(f.code), f.reason),
                        None => "editor closed the socket".to_string(),
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => break e.to_string(),
                None => break "socket ended".to_string(),
            }
        };

        // Unpublish before closing the outbox, so pick() stops choosing this socket first.
        conn.port.store(0, Ordering::Release);
        *conn.outbox.lock().unwrap() = None;
        self.connected.fetch_sub(1, Ordering::AcqRel);
        writer.abort();
        let orphans: Vec<Pending> = conn.pending.lock().unwrap().drain().map(|(_, p)| p).collect();
        conn.inflight.store(0, Ordering::Release);
        if !orphans.is_empty() {
            warn!(conn = conn.index, count = orphans.len(), "bridge socket dropped with calls in flight: {reason}");
        } else {
            debug!(conn = conn.index, "bridge socket closed: {reason}");
        }
        for p in orphans {
            let _ = p.tx.send(Err(BridgeError::ConnectionLost(reason.clone())));
        }
        self.changed.notify_waiters();
        Ok(())
    }

    fn store_caps(&self, caps: Value) {
        let methods = caps.get("actions").and_then(Value::as_array).map(|a| {
            Arc::new(a.iter().filter_map(Value::as_str).map(str::to_string).collect::<HashSet<_>>())
        });
        let concurrent = caps
            .get("features")
            .and_then(Value::as_array)
            .is_some_and(|f| f.iter().any(|x| x.as_str() == Some(FEATURE_CONCURRENT_DISPATCH)));
        let mut c = self.caps.lock().unwrap();
        if c.concurrent != concurrent || c.raw.is_none() {
            info!(concurrent, actions = methods.as_ref().map(|m| m.len()), "editor plugin capabilities");
        }
        *c = Capabilities { raw: Some(Arc::new(caps)), methods, concurrent };
    }

    fn on_answer(&self, conn: &Conn, text: &str) {
        let mut v: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                warn!("unparseable answer from the editor: {e}");
                return;
            }
        };
        let id = match v.get("id") {
            Some(Value::String(s)) => s.parse::<u64>().ok(),
            Some(Value::Number(n)) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
            _ => None,
        };
        let Some(id) = id else {
            debug!("answer without a request id: {}", truncate(text, 200));
            return;
        };
        let Some(pending) = conn.pending.lock().unwrap().remove(&id) else {
            return;
        };
        conn.inflight.fetch_sub(1, Ordering::AcqRel);
        conn.answered.fetch_add(1, Ordering::Relaxed);
        let answer = match v.get("error") {
            Some(err) if !err.is_null() => Err(BridgeError::Rpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                message: err.get("message").and_then(Value::as_str).unwrap_or("bridge error").to_string(),
            }),
            _ => Ok(v.get_mut("result").map(Value::take).unwrap_or(Value::Null)),
        };
        if pending.tx.send(answer).is_err() {
            info!(method = %pending.method, secs = pending.started.elapsed().as_secs(), "editor answered after the caller gave up");
        }
        self.changed.notify_waiters();
    }
}

fn truncate(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}
