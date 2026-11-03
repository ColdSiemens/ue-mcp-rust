//! Streamable HTTP transport (MCP 2025-03-26+), JSON responses only.
//!
//! Every agent, from any number of Claude Code processes, points at one URL.
//! Query parameters shape a session at `initialize`:
//!   ?agent=name      lock owner and label (sessions with one name share locks)
//!   ?tools=a,b,c     only these categories (plus project)
//!   ?compact=1       action names only in descriptions; details via describe_action
//!   ?epic=0          hide the epic_* wrappers of Unreal's toolsets

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use tracing::info;

use crate::dispatch::{Hub, Session};
use crate::mcp;
use crate::surface::View;

const SESSION_HEADER: &str = "mcp-session-id";
const SESSION_IDLE_LIMIT: Duration = Duration::from_secs(12 * 3600);
/// Above the plugin's own cap, so an oversize call gets the hub's explanation
/// rather than a bare 413.
const MAX_REQUEST_BYTES: usize = 4 * crate::bridge::BRIDGE_MAX_INBOUND_BYTES;

#[derive(Clone)]
struct AppState {
    hub: Arc<Hub>,
    base_view: View,
}

pub async fn serve(hub: Arc<Hub>, addr: SocketAddr, base_view: View) -> anyhow::Result<()> {
    let state = AppState { hub: hub.clone(), base_view };
    let app = Router::new()
        .route("/mcp", post(post_mcp).get(get_mcp).delete(delete_mcp))
        .route("/health", get(health))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state);

    tokio::spawn(reap_idle_sessions(hub));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("MCP endpoint: http://{addr}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn reap_idle_sessions(hub: Arc<Hub>) {
    loop {
        tokio::time::sleep(Duration::from_secs(600)).await;
        let now = Instant::now();
        hub.sessions.lock().unwrap().retain(|_, s| now.duration_since(*s.last_seen.lock().unwrap()) < SESSION_IDLE_LIMIT);
    }
}

/// Refuse browser pages on other origins (DNS rebinding), as the spec requires.
fn origin_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else { return true };
    let host = origin.split("://").nth(1).unwrap_or(origin);
    let host = host.split('/').next().unwrap_or(host);
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    matches!(host, "localhost" | "127.0.0.1" | "[::1]")
}

fn view_from(base: &View, q: &HashMap<String, String>) -> View {
    let mut view = base.clone();
    if let Some(tools) = q.get("tools").filter(|t| !t.trim().is_empty()) {
        view.tools = Some(tools.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<HashSet<_>>());
    }
    if let Some(c) = q.get("compact") {
        view.compact = matches!(c.as_str(), "1" | "true" | "yes");
    }
    if let Some(e) = q.get("epic") {
        view.hide_epic = matches!(e.as_str(), "0" | "false" | "no");
    }
    view
}

async fn post_mcp(State(st): State<AppState>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap, body: Bytes) -> Response {
    if !origin_allowed(&headers) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(mcp::error(Value::Null, -32700, "Parse error"))).into_response(),
    };
    let is_initialize = |m: &Value| m.get("method").and_then(Value::as_str) == Some("initialize");
    let initializing = match &msg {
        Value::Array(a) => a.iter().any(is_initialize),
        m => is_initialize(m),
    };

    let header_id = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()).map(str::to_string);
    let (session, new_id) = if initializing {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let owner = q.get("agent").filter(|a| !a.trim().is_empty()).cloned().unwrap_or_else(|| format!("agent-{}", &id[..8]));
        let session = Arc::new(Session::new(id.clone(), owner.clone(), view_from(&st.base_view, &q)));
        st.hub.sessions.lock().unwrap().insert(id.clone(), session.clone());
        info!(session = %id, agent = %owner, "session opened");
        (session, Some(id))
    } else {
        let Some(id) = header_id else {
            return (StatusCode::BAD_REQUEST, "missing Mcp-Session-Id; send initialize first").into_response();
        };
        let found = st.hub.sessions.lock().unwrap().get(&id).cloned();
        match found {
            Some(s) => (s, None),
            // 404 tells the client to start a new session.
            None => return (StatusCode::NOT_FOUND, "unknown or expired session").into_response(),
        }
    };

    let replies: Vec<Value> = match msg {
        Value::Array(batch) => {
            let futures = batch.into_iter().map(|m| mcp::handle(&st.hub, &session, m));
            futures_util::future::join_all(futures).await.into_iter().flatten().collect()
        }
        single => mcp::handle(&st.hub, &session, single).await.into_iter().collect(),
    };

    let mut response = if replies.is_empty() {
        StatusCode::ACCEPTED.into_response()
    } else if replies.len() == 1 && !body.starts_with(b"[") {
        Json(replies.into_iter().next().unwrap()).into_response()
    } else {
        Json(Value::Array(replies)).into_response()
    };
    if let Some(id) = new_id {
        response.headers_mut().insert(SESSION_HEADER, HeaderValue::from_str(&id).unwrap());
    }
    response
}

/// No server-initiated stream: this server never sends unsolicited messages.
async fn get_mcp() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, "this server does not offer an SSE stream").into_response()
}

async fn delete_mcp(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(id) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let removed = st.hub.sessions.lock().unwrap().remove(id);
    let Some(session) = removed else { return StatusCode::NOT_FOUND.into_response() };
    let owner_still_live = st.hub.sessions.lock().unwrap().values().any(|s| s.owner == session.owner);
    if !owner_still_live {
        // The agent is gone; its locks would otherwise block others until they expire.
        let released = st.hub.locks.release_owner(&session.owner);
        if released > 0 && st.hub.bridge.is_connected() {
            let _ = st.hub.bridge_call("release_session_locks", serde_json::json!({ "sessionId": session.owner }), None).await;
        }
    }
    info!(session = %id, agent = %session.owner, "session closed");
    StatusCode::OK.into_response()
}

async fn health(State(st): State<AppState>) -> Response {
    Json(st.hub.status().await).into_response()
}
