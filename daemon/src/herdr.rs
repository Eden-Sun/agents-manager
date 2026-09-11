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

/// One tab of a workspace. `pane_count` is what tells a caller whether a tab still holds
/// anything — the one thing the tab tidy-up in `lifecycle` needs from herdr.
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
    /// The pane's title with the spinner glyph removed — what the agent currently calls
    /// itself. herdr 0.8.2 reports it on `agent.list` / `agent.get`.
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

/// One foreground process of a pane (`pane.process_info`).
///
/// `argv` is the only field the daemon reads today: for an **adopted** agent — a pane started
/// by someone else, so nothing in our own database says what it was launched with — the CLI's
/// own flags are the only evidence of its model and reasoning effort
/// (`["claude","--dangerously-skip-permissions","--model","opus"]`).
#[derive(Debug, Clone, Deserialize)]
pub struct ProcessInfo {
    #[serde(default)]
    pub argv: Vec<String>,
    #[serde(default)]
    pub argv0: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// The process id herdr reports for this foreground process. `pane.process_info` carries
    /// argv and cwd but never env, so the pid is the only handle on the *account* a pane's CLI
    /// is running under — see [`crate::pane_identity`].
    #[serde(default)]
    pub pid: Option<i64>,
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

/// Put an argument on one line, because herdr will not launch with one that isn't.
///
/// It types the command into a pane, so an embedded newline would read as Enter; it rejects
/// the whole call with `invalid_agent_argument: agent arguments cannot be encoded safely for
/// the target shell` (verified 2026-09-06 against herdr 0.8.2 — newlines are the *only* thing
/// it refuses; backticks, quotes, `#` and CJK all pass).
///
/// A multi-line value is easy to arrive at by accident: a persona typed into the settings
/// textarea, or a team's generated role brief. Folding to spaces loses the paragraph breaks
/// but keeps every word, which beats a bot that silently refuses to start.
fn fold_newlines(arg: &str) -> String {
    if !arg.contains('\n') && !arg.contains('\r') {
        return arg.to_string();
    }
    arg.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).collect::<Vec<_>>().join(" ")
}

/// Roughly how many bytes of command line `agent.start` can actually type into a shell.
/// Measured 2026-09-06: a launch whose line reached 1256 bytes stopped after 1019, mid-word,
/// and Enter never arrived — the agent simply never started, and sixty seconds later the only
/// symptom was `agent_not_running`. Keep a margin under the observed cliff.
const MAX_COMMAND_BYTES: usize = 900;

/// What a trimmed argument ends with, so the loss is visible to whoever reads the persona.
/// Its byte length is part of the trim arithmetic — see `fit_command_line`.
const TRIM_MARK: &str = "…（後略）";

/// Trim the longest argument until the whole command fits.
///
/// Silent truncation is the worst outcome here: the shell is left holding half a line, the CLI
/// never runs, and nothing says why. A shortened `--append-system-prompt` still launches, and
/// the detail it loses is by convention also on disk (a team member's `TEAM.md`). The marker
/// makes the loss visible to whoever reads the persona.
fn fit_command_line(mut args: Vec<String>) -> Vec<String> {
    let total = |a: &[String]| a.iter().map(|s| s.len() + 3).sum::<usize>();
    while total(&args) > MAX_COMMAND_BYTES {
        let Some((i, _)) = args.iter().enumerate().max_by_key(|(_, s)| s.len()) else { break };
        let over = total(&args) - MAX_COMMAND_BYTES;
        // The marker is glued back on after the cut, so **its own bytes have to come out of
        // the budget too**. Charging a guessed 12 instead of its real 15 made a small
        // overshoot cut exactly as many bytes as the marker added back: 719 → 704 → 719 → …
        // for as long as the process lived, one WARN line per pass. (Observed 2026-09-06:
        // a team start filled the daemon log with 11 GB in six minutes.)
        let keep = args[i].len().saturating_sub(over + TRIM_MARK.len());
        if keep < 40 {
            break; // Nothing left worth trimming; let herdr answer for it rather than send junk.
        }
        // Char boundary, not byte: a cut through a multi-byte character panics.
        let cut = (0..=keep).rev().find(|n| args[i].is_char_boundary(*n)).unwrap_or(0);
        let trimmed = format!("{}{TRIM_MARK}", &args[i][..cut]);
        // Belt and braces: whatever the arithmetic above, a pass that does not actually
        // shorten the argument must stop the loop rather than spin on it.
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
        Self { socket: socket.into(), seq: Default::default() }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket
    }

    pub fn session_socket(session: &str) -> PathBuf {
        let base = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        if session == "default" {
            // Herdr's user-facing default session predates named sessions and keeps its
            // socket directly under ~/.config/herdr (not ~/.config/herdr/sessions/default).
            base.join(".config/herdr/herdr.sock")
        } else {
            base.join(".config/herdr/sessions").join(session).join("herdr.sock")
        }
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

    /// Kept for reference, deliberately uncalled: starting a bot used to go through here and
    /// now goes through [`Self::tab_create`], because panes of one tab divide a fixed width
    /// between them and tabs do not. Anything that reaches for this again should read the
    /// comment on `tab_create` first.
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

    /// Type literal text into a pane (no Enter). Used by the grok quota probe (SPEC §12.4).
    /// Pane rectangles for a workspace's active tab, so a caller can pick *which* pane to
    /// split instead of always taking the first one.
    pub async fn pane_rects(&self, workspace_id: &str) -> Result<Vec<(String, u32, u32)>> {
        self.rects(json!({"workspace_id": workspace_id})).await
    }

    /// What is running in a pane right now, front process first.
    ///
    /// herdr wraps the payload as `{"process_info":{"foreground_processes":[…]}}`. A pane with
    /// nothing in the foreground answers with an empty list rather than an error, so callers
    /// treat "no argv" and "no answer" the same way.
    pub async fn pane_process_info(&self, pane_id: &str) -> Result<Vec<ProcessInfo>> {
        let v = self.call("pane.process_info", json!({"pane_id": pane_id})).await?;
        let info = v.get("process_info").unwrap_or(&v);
        let Some(list) = info.get("foreground_processes") else { return Ok(Vec::new()) };
        Ok(serde_json::from_value(list.clone())?)
    }

    /// One pane's own size. `pane.layout` keyed by `workspace_id` only ever describes that
    /// workspace's **active tab**, so a pane living in any other tab is simply absent from it —
    /// which, now that each bot gets its own tab, is the normal case. Ask by `pane_id` instead.
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

    pub async fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        self.call("pane.send_text", json!({"pane_id": pane_id, "text": text})).await?;
        Ok(())
    }

    /// Send named key presses to a pane, e.g. `["Enter"]`, `["Escape"]`.
    pub async fn pane_send_keys(&self, pane_id: &str, keys: &[&str]) -> Result<()> {
        self.call("pane.send_keys", json!({"pane_id": pane_id, "keys": keys})).await?;
        Ok(())
    }

    // ----- tab -----

    /// Give a bot a **tab** of its own rather than a slice of somebody else's pane.
    ///
    /// Panes in one tab share the workspace's width — seven of them in a 185-column
    /// workspace left the narrowest at 18 columns, and below roughly 31 an agent's TUI wraps
    /// to a few glyphs a row without writing the spaces, which is unreadable to the user and
    /// unrecoverable from the terminal fallback (`is_shredded`). Tabs of the same workspace
    /// do not share width: five bots in five tabs each get the full 185.
    ///
    /// Takes the same arguments as `pane_split` and returns the new tab's root pane, so the
    /// caller gets the `pane_id` (and the `tab_id`) straight back.
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

    /// `Ok(false)` means the tab was already gone. herdr reaps a tab when its last pane
    /// closes, so a caller tidying up behind `pane.close` legitimately finds nothing there;
    /// that is success, not an upstream failure.
    pub async fn tab_close(&self, tab_id: &str) -> Result<bool> {
        match self.call("tab.close", json!({"tab_id": tab_id})).await {
            Ok(_) => Ok(true),
            Err(e) if is_not_found(&e) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Move a live pane into a tab of its own. This is a *move*, not a restart: herdr keeps
    /// the `pane_id` (verified against 0.8.2), so the run's mapping, its pane subscription
    /// and any in-flight turn are untouched — only `tab_id` changes.
    ///
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

#[cfg(test)]
mod arg_tests {
    use super::{fit_command_line, fold_newlines, MAX_COMMAND_BYTES};

    /// Reported 2026-09-06: a team failed to start with `invalid_agent_argument` because the
    /// generated PM persona spans four lines. Verified against herdr 0.8.2 that a newline is
    /// the only character it refuses, so folding is enough — and nothing else may change.
    /// 2026-09-06: a PM persona carrying four absolute worktree paths made the launch command
    /// 1256 bytes; the shell took 1019 of them, stopped mid-word, and Enter never arrived. The
    /// team then failed on `agent_not_running` with nothing on screen to explain it.
    #[test]
    fn an_over_long_command_is_trimmed_rather_than_truncated_by_the_shell() {
        let long = "你是 issue #1 的 PM，暱稱 `pm`。".repeat(40); // ~1200 bytes of CJK
        let args = vec!["--dangerously-skip-permissions".to_string(), "--append-system-prompt".to_string(), long.clone()];
        let out = fit_command_line(args);

        let total: usize = out.iter().map(|s| s.len() + 3).sum();
        assert!(total <= MAX_COMMAND_BYTES, "the whole line fits, got {total} bytes");
        // The flags survive; only the oversized value gives ground.
        assert_eq!(out[0], "--dangerously-skip-permissions");
        assert_eq!(out[1], "--append-system-prompt");
        assert!(out[2].len() < long.len() && out[2].ends_with("…（後略）"), "the loss is visible");
        assert!(long.starts_with(out[2].trim_end_matches("…（後略）")), "what is kept is a real prefix");

        // A command that already fits is returned byte-for-byte.
        let short = vec!["--model".to_string(), "sonnet".to_string()];
        assert_eq!(fit_command_line(short.clone()), short);
    }

    /// Regression, 2026-09-06: a team start hung the daemon in this loop and wrote 11 GB of
    /// identical WARN lines. A line that overshoots by only a few bytes cut fewer bytes than
    /// the trim marker adds back, so the argument never got shorter. Any overshoot from 1 byte
    /// upwards must converge — this test hangs rather than fails if it ever regresses.
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
    fn only_newlines_are_folded() {
        assert_eq!(fold_newlines("你是 issue #1 的 PM。\n成員：`pm`、`dev-1`。\n合併由 daemon 處理。"),
                   "你是 issue #1 的 PM。 成員：`pm`、`dev-1`。 合併由 daemon 處理。");
        assert_eq!(fold_newlines("a\r\nb"), "a b");
        // Blank lines are separators, not content: they must not become double spaces.
        assert_eq!(fold_newlines("a\n\n\nb"), "a b");
        assert_eq!(fold_newlines("trailing\n"), "trailing");

        // Everything herdr accepts has to survive byte-for-byte.
        for s in ["--append-system-prompt", "with `backtick` and 'quote' and \"dq\"", "#1 中文與符號、《》", ""] {
            assert_eq!(fold_newlines(s), s, "{s}");
        }
    }
}
