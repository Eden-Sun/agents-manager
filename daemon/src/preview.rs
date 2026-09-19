//! 預覽模式（issue #253，SPEC §6.12）：頂層 bot 的專案起一顆 vite dev server，UI 把它內嵌在右半面板。
//!
//! 一顆預覽＝該 bot 自己那個 tab 裡的一顆 service pane（`pane.split`）＋ `bot_previews` 一列。
//! 行程與 port 的查詢全走 [`PreviewEnv`]：正式是 herdr＋本機 TCP，測試換成決定性的假貨，
//! 不碰真 herdr、真行程（#211：那種測試只會在 CI 上紅）。
//!
//! 狀態轉移（[`next_status`]）、偵測（[`detect_dir`]）、挑 port（[`pick_port`]）都是純函式。
//! 所有會動 pane／DB 的入口共用一把全域鎖（[`gate`]）：預覽操作很短，而且 stop／delete bot
//! 是在 bot 鎖裡呼叫進來的，所以這裡**絕不**再拿 bot 鎖。

use crate::db;
use crate::herdr::HerdrClient;
use crate::lifecycle::{LcError, LcResult};
use crate::state::App;
use futures::future::BoxFuture;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// 5173 留給人手開的 `npx vite`。
pub const PORT_START: u16 = 5180;
/// 往上找多少顆；同時預覽二十顆頂層 bot 也夠。
pub const PORT_SPAN: u16 = 100;
/// port 開始 listen 之前最多等多久，逾時轉 `failed`。
pub const START_TIMEOUT_SECS: i64 = 60;
const POLL: Duration = Duration::from_secs(1);
const TAIL_LINES: u32 = 40;
const CONFIG_NAMES: [&str; 4] = ["vite.config.ts", "vite.config.mts", "vite.config.js", "vite.config.mjs"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Off,
    Starting,
    Running,
    Failed,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Off => "off",
            Status::Starting => "starting",
            Status::Running => "running",
            Status::Failed => "failed",
        }
    }
    fn parse(s: &str) -> Status {
        match s {
            "starting" => Status::Starting,
            "running" => Status::Running,
            "failed" => Status::Failed,
            _ => Status::Off,
        }
    }
    /// 這一列還佔著 port 與 pane。
    fn is_live(self) -> bool {
        matches!(self, Status::Starting | Status::Running)
    }
}

/// 外面世界的四個問題。`None`＝問不到（不是「沒有」）：呼叫端一律當「沒變」。
pub trait PreviewEnv: Send + Sync {
    /// 在 `target_pane` 所在的 tab 裡開一顆 pane（cwd＝`cwd`），送出 `cmd`；回新 pane 的 id。
    fn spawn<'a>(&'a self, target_pane: &'a str, cwd: &'a str, cmd: &'a str) -> BoxFuture<'a, anyhow::Result<String>>;
    fn pane_alive<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<bool>>;
    fn pane_tail<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, String>;
    /// 關 pane；那個 tab 因此空了就一起關。
    fn close_pane<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, ()>;
    /// 本機這個 port 現在有沒有人在 listen。
    fn port_listening(&self, port: u16) -> BoxFuture<'_, bool>;
}

/// herdr＋本機 TCP。
pub struct RealEnv {
    pub client: HerdrClient,
}

impl PreviewEnv for RealEnv {
    fn spawn<'a>(&'a self, target_pane: &'a str, cwd: &'a str, cmd: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
        Box::pin(async move {
            // 往下切：只吃高度，不像左右切會把 agent 的 TUI 擠到 ~31 欄以下而亂排。
            let pane = self.client.pane_split(target_pane, "down", cwd, json!({})).await?;
            let typed = async {
                self.client.pane_send_text(&pane.pane_id, cmd).await?;
                self.client.pane_send_keys(&pane.pane_id, &["enter"]).await
            }
            .await;
            if let Err(e) = typed {
                let _ = self.client.pane_close(&pane.pane_id).await;
                return Err(e);
            }
            Ok(pane.pane_id)
        })
    }
    fn pane_alive<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, Option<bool>> {
        Box::pin(async move { self.client.pane_get(pane_id).await.ok().map(|p| p.is_some()) })
    }
    fn pane_tail<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, String> {
        Box::pin(async move {
            self.client.pane_read(pane_id, "recent", TAIL_LINES).await.map(|r| r.text).unwrap_or_default()
        })
    }
    fn close_pane<'a>(&'a self, pane_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if let Some(p) = self.client.pane_get(pane_id).await.ok().flatten() {
                crate::lifecycle::close_pane_and_tab(&self.client, Some(&p.workspace_id), Some(&p.tab_id), pane_id).await;
            }
        })
    }
    fn port_listening(&self, port: u16) -> BoxFuture<'_, bool> {
        Box::pin(async move {
            let c = tokio::net::TcpStream::connect(("127.0.0.1", port));
            matches!(tokio::time::timeout(Duration::from_millis(400), c).await, Ok(Ok(_)))
        })
    }
}

#[cfg(test)]
pub(crate) type EnvOverride = std::sync::Mutex<Option<Arc<dyn PreviewEnv>>>;

/// 這顆 bot 的 run 用的 env：測試可以在 `App::preview_env` 塞假貨。
async fn env_for(app: &Arc<App>, run: &db::Run) -> LcResult<Arc<dyn PreviewEnv>> {
    #[cfg(test)]
    if let Some(e) = app.preview_env.lock().unwrap().clone() {
        return Ok(e);
    }
    let client = app
        .herdr_for_run(run)
        .await
        .ok_or_else(|| LcError::Upstream(format!("no Herdr session is available for run `{}`", run.id)))?;
    Ok(Arc::new(RealEnv { client }))
}

/// 沒有 run 可問（bot 已停）時關舊 pane 用的 env：本機的管理 session。
fn fallback_env(app: &Arc<App>) -> Arc<dyn PreviewEnv> {
    #[cfg(test)]
    if let Some(e) = app.preview_env.lock().unwrap().clone() {
        return e;
    }
    Arc::new(RealEnv { client: app.herdr.clone() })
}

// ───────────────────────────── 純函式 ─────────────────────────────

/// `<cwd>/vite.config.*`，再來 `<cwd>/web/vite.config.*`。回設定檔所在的目錄；找不到回試過的完整路徑。
pub fn detect_dir(cwd: &Path, exists: impl Fn(&Path) -> bool) -> Result<PathBuf, Vec<String>> {
    let mut tried = Vec::new();
    for dir in [cwd.to_path_buf(), cwd.join("web")] {
        for name in CONFIG_NAMES {
            let p = dir.join(name);
            if exists(&p) {
                return Ok(dir);
            }
            tried.push(p.to_string_lossy().into_owned());
        }
    }
    Err(tried)
}

/// 從 [`PORT_START`] 起往上找第一顆沒被別的預覽佔著、也沒人在 listen 的。
pub fn pick_port(taken: &HashSet<u16>, listening: &HashSet<u16>) -> Option<u16> {
    (PORT_START..PORT_START + PORT_SPAN).find(|p| !taken.contains(p) && !listening.contains(p))
}

/// `allow_lan` 開著（dev 的區網／Tailscale 存取）才綁全部介面。
pub fn command(allow_lan: bool, port: u16) -> String {
    let bind = if allow_lan { "0.0.0.0" } else { "127.0.0.1" };
    format!("bunx vite --host {bind} --port {port} --strictPort")
}

/// 一次觀察到的事實。`pane_alive` 為 `None`＝herdr 沒回答，當作還在。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub pane_alive: Option<bool>,
    pub listening: bool,
}

/// 轉移結果；`Failed` 帶原因（呼叫端再補 pane 尾巴）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    Stay,
    To(Status, Option<&'static str>),
}

/// `off`／`failed` 是靜止的；`starting` 等 port 出現（pane 沒了或逾時＝失敗）；`running` 在 pane 被關時回 `off`
/// （使用者自己關的，不是錯），port 不見但 pane 還在＝vite 掛了，`failed`。
pub fn next_status(cur: Status, obs: Observed, elapsed_secs: i64) -> Next {
    let gone = obs.pane_alive == Some(false);
    match cur {
        Status::Off | Status::Failed => Next::Stay,
        Status::Starting if obs.listening => Next::To(Status::Running, None),
        Status::Starting if gone => Next::To(Status::Failed, Some("pane 在 vite 起來之前就被關了")),
        Status::Starting if elapsed_secs >= START_TIMEOUT_SECS => {
            Next::To(Status::Failed, Some("60 秒內 port 沒有開始 listen"))
        }
        Status::Starting => Next::Stay,
        Status::Running if gone => Next::To(Status::Off, None),
        Status::Running if !obs.listening => Next::To(Status::Failed, Some("vite 已經停了（port 不再 listen）")),
        Status::Running => Next::Stay,
    }
}

// ───────────────────────────── DB ─────────────────────────────

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Row {
    pub bot_id: String,
    pub host: String,
    pub pane_id: Option<String>,
    pub port: Option<i64>,
    pub dir: Option<String>,
    pub status: String,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: String,
}

impl Row {
    fn status(&self) -> Status {
        Status::parse(&self.status)
    }
    /// `GET/POST/DELETE` 的回應形狀。
    pub fn body(&self) -> Value {
        if self.status() == Status::Off {
            return off_body();
        }
        json!({
            "status": self.status,
            "port": self.port,
            "dir": self.dir,
            "pane_id": self.pane_id,
            "error": self.error,
            "started_at": self.started_at,
        })
    }
}

fn off_body() -> Value {
    json!({"status": "off"})
}

pub async fn row(pool: &SqlitePool, bot_id: &str) -> anyhow::Result<Option<Row>> {
    Ok(sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE bot_id = ?").bind(bot_id).fetch_optional(pool).await?)
}

async fn live_rows(pool: &SqlitePool) -> anyhow::Result<Vec<Row>> {
    Ok(sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE status IN ('starting','running')").fetch_all(pool).await?)
}

async fn taken_ports(pool: &SqlitePool) -> anyhow::Result<HashSet<u16>> {
    Ok(live_rows(pool).await?.into_iter().filter_map(|r| r.port).filter_map(|p| u16::try_from(p).ok()).collect())
}

async fn put(pool: &SqlitePool, r: &Row) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO bot_previews (bot_id, host, pane_id, port, dir, status, error, started_at, updated_at)
         VALUES (?,?,?,?,?,?,?,?,?)
         ON CONFLICT(bot_id) DO UPDATE SET host=excluded.host, pane_id=excluded.pane_id, port=excluded.port,
           dir=excluded.dir, status=excluded.status, error=excluded.error, started_at=excluded.started_at,
           updated_at=excluded.updated_at",
    )
    .bind(&r.bot_id)
    .bind(&r.host)
    .bind(&r.pane_id)
    .bind(r.port)
    .bind(&r.dir)
    .bind(&r.status)
    .bind(&r.error)
    .bind(&r.started_at)
    .bind(&r.updated_at)
    .execute(pool)
    .await?;
    Ok(())
}

/// `/api/state` 每顆 bot 的 `preview`：`{"status","port"}`，沒開過或 `off` 是 `null`。
pub async fn state_map(pool: &SqlitePool) -> anyhow::Result<HashMap<String, Value>> {
    let rows = sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE status != 'off'").fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.bot_id.clone(), json!({"status": r.status, "port": r.port}))).collect())
}

async fn emit_changed(app: &Arc<App>, r: &Row) {
    app.emit("preview_changed", json!({"bot_id": r.bot_id, "status": r.status, "port": r.port})).await;
}

// ───────────────────────────── 入口 ─────────────────────────────

/// 預覽操作的全域鎖：見模組說明。
fn gate() -> &'static tokio::sync::Mutex<()> {
    static G: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    G.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

fn elapsed_secs(started_at: Option<&str>) -> i64 {
    started_at
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|t| (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).num_seconds())
        .unwrap_or(0)
}

async fn top_level_bot(app: &Arc<App>, bot_id: &str) -> LcResult<db::Bot> {
    let bot = db::bot(&app.db, bot_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    if bot.deleted_at.is_some() {
        return Err(LcError::NotFound("bot".into()));
    }
    Ok(bot)
}

/// `GET`：先對一次帳（pane 還在嗎、port 還在 listen 嗎），再回。
pub async fn get(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    top_level_bot(app, bot_id).await?;
    let _g = gate().lock().await;
    Ok(refresh_locked(app, bot_id).await.map(|r| r.body()).unwrap_or_else(off_body))
}

/// `POST`：冪等啟動。
pub async fn start(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    let bot = top_level_bot(app, bot_id).await?;
    if bot.parent_bot_id.is_some() || bot.managed_by != "user" {
        return Err(LcError::conflict("not_top_level", json!({"bot_id": bot_id})));
    }
    crate::lifecycle::refuse_default_session(&bot)?;
    let host = db::bot_host(&app.db, bot_id).await.map_err(up)?;
    if host != crate::config::LOCAL_HOST {
        // iframe 連的是瀏覽器所在那台的 port；遠端主機上的 vite 連不到。
        return Err(LcError::conflict("remote_host", json!({"bot_id": bot_id, "host": host})));
    }
    let _g = gate().lock().await;
    // 已經在跑（或在起）而且對得上帳：原樣回。
    if let Some(r) = refresh_locked(app, bot_id).await {
        if r.status().is_live() {
            return Ok(r.body());
        }
    }
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;
    let Some(run) = run.filter(|r| r.state == "running" && r.pane_id.is_some()) else {
        return Err(LcError::conflict("bot_not_running", json!({"bot_id": bot_id})));
    };
    let project = db::project(&app.db, &bot.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    let cwd = bot.cwd.clone().filter(|c| !c.trim().is_empty()).unwrap_or(project.path);
    let dir = detect_dir(Path::new(&cwd), |p| p.is_file())
        .map_err(|tried| LcError::conflict("no_vite_config", json!({"bot_id": bot_id, "tried": tried})))?;
    let dir = dir.to_string_lossy().into_owned();

    let env = env_for(app, &run).await?;
    let taken = taken_ports(&app.db).await.map_err(up)?;
    let listening = listening_window(env.as_ref()).await;
    let port = pick_port(&taken, &listening)
        .ok_or_else(|| LcError::conflict("no_free_port", json!({"from": PORT_START, "span": PORT_SPAN})))?;
    let pane = run.pane_id.clone().unwrap_or_default();
    let pane_id = env.spawn(&pane, &dir, &command(app.allow_lan, port)).await.map_err(up)?;
    let now = db::now();
    let r = Row {
        bot_id: bot_id.to_string(),
        host,
        pane_id: Some(pane_id.clone()),
        port: Some(port as i64),
        dir: Some(dir),
        status: Status::Starting.as_str().into(),
        error: None,
        started_at: Some(now.clone()),
        updated_at: now,
    };
    if let Err(e) = put(&app.db, &r).await {
        // 記不下來就不留孤兒 pane。
        env.close_pane(&pane_id).await;
        return Err(up(e));
    }
    emit_changed(app, &r).await;
    // 測試自己決定什麼時候看（不然背景那一拍會跟測試的手動轉移搶）；watcher 另有測試。
    #[cfg(not(test))]
    spawn_watcher(app.clone(), bot_id.to_string());
    Ok(r.body())
}

/// `DELETE`：關 pane、放掉 port。
pub async fn stop(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    top_level_bot(app, bot_id).await?;
    stop_for_bot(app, bot_id).await;
    Ok(off_body())
}

/// bot 被停止／刪除／閒置收掉時一起收預覽。盡力而為：讀不到就記 log，不擋 bot 的停機。
pub async fn stop_for_bot(app: &Arc<App>, bot_id: &str) {
    let _g = gate().lock().await;
    let r = match row(&app.db, bot_id).await {
        Ok(Some(r)) if r.status() != Status::Off => r,
        Ok(_) => return,
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot read the preview row while stopping the bot");
            return;
        }
    };
    close_row_pane(app, &r).await;
    let off = Row { pane_id: None, status: Status::Off.as_str().into(), error: None, updated_at: db::now(), ..r };
    if let Err(e) = put(&app.db, &off).await {
        tracing::warn!(bot = bot_id, error = %e, "preview: the preview was closed but its row could not be marked off");
        return;
    }
    emit_changed(app, &off).await;
}

async fn close_row_pane(app: &Arc<App>, r: &Row) {
    let Some(pane) = r.pane_id.as_deref() else { return };
    let env = match db::active_run(&app.db, &r.bot_id).await {
        Ok(Some(run)) => env_for(app, &run).await.unwrap_or_else(|_| fallback_env(app)),
        _ => fallback_env(app),
    };
    env.close_pane(pane).await;
}

/// 開機對帳：`starting`／`running` 的列拿「pane 還在不在」＋「port 有沒有在 listen」對回去；還在 `starting` 的補上 watcher。
pub async fn reconcile_all(app: &Arc<App>) {
    let rows = match live_rows(&app.db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "preview: cannot read bot_previews at startup");
            return;
        }
    };
    for r in rows {
        let _g = gate().lock().await;
        if let Some(now) = refresh_locked(app, &r.bot_id).await {
            if now.status() == Status::Starting {
                spawn_watcher(app.clone(), r.bot_id.clone());
            }
        }
    }
}

// ───────────────────────────── 對帳 ─────────────────────────────

/// 一次觀察＋轉移＋寫回＋通知；回最新的列（沒有這列回 `None`）。要在 [`gate`] 裡呼叫。
async fn refresh_locked(app: &Arc<App>, bot_id: &str) -> Option<Row> {
    let r = match row(&app.db, bot_id).await {
        Ok(r) => r?,
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot read the preview row");
            return None;
        }
    };
    if !r.status().is_live() {
        return Some(r);
    }
    let run = db::active_run(&app.db, bot_id).await.ok().flatten();
    // bot 自己掛了或被停了：預覽跟著收，不留一顆沒人管的 vite。
    if run.is_none() {
        close_row_pane(app, &r).await;
        let off = Row { pane_id: None, status: Status::Off.as_str().into(), error: None, updated_at: db::now(), ..r };
        return match put(&app.db, &off).await {
            Ok(()) => {
                emit_changed(app, &off).await;
                Some(off)
            }
            Err(e) => {
                tracing::warn!(bot = bot_id, error = %e, "preview: cannot mark the preview off");
                None
            }
        };
    }
    let env = env_for(app, run.as_ref()?).await.ok()?;
    let (pane_alive, listening) = match (&r.pane_id, r.port) {
        (Some(p), Some(port)) => (env.pane_alive(p).await, env.port_listening(port as u16).await),
        (_, Some(port)) => (Some(false), env.port_listening(port as u16).await),
        _ => (Some(false), false),
    };
    let next = next_status(r.status(), Observed { pane_alive, listening }, elapsed_secs(r.started_at.as_deref()));
    let Next::To(to, why) = next else { return Some(r) };
    let error = match (to, why) {
        (Status::Failed, Some(why)) => {
            let tail = match &r.pane_id {
                Some(p) if pane_alive != Some(false) => env.pane_tail(p).await,
                _ => String::new(),
            };
            Some(if tail.trim().is_empty() { why.to_string() } else { format!("{why}\n{}", tail.trim_end()) })
        }
        _ => None,
    };
    if to == Status::Off {
        close_row_pane(app, &r).await;
    }
    let pane_id = if to == Status::Off { None } else { r.pane_id.clone() };
    let updated = Row { status: to.as_str().into(), error, pane_id, updated_at: db::now(), ..r };
    match put(&app.db, &updated).await {
        Ok(()) => {
            emit_changed(app, &updated).await;
            Some(updated)
        }
        Err(e) => {
            tracing::warn!(bot = bot_id, error = %e, "preview: cannot record the new preview status");
            None
        }
    }
}

/// 起動期間每秒看一次，離開 `starting` 就結束。
fn spawn_watcher(app: Arc<App>, bot_id: String) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(POLL).await;
            let _g = gate().lock().await;
            match refresh_locked(&app, &bot_id).await {
                Some(r) if r.status() == Status::Starting => {}
                _ => return,
            }
        }
    });
}

/// 挑 port 前一次問完整個窗口。
async fn listening_window(env: &dyn PreviewEnv) -> HashSet<u16> {
    let checks = (PORT_START..PORT_START + PORT_SPAN).map(|p| async move { (p, env.port_listening(p).await) });
    futures::future::join_all(checks).await.into_iter().filter(|(_, l)| *l).map(|(p, _)| p).collect()
}

#[cfg(test)]
mod tests;
