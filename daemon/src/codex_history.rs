//! Codex 的結構化 turn evidence（issue #749）：用 app-server 的 `thread/items/list`／`thread/turns/list`
//! 讀 thread 歷史，當作 rollout JSONL／畫面 scraping **之前**的另一條證據。
//!
//! 鐵則：這裡只產生 **positive evidence**。歷史讀不到、cursor 過期、RPC error、基準 item 找不到
//! （上游有「history projection stalled」的回報，API 可能停止反映新 turn）都只回「沒有證據」，
//! 呼叫端照舊走 rollout／畫面那條路；**絕不**把「API 沒看到」當成「prompt 沒送到」，也不因此重送或收掉 turn。
//!
//! 綁定是嚴格的：`(host, CODEX_HOME, native thread id)` 三個一起，thread id 一律來自 `runs.native_session_id`，
//! 不從「最近一條 thread」猜。app-server 是短命的子行程（每次要用才開、用完即殺），不引入第二個長駐 Codex。
//! 增量讀取靠「基準 item id」：送出前記下最新那一筆，之後由新到舊翻頁讀到基準為止，不 hydrate 整份 transcript。
//!
//! **預設關**（#749 審查：裝著的 codex 0.159.3 的 app-server 不支援 `thread/items/list`，開著只是白起子行程）；要開在設定檔明寫
//! `[codex_history] enabled = true`。關著時所有呼叫端的行為與沒有這個模組時一致（測試 build 的 source 預設也是空的，
//! 要用就 `App.codex_history.set(...)` 換一個 stub）。

#[cfg(test)]
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::{json, Value};

use crate::config::LOCAL_HOST;
use crate::db;
use crate::state::App;

/// 一頁最多拿幾筆、最多翻幾頁（基準之後的新東西一定很少；翻不到基準就當證不出來）。
const PAGE_LIMIT: u32 = 50;
const MAX_PAGES: usize = 4;
/// 開 app-server + initialize + 一次請求的總時限。測試 build 放寬：整樹並行高負載時，起一個假 `codex` 子行程可能慢到數秒，
/// 沒有測試是靠這個逾時才過的。
const CALL_TIMEOUT: Duration = if cfg!(test) { Duration::from_secs(60) } else { Duration::from_secs(8) };

/// 嚴格綁定：哪台主機、哪個 `CODEX_HOME`、哪條 thread。三個一起才算同一份歷史。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Binding {
    pub host: String,
    pub codex_home: Option<PathBuf>,
    pub thread_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemKind {
    /// 使用者訊息；每個 text 輸入各一段。
    User(Vec<String>),
    /// agent 訊息；`phase` 是 `final_answer`／`commentary`／沒有。
    Assistant { text: String, phase: Option<String> },
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub turn_id: String,
    pub item_id: String,
    pub kind: ItemKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnInfo {
    pub id: String,
    /// `completed`／`interrupted`／`failed`／`inProgress`。
    pub status: String,
}

#[derive(Debug, Default)]
pub struct Page<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

/// 讀不到的原因。呼叫端一律當成「沒有證據」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    /// 沒開、這台主機不支援、找不到執行檔。
    Unavailable(String),
    /// app-server 回了 JSON-RPC error 或沒有回（逾時／行程掛了）。
    Rpc(String),
    /// 回的東西看不懂。
    Malformed(String),
}

impl std::fmt::Display for HistoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HistoryError::Unavailable(s) => write!(f, "unavailable: {s}"),
            HistoryError::Rpc(s) => write!(f, "rpc: {s}"),
            HistoryError::Malformed(s) => write!(f, "malformed: {s}"),
        }
    }
}

/// 一條已開好的歷史連線（真的是一個 app-server 子行程）。丟掉＝關掉。
pub trait HistoryConn: Send {
    /// 由新到舊，一頁。`cursor` 是上一頁回的 `next_cursor`。
    fn items_desc<'a>(&'a mut self, cursor: Option<&'a str>, limit: u32) -> BoxFuture<'a, Result<Page<Item>, HistoryError>>;
    /// 由新到舊的 turn 狀態。
    fn turns_desc<'a>(&'a mut self, cursor: Option<&'a str>, limit: u32) -> BoxFuture<'a, Result<Page<TurnInfo>, HistoryError>>;
}

pub trait HistorySource: Send + Sync {
    /// 這台主機讀得到嗎（真的 source 只認本機）。
    fn supports(&self, host: &str) -> bool;
    fn open<'a>(&'a self, app: &'a Arc<App>, binding: &'a Binding) -> BoxFuture<'a, Result<Box<dyn HistoryConn>, HistoryError>>;
}

/// `App` 上可換的 source，同 `kind_probe::KindProbeHook`。測試 build 預設是關的。
pub struct HistoryHook(RwLock<Option<Arc<dyn HistorySource>>>);

impl HistoryHook {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn set(&self, source: Option<Arc<dyn HistorySource>>) {
        *self.0.write().unwrap() = source;
    }
    pub fn get(&self) -> Option<Arc<dyn HistorySource>> {
        self.0.read().unwrap().clone()
    }
}

impl Default for HistoryHook {
    fn default() -> Self {
        #[cfg(test)]
        {
            Self(RwLock::new(None))
        }
        #[cfg(not(test))]
        {
            Self(RwLock::new(Some(Arc::new(AppServerSource))))
        }
    }
}

// ───────────────────────── metrics ─────────────────────────

/// 一則 codex prompt 最後靠哪一種證據判定（SPEC §4.4a）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    Structured,
    Rollout,
    Screen,
}

static STRUCTURED_HIT: AtomicU64 = AtomicU64::new(0);
static ROLLOUT_FALLBACK: AtomicU64 = AtomicU64::new(0);
static SCREEN_FALLBACK: AtomicU64 = AtomicU64::new(0);
static STRUCTURED_MISS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub structured_hit: u64,
    pub rollout_fallback: u64,
    pub screen_fallback: u64,
    /// 結構化路徑試了但沒有證據（RPC error、基準找不到、還沒出現）。
    pub structured_miss: u64,
}

pub fn counts() -> Counts {
    Counts {
        structured_hit: STRUCTURED_HIT.load(Ordering::Relaxed),
        rollout_fallback: ROLLOUT_FALLBACK.load(Ordering::Relaxed),
        screen_fallback: SCREEN_FALLBACK.load(Ordering::Relaxed),
        structured_miss: STRUCTURED_MISS.load(Ordering::Relaxed),
    }
}

/// 記一次判定來源並寫 log（`what` = `delivery`／`reply`／`interrupt`）。
pub fn note(what: &'static str, source: Evidence) {
    let counter = match source {
        Evidence::Structured => &STRUCTURED_HIT,
        Evidence::Rollout => &ROLLOUT_FALLBACK,
        Evidence::Screen => &SCREEN_FALLBACK,
    };
    counter.fetch_add(1, Ordering::Relaxed);
    let c = counts();
    tracing::info!(
        what,
        source = ?source,
        structured_hit = c.structured_hit,
        rollout_fallback = c.rollout_fallback,
        screen_fallback = c.screen_fallback,
        structured_miss = c.structured_miss,
        "codex evidence source"
    );
}

fn note_miss(what: &'static str, why: &str) {
    STRUCTURED_MISS.fetch_add(1, Ordering::Relaxed);
    tracing::debug!(what, why, "codex structured history gave no evidence; falling back");
}

// ───────────────────────── 純解析 ─────────────────────────

fn parse_item_entry(v: &Value) -> Option<Item> {
    let turn_id = v.get("turnId")?.as_str()?.to_string();
    let item = v.get("item")?;
    let item_id = item.get("id")?.as_str()?.to_string();
    let kind = match item.get("type")?.as_str()? {
        "userMessage" => ItemKind::User(
            item.get("content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str).map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        "agentMessage" => ItemKind::Assistant {
            text: item.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
            phase: item.get("phase").and_then(Value::as_str).map(str::to_string),
        },
        _ => ItemKind::Other,
    };
    Some(Item { turn_id, item_id, kind })
}

/// `thread/items/list` 的 result。
pub fn parse_items_page(result: &Value) -> Result<Page<Item>, HistoryError> {
    let data = result.get("data").and_then(Value::as_array).ok_or_else(|| HistoryError::Malformed("no data array".into()))?;
    let data = data.iter().map(|e| parse_item_entry(e).ok_or_else(|| HistoryError::Malformed("bad item entry".into()))).collect::<Result<Vec<_>, _>>()?;
    Ok(Page { data, next_cursor: result.get("nextCursor").and_then(Value::as_str).map(str::to_string) })
}

/// `thread/turns/list` 的 result。
pub fn parse_turns_page(result: &Value) -> Result<Page<TurnInfo>, HistoryError> {
    let data = result.get("data").and_then(Value::as_array).ok_or_else(|| HistoryError::Malformed("no data array".into()))?;
    let turn = |t: &Value| Some(TurnInfo { id: t.get("id")?.as_str()?.to_string(), status: t.get("status")?.as_str()?.to_string() });
    let data = data.iter().map(|t| turn(t).ok_or_else(|| HistoryError::Malformed("bad turn entry".into()))).collect::<Result<Vec<_>, _>>()?;
    Ok(Page { data, next_cursor: result.get("nextCursor").and_then(Value::as_str).map(str::to_string) })
}

/// 這個 user item 是我們送的 `text` 嗎？逐字（跟 rollout 路徑同一個標準）：單一 text 輸入等於 `text`，
/// 或所有 text 輸入接起來等於 `text`。
pub fn is_user_text(item: &Item, text: &str) -> bool {
    match &item.kind {
        ItemKind::User(parts) => parts.iter().any(|p| p == text) || (parts.len() > 1 && parts.concat() == text),
        _ => false,
    }
}

// ───────────────────────── 增量讀取 ─────────────────────────

/// 送出前記下的基準：thread 當時最新那筆 item 的 id（`None`＝thread 還是空的）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    pub binding: Binding,
    pub after: Option<String>,
}

/// 最新那一筆。
async fn newest_item(conn: &mut dyn HistoryConn) -> Result<Option<String>, HistoryError> {
    Ok(conn.items_desc(None, 1).await?.data.into_iter().next().map(|i| i.item_id))
}

/// 基準之後的新 item，由舊到新。翻不到基準（cursor 過期、歷史被截、投影停住）＝`Err`，不是空。
async fn items_since(conn: &mut dyn HistoryConn, after: Option<&str>) -> Result<Vec<Item>, HistoryError> {
    let mut newer: Vec<Item> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = conn.items_desc(cursor.as_deref(), PAGE_LIMIT).await?;
        for item in page.data {
            if Some(item.item_id.as_str()) == after {
                newer.reverse();
                return Ok(newer);
            }
            newer.push(item);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => {
                // 整份歷史讀完了。基準是 `None`（空 thread）才合理；否則基準不見了。
                if after.is_none() {
                    newer.reverse();
                    return Ok(newer);
                }
                return Err(HistoryError::Malformed("baseline item not in history".into()));
            }
        }
    }
    Err(HistoryError::Malformed("baseline item not within the page budget".into()))
}

/// 這顆 bot 現在能不能用結構化歷史：開著、是 codex、有 session、source 支援這台主機。
/// 回 `(source, binding)`；任何一項不成立＝`None`（照舊走 rollout／畫面）。
pub async fn eligible(app: &Arc<impl crate::capabilities::Cfg + crate::capabilities::Db + crate::codex_history::CodexHistoryState + crate::tools::ToolsEnv + 'static>, bot: &db::Bot, run: &db::Run) -> Option<(Arc<dyn HistorySource>, Binding)> {
    if bot.kind != "codex" || !app.cfg().get().await.codex_history.enabled {
        return None;
    }
    let source = app.codex_history().get()?;
    let thread_id = run.native_session_id.as_deref().map(str::trim).filter(|s| !s.is_empty())?;
    if !thread_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let host = db::bot_host(app.db(), &bot.id).await.ok()?;
    if !source.supports(&host) {
        return None;
    }
    let codex_home = if host == LOCAL_HOST { crate::models::app_ports_p13::codex_home(app, bot).await } else { None };
    Some((source, Binding { host, codex_home, thread_id: thread_id.to_string() }))
}

/// 送出前取基準。任何失敗＝`None`（沒有結構化證據可用，不影響送達本身）。
///
/// thread 看起來是空的也是 `None`：空歷史跟「投影停住、什麼都沒回」分不出來，而停住的投影之後恢復會一次吐出整份舊歷史，
/// 裡面若有一模一樣的舊 prompt 就會被當成這一次的證據。沒有真的基準 item，就不給 positive evidence。
pub async fn mark(app: &Arc<App>, bot: &db::Bot, run: &db::Run) -> Option<Mark> {
    let (source, binding) = eligible(app, bot, run).await?;
    let mut conn = match source.open(app, &binding).await {
        Ok(c) => c,
        Err(e) => {
            note_miss("mark", &e.to_string());
            return None;
        }
    };
    match newest_item(conn.as_mut()).await {
        Ok(Some(after)) => Some(Mark { binding, after: Some(after) }),
        Ok(None) => {
            note_miss("mark", "empty history gives no baseline");
            None
        }
        Err(e) => {
            note_miss("mark", &e.to_string());
            None
        }
    }
}

/// 基準之後，歷史裡有沒有出現這則 prompt（positive proof）。連線在 `conn` 裡重用；任何錯誤＝`false`。
pub async fn prompt_landed(app: &Arc<App>, mark: &Mark, conn: &mut Option<Box<dyn HistoryConn>>, text: &str) -> bool {
    let Some(source) = app.codex_history.get() else { return false };
    if conn.is_none() {
        match source.open(app, &mark.binding).await {
            Ok(c) => *conn = Some(c),
            Err(e) => {
                note_miss("delivery", &e.to_string());
                return false;
            }
        }
    }
    let Some(c) = conn.as_mut() else { return false };
    match items_since(c.as_mut(), mark.after.as_deref()).await {
        Ok(items) if items.iter().any(|i| is_user_text(i, text)) => true,
        Ok(_) => {
            note_miss("delivery", "prompt not in history yet");
            false
        }
        Err(e) => {
            // 連線可能已經壞了，下次重開。
            *conn = None;
            note_miss("delivery", &e.to_string());
            false
        }
    }
}

/// thread 裡最後一次出現 `sent` 任一句之後，同一個 turn 的最終回覆（`phase = final_answer`）。
/// 中間又出現別的 user 訊息＝`None`。讀不到、找不到、還沒有最終回覆＝`None`。
pub fn reply_after(newest_first: &[Item], sent: &[String]) -> Option<String> {
    let start = newest_first.iter().position(|i| sent.iter().any(|s| is_user_text(i, s)))?;
    let ours = &newest_first[start];
    // `newest_first[..start]` 是我們那則之後的東西（新到舊）。
    for item in newest_first[..start].iter().rev() {
        match &item.kind {
            ItemKind::User(_) => return None,
            ItemKind::Assistant { text, phase } if item.turn_id == ours.turn_id && phase.as_deref() == Some("final_answer") && !text.trim().is_empty() => {
                return Some(text.clone());
            }
            _ => {}
        }
    }
    None
}

/// 從新到舊讀到 `want` 這個條件成立的 item 為止（含它與它之後的一切），上限 `MAX_PAGES` 頁。
async fn read_back_to(conn: &mut dyn HistoryConn, sent: &[String]) -> Result<Vec<Item>, HistoryError> {
    let mut all: Vec<Item> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = conn.items_desc(cursor.as_deref(), PAGE_LIMIT).await?;
        all.extend(page.data);
        if all.iter().any(|i| sent.iter().any(|s| is_user_text(i, s))) {
            return Ok(all);
        }
        match page.next_cursor {
            Some(c) => cursor = Some(c),
            None => return Ok(all),
        }
    }
    Ok(all)
}

/// 補回覆：我們送的那則（`sent`）在 thread 歷史裡的最終回覆。沒有證據＝`None`（呼叫端讀 rollout）。
pub async fn exact_reply(app: &Arc<App>, bot: &db::Bot, run: &db::Run, sent: &[String]) -> Option<String> {
    let (source, binding) = eligible(app, bot, run).await?;
    let result = async {
        let mut conn = source.open(app, &binding).await?;
        let items = read_back_to(conn.as_mut(), sent).await?;
        Ok::<_, HistoryError>(reply_after(&items, sent))
    }
    .await;
    match result {
        Ok(Some(reply)) => {
            note("reply", Evidence::Structured);
            Some(reply)
        }
        Ok(None) => {
            note_miss("reply", "no final answer after our prompt");
            None
        }
        Err(e) => {
            note_miss("reply", &e.to_string());
            None
        }
    }
}

/// 我們那則 prompt 的那個 turn 是不是被使用者中斷了（`turn.status = interrupted`）。
/// 只回 positive：`true`＝歷史明說中斷；其他（沒找到、仍在跑、讀不到）都是 `false`＝沒有證據。
pub async fn interrupted_after(app: &Arc<App>, bot: &db::Bot, run: &db::Run, sent: &[String]) -> bool {
    let Some((source, binding)) = eligible(app, bot, run).await else { return false };
    let result = async {
        let mut conn = source.open(app, &binding).await?;
        let items = read_back_to(conn.as_mut(), sent).await?;
        let Some(ours) = items.iter().find(|i| sent.iter().any(|s| is_user_text(i, s))) else { return Ok(false) };
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let page = conn.turns_desc(cursor.as_deref(), PAGE_LIMIT).await?;
            if let Some(t) = page.data.iter().find(|t| t.id == ours.turn_id) {
                return Ok(t.status == "interrupted");
            }
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok::<_, HistoryError>(false)
    }
    .await;
    match result {
        Ok(true) => {
            note("interrupt", Evidence::Structured);
            true
        }
        Ok(false) => {
            note_miss("interrupt", "turn not interrupted or not found");
            false
        }
        Err(e) => {
            note_miss("interrupt", &e.to_string());
            false
        }
    }
}

// ───────────────────────── 真的 app-server ─────────────────────────

/// 短命的 `codex app-server`（stdio JSON-RPC）。只在本機。
pub struct AppServerSource;

impl HistorySource for AppServerSource {
    fn supports(&self, host: &str) -> bool {
        host == LOCAL_HOST
    }

    fn open<'a>(&'a self, app: &'a Arc<App>, binding: &'a Binding) -> BoxFuture<'a, Result<Box<dyn HistoryConn>, HistoryError>> {
        Box::pin(async move {
            let program = crate::tools::cached_path(app, LOCAL_HOST, "codex").await.unwrap_or_else(|| "codex".to_string());
            let conn = tokio::time::timeout(CALL_TIMEOUT, AppServerConn::start(&program, binding))
                .await
                .map_err(|_| HistoryError::Rpc("app-server start timed out".into()))??;
            Ok(Box::new(conn) as Box<dyn HistoryConn>)
        })
    }
}

struct AppServerConn {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: tokio::io::BufReader<tokio::process::ChildStdout>,
    thread_id: String,
    next_id: u64,
}

impl AppServerConn {
    async fn start(program: &str, binding: &Binding) -> Result<Self, HistoryError> {
        let mut cmd = tokio::process::Command::new(program);
        cmd.arg("app-server")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        if let Some(home) = &binding.codex_home {
            cmd.env("CODEX_HOME", home);
        }
        let mut child = cmd.spawn().map_err(|e| HistoryError::Unavailable(format!("spawn {program}: {e}")))?;
        let stdin = child.stdin.take().ok_or_else(|| HistoryError::Unavailable("no stdin".into()))?;
        let stdout = tokio::io::BufReader::new(child.stdout.take().ok_or_else(|| HistoryError::Unavailable("no stdout".into()))?);
        let mut conn = Self { child, stdin, stdout, thread_id: binding.thread_id.clone(), next_id: 1 };
        conn.request("initialize", json!({"clientInfo": {"name": "agents-manager", "title": null, "version": env!("CARGO_PKG_VERSION")}})).await?;
        conn.notify("initialized").await?;
        Ok(conn)
    }

    async fn send(&mut self, v: Value) -> Result<(), HistoryError> {
        use tokio::io::AsyncWriteExt;
        let mut line = v.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| HistoryError::Rpc(format!("write: {e}")))?;
        self.stdin.flush().await.map_err(|e| HistoryError::Rpc(format!("flush: {e}")))
    }

    async fn notify(&mut self, method: &str) -> Result<(), HistoryError> {
        self.send(json!({"method": method})).await
    }

    /// 送一個請求，讀到同 id 的回應為止（中間的 notification／server request 一律略過）。
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, HistoryError> {
        use tokio::io::AsyncBufReadExt;
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"id": id, "method": method, "params": params})).await?;
        let read = async {
            let mut line = String::new();
            loop {
                line.clear();
                let n = self.stdout.read_line(&mut line).await.map_err(|e| HistoryError::Rpc(format!("read: {e}")))?;
                if n == 0 {
                    return Err(HistoryError::Rpc("app-server closed".into()));
                }
                let Ok(v) = serde_json::from_str::<Value>(line.trim()) else { continue };
                if v.get("method").is_some() || v.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(e) = v.get("error") {
                    return Err(HistoryError::Rpc(e.get("message").and_then(Value::as_str).unwrap_or("error").to_string()));
                }
                return v.get("result").cloned().ok_or_else(|| HistoryError::Malformed("response without result".into()));
            }
        };
        tokio::time::timeout(CALL_TIMEOUT, read).await.map_err(|_| HistoryError::Rpc(format!("{method} timed out")))?
    }
}

impl Drop for AppServerConn {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

impl HistoryConn for AppServerConn {
    fn items_desc<'a>(&'a mut self, cursor: Option<&'a str>, limit: u32) -> BoxFuture<'a, Result<Page<Item>, HistoryError>> {
        Box::pin(async move {
            let params = json!({"threadId": self.thread_id, "limit": limit, "sortDirection": "desc", "cursor": cursor});
            parse_items_page(&self.request("thread/items/list", params).await?)
        })
    }

    fn turns_desc<'a>(&'a mut self, cursor: Option<&'a str>, limit: u32) -> BoxFuture<'a, Result<Page<TurnInfo>, HistoryError>> {
        Box::pin(async move {
            let params = json!({"threadId": self.thread_id, "limit": limit, "sortDirection": "desc", "cursor": cursor});
            parse_turns_page(&self.request("thread/turns/list", params).await?)
        })
    }
}

// ───────────────────────── 測試用 stub ─────────────────────────

/// 決定性的假 source：每個 [`Binding`] 一份歷史（新到舊），可以隨時改、可以讓它全部失敗。
#[cfg(test)]
#[derive(Default)]
pub struct StubSource {
    inner: std::sync::Mutex<StubState>,
    pub opens: AtomicU64,
}

#[cfg(test)]
#[derive(Default)]
struct StubState {
    items: HashMap<Binding, Vec<Item>>,
    turns: HashMap<Binding, Vec<TurnInfo>>,
    fail: bool,
    remote_ok: bool,
    /// 開了連線但 `items_desc` 永遠回空（投影停住）。
    stalled: bool,
    /// 第 `n` 次 `open` 時才冒出來的 user item（模擬「送出之後才進歷史」）：`(n, binding, turn, id, text)`。
    arrivals: Vec<(u64, Binding, String, String, String)>,
}

#[cfg(test)]
impl StubSource {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: std::sync::Mutex::new(StubState { remote_ok: true, ..Default::default() }), opens: AtomicU64::new(0) })
    }
    pub fn local_only() -> Arc<Self> {
        let s = Self::new();
        s.inner.lock().unwrap().remote_ok = false;
        s
    }
    /// 加在最新端。
    pub fn push(&self, b: &Binding, turn: &str, id: &str, kind: ItemKind) {
        self.inner.lock().unwrap().items.entry(b.clone()).or_default().insert(0, Item { turn_id: turn.into(), item_id: id.into(), kind });
    }
    pub fn push_user(&self, b: &Binding, turn: &str, id: &str, text: &str) {
        self.push(b, turn, id, ItemKind::User(vec![text.to_string()]));
    }
    pub fn push_final(&self, b: &Binding, turn: &str, id: &str, text: &str) {
        self.push(b, turn, id, ItemKind::Assistant { text: text.into(), phase: Some("final_answer".into()) });
    }
    pub fn set_turn(&self, b: &Binding, id: &str, status: &str) {
        self.inner.lock().unwrap().turns.entry(b.clone()).or_default().insert(0, TurnInfo { id: id.into(), status: status.into() });
    }
    pub fn arrive_on_open(&self, n: u64, b: &Binding, turn: &str, id: &str, text: &str) {
        self.inner.lock().unwrap().arrivals.push((n, b.clone(), turn.into(), id.into(), text.into()));
    }
    pub fn fail(&self, on: bool) {
        self.inner.lock().unwrap().fail = on;
    }
    pub fn stall(&self, on: bool) {
        self.inner.lock().unwrap().stalled = on;
    }
}

#[cfg(test)]
struct StubConn {
    src: Arc<StubSource>,
    binding: Binding,
}

#[cfg(test)]
fn stub_page<T: Clone>(all: &[T], cursor: Option<&str>, limit: u32) -> Page<T> {
    let start: usize = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
    let end = (start + limit as usize).min(all.len());
    Page { data: all[start.min(all.len())..end].to_vec(), next_cursor: (end < all.len()).then(|| end.to_string()) }
}

#[cfg(test)]
impl HistoryConn for StubConn {
    fn items_desc<'a>(&'a mut self, cursor: Option<&'a str>, limit: u32) -> BoxFuture<'a, Result<Page<Item>, HistoryError>> {
        Box::pin(async move {
            let st = self.src.inner.lock().unwrap();
            if st.fail {
                return Err(HistoryError::Rpc("stub rpc error".into()));
            }
            if st.stalled {
                return Ok(Page { data: vec![], next_cursor: None });
            }
            let all = st.items.get(&self.binding).cloned().unwrap_or_default();
            Ok(stub_page(&all, cursor, limit))
        })
    }

    fn turns_desc<'a>(&'a mut self, cursor: Option<&'a str>, limit: u32) -> BoxFuture<'a, Result<Page<TurnInfo>, HistoryError>> {
        Box::pin(async move {
            let st = self.src.inner.lock().unwrap();
            if st.fail {
                return Err(HistoryError::Rpc("stub rpc error".into()));
            }
            let all = st.turns.get(&self.binding).cloned().unwrap_or_default();
            Ok(stub_page(&all, cursor, limit))
        })
    }
}

/// `Arc<StubSource>` 當 source 用（stub 自己要留一份讓測試改歷史）。
#[cfg(test)]
impl HistorySource for Arc<StubSource> {
    fn supports(&self, host: &str) -> bool {
        host == LOCAL_HOST || self.inner.lock().unwrap().remote_ok
    }
    fn open<'a>(&'a self, _app: &'a Arc<App>, binding: &'a Binding) -> BoxFuture<'a, Result<Box<dyn HistoryConn>, HistoryError>> {
        Box::pin(async move {
            let n = self.opens.fetch_add(1, Ordering::Relaxed) + 1;
            if self.inner.lock().unwrap().fail {
                return Err(HistoryError::Rpc("stub open failed".into()));
            }
            let due: Vec<_> = {
                let mut st = self.inner.lock().unwrap();
                let (due, rest) = std::mem::take(&mut st.arrivals).into_iter().partition(|a| a.0 <= n);
                st.arrivals = rest;
                due
            };
            for (_, b, turn, id, text) in due {
                self.push_user(&b, &turn, &id, &text);
            }
            Ok(Box::new(StubConn { src: self.clone(), binding: binding.clone() }) as Box<dyn HistoryConn>)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bind(host: &str, home: Option<&str>, thread: &str) -> Binding {
        Binding { host: host.into(), codex_home: home.map(PathBuf::from), thread_id: thread.into() }
    }

    fn conn(src: &Arc<StubSource>, b: &Binding) -> StubConn {
        StubConn { src: src.clone(), binding: b.clone() }
    }

    /// 真實 app-server 的 `thread/items/list` 回應形狀（2026-10-01 對 codex 0.159.3 實測）。
    #[test]
    fn the_real_items_response_shape_is_parsed() {
        let r = json!({"data": [
            {"turnId": "t2", "item": {"type": "agentMessage", "id": "m2", "text": "done", "phase": "final_answer", "memoryCitation": null}, "startedAtMs": 1, "completedAtMs": 2},
            {"turnId": "t2", "item": {"type": "commandExecution", "id": "c1", "command": "ls"}},
            {"turnId": "t2", "item": {"type": "userMessage", "id": "u2", "content": [{"type": "text", "text": "line1\nline2", "text_elements": []}, {"type": "localImage", "path": "/x.png"}]}}
        ], "nextCursor": "c", "backwardsCursor": "b"});
        let page = parse_items_page(&r).unwrap();
        assert_eq!(page.next_cursor.as_deref(), Some("c"));
        assert_eq!(page.data[0].kind, ItemKind::Assistant { text: "done".into(), phase: Some("final_answer".into()) });
        assert_eq!(page.data[1].kind, ItemKind::Other);
        assert_eq!(page.data[2].kind, ItemKind::User(vec!["line1\nline2".into()]));
        assert!(is_user_text(&page.data[2], "line1\nline2"));
        assert!(!is_user_text(&page.data[2], "line1"), "只認逐字");
        assert!(matches!(parse_items_page(&json!({"nope": 1})), Err(HistoryError::Malformed(_))));
        let turns = parse_turns_page(&json!({"data": [{"id": "t1", "status": "interrupted", "items": []}]})).unwrap();
        assert_eq!(turns.data, vec![TurnInfo { id: "t1".into(), status: "interrupted".into() }]);
    }

    /// 多段 text 輸入接起來等於我們送的那句也算；跟別人的不能混。
    #[test]
    fn a_split_user_message_matches_only_when_it_concatenates_to_the_prompt() {
        let item = Item { turn_id: "t".into(), item_id: "u".into(), kind: ItemKind::User(vec!["ab".into(), "cd".into()]) };
        assert!(is_user_text(&item, "abcd"));
        assert!(is_user_text(&item, "ab"));
        assert!(!is_user_text(&item, "abc"));
    }

    /// 增量讀：基準之後的新東西由舊到新；空 thread 的基準是 None。
    #[tokio::test]
    async fn items_since_reads_only_what_is_after_the_baseline() {
        let src = StubSource::new();
        let b = bind("local", None, "th1");
        src.push_user(&b, "t1", "u1", "old");
        src.push_final(&b, "t1", "a1", "old reply");
        let mut c = conn(&src, &b);
        let base = newest_item(&mut c).await.unwrap();
        assert_eq!(base.as_deref(), Some("a1"));
        assert!(items_since(&mut c, base.as_deref()).await.unwrap().is_empty());
        src.push_user(&b, "t2", "u2", "new");
        src.push_final(&b, "t2", "a2", "new reply");
        let got = items_since(&mut c, base.as_deref()).await.unwrap();
        assert_eq!(got.iter().map(|i| i.item_id.as_str()).collect::<Vec<_>>(), ["u2", "a2"]);

        let empty = bind("local", None, "th-empty");
        let mut e = conn(&src, &empty);
        assert_eq!(newest_item(&mut e).await.unwrap(), None);
        src.push_user(&empty, "t1", "u1", "first");
        assert_eq!(items_since(&mut e, None).await.unwrap().len(), 1);
    }

    /// 基準不在歷史裡（被截、投影停住）＝錯誤，不是「沒有新東西」。
    #[tokio::test]
    async fn a_missing_baseline_is_an_error_not_an_empty_answer() {
        let src = StubSource::new();
        let b = bind("local", None, "th1");
        src.push_user(&b, "t1", "u1", "x");
        let mut c = conn(&src, &b);
        assert!(items_since(&mut c, Some("gone")).await.is_err());
        // 翻頁預算：基準埋在很深的地方也算讀不到。
        for i in 0..(PAGE_LIMIT as usize * MAX_PAGES + 5) {
            src.push(&b, "t9", &format!("x{i}"), ItemKind::Other);
        }
        assert!(items_since(&mut c, Some("u1")).await.is_err());
    }

    #[test]
    fn the_reply_is_the_final_answer_of_our_own_turn() {
        let it = |turn: &str, id: &str, kind| Item { turn_id: turn.into(), item_id: id.into(), kind };
        let user = |t: &str| ItemKind::User(vec![t.into()]);
        let fin = |t: &str| ItemKind::Assistant { text: t.into(), phase: Some("final_answer".into()) };
        let com = |t: &str| ItemKind::Assistant { text: t.into(), phase: Some("commentary".into()) };
        let sent = vec!["跑測試".to_string()];
        // 新到舊
        let ok = vec![it("t2", "a", fin("測試全過")), it("t2", "c", com("先跑")), it("t2", "u", user("跑測試")), it("t1", "o", fin("舊的"))];
        assert_eq!(reply_after(&ok, &sent).as_deref(), Some("測試全過"));
        let running = vec![it("t2", "c", com("先跑")), it("t2", "u", user("跑測試"))];
        assert_eq!(reply_after(&running, &sent), None, "還沒有 final_answer");
        let nophase = vec![it("t2", "a", ItemKind::Assistant { text: "hi".into(), phase: None }), it("t2", "u", user("跑測試"))];
        assert_eq!(reply_after(&nophase, &sent), None, "沒有 phase 不敢說是最終回覆");
        let next_turn = vec![it("t3", "a3", fin("別人的")), it("t3", "u3", user("下一句")), it("t2", "u", user("跑測試"))];
        assert_eq!(reply_after(&next_turn, &sent), None, "中間又有別的 user 訊息");
        assert_eq!(reply_after(&ok, &["沒送過".to_string()]), None);
    }

    #[test]
    fn the_hook_defaults_off_in_tests_and_swaps() {
        let hook = HistoryHook::default();
        assert!(hook.get().is_none());
        hook.set(Some(Arc::new(StubSource::new())));
        assert!(hook.get().is_some());
        hook.set(None);
        assert!(hook.get().is_none());
    }

    /// 寫一支假腳本（`body` 接在 `#!/bin/sh` 之後）並確定它已經可以被 exec（issue #189）：見 [`crate::testing::write_exec`]。
    fn write_exec(path: &std::path::Path, body: &str) {
        crate::testing::write_exec(path, format!("#!/bin/sh\n{body}"))
    }

    /// 真的 app-server 的 JSON-RPC 往返：用一支假的 `codex`（shell script）驗證 initialize／initialized、
    /// 略過 notification、error 轉成 `HistoryError::Rpc`、`CODEX_HOME` 帶進子行程。
    #[tokio::test]
    async fn the_app_server_conn_speaks_jsonrpc_over_stdio() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("agm-codex-history-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("codex");
        // 讀一行回一行：initialize 前先噴一則 notification；items/list 回一筆並帶 CODEX_HOME 當 item id；turns/list 回 error。
        write_exec(
            &script,
            r#"while IFS= read -r line; do
  case "$line" in
    *'"method":"initialize"'*) echo '{"method":"configWarning","params":{}}'; echo '{"id":1,"result":{"codexHome":"x"}}' ;;
    *'"method":"initialized"'*) : ;;
    *thread/items/list*) echo '{"id":2,"result":{"data":[{"turnId":"t","item":{"type":"userMessage","id":"'"$CODEX_HOME"'","content":[{"type":"text","text":"hi"}]}}],"nextCursor":null}}' ;;
    *thread/turns/list*) echo '{"id":3,"error":{"code":-32601,"message":"nope"}}' ;;
  esac
done
"#,
        );
        let b = bind("local", Some("/tmp/agm-home-a"), "th1");
        let mut c = AppServerConn::start(script.to_str().unwrap(), &b).await.unwrap();
        let page = c.items_desc(None, 10).await.unwrap();
        assert_eq!(page.data[0].item_id, "/tmp/agm-home-a", "CODEX_HOME 綁定帶進子行程");
        assert_eq!(c.turns_desc(None, 10).await.unwrap_err(), HistoryError::Rpc("nope".into()));
        drop(c);
        // 不存在的執行檔＝Unavailable，不是 panic。
        assert!(matches!(AppServerConn::start("/nonexistent/codex", &b).await, Err(HistoryError::Unavailable(_))));
        std::fs::remove_dir_all(&dir).ok();
    }

    use crate::testing as tt;

    async fn codex_bot_and_run(env: &tt::Env, session: Option<&str>) -> (db::Bot, db::Run) {
        let app = &env.app;
        let bot_id = db::ulid();
        sqlx::query("INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at) VALUES (?,?,?,'codex','[]',0,1,'tok',?)")
            .bind(&bot_id)
            .bind(&env.project_id)
            .bind(format!("hist-bot-{bot_id}"))
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(app, &bot_id).await;
        if let Some(s) = session {
            sqlx::query("UPDATE runs SET native_session_id = ? WHERE id = ?").bind(s).bind(&run_id).execute(&app.db).await.unwrap();
        }
        (db::bot(&app.db, &bot_id).await.unwrap().unwrap(), db::run(&app.db, &run_id).await.unwrap().unwrap())
    }

    /// 綁定：thread id 來自 run 的 durable `native_session_id`（daemon 重啟後同一個值），`CODEX_HOME` 來自 bot 的 env；
    /// 沒有 session、沒開、不是 codex、source 不支援那台主機，都不給 binding（不從「最近一條 thread」猜）。
    #[tokio::test]
    async fn the_binding_is_strict_and_survives_a_daemon_restart() {
        let env = tt::env().await;
        let app = env.app.clone();
        let stub = StubSource::new();
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let (bot, run) = codex_bot_and_run(&env, Some("thread-1")).await;
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?").bind(json!({"CODEX_HOME": "/srv/codex-a"}).to_string()).bind(&bot.id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();

        let (_, b) = eligible(&app, &bot, &run).await.unwrap();
        assert_eq!(b, Binding { host: LOCAL_HOST.into(), codex_home: Some(PathBuf::from("/srv/codex-a")), thread_id: "thread-1".into() });

        // 「重啟」：記憶體裡的東西全丟掉，只剩 DB 裡的 thread id，照樣得到同一個 binding 與同一份歷史。
        stub.push_user(&b, "t1", "u1", "hello");
        let reloaded_run = db::run(&app.db, &run.id).await.unwrap().unwrap();
        let (_, again) = eligible(&app, &bot, &reloaded_run).await.unwrap();
        assert_eq!(again, b);
        assert!(mark(&app, &bot, &reloaded_run).await.is_some_and(|m| m.after.as_deref() == Some("u1")));

        // 沒有 session / 不安全的 id / 不是 codex / 關掉：沒有 binding。
        let (bot2, no_session) = codex_bot_and_run(&env, None).await;
        assert!(eligible(&app, &bot2, &no_session).await.is_none());
        let (bot3, odd) = codex_bot_and_run(&env, Some("../../etc")).await;
        assert!(eligible(&app, &bot3, &odd).await.is_none());
        let mut not_codex = bot.clone();
        not_codex.kind = "claude".into();
        assert!(eligible(&app, &not_codex, &run).await.is_none());
        app.cfg.update(|c| { c.codex_history.enabled = false; Ok(()) }).await.unwrap();
        assert!(eligible(&app, &bot, &run).await.is_none());
    }

    /// 遠端主機：binding 帶 host、沒有本機的 `CODEX_HOME`；只認本機的 source 不會被拿去讀遠端。
    #[tokio::test]
    async fn a_remote_host_has_its_own_binding_and_a_local_only_source_declines_it() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot, run) = codex_bot_and_run(&env, Some("thread-1")).await;
        sqlx::query("UPDATE projects SET host = 'm4p' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();

        app.codex_history.set(Some(Arc::new(StubSource::local_only())));
        assert!(eligible(&app, &bot, &run).await.is_none(), "只支援本機的 source 不碰遠端 bot");
        assert!(!AppServerSource.supports("m4p"));
        assert!(AppServerSource.supports(LOCAL_HOST));

        let stub = StubSource::new();
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let (_, b) = eligible(&app, &bot, &run).await.unwrap();
        assert_eq!((b.host.as_str(), b.codex_home.as_ref(), b.thread_id.as_str()), ("m4p", None, "thread-1"));
        // 同名 thread 在本機（另一個 CODEX_HOME）的歷史不會跑到遠端這份。
        let local = Binding { host: LOCAL_HOST.into(), codex_home: Some(PathBuf::from("/srv/codex-a")), thread_id: "thread-1".into() };
        stub.push_user(&local, "t1", "u-local", "same text");
        stub.push_user(&b, "t1", "u-remote", "other");
        assert_eq!(mark(&app, &bot, &run).await.unwrap().after.as_deref(), Some("u-remote"));
    }

    /// 回覆補撈：thread 歷史裡我們那則之後、同一個 turn 的 final_answer；歷史讀不到／還沒回完＝`None`，不編。
    #[tokio::test]
    async fn the_exact_reply_comes_from_our_own_turn_only() {
        let env = tt::env().await;
        let app = env.app.clone();
        let stub = StubSource::new();
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let (bot, run) = codex_bot_and_run(&env, Some("thread-1")).await;
        let (_, b) = eligible(&app, &bot, &run).await.unwrap();
        let sent = vec!["跑測試".to_string()];

        stub.push_user(&b, "t1", "u1", "跑測試");
        assert_eq!(exact_reply(&app, &bot, &run, &sent).await, None, "還沒回完");
        stub.push(&b, "t1", "c1", ItemKind::Assistant { text: "先跑".into(), phase: Some("commentary".into()) });
        assert_eq!(exact_reply(&app, &bot, &run, &sent).await, None, "commentary 不是最終回覆");
        stub.push_final(&b, "t1", "a1", "測試全過");
        assert_eq!(exact_reply(&app, &bot, &run, &sent).await.as_deref(), Some("測試全過"));

        // 下一回合別人的回覆不會被當成我們的。
        stub.push_user(&b, "t2", "u2", "別的問題");
        stub.push_final(&b, "t2", "a2", "別人的答案");
        assert_eq!(exact_reply(&app, &bot, &run, &sent).await.as_deref(), Some("測試全過"), "我們那一回合的答案不變");
        assert_eq!(exact_reply(&app, &bot, &run, &["別的問題".to_string()]).await.as_deref(), Some("別人的答案"));

        stub.fail(true);
        assert_eq!(exact_reply(&app, &bot, &run, &["別的問題".to_string()]).await, None, "RPC error 就是沒有證據");
    }

    /// 中斷證據：只有 turn.status 明說 interrupted 才是 true；其餘（跑完、還在跑、找不到、RPC error）都是「沒有證據」。
    #[tokio::test]
    async fn interruption_is_only_ever_positive_evidence() {
        let env = tt::env().await;
        let app = env.app.clone();
        let stub = StubSource::new();
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let (bot, run) = codex_bot_and_run(&env, Some("thread-1")).await;
        let (_, b) = eligible(&app, &bot, &run).await.unwrap();
        let sent = vec!["長任務".to_string()];

        assert!(!interrupted_after(&app, &bot, &run, &sent).await, "歷史裡還沒有這則");
        stub.push_user(&b, "t1", "u1", "長任務");
        stub.set_turn(&b, "t1", "inProgress");
        assert!(!interrupted_after(&app, &bot, &run, &sent).await);
        stub.set_turn(&b, "t0", "interrupted");
        assert!(!interrupted_after(&app, &bot, &run, &sent).await, "別的 turn 被中斷不算");
        stub.set_turn(&b, "t1", "interrupted");
        assert!(interrupted_after(&app, &bot, &run, &sent).await);
        stub.fail(true);
        assert!(!interrupted_after(&app, &bot, &run, &sent).await, "RPC error 不是證據");
    }

    /// 計數：structured-hit／rollout-fallback／screen-fallback 各記各的。
    #[test]
    fn the_evidence_sources_are_counted_separately() {
        let before = counts();
        note("delivery", Evidence::Structured);
        note("delivery", Evidence::Rollout);
        note("delivery", Evidence::Screen);
        let after = counts();
        assert!(after.structured_hit > before.structured_hit);
        assert!(after.rollout_fallback > before.rollout_fallback);
        assert!(after.screen_fallback > before.screen_fallback);
    }
}

/// codex 歷史讀取的接線點。（欄位在 `App`，由 composition 層 `app_ports_p0` 實作這個窄能力。）
pub trait CodexHistoryState: Send + Sync {
    fn codex_history(&self) -> &crate::codex_history::HistoryHook;
}
