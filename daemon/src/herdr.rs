//! herdr Unix socket client.
//!
//! Wire contract (verified against herdr 0.8.2 / protocol 20):
//! - one connection per RPC: send `{"id":"<string>","method","params"}\n`, read one line, server closes.
//! - `events.subscribe` keeps the connection open and streams `{"event":"<name>","data":{...}}\n`.
//! - request `id` must be a string.

use anyhow::{anyhow, bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
    Unknown,
}

impl AgentStatus {
    /// `done` is the same underlying idle state; UI treats it as idle.
    pub fn normalized(self) -> AgentStatus {
        match self {
            AgentStatus::Done => AgentStatus::Idle,
            s => s,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            AgentStatus::Idle => "idle",
            AgentStatus::Working => "working",
            AgentStatus::Blocked => "blocked",
            AgentStatus::Done => "done",
            AgentStatus::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceInfo {
    pub workspace_id: String,
    pub label: Option<String>,
    #[serde(default)]
    pub pane_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<String>,
    pub agent: Option<String>,
    pub agent_status: Option<AgentStatus>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: Option<String>,
    pub agent: Option<String>,
    pub agent_status: AgentStatus,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub interactive_ready: bool,
    #[serde(default)]
    pub launch_pending: bool,
    #[serde(default)]
    pub state_change_seq: u64,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PaneRead {
    pub pane_id: String,
    pub source: String,
    pub format: String,
    pub text: String,
    pub revision: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pong {
    pub version: String,
    pub protocol: u32,
}

#[derive(Debug, thiserror::Error)]
#[error("herdr error {code}: {message}")]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct RawError {
    code: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    #[allow(dead_code)]
    id: Option<String>,
    result: Option<Value>,
    error: Option<RawError>,
}

/// A raw event from an `events.subscribe` stream.
#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    pub event: String,
    #[serde(default)]
    pub data: Value,
}

#[derive(Clone)]
pub struct HerdrClient {
    socket: PathBuf,
    seq: std::sync::Arc<AtomicU64>,
}

pub const EXPECTED_PROTOCOL: u32 = 20;

impl HerdrClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into(), seq: Default::default() }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn session_socket(session: &str) -> PathBuf {
        let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        base.join(".config/herdr/sessions").join(session).join("herdr.sock")
    }

    fn next_id(&self) -> String {
        format!("am-{}", self.seq.fetch_add(1, Ordering::Relaxed))
    }

    /// One RPC over a fresh connection. Returns the `result` object.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(method, params, Duration::from_secs(15)).await
    }

    pub async fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id();
        let req = json!({"id": id, "method": method, "params": params});
        let fut = async {
            let mut stream = UnixStream::connect(&self.socket)
                .await
                .with_context(|| format!("connect herdr socket {}", self.socket.display()))?;
            let mut line = serde_json::to_vec(&req)?;
            line.push(b'\n');
            stream.write_all(&line).await?;
            stream.flush().await?;
            let mut reader = BufReader::new(stream);
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).await?;
            if n == 0 {
                bail!("herdr closed connection without a response ({method})");
            }
            let resp: RawResponse = serde_json::from_str(buf.trim_end())
                .with_context(|| format!("parse herdr response for {method}: {buf}"))?;
            if let Some(e) = resp.error {
                return Err(HerdrError { code: e.code, message: e.message }.into());
            }
            resp.result.ok_or_else(|| anyhow!("herdr response for {method} had neither result nor error"))
        };
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| anyhow!("herdr rpc {method} timed out after {timeout:?}"))?
    }

    async fn call_as<T: DeserializeOwned>(&self, method: &str, params: Value, field: &str) -> Result<T> {
        let v = self.call(method, params).await?;
        let inner = v.get(field).cloned().ok_or_else(|| anyhow!("missing `{field}` in {method} result: {v}"))?;
        Ok(serde_json::from_value(inner)?)
    }

    // ----- server -----

    pub async fn ping(&self) -> Result<Pong> {
        let v = self.call("ping", json!({})).await?;
        Ok(serde_json::from_value(v)?)
    }

    /// herdr wraps the payload as `{"type":"session_snapshot","snapshot":{...}}`; unwrap it.
    pub async fn snapshot(&self) -> Result<Value> {
        let v = self.call("session.snapshot", json!({})).await?;
        Ok(v.get("snapshot").cloned().unwrap_or(v))
    }

    // ----- workspace -----

    pub async fn workspace_list(&self) -> Result<Vec<WorkspaceInfo>> {
        self.call_as("workspace.list", json!({}), "workspaces").await
    }

    pub async fn workspace_get(&self, workspace_id: &str) -> Result<Option<WorkspaceInfo>> {
        match self.call("workspace.get", json!({"workspace_id": workspace_id})).await {
            Ok(v) => Ok(Some(serde_json::from_value(v.get("workspace").cloned().unwrap_or(v))?)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Returns (workspace, root_pane).
    pub async fn workspace_create(&self, cwd: &str, label: &str, env: Value) -> Result<(WorkspaceInfo, PaneInfo)> {
        let v = self
            .call("workspace.create", json!({"cwd": cwd, "label": label, "focus": false, "env": env}))
            .await?;
        let ws = serde_json::from_value(v["workspace"].clone())?;
        let pane = serde_json::from_value(v["root_pane"].clone())?;
        Ok((ws, pane))
    }

    pub async fn workspace_close(&self, workspace_id: &str) -> Result<()> {
        self.call("workspace.close", json!({"workspace_id": workspace_id})).await?;
        Ok(())
    }

    // ----- pane -----

    pub async fn pane_list(&self, workspace_id: Option<&str>) -> Result<Vec<PaneInfo>> {
        let params = match workspace_id {
            Some(w) => json!({"workspace_id": w}),
            None => json!({}),
        };
        self.call_as("pane.list", params, "panes").await
    }

    pub async fn pane_get(&self, pane_id: &str) -> Result<Option<PaneInfo>> {
        match self.call("pane.get", json!({"pane_id": pane_id})).await {
            Ok(v) => Ok(Some(serde_json::from_value(v.get("pane").cloned().unwrap_or(v))?)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn pane_split(&self, target_pane_id: &str, direction: &str, cwd: &str, env: Value) -> Result<PaneInfo> {
        self.call_as(
            "pane.split",
            json!({"target_pane_id": target_pane_id, "direction": direction, "cwd": cwd, "focus": false, "env": env}),
            "pane",
        )
        .await
    }

    pub async fn pane_close(&self, pane_id: &str) -> Result<()> {
        self.call("pane.close", json!({"pane_id": pane_id})).await?;
        Ok(())
    }

    pub async fn pane_read(&self, pane_id: &str, source: &str, lines: u32) -> Result<PaneRead> {
        self.call_as("pane.read", json!({"pane_id": pane_id, "source": source, "lines": lines}), "read").await
    }

    // ----- agent -----

    pub async fn agent_list(&self) -> Result<Vec<AgentInfo>> {
        self.call_as("agent.list", json!({}), "agents").await
    }

    pub async fn agent_get(&self, target: &str) -> Result<Option<AgentInfo>> {
        match self.call("agent.get", json!({"target": target})).await {
            Ok(v) => Ok(Some(serde_json::from_value(v["agent"].clone())?)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Asynchronous on the socket: returns with `launch_pending: true`; follow with `agent_wait`.
    pub async fn agent_start(&self, name: &str, kind: &str, pane_id: &str, args: &[String], timeout_ms: u64) -> Result<AgentInfo> {
        self.call_as(
            "agent.start",
            json!({"name": name, "kind": kind, "pane_id": pane_id, "args": args, "timeout_ms": timeout_ms}),
            "agent",
        )
        .await
    }

    pub async fn agent_wait(&self, target: &str, until: &[AgentStatus], timeout_ms: u64) -> Result<AgentInfo> {
        let v = self
            .call_timeout(
                "agent.wait",
                json!({"target": target, "until": until, "timeout_ms": timeout_ms}),
                Duration::from_millis(timeout_ms + 5_000),
            )
            .await?;
        Ok(serde_json::from_value(v["agent"].clone())?)
    }

    pub async fn agent_prompt(&self, target: &str, text: &str) -> Result<AgentInfo> {
        self.call_as("agent.prompt", json!({"target": target, "text": text}), "agent").await
    }

    pub async fn agent_send_keys(&self, target: &str, keys: &[String]) -> Result<()> {
        self.call("agent.send_keys", json!({"target": target, "keys": keys})).await?;
        Ok(())
    }

    pub async fn agent_read(&self, target: &str, source: &str, lines: u32) -> Result<PaneRead> {
        self.call_as("agent.read", json!({"target": target, "source": source, "lines": lines}), "read").await
    }

    // ----- events -----

    /// Open a long-lived subscription. Events are delivered on the returned channel until the
    /// connection drops (channel closes). Caller is responsible for reconnecting.
    pub async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<mpsc::Receiver<Event>> {
        let mut stream = UnixStream::connect(&self.socket).await.context("connect herdr socket for subscribe")?;
        let req = json!({"id": self.next_id(), "method": "events.subscribe", "params": {"subscriptions": subscriptions}});
        let mut line = serde_json::to_vec(&req)?;
        line.push(b'\n');
        stream.write_all(&line).await?;
        stream.flush().await?;
        let mut reader = BufReader::new(stream);
        let mut first = String::new();
        reader.read_line(&mut first).await?;
        let ack: RawResponse = serde_json::from_str(first.trim_end()).context("parse subscribe ack")?;
        if let Some(e) = ack.error {
            return Err(HerdrError { code: e.code, message: e.message }.into());
        }
        let (tx, rx) = mpsc::channel(256);
        tokio::spawn(async move {
            let mut buf = String::new();
            loop {
                buf.clear();
                match reader.read_line(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let t = buf.trim_end();
                        if t.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Event>(t) {
                            Ok(ev) => {
                                if tx.send(ev).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => tracing::warn!(line = t, error = %e, "unparseable herdr event"),
                        }
                    }
                }
            }
            tracing::info!("herdr event stream closed");
        });
        Ok(rx)
    }
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HerdrError>()
        .map(|h| h.code.contains("not_found") || h.code == "unknown_target" || h.code == "invalid_target")
        .unwrap_or(false)
}
