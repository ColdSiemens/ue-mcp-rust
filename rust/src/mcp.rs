//! MCP over JSON-RPC, independent of transport.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::dispatch::{Hub, Session};

pub const LATEST_PROTOCOL: &str = "2025-06-18";
const SUPPORTED_PROTOCOLS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];

pub const INSTRUCTIONS: &str = "\
UE-MCP (Rust hub): Unreal Engine editor control through the UE_MCP_Bridge plugin. One hub serves many agents at once.

Every category tool takes `action` and `args`: level(action=\"place_actor\", args={actorClass: \"PointLight\"}).
Start with project(action=\"get_status\") to see whether the editor is connected.
Signatures read action(param, ...): ? optional, a|b alias, :s string :n number :i integer :b bool :o object :[t] array, ref = asset path.
project(action=\"describe_action\", args={name:\"level.place_actor\"}) gives full parameter docs; project(action=\"search_tools\", args={query:\"...\"}) finds actions.

Working alongside other agents:
- Claim assets before multi-step edits: asset(action=\"lock\", args={assetPath}) ... asset(action=\"unlock\"). A mutation of an asset another agent holds is refused, not queued.
- If you are one of several subagents sharing this connection, put agent:\"<your name>\" in args on every call so your locks are yours.
- Keep to your own folder/level where possible; the editor runs one game-thread operation at a time, so prefer batch actions over many small calls.
- A timed-out mutation may still have landed: read before retrying.
- Optional routing args: timeoutMs (raise the wait for long operations), select / omit (trim result fields).

Not available in this hub: editor start/stop/build, checkpoints, flows, C++ source tools. Use your own file and shell tools for source code.";

pub fn negotiate(requested: Option<&str>) -> &'static str {
    requested.and_then(|r| SUPPORTED_PROTOCOLS.iter().find(|p| **p == r).copied()).unwrap_or(LATEST_PROTOCOL)
}

/// Handle one JSON-RPC message. None for notifications and responses.
pub async fn handle(hub: &Arc<Hub>, session: &Arc<Session>, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let Some(method) = msg.get("method").and_then(Value::as_str) else {
        // A response to something we never send, or garbage.
        return id.map(|id| error(id, -32600, "Invalid Request"));
    };
    let id = match id {
        Some(id) if !id.is_null() => id,
        _ => return None, // notification: initialized, cancelled, ...
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => {
            if let Some(name) = params.pointer("/clientInfo/name").and_then(Value::as_str) {
                *session.client.lock().unwrap() = Some(name.to_string());
            }
            json!({
                "protocolVersion": negotiate(params.get("protocolVersion").and_then(Value::as_str)),
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "ue-mcp-rust", "version": env!("CARGO_PKG_VERSION") },
                "instructions": INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": hub.list_tools(session).await }),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return Some(error(id, -32602, "tools/call needs a tool name"));
            };
            let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
            hub.call_tool(session, name, arguments).await.to_mcp()
        }
        "resources/list" => json!({ "resources": [] }),
        "resources/templates/list" => json!({ "resourceTemplates": [] }),
        "prompts/list" => json!({ "prompts": [] }),
        _ => return Some(error(id, -32601, &format!("Method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

pub fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
