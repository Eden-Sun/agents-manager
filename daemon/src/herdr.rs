//! herdr Unix socket client.
//!
//! Wire contract (verified against herdr 0.8.2 / protocol 20 and 0.9.1 / protocol 22 — 同一組 RPC 的請求與回應形狀逐項比對過，#242):
//!   0.9.3（同為 protocol 22）對同一組 RPC 只多了欄位（`AgentInfo.completion_seq`、`PaneInfo.restore_error`）；codex unknown 的折疊依來源提供的啟動證據處理（#732、#745）。
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

/// herdr 0.9.2 起（上游 #4507），codex 畫面沒有任何規則對上時回 `unknown`（`agent explain`：`codex_state_ambiguous`）；
/// 0.9.1 同一個畫面回 `idle`（`default_known_agent_idle_fallback`）。codex 的 manifest 沒有 idle 規則，所以閒著的 codex
/// 在 0.9.2+ 永遠是 `unknown`，daemon 各處「閒下來」的判斷（flush、備援、閒置回收、啟動等 ready）都等不到（issue #732）。
/// 在 client 這一層折回 0.9.1 的語意：`working`／`blocked` 仍由規則判定，只有「判不出來」算閒著；還在啟動的不折。
/// 0.9.1 上是 no-op（它從不對啟動完的 codex 回 `unknown`）。
pub fn fold_codex_unknown(agent: Option<&str>, status: AgentStatus, launch_pending: bool) -> AgentStatus {
    if status == AgentStatus::Unknown && agent == Some("codex") && !launch_pending {
        AgentStatus::Idle
    } else {
        status
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
    #[serde(default)]
    pub scroll: Option<PaneScroll>,
}

/// `pane.get` 的 `scroll`：`viewport_rows` 是畫面的列數（#403：正式 herdr 0.9.1 上跟 `visible` 讀到的列數一致）。
#[derive(Debug, Clone, Deserialize)]
pub struct PaneScroll {
    pub viewport_rows: Option<u32>,
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

impl AgentInfo {
    fn folded(mut self) -> Self {
        self.agent_status = fold_codex_unknown(self.agent.as_deref(), self.agent_status, self.launch_pending);
        self
    }
}

/// claude 2.1.285 起輸入框與 prompt 回音寫成 `❯` 接 U+00A0（2026-10-01 cf-ox-2）：所有 `❯ ` 的比對（回音、送達證據、
/// 回覆擷取、輸入框判斷）都對不上——送出後證不出已送（delivery 變 unknown）、空框被當成有字（倒回 composer_busy）。
/// 在讀進來的那一刻換成一般空格，下游一套解析照舊。終端畫面裡的 NBSP 沒有別的意思。
fn nbsp_to_space(mut r: PaneRead) -> PaneRead {
    if r.text.contains('\u{a0}') {
        r.text = r.text.replace('\u{a0}', " ");
    }
    r
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

/// 分段貼上已經有前段進了 pane，後面某一段才失敗（#647）。外層是這個型別時，**不能**再當成
/// 「一個字都沒進去」——就算 source 是 [`HerdrError`]。
#[derive(Debug, thiserror::Error)]
#[error("partial paste: {sent_bytes} bytes already reached the pane")]
pub struct PartialPaste {
    pub sent_bytes: usize,
    #[source]
    pub source: anyhow::Error,
}

/// herdr **確定沒有執行**這個請求：它自己回了錯誤（herdr 先驗參數、找 pane，失敗就不動 pane），或根本連不上。
/// 其餘的失敗——逾時、送出後連線斷了沒回、回應讀不懂——都是**不知道**它做了沒有（#120、#147）。
/// 分段貼上已經寫進前幾段（[`PartialPaste`]）也不是「確定沒做」。
pub fn never_applied(e: &anyhow::Error) -> bool {
    if e.downcast_ref::<PartialPaste>().is_some() {
        return false;
    }
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
                .with_context(|| format!("parse herdr response for {method}: {}", snippet(method, &buf)))?;
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
        let parsed = match self.call("ping", json!({})).await {
            Ok(v) => serde_json::from_value::<Pong>(v).map_err(anyhow::Error::from),
            Err(e) => Err(e),
        };
        // 讀不到就清掉：舊的版本不能冒充現況（#254）。
        let pong = match parsed {
            Ok(p) => p,
            Err(e) => {
                *self.last_pong.lock().unwrap_or_else(|e| e.into_inner()) = None;
                return Err(e);
            }
        };
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
            Ok(v) => Ok(Some(serde_json::from_value::<PaneInfo>(v.get("pane").cloned().unwrap_or(v))?)),
            Err(e) if is_not_found(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// 畫面有幾列；pane 不在或 herdr 沒回 `scroll` 時 `None`。
    pub async fn pane_viewport_rows(&self, pane_id: &str) -> Result<Option<u32>> {
        Ok(self.pane_get(pane_id).await?.and_then(|p| p.scroll?.viewport_rows))
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
        self.call_as("pane.read", json!({"pane_id": pane_id, "source": source, "lines": lines}), "read").await.map(nbsp_to_space)
    }

    /// Same read with SGR styling kept (`format: ansi`): the only way to tell a TUI's dim
    /// placeholder from the same words typed by a person.
    pub async fn pane_read_ansi(&self, pane_id: &str, source: &str, lines: u32) -> Result<PaneRead> {
        self.call_as("pane.read", json!({"pane_id": pane_id, "source": source, "lines": lines, "format": "ansi"}), "read").await.map(nbsp_to_space)
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
    /// 一次 `pane.send_text` 超過約 1024 B，herdr 0.9.1 會丟掉最前面的 1024 B（#382），所以拆成小段依序送。
    pub async fn pane_send_text(&self, pane_id: &str, text: &str) -> Result<()> {
        let mut sent = 0usize;
        for piece in split_paste(text, SEND_TEXT_CHUNK) {
            if let Err(e) = self.call("pane.send_text", json!({"pane_id": pane_id, "text": piece})).await {
                if sent > 0 {
                    return Err(PartialPaste { sent_bytes: sent, source: e }.into());
                }
                return Err(e);
            }
            sent += piece.len();
        }
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
        let agents: Vec<AgentInfo> = self.call_as("agent.list", json!({}), "agents").await?;
        Ok(agents.into_iter().map(AgentInfo::folded).collect())
    }

    /// herdr clears a name when its agent exits, so a late exit after a same-named restart can
    /// clear the *new* agent's name (2026-09-11).
    pub async fn agent_rename(&self, target: &str, name: &str) -> Result<AgentInfo> {
        Ok(self.call_as::<AgentInfo>("agent.rename", json!({"target": target, "name": name}), "agent").await?.folded())
    }

    pub async fn agent_get(&self, target: &str) -> Result<Option<AgentInfo>> {
        match self.call("agent.get", json!({"target": target})).await {
            Ok(v) => Ok(Some(serde_json::from_value::<AgentInfo>(v["agent"].clone())?.folded())),
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
        Ok(serde_json::from_value::<AgentInfo>(v["agent"].clone())?.folded())
    }

    /// 啟動後等 ready（[`Self::agent_wait`]）。codex 在 herdr 0.9.2+ 閒著是 `unknown`，server 端的 `agent.wait` 等 idle
    /// 會等到逾時（issue #732）：改成輪詢折過的 `agent.get`。其他 kind 照舊交給 server 等。
    pub async fn agent_wait_ready(&self, kind: &str, target: &str, until: &[AgentStatus], timeout_ms: u64) -> Result<AgentInfo> {
        if kind != "codex" {
            return self.agent_wait(target, until, timeout_ms).await;
        }
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        let mut last = None;
        loop {
            if let Some(info) = self.agent_get(target).await? {
                if !info.launch_pending && until.contains(&info.agent_status) {
                    return Ok(info);
                }
                last = Some(info.agent_status);
            }
            if tokio::time::Instant::now() + AGENT_READY_POLL >= deadline {
                bail!("agent {target} did not reach {until:?} within {timeout_ms} ms (last {last:?})");
            }
            tokio::time::sleep(AGENT_READY_POLL).await;
        }
    }

    pub async fn agent_prompt(&self, target: &str, text: &str) -> Result<AgentInfo> {
        Ok(self.call_as::<AgentInfo>("agent.prompt", json!({"target": target, "text": text}), "agent").await?.folded())
    }

    pub async fn agent_send_keys(&self, target: &str, keys: &[String]) -> Result<()> {
        self.call("agent.send_keys", json!({"target": target, "keys": keys})).await?;
        Ok(())
    }

    pub async fn agent_read(&self, target: &str, source: &str, lines: u32) -> Result<PaneRead> {
        self.call_as("agent.read", json!({"target": target, "source": source, "lines": lines}), "read").await.map(nbsp_to_space)
    }

    /// 訂閱的**握手**最多等這麼久（issue #491）。其他 RPC 都走 [`Self::call_timeout`]，只有這裡
    /// 以前整段沒有逾時：herdr 接了連線卻不回 ack 時，`read_line` 會永遠等下去，而兩個呼叫端
    /// （`events.rs` 的全域訂閱與每個 pane 的狀態監看）都是「失敗才退避重連」的迴圈——卡在這裡
    /// 等於連 `Err` 分支的 log 與 `connected=false` 都不會發生，也不會再重試。本機的全域訂閱是
    /// 開機 spawn 一次、沒有 watchdog，於是 herdr 卡過一次之後 pane 事件就永遠收不到了。
    const SUBSCRIBE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

    /// Channel closes when the connection drops; caller reconnects.
    pub async fn subscribe(&self, subscriptions: Vec<Value>) -> Result<mpsc::Receiver<Event>> {
        let req = json!({"id": self.next_id(), "method": "events.subscribe", "params": {"subscriptions": subscriptions}});
        // **只包握手**：連線、送出、讀 ack。後面讀事件的那條是長連線，本來就該一直等，不能包。
        let handshake = async {
            let mut stream = UnixStream::connect(&self.socket).await.context("connect herdr socket for subscribe")?;
            let mut line = serde_json::to_vec(&req)?;
            line.push(b'\n');
            stream.write_all(&line).await?;
            stream.flush().await?;
            let mut reader = BufReader::new(stream);
            let mut first = String::new();
            reader.read_line(&mut first).await?;
            let ack: RawResponse =
                serde_json::from_str(first.trim_end()).with_context(|| format!("parse subscribe ack: {}", snippet("events.subscribe", &first)))?;
            if let Some(e) = ack.error {
                return Err(HerdrError { code: e.code, message: e.message }.into());
            }
            Ok::<_, anyhow::Error>(reader)
        };
        let mut reader = tokio::time::timeout(Self::SUBSCRIBE_HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| anyhow!("herdr events.subscribe 握手逾時（{:?}）", Self::SUBSCRIBE_HANDSHAKE_TIMEOUT))??;
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

/// 解析失敗時能放進錯誤訊息的那一段回應（issue #491）。
///
/// 兩件事：**長度**砍到 [`SNIPPET_CHARS`] 字（整份回應原樣塞進 `anyhow` 的 context，會經 log 與
/// `LcError::Upstream` 流到 API 回應去），以及**畫面內容不進去**——`pane.read`／`agent.read` 的
/// 回應就是使用者 pane 上的字，herdr 回半截 JSON 不是把整個畫面寫進 daemon.log 的理由。
fn snippet(method: &str, body: &str) -> String {
    let body = body.trim();
    if matches!(method, "pane.read" | "agent.read") {
        return format!("<{} 位元組的畫面內容，不記錄>", body.len());
    }
    let mut out: String = body.chars().take(SNIPPET_CHARS).collect();
    if body.chars().count() > SNIPPET_CHARS {
        out.push('…');
    }
    out
}

/// codex 等 ready 時輪詢 `agent.get` 的間隔（[`HerdrClient::agent_wait_ready`]）。
const AGENT_READY_POLL: Duration = Duration::from_millis(500);

/// 錯誤訊息裡最多帶幾個字的回應。
const SNIPPET_CHARS: usize = 200;

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<HerdrError>()
        .map(|h| h.code.contains("not_found") || h.code == "unknown_target" || h.code == "invalid_target")
        .unwrap_or(false)
}

/// `pane.send_text` 一段最多幾位元組（#382）。實測（2026-09-21，herdr 0.9.1＋claude）：單段 ≤ 1020 B 完整、
/// ≥ 1024 B 前面 1024 B 不見；1000 B 一段連續送 4 次（3604 B），claude 收到的與原文逐字相同。
/// 一段 > 800 字時 claude 會把它摺成 `[Pasted text #N +M lines]`，輸入框才不會被長段貼上撐高。
const SEND_TEXT_CHUNK: usize = 1000;

/// 把貼上的字拆成每段 ≤ `max` 位元組：盡量在換行之後切（一行放不下才在字元邊界硬切，不切斷多位元組字元），
/// 依序接起來就是原文。空字串沒有片段。
fn split_paste(text: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let end_of_line = start_of(text, line) + line.len();
        // 這一行放進目前這段會超過：先把目前這段收掉。
        while end_of_line - start > max {
            let line_start = start_of(text, line);
            if line_start > start {
                out.push(&text[start..line_start]);
                start = line_start;
                continue;
            }
            let mut cut = start + max;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push(&text[start..cut]);
            start = cut;
        }
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// `line` 是 `text` 的子切片：它在 `text` 裡的起點。
fn start_of(text: &str, line: &str) -> usize {
    line.as_ptr() as usize - text.as_ptr() as usize
}

#[cfg(test)]
mod rpc_tests {
    use super::*;

    /// accept 了但一個字都不回的假 herdr：socket 半開時最像的那種。
    fn wedged_socket(tag: &str) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!("am-herdr-{tag}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let handle = tokio::spawn(async move {
            // 接了就把連線握在手上，永遠不寫 ack。
            let mut held = Vec::new();
            while let Ok((conn, _)) = listener.accept().await {
                held.push(conn);
            }
        });
        (sock, handle)
    }

    /// issue #491：herdr 接了連線卻不回 ack 時，`subscribe` 必須在握手逾時之後回 `Err`，
    /// 不能永遠卡住——卡住的話兩個呼叫端的退避重連迴圈都不會再跑，本機的全域訂閱又沒有 watchdog，
    /// pane 事件就永遠收不到了。
    #[tokio::test(start_paused = true)]
    async fn a_socket_that_never_acks_times_out_instead_of_hanging_forever() {
        let (sock, server) = wedged_socket("noack");
        let client = HerdrClient::new(&sock);
        let started = tokio::time::Instant::now();
        let err = client.subscribe(vec![json!({"type": "pane.exited"})]).await.expect_err("不能成功");
        let msg = format!("{err:#}");
        assert!(msg.contains("握手逾時"), "要說是握手逾時：{msg}");
        // `start_paused` 下時鐘由 tokio 推進：真的等到門檻才逾時，不是立刻失敗。
        assert!(started.elapsed() >= HerdrClient::SUBSCRIBE_HANDSHAKE_TIMEOUT, "{:?}", started.elapsed());
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 同一個形狀的 socket 上，一般 RPC 本來就有逾時（對照組：#491 修的只有 subscribe 那條）。
    #[tokio::test(start_paused = true)]
    async fn an_ordinary_rpc_on_the_same_socket_already_times_out() {
        let (sock, server) = wedged_socket("rpc");
        let client = HerdrClient::new(&sock);
        let err = client.call_timeout("ping", json!({}), Duration::from_secs(15)).await.expect_err("不能成功");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 照腳本回話的假 herdr：每條連線讀一行請求，`reply` 決定回什麼；`events.subscribe` 回 ack 之後把 `events` 一行行吐出去。
    fn scripted_socket(tag: &str, reply: impl Fn(&Value) -> Value + Send + Sync + 'static, events: Vec<Value>) -> (PathBuf, tokio::task::JoinHandle<()>) {
        let dir = std::env::temp_dir().join(format!("am-herdr-{tag}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let sock = dir.join("herdr.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let reply = std::sync::Arc::new(reply);
        let handle = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                let (reply, events) = (reply.clone(), events.clone());
                tokio::spawn(async move {
                    let (r, mut w) = conn.into_split();
                    let mut line = String::new();
                    BufReader::new(r).read_line(&mut line).await.unwrap();
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let mut out = vec![json!({"id": req["id"], "result": reply(&req)})];
                    if req["method"] == "events.subscribe" {
                        out.extend(events);
                    }
                    for v in out {
                        w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
                    }
                });
            }
        });
        (sock, handle)
    }

    fn agent(name: &str, kind: &str, status: &str, launch_pending: bool) -> Value {
        json!({"name": name, "agent": kind, "agent_status": status, "launch_pending": launch_pending,
               "workspace_id": "w1", "tab_id": "w1:t1", "pane_id": format!("w1:{name}"), "cwd": "/tmp"})
    }

    /// #732：herdr 0.9.2+ 對閒著的 codex 回 `unknown`，折回 0.9.1 的 `idle`；其餘一律原樣。
    #[test]
    fn only_a_launched_codex_that_herdr_cannot_classify_is_folded_to_idle() {
        use AgentStatus::*;
        assert_eq!(fold_codex_unknown(Some("codex"), Unknown, false), Idle);
        assert_eq!(fold_codex_unknown(Some("codex"), Unknown, true), Unknown, "還在啟動：不是閒著");
        for s in [Working, Blocked, Done, Idle] {
            assert_eq!(fold_codex_unknown(Some("codex"), s, false), s, "規則判得出來的照舊");
        }
        for kind in [Some("claude"), Some("grok"), None] {
            assert_eq!(fold_codex_unknown(kind, Unknown, false), Unknown, "{kind:?} 的 unknown 不動");
        }
    }

    /// `AgentInfo` has `launch_pending`; panes and events do not, so their `unknown` must stay unknown here.
    #[tokio::test]
    async fn codex_unknown_is_folded_only_when_the_source_has_launch_pending_evidence() {
        let events = vec![
            json!({"event": "pane.agent_status_changed", "data": {"agent": "codex", "agent_status": "unknown", "pane_id": "w1:p2"}}),
            json!({"event": "pane.agent_status_changed", "data": {"agent": "claude", "agent_status": "unknown", "pane_id": "w1:p1"}}),
            json!({"event": "pane.agent_status_changed", "data": {"agent": "codex", "agent_status": "working", "pane_id": "w1:p2"}}),
            json!({"event": "pane_exited", "data": {"agent": "codex", "agent_status": "unknown", "pane_id": "w1:p2"}}),
        ];
        let (sock, server) = scripted_socket(
            "fold",
            |req| match req["method"].as_str().unwrap() {
                "agent.list" => json!({"agents": [agent("cx", "codex", "unknown", false), agent("cc", "claude", "unknown", false), agent("new", "codex", "unknown", true)]}),
                "agent.get" => json!({"agent": agent("cx", "codex", "unknown", false)}),
                "pane.list" => json!({"panes": [{"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1", "cwd": "/tmp", "agent": "codex", "agent_status": "unknown"}]}),
                "pane.get" => json!({"pane": {"pane_id": "w1:p2", "workspace_id": "w1", "tab_id": "w1:t1", "cwd": "/tmp", "agent": "codex", "agent_status": "unknown"}}),
                _ => json!({"type": "subscription_started"}),
            },
            events,
        );
        let c = HerdrClient::new(&sock);
        let got: Vec<_> = c.agent_list().await.unwrap().into_iter().map(|a| (a.name.unwrap(), a.agent_status)).collect();
        assert_eq!(got, [("cx".into(), AgentStatus::Idle), ("cc".into(), AgentStatus::Unknown), ("new".into(), AgentStatus::Unknown)]);
        assert_eq!(c.agent_get("cx").await.unwrap().unwrap().agent_status, AgentStatus::Idle);
        assert_eq!(c.pane_list(None).await.unwrap()[0].agent_status, Some(AgentStatus::Unknown));
        assert_eq!(c.pane_get("w1:p2").await.unwrap().unwrap().agent_status, Some(AgentStatus::Unknown));

        let mut rx = c.subscribe(vec![json!({"type": "pane.agent_status_changed"})]).await.unwrap();
        let mut seen = Vec::new();
        for _ in 0..4 {
            let ev = rx.recv().await.unwrap();
            seen.push((ev.event, ev.data["agent"].as_str().unwrap().to_string(), ev.data["agent_status"].as_str().unwrap().to_string()));
        }
        let want = [
            ("pane.agent_status_changed", "codex", "unknown"),
            ("pane.agent_status_changed", "claude", "unknown"),
            ("pane.agent_status_changed", "codex", "working"),
            ("pane_exited", "codex", "unknown"),
        ];
        assert_eq!(seen.iter().map(|(e, a, s)| (e.as_str(), a.as_str(), s.as_str())).collect::<Vec<_>>(), want);
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 啟動等 ready：codex 走輪詢（server 端的 `agent.wait` 等 idle 在 0.9.2+ 永遠逾時），啟動中的 `unknown` 不算 ready；
    /// 其他 kind 照舊一次 `agent.wait`。
    #[tokio::test]
    async fn a_codex_start_is_ready_once_it_leaves_launch_pending() {
        let gets = std::sync::Arc::new(AtomicU64::new(0));
        let waits = std::sync::Arc::new(AtomicU64::new(0));
        let (g, w) = (gets.clone(), waits.clone());
        let (sock, server) = scripted_socket(
            "ready",
            move |req| match req["method"].as_str().unwrap() {
                "agent.get" => {
                    let pending = g.fetch_add(1, Ordering::SeqCst) < 2;
                    json!({"agent": agent("cx", "codex", "unknown", pending)})
                }
                "agent.wait" => {
                    w.fetch_add(1, Ordering::SeqCst);
                    json!({"agent": agent("cc", "claude", "idle", false)})
                }
                m => panic!("unexpected {m}"),
            },
            vec![],
        );
        let c = HerdrClient::new(&sock);
        let until = [AgentStatus::Idle, AgentStatus::Done, AgentStatus::Blocked];
        let info = c.agent_wait_ready("codex", "cx", &until, 10_000).await.unwrap();
        assert_eq!(info.agent_status, AgentStatus::Idle);
        assert_eq!(gets.load(Ordering::SeqCst), 3, "兩次還在啟動、第三次才算 ready");
        assert_eq!(waits.load(Ordering::SeqCst), 0, "codex 不走 server 端的 agent.wait");

        c.agent_wait_ready("claude", "cc", &until, 10_000).await.unwrap();
        assert_eq!(waits.load(Ordering::SeqCst), 1, "claude 照舊");
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 一直在啟動（或一直 working）的 codex：到期回 `Err`（呼叫端退回 `agent.get`、不關 pane），不能無限等。
    #[tokio::test]
    async fn a_codex_that_never_settles_times_out() {
        let (sock, server) = scripted_socket("never", |_| json!({"agent": agent("cx", "codex", "working", false)}), vec![]);
        let c = HerdrClient::new(&sock);
        let started = std::time::Instant::now();
        let err = c.agent_wait_ready("codex", "cx", &[AgentStatus::Idle], 1_200).await.expect_err("不能成功");
        assert!(format!("{err:#}").contains("Working"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        server.abort();
        std::fs::remove_dir_all(sock.parent().unwrap()).ok();
    }

    /// 解析失敗的錯誤訊息不能把整份回應（尤其是畫面）原樣帶出去。
    #[test]
    fn a_parse_error_snippet_is_short_and_never_carries_screen_content() {
        let screen = "使用者的畫面內容\n".repeat(200);
        let masked = snippet("pane.read", &screen);
        assert!(!masked.contains("使用者的畫面內容"), "pane.read 的內容不進錯誤訊息：{masked}");
        assert!(masked.contains("位元組的畫面內容，不記錄"), "{masked}");
        assert_eq!(snippet("agent.read", &screen), snippet("pane.read", &screen), "agent.read 同一條規則");

        let long = "x".repeat(500);
        let cut = snippet("pane.get", &long);
        assert_eq!(cut.chars().count(), SNIPPET_CHARS + 1, "砍到 {SNIPPET_CHARS} 字再加一個省略號");
        assert!(cut.ends_with('…'));
        // 短的原樣保留（診斷還是要看得到東西）。
        assert_eq!(snippet("pane.get", "{\"oops\":1}"), "{\"oops\":1}");
    }
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

#[cfg(test)]
mod split_paste_tests {
    use super::{split_paste, SEND_TEXT_CHUNK};

    /// #382：拆完依序接起來一定是原文，每段都不超過上限；空字串沒有片段。
    #[test]
    fn pieces_rebuild_the_text_and_respect_the_limit() {
        let mut cases = vec![String::new(), "短".into(), "a\n".into(), "\n\n\n".into(), "x".repeat(1300), "你".repeat(500)];
        cases.push((0..58).map(|i| format!("2026081{}0{:08}", i % 10, i * 7919)).collect::<Vec<_>>().join("\n") + "\n\n以上用換行號");
        cases.push(format!("{}\n{}\n{}", "a".repeat(700), "b".repeat(10), "c".repeat(511)));
        for text in cases {
            for max in [4, 7, 1000] {
                let pieces = split_paste(&text, max);
                assert_eq!(pieces.concat(), text, "max {max}");
                assert!(pieces.iter().all(|p| !p.is_empty() && p.len() <= max), "max {max}: {:?}", pieces.iter().map(|p| p.len()).collect::<Vec<_>>());
            }
        }
        assert!(split_paste("", SEND_TEXT_CHUNK).is_empty());
    }

    #[test]
    fn a_partial_paste_is_not_never_applied_even_when_herdr_refused_the_later_chunk() {
        let refused = anyhow::Error::from(super::HerdrError { code: "pane_not_found".into(), message: "gone".into() });
        assert!(super::never_applied(&refused));
        let partial = anyhow::Error::from(super::PartialPaste { sent_bytes: 1000, source: refused });
        assert!(!super::never_applied(&partial));
    }

    /// 在換行之後切：不把一行數字從中間剖開（放得進一段的行不被切）。
    #[test]
    fn a_line_that_fits_is_never_cut() {
        let text = (0..58).map(|i| format!("2026081{}0{:08}\n", i % 10, i * 7919)).collect::<String>();
        let pieces = split_paste(&text, 100);
        assert!(pieces.len() > 5);
        assert!(pieces.iter().all(|p| p.ends_with('\n') && p.len() <= 100), "{pieces:?}");
    }

    /// 一行比上限長才硬切，而且切在字元邊界（中文一個字三個位元組）。
    #[test]
    fn an_overlong_line_is_cut_on_a_char_boundary() {
        let text = "你好".repeat(10);
        let pieces = split_paste(&text, 7);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|p| p.len() <= 7 && std::str::from_utf8(p.as_bytes()).is_ok()));
    }
}

#[cfg(test)]
mod nbsp_tests {
    use super::*;

    /// 2026-10-01 cf-ox-2（claude 2.1.285）：回音是 `❯` 接 U+00A0。讀進來就換成空格，送達證據（回音列）才對得上。
    #[test]
    fn a_no_break_space_after_the_prompt_marker_reads_as_a_space() {
        let raw = PaneRead {
            pane_id: "p".into(),
            source: "visible".into(),
            format: "text".into(),
            text: format!("❯\u{a0}ui 審查你自己做\n\n⏺ 好\n\n{r}\n❯\u{a0}\n{r}\n  ⏵⏵ bypass permissions on\n", r = "─".repeat(40)),
            revision: 1,
            truncated: false,
        };
        let r = nbsp_to_space(raw);
        assert!(!r.text.contains('\u{a0}'));
        assert_eq!(crate::lifecycle::echo_row_hits("claude", &r.text, "ui 審查你自己做"), 1, "回音列認得出來");
    }
}
