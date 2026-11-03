//! From a `tools/call` to a bridge call and back.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::bridge::{Bridge, BridgeError};
use crate::config::Project;
use crate::epic;
use crate::locks::{self, LockTable};
use crate::project_files;
use crate::surface::{Action, Dispatch, Effect, Surface, Tool, View};

/// What the plugin waits for a handler that registered no timeout of its own.
const DEFAULT_HANDLER_TIMEOUT_MS: u64 = 30_000;
/// Slack over the plugin's own deadline, so its error arrives before ours.
const SERVER_TIMEOUT_MARGIN_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 3_600_000;
const DEFAULT_LOCK_TTL_SECS: u64 = 300;

#[derive(Debug, Clone)]
pub struct HubOptions {
    pub enforce_locks: bool,
    pub hide_epic: bool,
    pub hide_unregistered: bool,
    /// Engine folder for start_editor and builds, when the .uproject's
    /// EngineAssociation does not lead to one.
    pub engine_root: Option<std::path::PathBuf>,
    /// Extra command-line arguments start_editor passes after the .uproject.
    pub editor_args: Vec<String>,
}

/// One MCP client: an agent, or a whole Claude Code process over stdio.
pub struct Session {
    pub id: String,
    /// Lock owner. Several sessions can share one by connecting with the same agent name.
    pub owner: String,
    pub view: View,
    pub client: Mutex<Option<String>>,
    pub calls: AtomicU64,
    pub last_seen: Mutex<Instant>,
}

impl Session {
    pub fn new(id: String, owner: String, view: View) -> Session {
        Session { id, owner, view, client: Mutex::new(None), calls: AtomicU64::new(0), last_seen: Mutex::new(Instant::now()) }
    }
}

pub struct Hub {
    pub bridge: Arc<Bridge>,
    pub surface: Surface,
    pub locks: LockTable,
    pub opts: HubOptions,
    pub project: Option<Project>,
    pub sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// (lock owner, asset) -> graphs that owner added nodes to since its last
    /// auto_layout, in the order it touched them.
    touched: Mutex<HashMap<(String, String), Vec<String>>>,
    started: Instant,
    calls: AtomicU64,
}

/// Blueprint actions after which a graph counts as touched by the agent.
pub const GRAPH_WRITES: &[&str] = &["add_node", "import_nodes_t3d", "connect_pins", "connect_pins_batch"];

/// What `tools/call` returns.
pub struct ToolResult {
    pub text: String,
    pub is_error: bool,
}

impl ToolResult {
    fn error(msg: impl Into<String>) -> ToolResult {
        ToolResult { text: format!("Error: {}", msg.into()), is_error: true }
    }

    pub fn to_mcp(&self) -> Value {
        json!({ "content": [{ "type": "text", "text": self.text }], "isError": self.is_error })
    }
}

type CallResult = Result<Value, String>;

/// Dispatch keys the hub consumes; never forwarded to a handler.
struct Routing {
    timeout_ms: Option<u64>,
    select: Option<Vec<String>>,
    omit: Option<Vec<String>>,
    /// Lock identity for this call. Subagents of one Claude Code process share
    /// one MCP session, so this is how they stay distinct owners.
    agent: Option<String>,
}

impl Routing {
    fn take(params: &mut Map<String, Value>) -> Routing {
        let timeout_ms = params.remove("timeoutMs").and_then(|v| match v {
            Value::Number(n) => n.as_f64().map(|f| f.max(0.0) as u64),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        });
        let list = |v: Option<Value>| -> Option<Vec<String>> {
            match v? {
                Value::Array(a) => Some(a.iter().filter_map(Value::as_str).map(str::to_string).collect()),
                Value::String(s) => Some(s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()),
                _ => None,
            }
        };
        let select = list(params.remove("select"));
        let omit = list(params.remove("omit"));
        let agent = match params.remove("agent") {
            Some(Value::String(s)) if !s.trim().is_empty() => Some(s.trim().to_string()),
            _ => None,
        };
        params.remove("editor");
        params.remove("toEditor");
        Routing { timeout_ms: timeout_ms.filter(|&t| t > 0), select, omit, agent }
    }

    fn shape(&self, v: Value) -> Value {
        let Value::Object(mut m) = v else { return v };
        if let Some(keep) = &self.select {
            m.retain(|k, _| k == "success" || k == "error" || keep.iter().any(|s| s == k));
        }
        if let Some(drop) = &self.omit {
            m.retain(|k, _| !drop.iter().any(|s| s == k));
        }
        Value::Object(m)
    }
}

impl Hub {
    pub fn new(bridge: Arc<Bridge>, surface: Surface, opts: HubOptions, project: Option<Project>) -> Hub {
        Hub {
            bridge,
            surface,
            locks: LockTable::default(),
            opts,
            project,
            sessions: Mutex::new(HashMap::new()),
            touched: Mutex::new(HashMap::new()),
            started: Instant::now(),
            calls: AtomicU64::new(0),
        }
    }

    pub fn touched_graphs(&self, owner: &str, asset: &str) -> Vec<String> {
        self.touched.lock().unwrap().get(&(owner.to_string(), locks::normalize(asset))).cloned().unwrap_or_default()
    }

    pub fn forget_touched(&self, owner: &str, asset: &str) {
        self.touched.lock().unwrap().remove(&(owner.to_string(), locks::normalize(asset)));
    }

    fn note_touched(&self, owner: &str, asset: &str, graph: &str) {
        let mut touched = self.touched.lock().unwrap();
        let graphs = touched.entry((owner.to_string(), locks::normalize(asset))).or_default();
        if !graphs.iter().any(|g| g == graph) {
            graphs.push(graph.to_string());
        }
    }

    /// The session's view plus what the running plugin actually registered.
    pub fn effective_view(&self, session: &Session) -> View {
        let mut view = session.view.clone();
        view.hide_epic |= self.opts.hide_epic;
        if self.opts.hide_unregistered {
            view.registered = self.bridge.registered_methods();
            // The plugin registers epic_call_tool on every engine, but the
            // toolset registry it calls into is UE 5.8+.
            if self.bridge.engine_version().as_deref().and_then(engine_minor).is_some_and(|v| v < (5, 8)) {
                view.hide_epic = true;
            }
        }
        view
    }

    pub async fn list_tools(&self, session: &Session) -> Vec<Value> {
        // A client that lists before the editor is up would otherwise get a
        // surface that changes later; give a starting editor a moment.
        if !self.bridge.is_connected() {
            self.bridge.wait_connected(Duration::from_secs(2)).await;
        }
        self.surface.list(&self.effective_view(session))
    }

    pub async fn call_tool(&self, session: &Session, name: &str, arguments: Value) -> ToolResult {
        self.calls.fetch_add(1, Ordering::Relaxed);
        session.calls.fetch_add(1, Ordering::Relaxed);
        *session.last_seen.lock().unwrap() = Instant::now();

        let Some(tool) = self.surface.tool(name).filter(|t| session.view.allows_tool(&t.name)) else {
            return ToolResult::error(format!("unknown tool '{name}'"));
        };
        let mut params = match unwrap_envelope(arguments) {
            Ok(p) => p,
            Err(msg) => return ToolResult::error(format!("{name}: {msg}")),
        };
        let action_name = match params.remove("action") {
            Some(Value::String(s)) => s,
            _ => return ToolResult::error(format!("{name}: 'action' is required, e.g. {name}(action=\"{}\", args={{...}})", tool.actions[0].name)),
        };
        let Some(action) = tool.action(&action_name) else {
            return ToolResult::error(unknown_action(tool, &action_name));
        };
        // A new session's first call can arrive before its sockets are up; the
        // guard below needs the plugin's handshake, so give it a moment.
        if !matches!(action.dispatch, Dispatch::Local) && !self.bridge.is_connected() {
            self.bridge.wait_connected(Duration::from_secs(2)).await;
        }
        let view = self.effective_view(session);
        // Guard: never send an editor a method its plugin build does not have.
        // Version-gated handlers are simply not registered on older engines.
        if !view.allows_action(action) {
            return ToolResult::error(self.unavailable_message(tool, action));
        }
        let routing = Routing::take(&mut params);
        let owner = routing.agent.clone().unwrap_or_else(|| session.owner.clone());
        self.coerce_json_strings(action, &mut params);

        // Writes are checked twice: here against the locks this process
        // granted, and by the plugin against every lock in the editor, which
        // covers the ones other server processes (other sessions) took.
        let mut lock_check = None;
        if self.opts.enforce_locks && action.effect != Effect::Read && !matches!(action.dispatch, Dispatch::Local) {
            let paths = locks::write_targets(&params, action.signature.as_deref());
            if let Some(conflict) = self.locks.conflict(&paths, &owner) {
                let body = json!({
                    "success": false,
                    "error": "Another agent holds a lock on this asset. Work on something else, or wait and retry.",
                    "lock": conflict,
                });
                return ToolResult { text: body.to_string(), is_error: true };
            }
            // Sent without waiting for the handshake to say the plugin checks
            // it: a new session's first write can beat the handshake, and a
            // plugin without the check ignores the field.
            if !paths.is_empty() {
                lock_check = Some(json!({ "sessionId": owner, "paths": paths }));
            }
        }

        let wants_wait = action.follow_up && params.get("wait") == Some(&Value::Bool(true));
        let wait_timeout = params.get("timeoutSeconds").and_then(Value::as_f64);
        // Which graph this call adds nodes to, so auto_layout can default to it.
        let touches = (tool.name == "blueprint" && GRAPH_WRITES.contains(&action.name.as_str()))
            .then(|| {
                let asset = params.get("assetPath").or_else(|| params.get("path")).and_then(Value::as_str)?;
                let graph = params.get("graphSelector").or_else(|| params.get("graphName")).and_then(Value::as_str).unwrap_or("EventGraph");
                Some((asset.to_string(), graph.to_string()))
            })
            .flatten();

        let outcome = match &action.dispatch {
            Dispatch::Bridge { method } => self.bridge_call_checked(method, Value::Object(params), routing.timeout_ms.or(action.timeout_ms), lock_check).await,
            Dispatch::Epic { toolset, tool: epic_tool, schema } => match epic::build_call(toolset, epic_tool, schema, &params) {
                Ok(envelope) => self.bridge_call_checked("epic_call_tool", envelope, routing.timeout_ms.or(action.timeout_ms), lock_check).await,
                Err(msg) => Err(msg),
            },
            Dispatch::Local => self.local(session, tool, action, params, &routing).await,
        };
        let outcome = match outcome {
            Ok(answer) if wants_wait && tool.name == "pcg" => Ok(self.wait_after_regenerate(answer, wait_timeout).await),
            other => other,
        };

        match outcome {
            Ok(v) => {
                let is_error = v.get("success") == Some(&Value::Bool(false));
                if let (false, Some((asset, graph))) = (is_error, &touches) {
                    self.note_touched(&owner, asset, graph);
                }
                let v = routing.shape(v);
                let text = match v {
                    Value::String(s) => s,
                    other => serde_json::to_string(&other).unwrap_or_default(),
                };
                ToolResult { text, is_error }
            }
            Err(msg) => ToolResult::error(msg),
        }
    }

    fn unavailable_message(&self, tool: &Tool, action: &Action) -> String {
        let engine = self.bridge.engine_version().map(|v| format!("UE {v}")).unwrap_or_else(|| "this engine version".into());
        if action.is_epic() {
            return format!(
                "{}.{} wraps Unreal's editor toolset registry, which is UE 5.8+ (running editor: {engine}), or epic actions are turned off on this server. Nothing was sent to the editor; project(action=\"search_tools\") finds alternatives.",
                tool.name, action.name
            );
        }
        format!(
            "{}.{} is not available in the running editor ({engine}): its UE_MCP_Bridge build does not register '{}'. The engine API it needs is newer than this version, or the plugin is older than this server. Nothing was sent to the editor; project(action=\"search_tools\") finds alternatives.",
            tool.name,
            action.name,
            action.bridge_method().unwrap_or("?")
        )
    }

    /// Strings holding JSON where the handler declares an array or object, which models often send.
    fn coerce_json_strings(&self, action: &Action, params: &mut Map<String, Value>) {
        let Some(method) = action.bridge_method() else { return };
        let Some(specs) = self.surface.handler_spec(method).and_then(|s| s.get("params")).and_then(Value::as_array) else { return };
        for spec in specs {
            let ty = spec.get("type").and_then(Value::as_str).unwrap_or("");
            if ty != "array" && ty != "object" {
                continue;
            }
            let names = std::iter::once(spec.get("name")).chain(spec.get("aliases").and_then(Value::as_array).into_iter().flatten().map(Some));
            for name in names.flatten().filter_map(Value::as_str) {
                if let Some(v @ Value::String(_)) = params.get(name) {
                    let parsed = epic::parse_objectish(v);
                    if !parsed.is_string() {
                        params.insert(name.to_string(), parsed);
                    }
                }
            }
        }
    }

    pub async fn bridge_call(&self, method: &str, params: Value, frame_timeout_ms: Option<u64>) -> CallResult {
        self.bridge_call_checked(method, params, frame_timeout_ms, None).await
    }

    async fn bridge_call_checked(&self, method: &str, params: Value, frame_timeout_ms: Option<u64>, lock_check: Option<Value>) -> CallResult {
        let frame_timeout_ms = frame_timeout_ms.map(|t| t.min(MAX_TIMEOUT_MS));
        let registered = self.surface.handler_timeout_secs(method).map(|s| s * 1000).unwrap_or(DEFAULT_HANDLER_TIMEOUT_MS);
        let wait = registered.max(frame_timeout_ms.unwrap_or(0)).min(MAX_TIMEOUT_MS) + SERVER_TIMEOUT_MARGIN_MS;
        self.bridge
            .call_checked(method, strip_nulls(params), frame_timeout_ms, Duration::from_millis(wait), lock_check)
            .await
            .map_err(|e| describe(method, e))
    }

    async fn local(&self, session: &Session, tool: &Tool, action: &Action, mut p: Map<String, Value>, routing: &Routing) -> CallResult {
        let default_owner = routing.agent.as_deref().unwrap_or(&session.owner);
        let owner = p.get("sessionId").and_then(Value::as_str).filter(|s| !s.trim().is_empty()).unwrap_or(default_owner).to_string();
        let asset = p.get("assetPath").or_else(|| p.get("path")).and_then(Value::as_str).map(str::to_string);
        match (tool.name.as_str(), action.name.as_str()) {
            ("asset", "lock") => {
                let path = asset.ok_or("asset(lock) needs assetPath")?;
                let ttl = p.get("ttlSeconds").and_then(Value::as_f64).filter(|t| *t > 0.0).map(|t| t as u64).unwrap_or(DEFAULT_LOCK_TTL_SECS);
                if let Some(conflict) = self.locks.conflict(&[locks::normalize(&path)], &owner) {
                    return Ok(json!({ "success": true, "acquired": false, "holder": conflict["holder"], "sessionId": owner }));
                }
                let answer = self.bridge_call("acquire_lock", json!({ "assetPath": path, "sessionId": owner, "ttlSeconds": ttl }), None).await?;
                if answer.get("acquired") == Some(&Value::Bool(true)) {
                    self.locks.record(&path, &owner, Duration::from_secs(ttl));
                }
                Ok(answer)
            }
            ("asset", "unlock") => {
                let path = asset.ok_or("asset(unlock) needs assetPath")?;
                let force = p.get("force") == Some(&Value::Bool(true));
                let answer = self.bridge_call("release_lock", json!({ "assetPath": path, "sessionId": owner, "force": force }), None).await?;
                if answer.get("released") == Some(&Value::Bool(true)) || force {
                    self.locks.release(&path, &owner, force);
                }
                Ok(answer)
            }
            ("asset", "unlock_all") => {
                let mut answer = self.bridge_call("release_session_locks", json!({ "sessionId": owner }), None).await?;
                let mirrored = self.locks.release_owner(&owner);
                if let Value::Object(m) = &mut answer {
                    m.insert("hubReleased".into(), json!(mirrored));
                }
                Ok(answer)
            }
            ("asset", "search") => {
                if !p.contains_key("limit") {
                    if let Some(m) = p.get("maxResults").cloned() {
                        p.insert("limit".into(), m);
                    }
                }
                self.bridge_call("search_assets", Value::Object(p), routing.timeout_ms).await
            }
            ("asset", "migrate") => self.bridge_call("migrate", Value::Object(p), routing.timeout_ms).await,
            ("blueprint", "author") => self.author_blueprint(p).await,
            ("editor", "get_engine_state") => {
                if !self.bridge.is_connected() {
                    return Ok(json!({ "success": true, "connected": false, "hub": self.bridge.status() }));
                }
                self.bridge_call("get_engine_state", Value::Object(p), None).await
            }
            ("editor", "execute_python") => {
                let code = p.get("code").and_then(Value::as_str).ok_or("execute_python needs code")?;
                let forwarded = json!({
                    "code": code,
                    "resultVariable": p.get("resultVariable"),
                    "captureLog": p.get("captureLog"),
                    "maxLogChars": p.get("maxLogChars"),
                });
                self.bridge_call("execute_python", forwarded, routing.timeout_ms).await
            }
            ("editor", "play_in_editor_ignore_blueprint_errors") => {
                self.bridge_call("pie_start_ignoring_blueprint_errors", Value::Object(p), routing.timeout_ms).await
            }
            ("niagara", "batch") => self.niagara_batch(tool, p).await,
            ("pcg", "wait_for_generation") => {
                let selector = pick(&p, &["actorLabel", "actorPath", "componentName"]);
                Ok(self
                    .wait_for_generation(
                        selector,
                        p.get("runId").and_then(Value::as_i64),
                        p.get("timeoutSeconds").and_then(Value::as_f64).unwrap_or(600.0),
                        p.get("graceSeconds").and_then(Value::as_f64).unwrap_or(2.0),
                    )
                    .await)
            }
            ("project", "get_status") => Ok(self.status().await),
            ("project", "get_info") => self.project_info(),
            ("project", "describe_action") => {
                let name = p.get("name").and_then(Value::as_str).ok_or("describe_action needs name, e.g. \"level.place_actor\"")?;
                let category = p.get("category").and_then(Value::as_str);
                let hits = self.surface.find(name, category);
                match hits.len() {
                    0 => Ok(json!({
                        "success": false,
                        "error": format!("no action named '{name}'"),
                        "closest": self.surface.search(name, &session.view, 8),
                    })),
                    1 => {
                        let (t, a) = hits[0];
                        let mut out = self.surface.describe(t, a);
                        if self.bridge.registered_methods().is_some() {
                            let reg = View { registered: self.bridge.registered_methods(), ..View::default() };
                            out["availableInRunningEditor"] = json!(reg.allows_action(a));
                            out["engineVersion"] = json!(self.bridge.engine_version());
                        }
                        Ok(out)
                    }
                    _ => Ok(json!({
                        "ambiguous": true,
                        "matches": hits.iter().map(|(t, a)| json!({ "call": format!("{}.{}", t.name, a.name), "signature": a.signature })).collect::<Vec<_>>(),
                    })),
                }
            }
            ("project", "list_available_actions") => {
                let view = self.effective_view(session);
                let include_names = p.get("includeNames") == Some(&Value::Bool(true));
                let category = p.get("category").and_then(Value::as_str);
                let tools: Vec<Value> = self
                    .surface
                    .tools
                    .iter()
                    .filter(|t| view.allows_tool(&t.name) && category.is_none_or(|c| c == t.name))
                    .map(|t| {
                        let actions: Vec<&Action> = t.actions.iter().filter(|a| view.allows_action(a)).collect();
                        let mut entry = json!({
                            "category": t.name,
                            "summary": t.summary,
                            "actionCount": actions.len(),
                            "epicCount": actions.iter().filter(|a| a.is_epic()).count(),
                        });
                        if include_names || category.is_some() {
                            entry["actions"] = json!(actions.iter().map(|a| a.signature.clone().unwrap_or_else(|| a.name.clone())).collect::<Vec<_>>());
                        }
                        entry
                    })
                    .collect();
                Ok(json!({ "categories": tools }))
            }
            ("project", "search_tools") => {
                let query = p.get("query").and_then(Value::as_str).ok_or("search_tools needs query")?;
                let limit = p.get("limit").and_then(Value::as_u64).unwrap_or(10).clamp(1, 50) as usize;
                Ok(json!({ "query": query, "results": self.surface.search(query, &self.effective_view(session), limit) }))
            }
            ("editor", "start_editor") => self.start_editor(&p).await,
            ("editor", "stop_editor") => self.stop_editor().await,
            ("editor", "restart_editor") => self.restart_editor(&p).await,
            ("editor", "request_editor_shutdown") => {
                let flag = |k: &str| p.get(k).and_then(Value::as_bool).unwrap_or(true);
                let forwarded = json!({ "requireClean": flag("requireClean"), "endPIE": flag("endPIE") });
                self.bridge_call("request_editor_shutdown", forwarded, None).await
            }
            ("editor", "build_project") | ("project", "build") => self.build_project(&p, routing.timeout_ms).await,
            ("project", "read_config") => {
                let project = self.project.as_ref().ok_or("no project loaded")?;
                let name = p.get("configName").and_then(Value::as_str).ok_or("read_config needs configName, e.g. \"Engine\"")?;
                project_files::read_config(project, name)
            }
            ("project", "search_config") => {
                let project = self.project.as_ref().ok_or("no project loaded")?;
                let query = p.get("query").and_then(Value::as_str).ok_or("search_config needs query")?;
                project_files::search_config(project, query)
            }
            ("project", "list_config_tags") => project_files::list_config_tags(self.project.as_ref().ok_or("no project loaded")?),
            ("blueprint", "auto_layout") => self.auto_layout(&owner, &p).await,
            ("blueprint", "restore_layout") => self.restore_layout(&owner, &p).await,
            (t, a) => Err(format!("{t}({a}) is not available in ue-mcp-rust")),
        }
    }

    pub async fn status(&self) -> Value {
        let sessions: Vec<Value> = self
            .sessions
            .lock()
            .unwrap()
            .values()
            .map(|s| {
                json!({
                    "id": s.id,
                    "agent": s.owner,
                    "client": s.client.lock().unwrap().clone(),
                    "calls": s.calls.load(Ordering::Relaxed),
                    "idleSeconds": s.last_seen.lock().unwrap().elapsed().as_secs(),
                })
            })
            .collect();
        let mut out = json!({
            "success": true,
            "server": { "name": "ue-mcp-rust", "version": env!("CARGO_PKG_VERSION"), "uptimeSeconds": self.started.elapsed().as_secs(), "calls": self.calls.load(Ordering::Relaxed) },
            "project": self.project.as_ref().map(|p| json!({ "root": p.root, "name": p.name() })),
            "editor": self.bridge.status(),
            "surface": { "categories": self.surface.tools.len(), "actions": self.surface.action_count() },
            "sessions": sessions,
            "locks": self.locks.snapshot(),
        });
        if self.bridge.is_connected() {
            if let Ok(state) = self.bridge.call("get_engine_state", json!({}), None, Duration::from_secs(5)).await {
                out["engineState"] = state;
            }
        }
        out
    }

    fn project_info(&self) -> CallResult {
        let project = self.project.as_ref().ok_or("no project: start ue-mcp-rs with the .uproject path")?;
        let uproject = project.uproject.as_ref().ok_or("no .uproject file found")?;
        let text = std::fs::read_to_string(uproject).map_err(|e| format!("{}: {e}", uproject.display()))?;
        let contents: Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", uproject.display()))?;
        Ok(json!({
            "success": true,
            "projectName": project.name(),
            "engineAssociation": contents.get("EngineAssociation"),
            "projectDir": project.root,
            "contentDir": project.root.join("Content"),
            "uprojectContents": contents,
        }))
    }

    async fn author_blueprint(&self, p: Map<String, Value>) -> CallResult {
        let path = p.get("assetPath").and_then(Value::as_str).ok_or("author needs assetPath")?.to_string();
        let mut steps = Vec::new();
        let mut created = false;
        let mut run = |step: &str, target: String, answer: CallResult| {
            let ok = matches!(&answer, Ok(v) if v.get("success") != Some(&Value::Bool(false)));
            let entry = match answer {
                Ok(v) => json!({ "step": step, "target": target, "ok": ok, "result": v }),
                Err(e) => json!({ "step": step, "target": target, "ok": false, "error": e }),
            };
            steps.push(entry);
            ok
        };
        if let Some(parent) = p.get("parentClass").filter(|v| !v.is_null()) {
            let answer = self.bridge_call("create_blueprint", json!({ "path": path, "parentClass": parent }), None).await;
            created = matches!(&answer, Ok(v) if v.get("created") != Some(&Value::Bool(false)));
            run("create", path.clone(), answer);
        }
        let items = |key: &str| p.get(key).and_then(Value::as_array).cloned().unwrap_or_default();
        for c in items("components") {
            let class = c.get("componentClass").cloned().unwrap_or(Value::Null);
            let answer = self
                .bridge_call(
                    "add_component",
                    json!({
                        "path": path,
                        "componentClass": class,
                        "componentName": c.get("componentName").filter(|v| !v.is_null()).unwrap_or(&class),
                        "parentComponent": c.get("parentComponent"),
                        "childActorClass": c.get("childActorClass"),
                    }),
                    None,
                )
                .await;
            run("add_component", class.as_str().unwrap_or_default().to_string(), answer);
        }
        for v in items("variables") {
            let name = v.get("name").cloned().unwrap_or(Value::Null);
            let ty = v.get("varType").or_else(|| v.get("type")).cloned();
            let answer = self.bridge_call("add_variable", json!({ "path": path, "name": name, "type": ty }), None).await;
            run("add_variable", name.as_str().unwrap_or_default().to_string(), answer);
        }
        for f in items("functions") {
            let name = f.get("functionName").cloned().unwrap_or(Value::Null);
            let answer = self.bridge_call("create_function", json!({ "path": path, "functionName": name }), None).await;
            run("create_function", name.as_str().unwrap_or_default().to_string(), answer);
        }
        if p.get("compile") != Some(&Value::Bool(false)) {
            let answer = self.bridge_call("compile_blueprint", json!({ "path": path }), None).await;
            run("compile", path.clone(), answer);
        }
        let failed = steps.iter().filter(|s| s["ok"] != Value::Bool(true)).count();
        Ok(json!({ "assetPath": path, "created": created, "stepCount": steps.len(), "failedCount": failed, "ok": failed == 0, "steps": steps }))
    }

    async fn niagara_batch(&self, tool: &Tool, p: Map<String, Value>) -> CallResult {
        let ops = p.get("ops").map(epic::parse_objectish).and_then(|v| v.as_array().cloned()).ok_or("batch needs ops: [{action, params}]")?;
        let mut results = Vec::new();
        for (i, op) in ops.iter().enumerate() {
            let name = op.get("action").and_then(Value::as_str).unwrap_or_default();
            let Some(action) = tool.action(name).filter(|a| !matches!(a.dispatch, Dispatch::Local)) else {
                return Ok(json!({ "success": false, "completed": i, "results": results, "error": format!("op {i}: unknown niagara action '{name}'") }));
            };
            let params = op.get("params").or_else(|| op.get("args")).and_then(Value::as_object).cloned().unwrap_or_default();
            let answer = match &action.dispatch {
                Dispatch::Bridge { method } => self.bridge_call(method, Value::Object(params), action.timeout_ms).await,
                Dispatch::Epic { toolset, tool: t, schema } => match epic::build_call(toolset, t, schema, &params) {
                    Ok(env) => self.bridge_call("epic_call_tool", env, action.timeout_ms).await,
                    Err(e) => Err(e),
                },
                Dispatch::Local => unreachable!(),
            };
            match answer {
                Ok(v) if v.get("success") != Some(&Value::Bool(false)) => results.push(json!({ "action": name, "result": v })),
                Ok(v) => {
                    results.push(json!({ "action": name, "result": v }));
                    return Ok(json!({ "success": false, "completed": i, "results": results, "error": format!("op {i} ({name}) failed") }));
                }
                Err(e) => return Ok(json!({ "success": false, "completed": i, "results": results, "error": format!("op {i} ({name}): {e}") })),
            }
        }
        Ok(json!({ "success": true, "completed": results.len(), "results": results }))
    }

    /// Port of upstream pcg-wait.ts: poll the generation state until the run settles.
    async fn wait_for_generation(&self, selector: Map<String, Value>, run_id: Option<i64>, timeout_s: f64, grace_s: f64) -> Value {
        const FINISHED: &[&str] = &["generated", "cancelled", "cleaned", "not_started"];
        let started = Instant::now();
        let timeout = Duration::from_secs_f64(timeout_s.max(0.0));
        let grace = Duration::from_secs_f64(grace_s.max(0.0));
        let mut polls = 0u32;
        let mut poll = Duration::from_millis(250);

        let finish = |state: &str, last: &Value, error: Option<String>, polls: u32| {
            let mut out = json!({
                "success": state != "timed_out" && error.is_none(),
                "state": state,
                "durationMs": started.elapsed().as_millis() as u64,
                "polls": polls,
            });
            if let Some(id) = run_id {
                out["runId"] = json!(id);
            }
            for key in ["componentName", "lastRun"] {
                if let Some(v) = last.get(key) {
                    out[key] = v.clone();
                }
            }
            if state == "not_started" {
                let generated = last.get("generated").and_then(Value::as_bool).unwrap_or(false);
                out["alreadyGenerated"] = json!(generated);
                for key in ["lastFinishUtc", "lastFinishState"] {
                    if let Some(v) = last.get(key) {
                        out[key] = v.clone();
                    }
                }
                out["note"] = json!(if generated {
                    "Generate started nothing: the component's output was already up to date."
                } else {
                    "Generate started nothing and the component holds no generated output; check that it has a graph and is active."
                });
            }
            if let Some(e) = error {
                out["error"] = json!(e);
            }
            out
        };

        let read = |polls: &mut u32| {
            *polls += 1;
            self.bridge_call("get_pcg_generation_state", Value::Object(selector.clone()), None)
        };
        let mut last = match read(&mut polls).await {
            Ok(v) => v,
            Err(e) => return finish("timed_out", &Value::Null, Some(e), polls),
        };
        if last.get("success") == Some(&Value::Bool(false)) {
            let e = last.get("error").and_then(Value::as_str).unwrap_or("get_pcg_generation_state failed").to_string();
            return finish("timed_out", &last, Some(e), polls);
        }
        let start_count = last.get("finishCount").and_then(Value::as_i64).unwrap_or(0);
        let busy = |v: &Value| v.get("busy") == Some(&Value::Bool(true));
        let mut saw_busy = busy(&last);

        loop {
            if let Some(id) = run_id {
                let run = last.get("lastRun").filter(|r| !r.is_null());
                let run_state = run.and_then(|r| r.get("state")).and_then(Value::as_str);
                let run_no = run.and_then(|r| r.get("id")).and_then(Value::as_i64);
                let settled_early = run_state == Some("not_started") && started.elapsed() < grace;
                if run_no == Some(id) && run_state.is_some_and(|s| FINISHED.contains(&s)) && !busy(&last) && !settled_early {
                    return finish(run_state.unwrap(), &last, None, polls);
                }
                if run_no.is_some_and(|n| n > id) && !busy(&last) {
                    return finish("cancelled", &last, None, polls);
                }
            } else if !busy(&last) {
                if saw_busy || last.get("finishCount").and_then(Value::as_i64).unwrap_or(0) > start_count {
                    let state = last.get("lastFinishState").and_then(Value::as_str).unwrap_or("generated");
                    return finish(if FINISHED.contains(&state) { state } else { "generated" }, &last, None, polls);
                }
                if started.elapsed() >= grace {
                    return finish("idle", &last, None, polls);
                }
            }
            if started.elapsed() >= timeout {
                return finish("timed_out", &last, Some(format!("The generation did not finish within {}s.", timeout.as_secs())), polls);
            }
            tokio::time::sleep(poll).await;
            poll = (poll * 2).min(Duration::from_secs(1));
            last = match read(&mut polls).await {
                Ok(v) => v,
                Err(e) => return finish("timed_out", &Value::Null, Some(e), polls),
            };
            if last.get("success") == Some(&Value::Bool(false)) {
                let e = last.get("error").and_then(Value::as_str).unwrap_or("get_pcg_generation_state failed").to_string();
                return finish("timed_out", &last, Some(e), polls);
            }
            saw_busy |= busy(&last);
        }
    }

    async fn wait_after_regenerate(&self, answered: Value, timeout_s: Option<f64>) -> Value {
        let Value::Object(mut body) = answered else { return answered };
        let Some(run_id) = body.get("runId").and_then(Value::as_i64).filter(|_| body.get("success") != Some(&Value::Bool(false))) else {
            return Value::Object(body);
        };
        let selector = pick(&body, &["actorPath", "componentName"]);
        let wait = self.wait_for_generation(selector, Some(run_id), timeout_s.unwrap_or(600.0), 2.0).await;
        if wait["state"] == "timed_out" {
            let warning = wait.get("error").cloned().unwrap_or_else(|| json!("The wait timed out; the run is still going."));
            match body.get_mut("warnings") {
                Some(Value::Array(w)) => w.push(warning),
                _ => {
                    body.insert("warnings".into(), json!([warning]));
                }
            }
        }
        body.insert("wait".into(), wait);
        Value::Object(body)
    }
}

/// (major, minor) from an engine version like "5.5.4-40574608+++UE5+Release-5.5".
fn engine_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split(['.', '-']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

fn describe(method: &str, e: BridgeError) -> String {
    match e {
        BridgeError::Rpc { code: -32601, message } => {
            format!("{message}. The editor plugin does not register '{method}': it is older or newer than this hub's surface.")
        }
        other => other.to_string(),
    }
}

fn pick(p: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    keys.iter().filter_map(|k| p.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect()
}

/// `undefined` never reached the wire upstream; JSON null here is the same intent.
fn strip_nulls(v: Value) -> Value {
    match v {
        Value::Object(m) => Value::Object(m.into_iter().filter(|(_, v)| !v.is_null()).collect()),
        other => other,
    }
}

/// Merge `args` into the top level: tool(action, args={...}) is the documented
/// form, flat parameters beside `action` are accepted too.
pub fn unwrap_envelope(raw: Value) -> Result<Map<String, Value>, String> {
    let mut obj = match raw {
        Value::Object(m) => m,
        Value::Null => Map::new(),
        _ => return Err("arguments must be an object".into()),
    };
    let args = match obj.remove("args") {
        None | Some(Value::Null) => return Ok(obj),
        Some(Value::Object(m)) => m,
        Some(Value::String(s)) => match serde_json::from_str::<Value>(&s) {
            Ok(Value::Object(m)) => m,
            _ => return Err("args must be an object holding the action's parameters".into()),
        },
        Some(_) => return Err("args must be an object holding the action's parameters".into()),
    };
    for (k, v) in args {
        match obj.get(&k) {
            Some(existing) if existing != &v && k != "action" => {
                return Err(format!("'{k}' is given both inside args and beside it, with different values. Pass it once, inside args."));
            }
            _ => {
                obj.entry(k).or_insert(v);
            }
        }
    }
    Ok(obj)
}

fn unknown_action(tool: &Tool, name: &str) -> String {
    let mut scored: Vec<(usize, &str)> = tool
        .actions
        .iter()
        .map(|a| {
            let d = if a.name.contains(name) || name.contains(a.name.as_str()) { 0 } else { edit_distance(&a.name, name) };
            (d, a.name.as_str())
        })
        .collect();
    scored.sort();
    let close: Vec<&str> = scored.iter().take(6).map(|(_, n)| *n).collect();
    format!(
        "{}: unknown action '{name}'. Closest: {}. project(action=\"list_available_actions\", args={{category:\"{}\"}}) lists all {}.",
        tool.name,
        close.join(", "),
        tool.name,
        tool.actions.len()
    )
}

fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            cur[j + 1] = (prev[j] + usize::from(ca != *cb)).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_merges_args_and_flat_params() {
        let p = unwrap_envelope(json!({ "action": "x", "args": { "a": 1 }, "b": 2 })).unwrap();
        assert_eq!(p["a"], 1);
        assert_eq!(p["b"], 2);
        assert_eq!(p["action"], "x");
    }

    #[test]
    fn engine_versions_parse() {
        assert_eq!(engine_minor("5.5.4-40574608+++UE5+Release-5.5"), Some((5, 5)));
        assert_eq!(engine_minor("5.8.2-56702186+++UE5+Release-5.8"), Some((5, 8)));
        assert_eq!(engine_minor("garbage"), None);
    }

    #[test]
    fn envelope_rejects_disagreeing_duplicates() {
        assert!(unwrap_envelope(json!({ "args": { "a": 1 }, "a": 2 })).is_err());
    }

    #[test]
    fn envelope_accepts_stringified_args() {
        let p = unwrap_envelope(json!({ "args": "{\"a\": 1}" })).unwrap();
        assert_eq!(p["a"], 1);
    }

    #[test]
    fn routing_keys_are_consumed() {
        let mut p = json!({ "timeoutMs": "120000", "select": "a,b", "editor": "x", "keep": 1 }).as_object().unwrap().clone();
        let r = Routing::take(&mut p);
        assert_eq!(r.timeout_ms, Some(120_000));
        assert_eq!(r.select.unwrap(), vec!["a", "b"]);
        assert_eq!(p.len(), 1);
    }
}
