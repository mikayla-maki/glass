//! Unix socket server the Glass companion tools talk to.
//!
//! Bound at `$GLASS_SYSTEM_DATA/orchestrator.sock`. The orchestrator passes
//! the absolute path to each loom subprocess via the `GLASS_ORCHESTRATOR_SOCK`
//! env var; Loom's `EnvSecretsStore` surfaces it to tools as a declared
//! secret. Tools read it from `ctx.secrets["GLASS_ORCHESTRATOR_SOCK"]`, not
//! `process.env`.
//!
//! Newline-delimited JSON protocol — one request line in, one response line
//! out, connection closed.
//!
//!   request:   `{"id": "...", "kind": "send_dm" | "schedule", ...}`
//!   response:  `{"id": "...", "ok": true, "result"?: ...}`
//!              `{"id": "...", "ok": false, "error": "..."}`
//!
//! `send_dm` requests are coalesced through a 2-second quiet window before
//! posting to Discord; rapid bursts of calls merge into one message so a
//! chatty agent doesn't spam the DM or trip Discord's ~5 msg/5s rate limit.
//! `schedule` requests delegate to [`crate::cron::CronStore`], which owns
//! the cron file and shares its mutex with the poller.

use crate::bus::{ConversationId, MessageBus};
use crate::cron::CronStore;
use crate::dm_log::{self, DmLog};
use anyhow::{anyhow, Context, Result};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

const SEND_DM_COALESCE: Duration = Duration::from_millis(2000);
const SEND_DM_MAX_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Request {
    SendDm {
        content: String,
    },
    Schedule {
        what: String,
        #[serde(default)]
        when: Option<String>,
        #[serde(default)]
        cron: Option<String>,
    },
}

#[derive(Debug, Serialize)]
struct Response {
    id: String,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<serde_json::Value>,
}

impl Response {
    fn ok(id: String, result: Option<serde_json::Value>) -> Self {
        Self {
            id,
            ok: true,
            error: None,
            result,
        }
    }
    fn err(id: String, error: String) -> Self {
        Self {
            id,
            ok: false,
            error: Some(error),
            result: None,
        }
    }
}

#[derive(Clone)]
pub struct ServerHandles {
    dm_tx: mpsc::Sender<String>,
    cron_store: CronStore,
}

/// Spin up the socket server and the dm coalescer task. Returns immediately;
/// background tasks run for the lifetime of the process. The caller is
/// responsible for ensuring `socket_path`'s parent directory exists.
///
/// If a stale socket file exists at `socket_path` it is unlinked first
/// (typical after an unclean shutdown).
pub async fn spawn(
    socket_path: PathBuf,
    cron_store: CronStore,
    bus: Arc<dyn MessageBus>,
    owner_channel: ConversationId,
    dm_log: DmLog,
) -> Result<()> {
    if socket_path.exists() {
        std::fs::remove_file(&socket_path).with_context(|| {
            format!("failed to remove stale socket at {}", socket_path.display())
        })?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("failed to bind socket at {}", socket_path.display()))?;
    tracing::info!(socket = %socket_path.display(), "orchestrator socket listening");

    let (dm_tx, dm_rx) = mpsc::channel::<String>(64);
    tokio::spawn(dm_coalescer(
        dm_rx,
        bus,
        owner_channel,
        dm_log,
        SEND_DM_COALESCE,
        SEND_DM_MAX_WAIT,
    ));

    let handles = ServerHandles { dm_tx, cron_store };

    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    let h = handles.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_connection(stream, h).await {
                            tracing::warn!("orchestrator_socket: connection error: {e:#}");
                        }
                    });
                }
                Err(e) => {
                    tracing::error!("orchestrator_socket: accept failed: {e}");
                    break;
                }
            }
        }
    });

    Ok(())
}

async fn handle_connection(stream: UnixStream, h: ServerHandles) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    let line = lines
        .next_line()
        .await?
        .ok_or_else(|| anyhow!("client closed before sending a request"))?;

    // Parse the id first so we can echo it even when the body is malformed.
    let raw: serde_json::Value = serde_json::from_str(&line).context("request: invalid JSON")?;
    let id = raw
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("(missing id)")
        .to_string();

    let resp = match serde_json::from_value::<Request>(raw) {
        Ok(req) => dispatch(req, &h, id.clone()).await,
        Err(e) => Response::err(id, format!("request: {e}")),
    };

    let mut out = serde_json::to_string(&resp).context("response: serialize")?;
    out.push('\n');
    write_half
        .write_all(out.as_bytes())
        .await
        .context("response: write")?;
    write_half.shutdown().await.ok();
    Ok(())
}

async fn dispatch(req: Request, h: &ServerHandles, id: String) -> Response {
    match req {
        Request::SendDm { content } => {
            let content = content.trim().to_string();
            if content.is_empty() {
                return Response::err(id, "send_dm: empty content".into());
            }
            match h.dm_tx.send(content).await {
                Ok(()) => Response::ok(id, None),
                Err(_) => Response::err(id, "send_dm: dm coalescer is gone".into()),
            }
        }
        Request::Schedule { what, when, cron } => {
            let now = Local::now();
            match h
                .cron_store
                .append(&what, when.as_deref(), cron.as_deref(), now)
                .await
            {
                Ok(entry_id) => Response::ok(id, Some(serde_json::json!({ "id": entry_id }))),
                Err(e) => Response::err(id, format!("schedule: {e:#}")),
            }
        }
    }
}

async fn dm_coalescer(
    mut rx: mpsc::Receiver<String>,
    bus: Arc<dyn MessageBus>,
    channel: ConversationId,
    dm_log: DmLog,
    coalesce: Duration,
    max_wait: Duration,
) {
    loop {
        let first = match rx.recv().await {
            Some(m) => m,
            None => return,
        };
        let started = tokio::time::Instant::now();
        let mut buffer: Vec<String> = vec![first];

        loop {
            let elapsed = started.elapsed();
            if elapsed >= max_wait {
                break;
            }
            let remaining = max_wait - elapsed;
            let window = coalesce.min(remaining);
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(m) => buffer.push(m),
                    None => break,
                },
                _ = tokio::time::sleep(window) => break,
            }
        }

        flush_batch(buffer, &bus, channel, &dm_log).await;
    }
}

async fn flush_batch(
    batch: Vec<String>,
    bus: &Arc<dyn MessageBus>,
    channel: ConversationId,
    dm_log: &DmLog,
) {
    if batch.is_empty() {
        return;
    }
    let combined = batch.join("\n\n");
    if let Err(e) = dm_log.append(dm_log::Direction::Out, &combined).await {
        tracing::warn!("dm_log: failed to log coalesced send_dm: {e:#}");
    }
    if let Err(e) = bus.reply(channel, &combined).await {
        tracing::error!("send_dm: bus.reply failed: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::testing::StubBus;

    #[tokio::test]
    async fn dm_coalescer_merges_burst_into_one_reply() {
        let bus_concrete = Arc::new(StubBus::new());
        let bus: Arc<dyn MessageBus> = bus_concrete.clone();
        let dir = tempfile::TempDir::new().unwrap();
        let dm_log = DmLog::new(dir.path().join("dm-log.jsonl"));
        let channel = ConversationId(7);
        let (tx, rx) = mpsc::channel::<String>(8);

        let bus_for_task = bus.clone();
        let dm_log_for_task = dm_log.clone();
        let coalescer = tokio::spawn(async move {
            dm_coalescer(
                rx,
                bus_for_task,
                channel,
                dm_log_for_task,
                Duration::from_millis(50),
                Duration::from_millis(500),
            )
            .await;
        });

        for content in ["first", "second", "third"] {
            tx.send(content.into()).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(tx);
        coalescer.await.unwrap();

        let replies = bus_concrete.replies().await;
        assert_eq!(replies.len(), 1, "burst should coalesce to one reply");
        assert_eq!(replies[0].0, channel);
        assert_eq!(replies[0].1, "first\n\nsecond\n\nthird");
    }

    #[tokio::test]
    async fn dm_coalescer_separate_messages_after_quiet_window() {
        let bus_concrete = Arc::new(StubBus::new());
        let bus: Arc<dyn MessageBus> = bus_concrete.clone();
        let dir = tempfile::TempDir::new().unwrap();
        let dm_log = DmLog::new(dir.path().join("dm-log.jsonl"));
        let channel = ConversationId(7);
        let (tx, rx) = mpsc::channel::<String>(8);

        let bus_for_task = bus.clone();
        let dm_log_for_task = dm_log.clone();
        let coalescer = tokio::spawn(async move {
            dm_coalescer(
                rx,
                bus_for_task,
                channel,
                dm_log_for_task,
                Duration::from_millis(30),
                Duration::from_millis(500),
            )
            .await;
        });

        tx.send("first".into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        tx.send("second".into()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(tx);
        coalescer.await.unwrap();

        let replies = bus_concrete.replies().await;
        assert_eq!(replies.len(), 2);
        assert_eq!(replies[0].1, "first");
        assert_eq!(replies[1].1, "second");
    }
}
