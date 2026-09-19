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
pub const SOURCE_SPAWNED: &str = "spawned";
pub const SOURCE_ATTACHED: &str = "attached";
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
    /// 本機上在 listen 的 vite 行程（pid、port、cwd）；`None`＝掃不到（不是「沒有」）。
    fn scan_vites(&self) -> BoxFuture<'_, Option<Vec<ViteProc>>>;
    /// 這個目錄屬於哪個 git repo；不是 repo 或讀不到＝`None`。
    fn repo_key<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Option<RepoKey>>;
}

/// 判斷「同一個 repo」用：git common dir（同一個 repo 的所有 worktree 共用）與 origin URL（各自 clone 的同一個 repo）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoKey {
    pub common: String,
    pub origin: Option<String>,
}

/// 兩個目錄是不是同一個 repo：common dir 相同，或兩邊都有 origin 而且相同。
pub fn same_repo(a: &RepoKey, b: &RepoKey) -> bool {
    a.common == b.common || matches!((&a.origin, &b.origin), (Some(x), Some(y)) if x == y)
}

/// `git rev-parse --git-common-dir`（可能是相對於 `dir` 的路徑）與 `git config remote.origin.url` 的輸出。
pub fn parse_repo_key(dir: &str, common_out: &str, origin_out: &str) -> Option<RepoKey> {
    let common = common_out.trim();
    if common.is_empty() {
        return None;
    }
    let p = Path::new(common);
    let abs = if p.is_absolute() { p.to_path_buf() } else { Path::new(dir).join(p) };
    let common = std::fs::canonicalize(&abs).unwrap_or(abs).to_string_lossy().into_owned();
    let origin = Some(origin_out.trim().to_string()).filter(|o| !o.is_empty());
    Some(RepoKey { common, origin })
}

/// 一顆已經在跑、而且有 TCP listen 的 vite。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViteProc {
    pub pid: i32,
    pub port: u16,
    pub cwd: String,
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
    fn scan_vites(&self) -> BoxFuture<'_, Option<Vec<ViteProc>>> {
        Box::pin(scan_real())
    }
    fn repo_key<'a>(&'a self, dir: &'a str) -> BoxFuture<'a, Option<RepoKey>> {
        Box::pin(async move {
            let q = crate::hosts::sh_quote(dir);
            let t = Duration::from_secs(5);
            let common = crate::hosts::sh_local(&format!("git -C {q} rev-parse --git-common-dir 2>/dev/null"), t).await.ok().flatten()?;
            if !common.status.success() {
                return None;
            }
            let origin = crate::hosts::sh_local(&format!("git -C {q} config --get remote.origin.url 2>/dev/null"), t).await.ok().flatten()?;
            parse_repo_key(dir, &String::from_utf8_lossy(&common.stdout), &String::from_utf8_lossy(&origin.stdout))
        })
    }
}

/// `ps` 找出命令列是 vite 的 pid，再各問一次 `lsof`：listen 的 port、cwd。
async fn scan_real() -> Option<Vec<ViteProc>> {
    let t = Duration::from_secs(10);
    let ps = crate::hosts::sh_local("ps -axo pid=,command=", t).await.ok().flatten()?;
    let pids = parse_ps_vites(&String::from_utf8_lossy(&ps.stdout));
    if pids.is_empty() {
        return Some(Vec::new());
    }
    let list = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(",");
    let ports = crate::hosts::sh_local(&format!("lsof -nP -iTCP -sTCP:LISTEN -a -p {list} -Fpn 2>/dev/null"), t).await.ok().flatten()?;
    let cwds = crate::hosts::sh_local(&format!("lsof -nP -a -d cwd -p {list} -Fpn 2>/dev/null"), t).await.ok().flatten()?;
    Some(join_vites(
        &crate::panes::parse_lsof(&String::from_utf8_lossy(&ports.stdout)),
        &parse_lsof_cwd(&String::from_utf8_lossy(&cwds.stdout)),
    ))
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

/// 候選目錄，依序：`<cwd>`、`<cwd>/web`、`<cwd>/apps/*`、`<cwd>/packages/*`（monorepo 常見；後兩層各自照名字排序），
/// 只留下有 `vite.config.*` 的。全都沒有回試過的路徑（`apps/*`、`packages/*` 寫成樣式）。
pub fn detect_dirs(
    cwd: &Path,
    exists: impl Fn(&Path) -> bool,
    subdirs: impl Fn(&Path) -> Vec<PathBuf>,
) -> Result<Vec<PathBuf>, Vec<String>> {
    let has_config = |dir: &Path| CONFIG_NAMES.iter().any(|n| exists(&dir.join(n)));
    let mut dirs = vec![cwd.to_path_buf(), cwd.join("web")];
    for group in ["apps", "packages"] {
        let mut subs = subdirs(&cwd.join(group));
        subs.sort();
        dirs.extend(subs);
    }
    let found: Vec<PathBuf> = dirs.into_iter().filter(|d| has_config(d)).collect();
    if !found.is_empty() {
        return Ok(found);
    }
    let mut tried = Vec::new();
    for dir in [cwd.to_path_buf(), cwd.join("web")] {
        tried.extend(CONFIG_NAMES.iter().map(|n| dir.join(n).to_string_lossy().into_owned()));
    }
    for group in ["apps", "packages"] {
        tried.push(cwd.join(group).join("*").join("vite.config.*").to_string_lossy().into_owned());
    }
    Err(tried)
}

/// 命令列的某個字是 `vite` 或 `.../vite`、`.../vite.js`（`vitest` 之類不算）。
pub fn is_vite_command(cmd: &str) -> bool {
    cmd.split_whitespace().any(|t| {
        let base = t.rsplit('/').next().unwrap_or(t);
        base == "vite" || base == "vite.js"
    })
}

/// `ps -axo pid=,command=` 的輸出裡，命令列是 vite 的 pid。
pub fn parse_ps_vites(out: &str) -> Vec<i32> {
    out.lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let (pid, cmd) = l.split_once(char::is_whitespace)?;
            is_vite_command(cmd).then(|| pid.parse().ok()).flatten()
        })
        .collect()
}

/// `lsof -a -d cwd -Fpn`：`p<pid>` 之後的 `n<path>` 是那個 pid 的 cwd。
pub fn parse_lsof_cwd(out: &str) -> HashMap<i32, String> {
    let mut cur = None;
    let mut m = HashMap::new();
    for l in out.lines() {
        if let Some(r) = l.strip_prefix('p') {
            cur = r.trim().parse().ok();
        } else if let (Some(r), Some(pid)) = (l.strip_prefix('n'), cur) {
            m.insert(pid, r.to_string());
        }
    }
    m
}

/// 有 listen port 又查得到 cwd 的才算一顆可接的 vite（一顆行程多個 port 就一個 port 一筆）。
pub fn join_vites(ports: &HashMap<i32, Vec<u16>>, cwds: &HashMap<i32, String>) -> Vec<ViteProc> {
    let mut v: Vec<ViteProc> = ports
        .iter()
        .filter_map(|(pid, ps)| Some((pid, ps, cwds.get(pid)?)))
        .flat_map(|(pid, ps, cwd)| ps.iter().map(move |port| ViteProc { pid: *pid, port: *port, cwd: cwd.clone() }))
        .collect();
    v.sort_by_key(|p| (p.port, p.pid));
    v
}

fn norm(p: &str) -> &str {
    let t = p.trim_end_matches('/');
    if t.is_empty() { "/" } else { t }
}

/// 掃到的 vite 分兩類：cwd 正好是候選目錄的（同一份 checkout，`attach`；候選順序在前的優先），
/// 其餘全部是 `others`（別份 checkout 或別的專案：畫面上看到的不是這顆 bot 工作樹裡的程式碼，不自動接）。
pub fn classify(procs: &[ViteProc], candidates: &[PathBuf]) -> (Option<ViteProc>, Vec<ViteProc>) {
    let hit = candidates
        .iter()
        .find_map(|c| procs.iter().find(|p| norm(&p.cwd) == norm(&c.to_string_lossy())))
        .cloned();
    let others = procs.iter().filter(|p| Some(*p) != hit.as_ref()).cloned().collect();
    (hit, others)
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
pub fn next_status(cur: Status, attached: bool, obs: Observed, elapsed_secs: i64) -> Next {
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
        // 接上的是別人的 server：它結束了就是斷開，不算我們的失敗。
        Status::Running if attached && !obs.listening => Next::To(Status::Off, None),
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
    /// `spawned`（AG Man 起的，有 pane）或 `attached`（接上既有的 vite，沒有 pane；只斷開、不殺）。
    pub source: String,
    pub pid: Option<i64>,
}

impl Row {
    fn status(&self) -> Status {
        Status::parse(&self.status)
    }
    fn attached(&self) -> bool {
        self.source == SOURCE_ATTACHED
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
            "source": self.source,
            "pid": self.pid,
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
        "INSERT INTO bot_previews (bot_id, host, pane_id, port, dir, status, error, started_at, updated_at, source, pid)
         VALUES (?,?,?,?,?,?,?,?,?,?,?)
         ON CONFLICT(bot_id) DO UPDATE SET host=excluded.host, pane_id=excluded.pane_id, port=excluded.port,
           dir=excluded.dir, status=excluded.status, error=excluded.error, started_at=excluded.started_at,
           updated_at=excluded.updated_at, source=excluded.source, pid=excluded.pid",
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
    .bind(&r.source)
    .bind(r.pid)
    .execute(pool)
    .await?;
    Ok(())
}

/// `/api/state` 每顆 bot 的 `preview`：`{"status","port"}`，沒開過或 `off` 是 `null`。
pub async fn state_map(pool: &SqlitePool) -> anyhow::Result<HashMap<String, Value>> {
    let rows = sqlx::query_as::<_, Row>("SELECT * FROM bot_previews WHERE status != 'off'").fetch_all(pool).await?;
    Ok(rows.into_iter().map(|r| (r.bot_id.clone(), json!({"status": r.status, "port": r.port, "source": r.source}))).collect())
}

async fn emit_changed(app: &Arc<App>, r: &Row) {
    app.emit("preview_changed", json!({"bot_id": r.bot_id, "status": r.status, "port": r.port, "source": r.source})).await;
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

/// 這顆 bot 的 vite 候選目錄（[`detect_dirs`] 對真的檔案系統）。
async fn candidates_of(app: &Arc<App>, bot: &db::Bot) -> LcResult<Result<Vec<PathBuf>, Vec<String>>> {
    let project = db::project(&app.db, &bot.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    let cwd = bot.cwd.clone().filter(|c| !c.trim().is_empty()).unwrap_or(project.path);
    let subdirs = |d: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(d).map(|it| it.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect()).unwrap_or_default()
    };
    Ok(detect_dirs(Path::new(&cwd), |p| p.is_file(), subdirs))
}

/// 回應在 [`Row::body`] 之外多兩欄：`candidates`（可以起 vite 的目錄）、`others`（沒在用的狀態下才掃：
/// 同一個 repo 的別份 checkout／worktree 已經在跑的 vite，列出來讓使用者自己選）。
async fn decorated(app: &Arc<App>, bot: &db::Bot, mut body: Value, live: bool) -> LcResult<Value> {
    let cands = candidates_of(app, bot).await?.unwrap_or_default();
    let mut others = Vec::new();
    if !live {
        let env = match db::active_run(&app.db, &bot.id).await {
            Ok(Some(run)) => env_for(app, &run).await.unwrap_or_else(|_| fallback_env(app)),
            _ => fallback_env(app),
        };
        // 掃不到就當沒有：這只是「順便列出來」，不是決定。
        let scan = env.scan_vites().await.unwrap_or_default();
        let (_, rest) = classify(&scan, &cands);
        // 只列同一個 repo 的其他 checkout／worktree；別的 repo 一律不列。bot 自己的 repo 或某顆 vite 的 repo
        // 判不出來（不是 git、git 讀不到）也不列：寧可少列，不要拿別人的畫面誤導使用者。
        if let Some(mine) = cands.first() {
            if let Some(mine) = env.repo_key(&mine.to_string_lossy()).await {
                for p in rest {
                    if env.repo_key(&p.cwd).await.is_some_and(|k| same_repo(&mine, &k)) {
                        others.push(p);
                    }
                }
            }
        }
    }
    if let Some(o) = body.as_object_mut() {
        o.insert("candidates".into(), json!(cands.iter().map(|c| c.to_string_lossy()).collect::<Vec<_>>()));
        o.insert("others".into(), json!(others.iter().map(|p| json!({"port": p.port, "dir": p.cwd, "pid": p.pid})).collect::<Vec<_>>()));
    }
    Ok(body)
}

/// `GET`：先對一次帳（pane 還在嗎、port 還在 listen 嗎），再回。
pub async fn get(app: &Arc<App>, bot_id: &str) -> LcResult<Value> {
    let bot = top_level_bot(app, bot_id).await?;
    let _g = gate().lock().await;
    let r = refresh_locked(app, bot_id).await;
    let live = r.as_ref().is_some_and(|r| r.status().is_live());
    decorated(app, &bot, r.map(|r| r.body()).unwrap_or_else(off_body), live).await
}

#[derive(Debug, Default, Clone, serde::Deserialize)]
pub struct StartReq {
    /// `auto`（預設：同目錄有在跑的就接、沒有就起）／`attach`（要帶 `port`）／`spawn`。
    pub mode: Option<String>,
    pub port: Option<u16>,
    pub dir: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Attach,
    Spawn,
}

/// `POST`：冪等啟動。
pub async fn start(app: &Arc<App>, bot_id: &str, req: StartReq) -> LcResult<Value> {
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
    let mode = match req.mode.as_deref().unwrap_or("auto") {
        "auto" => Mode::Auto,
        "attach" => Mode::Attach,
        "spawn" => Mode::Spawn,
        other => return Err(LcError::Bad(format!("mode 只能是 auto／attach／spawn，收到 `{other}`"))),
    };
    if mode == Mode::Attach && req.port.is_none() {
        return Err(LcError::Bad("mode=attach 必須帶 port".into()));
    }
    let explicit = mode != Mode::Auto || req.port.is_some() || req.dir.is_some();
    let _g = gate().lock().await;
    // 已經在跑（或在起）而且對得上帳：原樣回；明確指定了別的（換目錄、換接哪顆）才先斷開再來。
    if let Some(r) = refresh_locked(app, bot_id).await {
        if r.status().is_live() {
            if !explicit {
                return decorated(app, &bot, r.body(), true).await;
            }
            disconnect_locked(app, r).await;
        }
    }
    let cands = candidates_of(app, &bot).await?;
    let no_config = |tried: Vec<String>| LcError::conflict("no_vite_config", json!({"bot_id": bot_id, "tried": tried}));
    let run = db::active_run(&app.db, bot_id).await.map_err(up)?;
    let env = match &run {
        Some(run) => env_for(app, run).await.unwrap_or_else(|_| fallback_env(app)),
        None => fallback_env(app),
    };
    // 掃不到只影響「自動接」，不擋 spawn；明確要求 attach 時掃不到就是失敗。
    let scan = env.scan_vites().await;

    let attach_to: Option<ViteProc> = match mode {
        Mode::Attach => {
            let port = req.port.unwrap_or_default();
            let hit = scan.as_deref().unwrap_or_default().iter().find(|p| p.port == port).cloned();
            Some(hit.ok_or_else(|| LcError::conflict("not_vite", json!({"bot_id": bot_id, "port": port})))?)
        }
        Mode::Auto => {
            let all = cands.clone().map_err(no_config)?;
            let pool = match req.dir.as_deref() {
                Some(d) => vec![all.iter().find(|c| norm(&c.to_string_lossy()) == norm(d)).cloned().ok_or_else(|| {
                    LcError::Bad(format!("dir `{d}` 不是這顆 bot 的 vite 候選目錄"))
                })?],
                None => all,
            };
            classify(scan.as_deref().unwrap_or_default(), &pool).0
        }
        Mode::Spawn => None,
    };
    if let Some(p) = attach_to {
        let now = db::now();
        let r = Row {
            bot_id: bot_id.to_string(),
            host,
            pane_id: None,
            port: Some(p.port as i64),
            dir: Some(p.cwd),
            status: Status::Running.as_str().into(),
            error: None,
            started_at: Some(now.clone()),
            updated_at: now,
            source: SOURCE_ATTACHED.into(),
            pid: Some(p.pid as i64),
        };
        put(&app.db, &r).await.map_err(up)?;
        emit_changed(app, &r).await;
        return decorated(app, &bot, r.body(), true).await;
    }

    let cands = cands.map_err(no_config)?;
    let dir = match req.dir.as_deref() {
        Some(d) => cands
            .iter()
            .find(|c| norm(&c.to_string_lossy()) == norm(d))
            .cloned()
            .ok_or_else(|| LcError::Bad(format!("dir `{d}` 不是這顆 bot 的 vite 候選目錄")))?,
        None => cands[0].clone(),
    }
    .to_string_lossy()
    .into_owned();
    let Some(run) = run.filter(|r| r.state == "running" && r.pane_id.is_some()) else {
        return Err(LcError::conflict("bot_not_running", json!({"bot_id": bot_id})));
    };
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
        source: SOURCE_SPAWNED.into(),
        pid: None,
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
    decorated(app, &bot, r.body(), true).await
}

/// 斷開一列：`spawned` 關 pane，`attached` 什麼都不動（那顆 vite 是別人的），然後標 `off`。要在 [`gate`] 裡呼叫。
async fn disconnect_locked(app: &Arc<App>, r: Row) -> Option<Row> {
    close_row_pane(app, &r).await;
    let off = Row { pane_id: None, status: Status::Off.as_str().into(), error: None, updated_at: db::now(), ..r };
    match put(&app.db, &off).await {
        Ok(()) => {
            emit_changed(app, &off).await;
            Some(off)
        }
        Err(e) => {
            tracing::warn!(bot = %off.bot_id, error = %e, "preview: cannot mark the preview off");
            None
        }
    }
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
    disconnect_locked(app, r).await;
}

async fn close_row_pane(app: &Arc<App>, r: &Row) {
    // 接上的那顆是別人開的 server：只斷開，絕不動它。
    if r.attached() {
        return;
    }
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
    let attached = r.attached();
    let run = db::active_run(&app.db, bot_id).await.ok().flatten();
    // bot 自己掛了或被停了：預覽跟著收，不留一顆沒人管的 vite。接上的只看它自己的 port，不看 bot。
    if run.is_none() && !attached {
        return disconnect_locked(app, r).await;
    }
    let env = match (&run, attached) {
        (Some(run), false) => env_for(app, run).await.ok()?,
        _ => fallback_env(app),
    };
    let (pane_alive, listening) = match (&r.pane_id, r.port) {
        (Some(p), Some(port)) => (env.pane_alive(p).await, env.port_listening(port as u16).await),
        (None, Some(port)) if attached => (None, env.port_listening(port as u16).await),
        (_, Some(port)) => (Some(false), env.port_listening(port as u16).await),
        _ => (Some(false), false),
    };
    let next = next_status(r.status(), attached, Observed { pane_alive, listening }, elapsed_secs(r.started_at.as_deref()));
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
