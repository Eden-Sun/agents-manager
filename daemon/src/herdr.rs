//! herdr Unix socket client.
//!
//! Wire contract (verified against herdr 0.8.2 / protocol 20 and 0.9.1 / protocol 22 — 同一組 RPC 的請求與回應形狀逐項比對過，#242):
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
    #[allow(dead_code)]
    pub pane_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TabInfo {
    pub tab_id: String,
    #[serde(default)]
    pub pane_count: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaneInfo {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    pub agent: Option<String>,
    pub agent_status: Option<AgentStatus>,
    #[serde(default)]
    pub revision: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentInfo {
    pub name: Option<String>,
    pub agent: Option<String>,
    /// herdr's binding to the CLI's own session. Missing on an agent herdr has adopted but never
    /// bound; `agent.prompt` answered ok on such an agent without the text reaching the pane
    /// (2026-09-14 wits-c1-op-xh), so the daemon types into the pane instead.
    #[serde(default)]
    pub agent_session: Option<Value>,

    /// Pane title minus spinner glyph (herdr 0.8.2 `agent.list` / `agent.get`).
    #[serde(default)]
    pub terminal_title_stripped: Option<String>,
    pub agent_status: AgentStatus,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub foreground_cwd: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
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

/// `argv` is the only evidence of an **adopted** agent's model/effort (we never launched it).
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessInfo {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// No env in `pane.process_info`, so the pid is the only handle on the pane's *account*
    /// (see [`crate::pane_identity`]).
    #[serde(default)]
    pub pid: Option<i64>,
}

/// `pane.process_info` 的整份（§6.5e 的 GC 守門）：`shell_pid` 是 herdr 自己記的那顆 shell，
/// 不靠行程環境——macOS 的 `ps -E` 讀不到 `-zsh` 本身的環境，root 的子行程更讀不到。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PaneShell {
    #[serde(default)]
    pub shell_pid: Option<i64>,
    #[serde(default)]
    pub foreground_processes: Option<Vec<ProcessInfo>>,
}

#[derive(Debug, thiserror::Error)]
#[error("herdr error {code}: {message}")]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

/// 連不上 herdr 的 socket：請求一個 byte 都沒送出去。跟 [`HerdrError`] 一樣是「herdr 確定沒做」，
/// 見 [`never_applied`]。
#[derive(Debug, thiserror::Error)]
#[error("connect herdr socket {socket}")]
pub struct HerdrUnreachable {
    pub socket: String,
    #[source]
    pub source: std::io::Error,
}

/// herdr **確定沒有執行**這個請求：它自己回了錯誤（herdr 先驗參數、找 pane，失敗就不動 pane），或根本連不上。
/// 其餘的失敗——逾時、送出後連線斷了沒回、回應讀不懂——都是**不知道**它做了沒有（#120、#147）。
pub fn never_applied(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HerdrError>().is_some() || e.downcast_ref::<HerdrUnreachable>().is_some()
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
    /// 最近一次 ping 成功的回應（UI 顯示 herdr 版本用，[`crate::herdr_version`]）；clone 共用。
    last_pong: std::sync::Arc<std::sync::Mutex<Option<Pong>>>,
}

pub const EXPECTED_PROTOCOL: u32 = 20;

/// 實測過、RPC 形狀相容的 protocol：20（0.8.2，正式現況）與 22（0.9.1）。
/// 升級窗口內 0.8.2 server 還在跑、升完是 0.9.1，兩邊都不該報「未預期版本」。
pub const SUPPORTED_PROTOCOLS: &[u32] = &[20, 22];

pub fn protocol_supported(protocol: u32) -> bool {
    SUPPORTED_PROTOCOLS.contains(&protocol)
}

/// herdr rejects any argument with a newline (`invalid_agent_argument`; only newlines, verified
/// 2026-09-06 herdr 0.8.2). Multi-line personas are common, so fold rather than fail to start.
fn fold_newlines(arg: &str) -> String {
    if !arg.contains('\n') && !arg.contains('\r') {
        return arg.to_string();
    }
    arg.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join(" ")
}

/// Margin under the shell typing cliff: 2026-09-06 a 1256-byte line stopped at 1019 and the
/// agent never started (`agent_not_running`).
const MAX_COMMAND_BYTES: usize = 900;

/// Its byte length is part of the trim arithmetic — see `fit_command_line`.
const TRIM_MARK: &str = "…（後略）";

/// Trim the longest argument until it fits: a shortened persona still launches, whereas shell
/// truncation silently never runs the CLI.
fn fit_command_line(mut args: Vec<String>) -> Vec<String> {
    let total = |a: &[String]| a.iter().map(|s| s.len() + 3).sum::<usize>();
    while total(&args) > MAX_COMMAND_BYTES {
        let Some((i, _)) = args.iter().enumerate().max_by_key(|(_, s)| s.len()) else { break };
        let over = total(&args) - MAX_COMMAND_BYTES;
        // The marker's own bytes must come out of the budget, else a small overshoot loops
        // forever (2026-09-06: 11 GB of WARN in six minutes).
        let keep = args[i].len().saturating_sub(over + TRIM_MARK.len());
        if keep < 40 {
            break; // Nothing left worth trimming; let herdr answer for it rather than send junk.
        }
        // Char boundary, not byte: a cut through a multi-byte character panics.
        let cut = (0..=keep).rev().find(|n| args[i].is_char_boundary(*n)).unwrap_or(0);
        let trimmed = format!("{}{TRIM_MARK}", &args[i][..cut]);
        // Belt and braces: never spin on a pass that does not shorten.
        if trimmed.len() >= args[i].len() {
            break;
        }
        tracing::warn!(arg = i, from = args[i].len(), to = trimmed.len(), "trimming an over-long agent argument");
        args[i] = trimmed;
    }
    args
}

impl HerdrClient {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self { socket: socket.into(), seq: Default::default(), last_pong: Default::default() }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn session_socket(session: &str) -> PathBuf {
        let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        if session == "default" {
            // The default session predates named sessions: socket is not under sessions/.
            base.join(".config/herdr/herdr.sock")
        } else {
            base.join(".config/herdr/sessions").join(session).join("herdr.sock")
        }
    }

    fn next_id(&self) -> String {
        format!("am-{}", self.seq.fetch_add(1, Ordering::Relaxed))
    }

    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        self.call_timeout(method, params, Duration::from_secs(15)).await
    }

    pub async fn call_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id();
        let req = json!({"id": id, "method": method, "params": params});
        let fut = async {
            let mut stream = UnixStream::connect(&self.socket)
                .await
                .map_err(|source| HerdrUnreachable { socket: self.socket.display().to_string(), source })?;
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

    pub async fn ping(&self) -> Result<Pong> {
        let v = self.call("ping", json!({})).await?;
        let pong: Pong = serde_json::from_value(v)?;
        *self.last_pong.lock().unwrap_or_else(|e| e.into_inner()) = Some(pong.clone());
        Ok(pong)
    }

    pub fn last_pong(&self) -> Option<Pong> {
        self.last_pong.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// herdr wraps the payload as `{"type":"session_snapshot","snapshot":{...}}`; unwrap it.
    ///
    /// 沒有 `snapshot` 欄位就是**回應形狀跟我們以為的不一樣**（herdr 換版最常見的樣子），
    /// 以前是「把整個信封當酬載」回去——下游三個 `unwrap_or_default()` 於是拿到空清單，
    /// 對帳把整台主機的 workspace 映射清成 NULL（review 2026-09-16）。假空值比錯誤難查太多。
    pub async fn snapshot(&self) -> Result<Value> {
        let v = self.call("session.snapshot", json!({})).await?;
        match v.get("snapshot") {
            Some(s) => Ok(s.clone()),
            None => anyhow::bail!("session.snapshot 的回應沒有 `snapshot` 欄位（herdr 換版？）：不拿信封當酬載"),
        }
    }

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

    /// Deliberately uncalled: bots get tabs ([`Self::tab_create`]), since panes share width.
    #[allow(dead_code)]
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

    /// Same read with SGR styling kept (`format: ansi`): the only way to tell a TUI's dim
    /// placeholder from the same words typed by a person.
    pub async fn pane_read_ansi(&self, pane_id: &str, source: &str, lines: u32) -> Result<PaneRead> {
        self.call_as("pane.read", json!({"pane_id": pane_id, "source": source, "lines": lines, "format": "ansi"}), "read").await
    }

    /// 把使用者的視窗切到這顆 pane（§6.5e 的「聚焦」按鈕）。只動焦點，不改內容。
    pub async fn pane_focus(&self, pane_id: &str) -> Result<()> {
        self.call("pane.focus", json!({"pane_id": pane_id})).await?;
        Ok(())
    }

    /// herdr 上顯示的 pane 名字（§6.5e：固定的 scratch 叫 `[panes] scratch_name`）。純顯示。
    pub async fn pane_rename(&self, pane_id: &str, label: &str) -> Result<()> {
        self.call("pane.rename", json!({"pane_id": pane_id, "label": label})).await?;
        Ok(())
    }

    pub async fn pane_rects(&self, workspace_id: &str) -> Result<Vec<(String, u32, u32)>> {
        self.rects(json!({"workspace_id": workspace_id})).await
    }

    /// An empty foreground answers with an empty list, so "no argv" and "no answer" are the same.
    pub async fn pane_process_info(&self, pane_id: &str) -> Result<Vec<ProcessInfo>> {
        let v = self.call("pane.process_info", json!({"pane_id": pane_id})).await?;
        let info = v.get("process_info").unwrap_or(&v);
        let Some(list) = info.get("foreground_processes") else { return Ok(Vec::new()) };
        Ok(serde_json::from_value(list.clone())?)
    }

    /// Same call as [`Self::pane_process_info`], keeping `shell_pid`.
    pub async fn pane_shell(&self, pane_id: &str) -> Result<PaneShell> {
        let v = self.call("pane.process_info", json!({"pane_id": pane_id})).await?;
        Ok(serde_json::from_value(v.get("process_info").unwrap_or(&v).clone())?)
    }

    /// Ask by `pane_id`: `pane.layout` by workspace only covers the active tab, and each bot has
    /// its own tab.
    pub async fn pane_size(&self, pane_id: &str) -> Result<Option<(u32, u32)>> {
        Ok(self.rects(json!({"pane_id": pane_id})).await?.into_iter().find(|(id, _, _)| id == pane_id).map(|(_, w, h)| (w, h)))
    }

    async fn rects(&self, params: Value) -> Result<Vec<(String, u32, u32)>> {
        let v = self.call("pane.layout", params).await?;
        let panes = v.get("layout").and_then(|l| l.get("panes")).and_then(|p| p.as_array()).cloned().unwrap_or_default();
        Ok(panes
            .iter()
            .filter_map(|p| {
                let id = p.get("pane_id")?.as_str()?.to_string();
                let r = p.get("rect")?;
                Some((id, r.get("width")?.as_u64()? as u32, r.get("height")?.as_u64()? as u32))
            })
            .collect())
    }

    /// Literal text, no Enter. Used by the grok quota probe (SPEC §12.4).
    pub async fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.call("pane.send_text", json!({"pane_id": pane_id, "text": text})).await?;
        Ok(())
    }

    pub async fn pane_send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<()> {
        self.call("pane.send_keys", json!({"pane_id": pane_id, "keys": keys})).await?;
        Ok(())
    }

    /// A bot gets its own **tab**: panes in one tab share width, and under ~31 columns the TUI
    /// shreds beyond recovery (`is_shredded`). Returns the tab's root pane.
    pub async fn tab_create(&self, workspace_id: &str, cwd: &str, label: &str, env: Value) -> Result<PaneInfo> {
        self.call_as(
            "tab.create",
            json!({"workspace_id": workspace_id, "cwd": cwd, "label": label, "focus": false, "env": env}),
            "root_pane",
        )
        .await
    }

    pub async fn tab_list(&self, workspace_id: &str) -> Result<Vec<TabInfo>> {
        self.call_as("tab.list", json!({"workspace_id": workspace_id}), "tabs").await
    }

    /// `Ok(false)` = already gone (herdr reaps a tab when its last pane closes); not a failure.
    pub async fn tab_close(&self, tab_id: &str) -> Result<bool> {
        match self.call("tab.close", json!({"tab_id": tab_id})).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// A move keeps `pane_id` (verified herdr 0.8.2), so run mapping and in-flight turn survive.
    /// Returns `(new_tab_id, previous_tab_id)`.
    pub async fn pane_move_to_new_tab(&self, pane_id: &str, label: &str) -> Result<(String, String)> {
        let v = self
            .call(
                "pane.move",
                json!({"pane_id": pane_id, "destination": {"type": "new_tab", "label": label}, "focus": false}),
            )
            .await?;
        let r = v.get("move_result").cloned().unwrap_or(v);
        let new_tab = r
            .get("pane")
            .and_then(|p| p.get("tab_id"))
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("pane.move result had no pane.tab_id: {r}"))?
            .to_string();
        let previous = r.get("previous_tab_id").and_then(|s| s.as_str()).unwrap_or_default().to_string();
        Ok((new_tab, previous))
    }

    pub async fn agent_list(&self) -> Result<Vec<AgentInfo>> {
        self.call_as("agent.list", json!({}), "agents").await
    }

    /// herdr clears a name when its agent exits, so a late exit after a same-named restart can
    /// clear the *new* agent's name (2026-09-11).
    pub async fn agent_rename(&self, target: &str, name: &str) -> Result<AgentInfo> {
        self.call_as("agent.rename", json!({"target": target, "name": name}), "agent").await
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
        let args: Vec<String> = args.iter().map(|a| fold_newlines(a)).collect();
        let args = fit_command_line(args);
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

    /// Channel closes when the connection drops; caller reconnects.
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

#[cfg(test)]
mod arg_tests {
    use super::{fit_command_line, fold_newlines, MAX_COMMAND_BYTES};

    /// 2026-09-06: a 1256-byte launch line was cut at 1019 by the shell and never started.
    #[test]
    fn an_over_long_command_is_trimmed_rather_than_truncated_by_the_shell() {
        let long = "你是 issue #1 的 PM，暱稱 `pm`。".repeat(40); // ~1200 bytes of CJK
        let args = vec!["--dangerously-skip-permissions".to_string(), "--append-system-prompt".to_string(), long.clone()];
        let out = fit_command_line(args);

        let total: usize = out.iter().map(|s| s.len() + 3).sum();
        assert!(total <= MAX_COMMAND_BYTES, "the whole line fits, got {total} bytes");
        assert_eq!(out[0], "--dangerously-skip-permissions");
        assert_eq!(out[1], "--append-system-prompt");
        assert!(out[2].len() < long.len() && out[2].ends_with("…（後略）"), "the loss is visible");
        assert!(long.starts_with(out[2].trim_end_matches("…（後略）")), "what is kept is a real prefix");

        let short = vec!["--model".to_string(), "sonnet".to_string()];
        assert_eq!(fit_command_line(short.clone()), short);
    }

    /// Regression 2026-09-06 (11 GB of WARN): small overshoots must converge; hangs if broken.
    #[test]
    fn a_command_that_only_just_overshoots_still_converges() {
        for overshoot in [1usize, 2, 3, 4, 15, 16, 40] {
            let flag = "--append-system-prompt";
            // total() charges len + 3 per argument.
            let value_len = MAX_COMMAND_BYTES + overshoot - (flag.len() + 3) - 3;
            let args = vec![flag.to_string(), "x".repeat(value_len)];
            let out = fit_command_line(args);

            let total: usize = out.iter().map(|s| s.len() + 3).sum();
            assert!(total <= MAX_COMMAND_BYTES, "overshoot {overshoot}: still {total} bytes");
            assert_eq!(out[0], flag);
            assert!(out[1].len() < value_len, "overshoot {overshoot}: the value did shrink");
        }
    }

    #[test]
    fn protocols_verified_against_both_herdr_versions_are_supported() {
        assert!(super::protocol_supported(20), "0.8.2（正式現況）");
        assert!(super::protocol_supported(22), "0.9.1（#242 實測過）");
        assert!(!super::protocol_supported(21), "沒實測過的版本不能默默放行");
        assert!(!super::protocol_supported(23));
    }

    #[test]
    fn only_newlines_are_folded() {
        assert_eq!(fold_newlines("你是 issue #1 的 PM。\n成員：`pm`、`dev-1`。\n合併由 daemon 處理。"),
                   "你是 issue #1 的 PM。 成員：`pm`、`dev-1`。 合併由 daemon 處理。");
        assert_eq!(fold_newlines("a\r\nb"), "a b");
        assert_eq!(fold_newlines("a\n\n\nb"), "a b");
        assert_eq!(fold_newlines("trailing\n"), "trailing");

        // Everything herdr accepts has to survive byte-for-byte.
        for s in ["--append-system-prompt", "with `backtick` and 'quote' and \"dq\"", "#1 中文與符號、《》", ""] {
            assert_eq!(fold_newlines(s), s, "{s}");
        }
    }
}
