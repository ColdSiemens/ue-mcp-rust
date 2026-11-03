//! stdio transport: one session, every request handled concurrently.
//!
//! Claude Code runs one MCP process per server for the main agent and all of
//! its subagents, so the requests on this pipe come from many agents at once.
//! Handling them one at a time would put every subagent in one queue.

use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::warn;

use crate::dispatch::{Hub, Session};
use crate::mcp;

pub async fn serve(hub: Arc<Hub>, session: Arc<Session>) -> anyhow::Result<()> {
    hub.sessions.lock().unwrap().insert(session.id.clone(), session.clone());
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let writer = tokio::spawn(async move {
        let mut out = tokio::io::stdout();
        while let Some(line) = rx.recv().await {
            if out.write_all(line.as_bytes()).await.is_err() || out.write_all(b"\n").await.is_err() || out.flush().await.is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::with_capacity(1 << 20, tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                warn!("unparseable message on stdin: {e}");
                let _ = tx.send(mcp::error(Value::Null, -32700, "Parse error").to_string());
                continue;
            }
        };
        let (hub, session, tx) = (hub.clone(), session.clone(), tx.clone());
        tokio::spawn(async move {
            let replies: Vec<Value> = match msg {
                Value::Array(batch) => {
                    let futures = batch.into_iter().map(|m| mcp::handle(&hub, &session, m));
                    futures_util::future::join_all(futures).await.into_iter().flatten().collect()
                }
                single => mcp::handle(&hub, &session, single).await.into_iter().collect(),
            };
            for reply in replies {
                let _ = tx.send(reply.to_string());
            }
        });
    }
    // stdin closed: the client is gone.
    drop(tx);
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), writer).await;
    Ok(())
}
