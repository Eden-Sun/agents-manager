//! herdr 一鍵更新（完整重啟版，SPEC §6.9b）：`POST /api/hosts/{name}/herdr-update`。
//!
//! 流程照 2026-09-17 那次手動升級（`scripts/ops/herdr-upgrade.sh`）的教訓排：
//! 1. **先下載到 staging、驗 `--version`＝目標，才換**：下載壞掉或版本不對，現行 binary 一個位元都沒動。
//! 2. 等頂層 bot 都閒置（最多 30 分鐘）：重啟 herdr server 會把每一顆 bot 的回合砍掉。逾時就結束，什麼都沒動。
//! 3. 開 herdr 維護窗口（`herdr_maintenance`）：pane 同時消失時，reconcile 不能把它當成「子 agent 做完了」一次刪光。
//! 4. 原子換 binary（舊的留 `herdr.bak-<舊版>`）→ 只重啟這顆 daemon 用的 session（Linux `herdr@<session>`、macOS launchd job）。
//! 5. 等 ping 回來而且 server 版本＝目標；不是就換回 `.bak`、再重啟、`reason:"restart_failed"`，bot 照樣接回。
//! 6. 記下的頂層 bot 逐顆 `resume_native` 接回；子 agent 一律沒了（pane 是父 bot 開的，daemon 重建不了），
//!    給母 bot 一則系統訊息列出失去的子 agent 與原任務標題，由母 bot 決定要不要重開。
//! 7. 關窗口，推 `herdr_update_done`。
//!
//! 進行中的狀態只在記憶體（同 `bulk_restart` 的批次）：daemon 半途重啟時，維護窗口最多 30 分鐘自己到期，
//! 換到一半的 binary 有 `.bak` 可人工換回，每一步都寫在 `<data_dir>/herdr-update.log`。
//! 會碰外面世界的動作（下載、`--version`、重啟 server、接回 bot、通知母 bot）都經過 [`Ops`]，測試用假的跑完整流程。
//! 本機與非 shared_session 遠端共用流程；主機上的檔案與服務操作都經過可替換介面。

use crate::changelog::{cli_version_string, parse_version, version_string};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path as AxPath, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub const LOG_FILE: &str = "herdr-update.log";
const STAGING_DIR: &str = "herdr-staging";
const RELEASE_BASE: &str = "https://github.com/herdrdev/herdr/releases/download";

/// 各步驟的等待上限。測試換成毫秒級。
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// 等頂層 bot 都閒置的上限（契約：30 分鐘）。
    pub idle_max: Duration,
    pub idle_poll: Duration,
    /// 重啟後等 ping 回來且版本對的上限（契約：2 分鐘）。
    pub restart_max: Duration,
    pub restart_poll: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Timing {
            idle_max: Duration::from_secs(30 * 60),
            idle_poll: Duration::from_secs(5),
            restart_max: Duration::from_secs(120),
            restart_poll: Duration::from_secs(1),
        }
    }
}

/// 會碰外面世界的動作。`Err` 都是給人看的原因。
pub trait Ops: Send + Sync {
    /// `session` 的 server 實際執行的 herdr binary 路徑（launchd job 的 program／systemd 的 ExecStart，重啟時 exec 的那一個）。
    fn install_path<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<PathBuf, String>>;
    /// 這台主機上 bot／shim 解析到的 herdr CLI（login shell 的 `command -v herdr`）：跟 `server` 是**不同檔**才回 `Some`
    /// （同機常有兩份：launchd 跑 `/opt/homebrew/bin/herdr`、login shell 解析到 `~/.local/bin/herdr`）。兩份都要換，
    /// 只換一份就會留下 server／CLI 版本不一致（protocol_mismatch，畫面繼續顯示要更新）。
    fn cli_path<'a>(&'a self, _server: &'a Path) -> BoxFuture<'a, Result<Option<PathBuf>, String>> {
        Box::pin(async { Ok(None) })
    }
    fn staging<'a>(&'a self, app: &'a Arc<App>, version: &'a str) -> BoxFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move {
            let path = app.data_dir.join(STAGING_DIR).join(version).join("herdr");
            std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
            Ok(path)
        })
    }
    fn cleanup<'a>(&'a self, app: &'a Arc<App>, version: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let dir = app.data_dir.join(STAGING_DIR).join(version);
            if dir.exists() { std::fs::remove_dir_all(dir).map_err(|e| e.to_string())?; }
            Ok(())
        })
    }
    fn swap<'a>(&'a self, staging: &'a Path, install: &'a Path, from: &'a str) -> BoxFuture<'a, Result<PathBuf, String>> {
        Box::pin(async move { swap_in(staging, install, from) })
    }
    fn swap_outcome_uncertain(&self) -> bool { false }
    fn rollback<'a>(&'a self, backup: &'a Path, install: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { restore(backup, install) })
    }
    /// 把 `version` 的 release asset 下載到 `dest`（可執行）。
    fn download<'a>(&'a self, version: &'a str, dest: &'a Path) -> BoxFuture<'a, Result<(), String>>;
    /// 下載好的 `path` 對得上 release 公布的 sha256 嗎。**在執行它之前**（`--version` 就是執行）；對不上、或查不到公布值都是 `Err`。
    fn verify_download<'a>(&'a self, version: &'a str, path: &'a Path) -> BoxFuture<'a, Result<(), String>>;
    /// 換 binary **之前**確認重啟這條路走得通（server 真的是 systemd／launchd 在管的）。`Err`＝什麼都不動。
    fn preflight_restart<'a>(&'a self, session: &'a str, install: &'a Path) -> BoxFuture<'a, Result<(), String>>;
    /// `<path> --version` 讀到的版本（`0.9.3`）。
    fn binary_version<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>>;
    /// 重啟 `session` 的 herdr server（只動這一個 session）。
    fn restart_server<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<(), String>>;
    /// 把頂層 bot 以 `resume_native` 接回，回新的 run id。
    fn resume<'a>(&'a self, app: &'a Arc<App>, bot_id: &'a str) -> BoxFuture<'a, Result<String, String>>;
    /// 給母 bot 一則系統訊息（子 agent 沒了）。
    fn notify_parent<'a>(&'a self, app: &'a Arc<App>, parent_bot_id: &'a str, text: &'a str, request_id: &'a str) -> BoxFuture<'a, Result<(), String>>;
}

// ---------------------------------------------------------------- 進行中（記憶體）

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Progress {
    pub update_id: String,
    pub host: String,
    pub target_version: String,
    pub phase: String,
    pub started_at: String,
    pub session: String,
    pub manual_upgrade_command: String,
}

/// 以 data_dir 分（測試同一個行程裡有很多顆 App）。同時只跑一個。
fn running() -> &'static Mutex<HashMap<PathBuf, Progress>> {
    static M: OnceLock<Mutex<HashMap<PathBuf, Progress>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 這顆 daemon 是否正在更新指定主機的 herdr（cli_update 開工前問）。
pub fn is_running(app: &impl crate::capabilities::DataDir, host: &str) -> bool {
    running().lock().unwrap_or_else(|e| e.into_inner()).get(app.data_dir()).is_some_and(|p| p.host == host)
}

/// 這幾個階段 herdr server 即將被（或正在）砍：新的 prompt／relay／交辦打進去，回合會直接被殺，
/// 接回之後也沒有人重送（issue #893）。`stopping` 在「開維護窗口之後、複查閒置之前」就設好，閘門先關、複查才算數。
const GATED_PHASES: [&str; 3] = ["stopping", "restarting", "rolling_back"];

/// 送出入口的閘門：這台 daemon 的 herdr 更新走到「擋新派送」的階段就回那一筆進度。
/// `MaintenancePort::window_held` 的 App 實作讀它，於是 `lifecycle::prompt`、排隊 flush 與交辦派送共用同一份判斷。
pub fn gate(app: &impl crate::capabilities::DataDir) -> Option<Progress> {
    running()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(app.data_dir())
        .filter(|p| GATED_PHASES.contains(&p.phase.as_str()))
        .cloned()
}

/// 測試用：佔住這顆 App 的更新位置，掉了就放。
#[cfg(test)]
pub(crate) fn hold_slot_for_test(app: &App) -> impl Drop + '_ {
    hold_slot_for_host_for_test(app, "local")
}

#[cfg(test)]
pub(crate) fn hold_slot_for_host_for_test<'a>(app: &'a App, host: &str) -> impl Drop + 'a {
    struct Held<'a>(&'a App);
    impl Drop for Held<'_> {
        fn drop(&mut self) {
            running().lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0.data_dir);
        }
    }
    running().lock().unwrap_or_else(|e| e.into_inner()).insert(
        app.data_dir.clone(),
        Progress { update_id: "held".into(), host: host.into(), target_version: "0.0.0".into(), phase: "downloading".into(), started_at: crate::db::now(), session: "agents-manager".into(), manual_upgrade_command: manual_upgrade_command().into() },
    );
    Held(app)
}

/// `GET /api/state` 的 `herdr_updates`：進行中的那一筆（沒有就空陣列）。
pub fn running_list(app: &impl crate::capabilities::DataDir) -> Vec<Progress> {
    running().lock().unwrap_or_else(|e| e.into_inner()).get(app.data_dir()).cloned().into_iter().collect()
}

fn set_phase(app: &impl crate::capabilities::DataDir, update_id: &str, phase: &str) {
    if let Some(p) = running().lock().unwrap_or_else(|e| e.into_inner()).get_mut(app.data_dir()) {
        if p.update_id == update_id {
            p.phase = phase.to_string();
        }
    }
}

fn release_slot(app: &impl crate::capabilities::DataDir, update_id: &str) {
    let mut g = running().lock().unwrap_or_else(|e| e.into_inner());
    if g.get(app.data_dir()).is_some_and(|p| p.update_id == update_id) {
        g.remove(app.data_dir());
    }
}

// ---------------------------------------------------------------- 影響範圍

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Resumable {
    pub bot_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LostChild {
    pub bot_id: String,
    pub name: String,
    pub parent_bot_id: Option<String>,
    /// herdr 的 terminal title（Claude Code 的任務摘要）；給母 bot 的通知用，不進 API。
    #[serde(skip)]
    pub title: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct Affected {
    pub resume: Vec<Resumable>,
    pub children: Vec<LostChild>,
    /// 頂層 bot 中正在忙的（`working`／`blocked`／有回合在飛），`name (why)`。
    pub busy: Vec<String>,
}

/// 目標主機上會被 server 重啟打斷的 bot。使用者自己的 `default` session 不在這個 server 上，不算。
pub async fn affected(app: &impl crate::capabilities::Db, host: &str) -> anyhow::Result<Affected> {
    let mut out = Affected::default();
    for run in crate::db::all_active_runs(app.db()).await? {
        let Some(bot) = crate::db::bot(app.db(), &run.bot_id).await? else { continue };
        if bot.deleted_at.is_some() || crate::db::bot_host(app.db(), &bot.id).await? != host {
            continue;
        }
        if crate::app_ports_p12::in_default_session(&run) || bot.herdr_session.as_deref() == Some("default") {
            continue;
        }
        let parent = bot.parent_bot_id.clone().filter(|p| !p.is_empty());
        if bot.managed_by == "child" || parent.is_some() {
            let name = if bot.managed_by == "child" {
                let project_label = crate::db::project(app.db(), &bot.project_id).await?.map(|p| p.label).unwrap_or_default();
                crate::config::short_child_name(&project_label, &bot.name)
            } else {
                bot.name
            };
            out.children.push(LostChild { bot_id: bot.id, name, parent_bot_id: parent, title: run.agent_title.clone() });
            continue;
        }
        let why = if run.agent_status == "working" {
            Some("working")
        } else if run.agent_status == "blocked" {
            Some("blocked")
        } else if crate::db::in_flight_turn(app.db(), &run.id).await?.is_some() {
            Some("turn_in_flight")
        } else {
            None
        };
        if let Some(why) = why {
            out.busy.push(format!("{} ({why})", bot.name));
        }
        out.resume.push(Resumable { bot_id: bot.id, name: bot.name });
    }
    let supervisor: Option<String> = sqlx::query_scalar::<_, Option<String>>("SELECT bot_id FROM supervisors LIMIT 1")
        .fetch_optional(app.db())
        .await?
        .flatten()
        .filter(|s| !s.is_empty());
    // 總管最後接回：它負責修其他顆（同 bulk_restart）。
    let pairs = out.resume.iter().map(|r| (r.bot_id.clone(), r.name.clone())).collect();
    out.resume = crate::bulk_restart::supervisor_last(pairs, supervisor.as_deref())
        .into_iter()
        .map(|(bot_id, name)| Resumable { bot_id, name })
        .collect();
    Ok(out)
}

// ---------------------------------------------------------------- API

#[derive(Deserialize, Default)]
pub struct HerdrUpdateIn {
    pub target_version: Option<String>,
}

/// `POST /api/hosts/{name}/herdr-update`
pub async fn post_herdr_update(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    AxPath(host): AxPath<String>,
    body: Option<Json<HerdrUpdateIn>>,
) -> Result<Response, LcError> {
    let b = body.map(|Json(b)| b).unwrap_or_default();
    let current = crate::upstream_update::behind_target_for_host(&app, "herdr", &host).await;
    let conn = app.hosts.get(&host).await.ok_or_else(|| LcError::NotFound("host".into()))?;
    let ops = Arc::new(Real::new(conn));
    let v = start(&app, &headers, &host, b.target_version.as_deref(), current, ops, Timing::default()).await?;
    Ok((StatusCode::ACCEPTED, Json(v)).into_response())
}

/// 檢查、佔位、背景執行；`current_target` 是上游快照裡這台該升到的版本（呼叫端讀，測試直接給）。
pub async fn start(
    app: &Arc<App>,
    headers: &HeaderMap,
    host: &str,
    target: Option<&str>,
    current_target: Option<String>,
    ops: Arc<dyn Ops>,
    timing: Timing,
) -> Result<Value, LcError> {
    if headers.contains_key("X-AM-Bot-Id") || headers.contains_key("X-AM-Bot-Token") {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "ui_only",
            "message": "herdr 更新會中斷所有 bot，只給使用者在 UI 上按"})));
    }
    let Some(target) = target.and_then(|t| version_string(t.trim())) else {
        return Err(LcError::Bad("要帶 target_version（確認框寫的那一版，例如 0.9.3）".into()));
    };
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::NotFound("host".into()));
    }
    if crate::shared_host::is_shared(app, host).await {
        return Err(LcError::conflict("shared_session", json!({"host": host,
            "manual_upgrade_command": manual_upgrade_command(),
            "message": format!("{host} 的 herdr session 也是別顆 daemon 在用的；確認沒有別的 daemon 後，到主機設定關閉共用 session 再試")})));
    }
    if current_target.as_deref().and_then(parse_version) != parse_version(&target) {
        let message = match &current_target {
            Some(t) => format!("{host} 的 herdr 現在要升的是 {t}，不是確認框寫的 {target}；重新開確認框再按一次"),
            None => format!("{host} 的 herdr 已經沒有等著升級的新版（可能剛升好了），這一下沒有動作"),
        };
        return Err(LcError::conflict("stale_target",
            json!({"host": host, "target_version": target, "current_target": current_target, "message": message})));
    }
    let plan = affected(app, host).await.map_err(|e| LcError::Upstream(format!("讀不到 bot 狀態，沒有開始：{e:#}")))?;
    let update_id = crate::db::ulid();
    let session = session_for_response(app, host).await;
    let progress = Progress {
        update_id: update_id.clone(),
        host: host.to_string(),
        target_version: target.clone(),
        phase: "downloading".into(),
        started_at: crate::db::now(),
        session: session.clone(),
        manual_upgrade_command: manual_upgrade_command().into(),
    };
    {
        let mut g = running().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cur) = g.get(&app.data_dir) {
            return Err(LcError::conflict("herdr_update_in_progress", json!({"update_id": cur.update_id, "host": cur.host,
                "message": format!("herdr 已經在更新（{}），這一下沒有再開一次", cur.phase)})));
        }
        g.insert(app.data_dir.clone(), progress);
    }
    // 先佔自己的位、再看對方（cli_update 反過來：先寫列、再看這邊）：兩邊同時開的最壞結果是互相退讓，不會兩個都跑。
    // herdr server 一重啟，CLI 安裝後 scoped 重啟正在開的 pane 全部消失。
    match sqlx::query_as::<_, (String, String)>("SELECT id, kind FROM cli_updates WHERE host = ? AND status = 'running'")
        .bind(host)
        .fetch_optional(&app.db)
        .await
    {
        Ok(None) => {}
        Ok(Some((id, kind))) => {
            release_slot(app, &update_id);
            return Err(LcError::conflict("cli_update_in_progress", json!({"update_id": id, "host": host, "kind": kind,
                "message": format!("{host} 正在安裝 {kind}（安裝完會重啟 bot），先等它結束再更新 herdr")})));
        }
        Err(e) => {
            release_slot(app, &update_id);
            return Err(LcError::Upstream(format!("讀不到 CLI 安裝紀錄，沒有開始：{e}")));
        }
    }
    let ctx = Ctx { update_id: update_id.clone(), host: host.to_string(), session: session.clone(), target: target.clone() };
    let app2 = app.clone();
    tokio::spawn(Box::pin(async move {
        let done = run(&app2, ops.as_ref(), timing, &ctx).await;
        release_slot(&app2, &ctx.update_id);
        app2.emit("herdr_update_done", done).await;
    }));
    Ok(json!({
        "update_id": update_id,
        "host": host,
        "target_version": target,
        "started": true,
        "phase": "downloading",
        "session": session,
        "manual_upgrade_command": manual_upgrade_command(),
        "will_resume": plan.resume,
        "children_lost": plan.children,
    }))
}

// ---------------------------------------------------------------- 背景流程

struct Ctx {
    update_id: String,
    host: String,
    session: String,
    target: String,
}

#[derive(Default)]
struct Outcome {
    failed_phase: Option<String>,
    from: Option<String>,
    reason: Option<&'static str>,
    detail: Option<String>,
    resumed: Vec<Value>,
    failed: Vec<Value>,
    children_lost: Vec<LostChild>,
    /// 這台上要換的每一份 herdr（server 跑的那份、CLI 那份；同一檔只列一次）：`role`／`path`／`version`／`stale`。
    binaries: Vec<Value>,
}

/// 一份要處理的 herdr binary。
struct Binary {
    role: &'static str,
    path: PathBuf,
    version: Option<String>,
}

impl Binary {
    fn needs_swap(&self, target: &str) -> bool {
        self.version.as_deref().and_then(parse_version) != parse_version(target)
    }
    fn json(&self, target: &str) -> Value {
        json!({"role": self.role, "path": self.path.to_string_lossy(), "version": self.version, "stale": self.needs_swap(target)})
    }
    fn label(&self) -> String {
        format!("{} {}（{}）", self.role_text(), self.path.display(), self.version.as_deref().unwrap_or("讀不到版本"))
    }
    fn role_text(&self) -> &'static str {
        if self.role == "server" { "server 跑的" } else { "CLI／shim 解析到的" }
    }
}

impl Outcome {
    fn fail(mut self, reason: &'static str, detail: impl Into<String>) -> Self {
        self.reason = Some(reason);
        self.detail = Some(detail.into());
        self
    }
}

fn log_line(app: &impl crate::capabilities::DataDir, ctx: &Ctx, phase: &str, msg: &str) {
    tracing::info!(update = %ctx.update_id, phase, "herdr update: {msg}");
    let line = format!("{} {} {} {} {}\n", crate::db::now(), ctx.update_id, ctx.target, phase, msg.replace('\n', " "));
    let path = app.data_dir().join(LOG_FILE);
    if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| f.write_all(line.as_bytes())) {
        tracing::warn!(path = %path.display(), error = %e, "cannot append to herdr-update.log");
    }
}

async fn phase(app: &(impl crate::capabilities::DataDir + crate::capabilities::Db + crate::capabilities::Emit), ctx: &Ctx, phase: &str, msg: &str) {
    set_phase(app, &ctx.update_id, phase);
    log_line(app, ctx, phase, msg);
    app.emit("herdr_update_progress", json!({"update_id": ctx.update_id, "host": ctx.host, "target_version": ctx.target, "phase": phase,
        "session": ctx.session, "manual_upgrade_command": manual_upgrade_command()}))
        .await;
}

async fn run(app: &Arc<App>, ops: &dyn Ops, timing: Timing, ctx: &Ctx) -> Value {
    let mut out = steps(app, ops, timing, ctx).await;
    if out.reason.is_some() && out.failed_phase.is_none() {
        out.failed_phase = running_list(app).first().map(|p| p.phase.clone());
    }
    // staging 只在 steps 裡用：成功時已經複製進安裝位置，失敗時下次會重新下載、重新驗。不清的話每一版永久留一顆（#1048）。
    if let Err(e) = ops.cleanup(app, &ctx.target).await {
        log_line(app, ctx, "cleanup", &e);
    }
    let ok = out.reason.is_none();
    let msg = match (&out.reason, &out.detail) {
        (None, _) => format!("ok：resumed {}，failed {}，children_lost {}", out.resumed.len(), out.failed.len(), out.children_lost.len()),
        (Some(r), d) => format!("{r}：{}", d.as_deref().unwrap_or("")),
    };
    log_line(app, ctx, "done", &msg);
    if ok || out.reason == Some("resume_failed") {
        crate::upstream_update::note_installed(app, "herdr", &ctx.host, &ctx.target).await;
    }
    // 版本快取（hosts[].herdr）不等下一輪 60 秒巡邏。測試不跑：它會去執行本機真的 `herdr --version`。
    if !cfg!(test) {
        crate::runners::herdr_version::refresh(app, &ctx.host).await;
    }
    let mut v = json!({
        "update_id": ctx.update_id, "host": ctx.host, "ok": ok, "from": out.from, "to": ctx.target,
        "resumed": out.resumed, "failed": out.failed, "children_lost": out.children_lost,
        "phase": "done", "failed_phase": out.failed_phase, "session": ctx.session,
        "manual_upgrade_command": manual_upgrade_command(), "binaries": out.binaries,
    });
    if let Some(r) = out.reason {
        v["reason"] = json!(r);
        v["detail"] = json!(out.detail);
    }
    v
}

async fn steps(app: &Arc<App>, ops: &dyn Ops, timing: Timing, ctx: &Ctx) -> Outcome {
    let mut out = Outcome::default();
    // 1. 下載到 staging、驗版本；現行 binary 不動。
    phase(app, ctx, "downloading", "下載到 staging").await;
    let install = match ops.install_path(&ctx.session).await {
        Ok(p) => p,
        Err(e) => return out.fail("install_path_unsupported", e),
    };
    // 同機常有兩份 herdr（launchd 跑的與 login shell／shim 解析到的是不同檔）：兩份都看、都換，不然 server／CLI 會留下版本不一致。
    let cli = match ops.cli_path(&install).await {
        Ok(p) => p,
        Err(e) => return out.fail("install_path_unsupported", e),
    };
    if let Err(e) = ops.preflight_restart(&ctx.session, &install).await {
        return out.fail("restart_unsupported", format!("{e}；什麼都沒動，請自己重啟 herdr server"));
    }
    out.from = ops.binary_version(&install).await.ok();
    let mut binaries = vec![Binary { role: "server", path: install.clone(), version: out.from.clone() }];
    if let Some(path) = cli {
        let version = ops.binary_version(&path).await.ok();
        binaries.push(Binary { role: "cli", path, version });
    }
    out.binaries = binaries.iter().map(|b| b.json(&ctx.target)).collect();
    let staging = match ops.staging(app, &ctx.target).await {
        Ok(p) => p,
        Err(e) => return out.fail("download_failed", e),
    };
    if let Err(e) = ops.download(&ctx.target, &staging).await {
        return out.fail("download_failed", e);
    }
    if let Err(e) = ops.verify_download(&ctx.target, &staging).await {
        return out.fail("checksum_mismatch", format!("下載的 binary 沒通過校驗（沒有執行它）：{e}；現行 binary 沒動"));
    }
    match ops.binary_version(&staging).await {
        Ok(v) if parse_version(&v) == parse_version(&ctx.target) => {
            log_line(app, ctx, "downloading", &format!("staging {} = {v}；現行 {}", staging.display(), binaries.iter().map(Binary::label).collect::<Vec<_>>().join("；")));
        }
        got => {
            return out.fail("version_mismatch", format!("下載的 binary 回報 {got:?}，不是 {}；現行 binary 沒動", ctx.target));
        }
    }

    // 2–3. 等閒置 → 開維護窗口 → 窗口裡再看一次（開窗口之前剛好有人送出一則就回去等）。
    phase(app, ctx, "waiting_idle", "等頂層 bot 都閒置").await;
    let deadline = Instant::now() + timing.idle_max;
    let (window, plan) = loop {
        match affected(app, &ctx.host).await {
            Ok(a) if a.busy.is_empty() => {
                match crate::runners::herdr_maintenance::open_as(app, crate::herdr_maintenance::MAX_MINUTES, "herdr_update", &format!("herdr 一鍵更新 → {}", ctx.target)).await {
                    Ok(Some(w)) => {
                        // 先關閘門再複查（issue #893）：複查之後到重啟之間不會再有新派送打進來；複查前剛送進去的，複查看得到。
                        set_phase(app, &ctx.update_id, "stopping");
                        match affected(app, &ctx.host).await {
                            Ok(a) if a.busy.is_empty() => break (w, a),
                            recheck => {
                                set_phase(app, &ctx.update_id, "waiting_idle");
                                log_line(app, ctx, "waiting_idle", &format!("開窗口時又忙起來了：{:?}", recheck.map(|a| a.busy)));
                                let _ = crate::runners::herdr_maintenance::close_as(app, &w, "herdr_update", Some("又忙起來了，晚點再試")).await;
                            }
                        }
                    }
                    Ok(None) => return out.fail("maintenance_busy", "已經有別人開著 herdr 維護窗口；什麼都沒動"),
                    Err(e) => return out.fail("maintenance_busy", format!("開不了 herdr 維護窗口：{e:#}；什麼都沒動")),
                }
            }
            Ok(a) => {
                if Instant::now() >= deadline {
                    return out.fail("busy_timeout", format!("等了 {:?} 仍有 bot 在忙：{}；什麼都沒動", timing.idle_max, a.busy.join("、")));
                }
            }
            Err(e) => tracing::warn!(error = ?e, "herdr update: cannot read bot states; retrying"),
        }
        if Instant::now() >= deadline {
            return out.fail("busy_timeout", format!("等了 {:?} 仍沒等到全部閒置；什麼都沒動", timing.idle_max));
        }
        tokio::time::sleep(timing.idle_poll).await;
    };
    out.children_lost = plan.children.clone();
    let names: Vec<&str> = plan.resume.iter().map(|r| r.name.as_str()).collect();
    phase(app, ctx, "stopping", &format!("窗口 {} 開了；要接回 {:?}，子 agent 會沒了 {}", window.opened_at, names, plan.children.len())).await;

    // 4. 換 binary → 重啟。
    // 主機側先備份，再複製到同目錄、原子 rename；傳輸與檔案操作都經 Ops。已經是目標版的那份不動。
    let mut swapped: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut swap_error = None;
    for bin in binaries.iter().filter(|b| b.needs_swap(&ctx.target)) {
        let from = bin.version.clone().unwrap_or_else(|| "unknown".into());
        match ops.swap(&staging, &bin.path, &from).await {
            Ok(backup) => swapped.push((bin.path.clone(), backup)),
            Err(e) if ops.swap_outcome_uncertain() => {
                // SSH may lose its acknowledgement after rename. Treat the install as changed and restore the known backup.
                let name = bin.path.file_name().and_then(|n| n.to_str()).unwrap_or("herdr");
                swapped.push((bin.path.clone(), bin.path.with_file_name(format!("{name}.bak-{from}"))));
                swap_error = Some(e);
                break;
            }
            Err(e) => {
                // 前面已經換掉的換回去（server 還沒動過，不必重啟）。
                let undo = restore_all(ops, &swapped).await;
                let _ = crate::runners::herdr_maintenance::close_as(app, &window, "herdr_update", Some("換 binary 失敗，沒有重啟")).await;
                let tail = match undo {
                    Ok(()) if swapped.is_empty() => String::new(),
                    Ok(()) => "；已換的那份換回舊版".into(),
                    Err(e2) => format!("；前面已換的那份換回舊版也失敗：{e2}（備份在 {}）", swapped.iter().map(|(_, b)| b.display().to_string()).collect::<Vec<_>>().join("、")),
                };
                return out.fail("swap_failed", format!("{}：{e}；server 沒有重啟{tail}", bin.label()));
            }
        }
    }
    let swapped_text = swapped.iter().map(|(i, b)| format!("{}（舊的在 {}）", i.display(), b.display())).collect::<Vec<_>>().join("、");
    phase(app, ctx, "restarting", &format!("{} 換成 {}，重啟 herdr@{}", if swapped.is_empty() { "沒有檔案需要換".to_string() } else { swapped_text }, ctx.target, ctx.session)).await;
    let restarted = if let Some(e) = &swap_error {
        Err(e.clone())
    } else {
        match ops.restart_server(&ctx.session).await {
            Ok(()) => wait_server(app, &ctx.host, &ctx.target, timing).await,
            Err(e) => Err(e),
        }
    };
    // 5. 失敗就換回 `.bak` 再重啟一次；bot 照樣接回（舊版也比沒有 bot 好）。
    if let Err(e) = restarted {
        out.reason = Some(if swap_error.is_some() { "swap_failed" } else { "restart_failed" });
        let backups = swapped.iter().map(|(_, b)| b.display().to_string()).collect::<Vec<_>>().join("、");
        phase(app, ctx, "rolling_back", &format!("新版沒起來：{e}；換回 {backups}")).await;
        out.failed_phase = Some(if swap_error.is_some() { "stopping" } else { "restarting" }.into());
        let rolled_back = restore_all(ops, &swapped).await;
        let rolled = match rolled_back {
            Ok(()) => match ops.restart_server(&ctx.session).await {
                Ok(()) => match out.from.as_deref() {
                    Some(from) => wait_server(app, &ctx.host, from, timing).await,
                    None => wait_server_any(app, &ctx.host, timing).await,
                },
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        out.detail = Some(match rolled {
            Ok(v) => format!("新版沒起來（{e}）；已換回 {v}"),
            Err(e2) => {
                out.reason = Some("rollback_failed");
                out.failed_phase = Some("rolling_back".into());
                format!("新版沒起來（{e}）；換回舊版也沒起來（{e2}），要人工處理 {backups}")
            },
        });
    }

    // 6. 接回頂層 bot；子 agent 通知母 bot。
    phase(app, ctx, "resuming", &format!("接回 {} 顆", plan.resume.len())).await;
    for r in &plan.resume {
        let resumed = if out.reason == Some("rollback_failed") {
            Err("server 未通過回滾後 ping，沒有嘗試 resume".into())
        } else {
            ops.resume(app, &r.bot_id).await
        };
        match resumed {
            Ok(run_id) => {
                log_line(app, ctx, "resuming", &format!("{} → {run_id}", r.name));
                out.resumed.push(json!({"bot_id": r.bot_id, "name": r.name, "run_id": run_id}));
            }
            Err(e) => {
                log_line(app, ctx, "resuming", &format!("{} 失敗：{e}", r.name));
                out.failed.push(json!({"bot_id": r.bot_id, "name": r.name, "error": e}));
            }
        }
    }
    if !out.failed.is_empty() && out.reason.is_none() {
        out.reason = Some("resume_failed");
        out.failed_phase = Some("resuming".into());
        out.detail = Some("binary 已更新，但部分頂層 bot 接回失敗，請檢查 failed".into());
    }
    for (parent, kids) in group_by_parent(&plan.children) {
        let mut text = parent_notice(&ctx.target, out.reason.is_none() || out.reason == Some("resume_failed"), &kids);
        if out.reason == Some("rollback_failed") {
            text = format!("[系統] herdr 更新與回滾失敗，請先檢查主機 {} 的 server 與 binary。\n失去的子 agent：\n{}", ctx.host,
                kids.iter().map(|k| format!("- {}：{}", k.name, k.title.as_deref().unwrap_or("（沒有任務標題）"))).collect::<Vec<_>>().join("\n"));
        }
        let request_id = format!("herdr-update-{}-{parent}", ctx.update_id);
        if let Err(e) = ops.notify_parent(app, &parent, &text, &request_id).await {
            log_line(app, ctx, "resuming", &format!("通知母 bot {parent} 失敗：{e}"));
        }
    }

    // 7. 關窗口（沒接回的子 agent 由它照原規則退休）。
    match crate::runners::herdr_maintenance::close_as(app, &window, "herdr_update", Some("herdr 一鍵更新結束")).await {
        Ok(retired) => log_line(app, ctx, "resuming", &format!("維護窗口關了；退休的子 agent：{retired:?}")),
        Err(e) => log_line(app, ctx, "resuming", &format!("關維護窗口失敗（{e:#}），會在截止時間自己結束")),
    }
    out
}

/// 把 `(安裝位置, 備份)` 全部換回（後換的先換回）；任何一份失敗都繼續換其餘的，回第一個錯誤。
async fn restore_all(ops: &dyn Ops, swapped: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    let mut first_error = None;
    for (install, backup) in swapped.iter().rev() {
        if let Err(e) = ops.rollback(backup, install).await {
            first_error.get_or_insert(e);
        }
    }
    first_error.map_or(Ok(()), Err)
}

/// 等目標主機的 herdr ping 回來且版本＝`expected`，回讀到的版本。
async fn wait_server(app: &impl crate::hosts::HostsAccess, host: &str, expected: &str, timing: Timing) -> Result<String, String> {
    let deadline = Instant::now() + timing.restart_max;
    loop {
        let last = match ping_version(app, host).await {
            // 版本對了還要 daemon 認得它的 protocol：不認得的話之後每個 RPC 都會壞，bot 一顆也接不回，不如現在換回舊版。
            Ok((v, protocol)) if parse_version(&v) == parse_version(expected) && crate::herdr::protocol_supported(protocol) => return Ok(v),
            Ok((v, protocol)) if parse_version(&v) == parse_version(expected) => {
                format!("server 是 {v}，但 protocol {protocol} 不是這顆 daemon 認得的（{:?}）", crate::herdr::SUPPORTED_PROTOCOLS)
            }
            Ok((v, _)) => format!("server 回報 {v}，不是 {expected}"),
            Err(e) => e,
        };
        if Instant::now() >= deadline {
            return Err(format!("{:?} 內沒等到 {expected}：{last}", timing.restart_max));
        }
        tokio::time::sleep(timing.restart_poll).await;
    }
}

/// 舊版讀不到版本時，換回之後只要 server 回來就算。
async fn wait_server_any(app: &impl crate::hosts::HostsAccess, host: &str, timing: Timing) -> Result<String, String> {
    let deadline = Instant::now() + timing.restart_max;
    loop {
        match ping_version(app, host).await {
            Ok((v, _)) => return Ok(v),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(timing.restart_poll).await,
        }
    }
}

async fn ping_version(app: &impl crate::hosts::HostsAccess, host: &str) -> Result<(String, u32), String> {
    let conn = app.hosts().get(host).await.ok_or_else(|| format!("沒有 {host} 主機"))?;
    let pong = conn.client.ping().await.map_err(|e| format!("ping：{e:#}"))?;
    let protocol = pong.protocol;
    Ok((cli_version_string(&pong.version).or_else(|| version_string(&pong.version)).unwrap_or(pong.version), protocol))
}

/// staging → 現行位置：先把新檔複製到同一個目錄（rename 才原子），舊的留 `herdr.bak-<舊版>`（硬連結，不行就複製），
/// 再 rename 蓋過去——途中任何時刻現行路徑上都有一個完整的 binary。
fn swap_in(staging: &Path, install: &Path, from: &str) -> Result<PathBuf, String> {
    let dir = install.parent().ok_or_else(|| format!("{} 沒有上層目錄", install.display()))?;
    let name = install.file_name().and_then(|n| n.to_str()).unwrap_or("herdr");
    let backup = dir.join(format!("{name}.bak-{from}"));
    let fresh = dir.join(format!(".{name}.new-{}", crate::db::ulid()));
    let _ = std::fs::remove_file(&backup);
    if std::fs::hard_link(install, &backup).is_err() {
        std::fs::copy(install, &backup).map_err(|e| format!("備份 {} 失敗：{e}", backup.display()))?;
    }
    std::fs::copy(staging, &fresh).map_err(|e| format!("複製 staging 到 {} 失敗：{e}", fresh.display()))?;
    set_exec(&fresh)?;
    if let Err(e) = std::fs::rename(&fresh, install) {
        let _ = std::fs::remove_file(&fresh);
        return Err(format!("換上 {} 失敗：{e}", install.display()));
    }
    Ok(backup)
}

/// `.bak` → 現行位置（同樣先複製再 rename，`.bak` 留著）。
fn restore(backup: &Path, install: &Path) -> Result<(), String> {
    let dir = install.parent().ok_or_else(|| format!("{} 沒有上層目錄", install.display()))?;
    let fresh = dir.join(format!(".herdr.rollback-{}", crate::db::ulid()));
    std::fs::copy(backup, &fresh).map_err(|e| format!("複製 {} 失敗：{e}", backup.display()))?;
    set_exec(&fresh)?;
    std::fs::rename(&fresh, install).map_err(|e| {
        let _ = std::fs::remove_file(&fresh);
        format!("換回 {} 失敗：{e}", install.display())
    })
}

fn set_exec(p: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).map_err(|e| format!("chmod {}: {e}", p.display()))
}

fn group_by_parent(kids: &[LostChild]) -> Vec<(String, Vec<LostChild>)> {
    let mut out: Vec<(String, Vec<LostChild>)> = Vec::new();
    for k in kids {
        let Some(p) = k.parent_bot_id.clone() else { continue };
        match out.iter_mut().find(|(id, _)| *id == p) {
            Some((_, v)) => v.push(k.clone()),
            None => out.push((p, vec![k.clone()])),
        }
    }
    out
}

pub fn parent_notice(target: &str, ok: bool, kids: &[LostChild]) -> String {
    let head = if ok {
        format!("[系統] herdr 已更新到 {target}，herdr server 重啟過一次；你已用原對話接回。")
    } else {
        format!("[系統] herdr 更新到 {target} 沒成功、已換回舊版，herdr server 重啟過；你已用原對話接回。")
    };
    let mut s = format!("{head}\n你開的子 agent 都隨 pane 一起結束了（daemon 無法替你重建）：\n");
    for k in kids {
        s.push_str(&format!("- {}：{}\n", k.name, k.title.as_deref().filter(|t| !t.trim().is_empty()).unwrap_or("（沒有任務標題）")));
    }
    s.push_str("請檢查它們的 worktree／分支，視需要用 herdr 重開（可 fork 原 session 接續）。");
    s
}

// ---------------------------------------------------------------- 真的實作

#[path = "herdr_upgrade/real.rs"]
mod real;
use real::Real;

fn manual_upgrade_command() -> &'static str {
    // Homebrew 的 formula 常被 `brew pin` 住（pinned 時 `brew upgrade` 直接拒絕）：先 unpin、升完照原樣 pin 回去。
    // Homebrew 與 `~/.local/bin` 是兩份獨立的檔：兩份都在就兩份都升。
    "if command -v brew >/dev/null 2>&1 && brew list --versions herdr >/dev/null 2>&1; then if brew list --pinned 2>/dev/null | grep -qx herdr; then brew unpin herdr; brew upgrade herdr; brew pin herdr; else brew upgrade herdr; fi; if [ -f \"$HOME/.local/bin/herdr\" ] && [ ! -L \"$HOME/.local/bin/herdr\" ]; then curl -fsSL https://herdr.dev/install.sh | sh; fi; else curl -fsSL https://herdr.dev/install.sh | sh; fi"
}

async fn session_for_response(app: &Arc<App>, host: &str) -> String {
    match app.hosts.get(host).await.and_then(|c| c.cfg.clone()) {
        Some(cfg) => cfg.herdr_session,
        None => app.cfg.get().await.server.herdr_session.clone(),
    }
}

/// 下載 release asset 的 curl 參數：只准 https（含被導向的下一跳），不接受降到 http／其他協定。
fn curl_args(url: &str, part: &Path) -> Vec<String> {
    let mut a: Vec<String> = ["-fsSL", "--proto", "=https", "--proto-redir", "=https", "--retry", "2", "--max-time", "300", "-o"].map(String::from).to_vec();
    a.push(part.display().to_string());
    a.push(url.to_string());
    a
}

/// GitHub release JSON 裡 `asset` 的 `digest: "sha256:<hex>"`（小寫）。找不到 asset、沒有 digest、不是 sha256 都是 `Err`。
fn published_digest(body: &str, asset: &str) -> Result<String, String> {
    let v: Value = serde_json::from_str(body).map_err(|e| format!("release API 回的不是 JSON：{e}"))?;
    let entry = v["assets"].as_array().and_then(|a| a.iter().find(|x| x["name"] == asset)).ok_or_else(|| format!("release 裡沒有 {asset}"))?;
    let digest = entry["digest"].as_str().ok_or_else(|| format!("{asset} 沒有公布 digest"))?;
    digest
        .strip_prefix("sha256:")
        .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| format!("{asset} 的 digest 不是 sha256：{digest}"))
}

#[cfg(test)]
fn sha256_hex(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{:x}", h.finalize()))
}

/// systemd 實例名／launchd label 要原樣可用（同 `herdr_unit`）。
fn session_ok(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// launchd `program` must resolve to the file being replaced, otherwise kickstart reloads another binary.
#[cfg(test)]
fn ensure_launchd_program(output: &str, install: &Path) -> Result<(), String> {
    let program = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("program = ").map(str::trim))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "launchd did not report a program path".to_string())?;
    let launched = std::fs::canonicalize(program).map_err(|e| format!("resolve launchd program {program}: {e}"))?;
    let installing = std::fs::canonicalize(install).map_err(|e| format!("resolve updater binary {}: {e}", install.display()))?;
    if launched == installing {
        Ok(())
    } else {
        Err(format!(
            "launchd runs {}, but updater would replace {}; refusing to restart a different binary",
            launched.display(), installing.display()
        ))
    }
}

async fn command_output(mut cmd: tokio::process::Command, timeout: Duration) -> Result<String, String> {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    // 驗的常常是 daemon 自己剛複製／換進去的 binary：同一個行程裡別的任務在寫入 fd 還開著時 fork，exec 會回 `ETXTBSY`（#189），
    // 這不是 binary 壞了，重試就好；其他錯誤照舊。
    let out = tokio::time::timeout(timeout, crate::exec_retry::output_async(&mut cmd))
        .await
        .map_err(|_| format!("超過 {timeout:?}"))?
        .map_err(|e| e.to_string())?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(format!("rc={:?} {}", out.status.code(), text.trim()))
    }
}

#[cfg(test)]
mod tests {
    /// 控制組（不依賴 kernel 的 ETXTBSY）：binary 寫完、fd 已關，版本檢查一次就讀到版本。
    #[tokio::test]
    async fn the_version_check_reads_a_binary_that_is_already_written() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::testing::scratch_dir("am-herdr-plain");
        let bin = dir.join("herdr");
        let mut f = std::fs::File::create(&bin).unwrap();
        f.write_all(b"#!/bin/sh\necho 'herdr 0.9.3'\n").unwrap();
        drop(f);
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let got = Real::local().binary_version(&bin).await;
        assert_eq!(got.as_deref(), Ok("0.9.3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 驗版本用的 `--version` 撞上 `ETXTBSY`（binary 剛複製進來、寫入 fd 還被別的 fork 出來的子行程握著）不能算「binary 壞了」：
    /// 重試到 exec 得動為止。只在 Linux 跑：macOS 寫入 fd 還開著時 exec 不回 ETXTBSY，前提不成立；控制組見上面。
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn the_version_check_waits_out_a_binary_that_is_still_busy_being_written() {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = crate::testing::scratch_dir("am-herdr-etxt");
        let bin = dir.join("herdr");
        let mut f = std::fs::File::create(&bin).unwrap();
        f.write_all(b"#!/bin/sh\necho 'herdr 0.9.3'\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(crate::exec_retry::is_text_busy(&std::process::Command::new(&bin).output().unwrap_err()), "前提：寫入 fd 開著時 exec 回 ETXTBSY");
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(80));
            drop(f);
        });
        let got = Real::local().binary_version(&bin).await;
        release.join().unwrap();
        assert_eq!(got.as_deref(), Ok("0.9.3"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use crate::testing as tt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn fast() -> Timing {
        Timing {
            idle_max: Duration::from_millis(400),
            idle_poll: Duration::from_millis(20),
            restart_max: Duration::from_millis(400),
            restart_poll: Duration::from_millis(20),
        }
    }

    /// 假的外部世界：binary 是寫著 `herdr X.Y.Z` 的純文字檔；「重啟」＝讓 mock herdr 的 ping 回報現行檔案寫的版本
    /// （就像 systemd 用現行路徑上的 binary 拉起 server）。
    struct Fake {
        version_probes: Mutex<Vec<PathBuf>>,
        fail_at: Mutex<Option<String>>,
        install: PathBuf,
        /// 另一份 herdr（login shell／shim 解析到的 CLI，跟 server 跑的不同檔）。
        cli: Mutex<Option<PathBuf>>,
        cli_err: Mutex<Option<String>>,
        /// 換這個路徑時失敗（模擬第二份換不上去）。
        fail_swap_path: Mutex<Option<PathBuf>>,
        download_as: String,
        /// 前 N 次重啟起來的不是現行檔案寫的版本（模擬新版起不來／起成別的）。
        bad_restarts: AtomicUsize,
        /// 下載的 binary 對不上 release 公布的 sha256。
        bad_checksum: AtomicUsize,
        /// 重啟前的檢查（server 不是 systemd／launchd 在管）失敗的原因。
        preflight_err: Mutex<Option<String>>,
        /// 新版 server 回報的 protocol（舊版 daemon 認不得的 protocol 就是起不來）。
        new_protocol: AtomicUsize,
        pong: Arc<std::sync::Mutex<(String, u32)>>,
        restarts: Mutex<Vec<String>>,
        resumed: Mutex<Vec<String>>,
        notified: Mutex<Vec<(String, String)>>,
        /// 重啟 server 的前一刻跑一次（模擬那一刻剛好進來的 prompt）。
        before_restart: Mutex<Option<BoxFuture<'static, ()>>>,
    }

    impl Fake {
        fn new(env: &tt::Env, installed: &str, download_as: &str) -> Arc<Fake> {
            let bin = env.dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let install = bin.join("herdr");
            std::fs::write(&install, format!("herdr {installed}\n")).unwrap();
            *env.herdr.pong.lock().unwrap() = (installed.to_string(), 22);
            Arc::new(Fake {
                version_probes: Mutex::new(vec![]),
                fail_at: Mutex::new(None),
                install,
                cli: Mutex::new(None),
                cli_err: Mutex::new(None),
                fail_swap_path: Mutex::new(None),
                download_as: download_as.into(),
                bad_restarts: AtomicUsize::new(0),
                bad_checksum: AtomicUsize::new(0),
                preflight_err: Mutex::new(None),
                new_protocol: AtomicUsize::new(22),
                pong: env.herdr.pong.clone(),
                restarts: Mutex::new(vec![]),
                resumed: Mutex::new(vec![]),
                notified: Mutex::new(vec![]),
                before_restart: Mutex::new(None),
            })
        }
        fn check(&self, stage: &str) -> Result<(), String> {
            if self.fail_at.lock().unwrap().as_deref() == Some(stage) { Err(format!("fake {stage} failed")) } else { Ok(()) }
        }
        fn installed(&self) -> String {
            std::fs::read_to_string(&self.install).unwrap().trim().to_string()
        }
        /// 另放一份 CLI（不同目錄、不同檔），回它的路徑。
        fn add_cli(&self, env: &tt::Env, version: &str) -> PathBuf {
            let dir = env.dir.join("cli");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("herdr");
            std::fs::write(&path, format!("herdr {version}\n")).unwrap();
            *self.cli.lock().unwrap() = Some(path.clone());
            path
        }
    }

    impl Ops for Fake {
        fn install_path<'a>(&'a self, _session: &'a str) -> BoxFuture<'a, Result<PathBuf, String>> {
            Box::pin(async move { self.check("install")?; Ok(self.install.clone()) })
        }
        fn cli_path<'a>(&'a self, _server: &'a Path) -> BoxFuture<'a, Result<Option<PathBuf>, String>> {
            Box::pin(async move {
                if let Some(e) = self.cli_err.lock().unwrap().clone() {
                    return Err(e);
                }
                Ok(self.cli.lock().unwrap().clone())
            })
        }
        fn staging<'a>(&'a self, app: &'a Arc<App>, version: &'a str) -> BoxFuture<'a, Result<PathBuf, String>> {
            Box::pin(async move {
                self.check("staging")?;
                let p = app.data_dir.join(STAGING_DIR).join(version).join("herdr");
                std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
                Ok(p)
            })
        }
        fn swap<'a>(&'a self, staging: &'a Path, install: &'a Path, from: &'a str) -> BoxFuture<'a, Result<PathBuf, String>> {
            Box::pin(async move {
                self.check("swap")?;
                if self.fail_swap_path.lock().unwrap().as_deref() == Some(install) {
                    return Err(format!("fake swap of {} failed", install.display()));
                }
                let backup = swap_in(staging, install, from)?;
                self.check("swap_ack")?;
                Ok(backup)
            })
        }
        fn swap_outcome_uncertain(&self) -> bool { self.fail_at.lock().unwrap().as_deref() == Some("swap_ack") }
        fn rollback<'a>(&'a self, backup: &'a Path, install: &'a Path) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move { self.check("rollback")?; restore(backup, install) })
        }
        fn download<'a>(&'a self, _v: &'a str, dest: &'a Path) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move { self.check("download")?; std::fs::write(dest, format!("herdr {}\n", self.download_as)).map_err(|e| e.to_string()) })
        }
        fn binary_version<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.version_probes.lock().unwrap().push(path.to_path_buf());
                let s = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
                cli_version_string(&s).ok_or(s)
            })
        }
        fn verify_download<'a>(&'a self, _v: &'a str, _path: &'a Path) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                if self.bad_checksum.load(Ordering::SeqCst) > 0 {
                    Err("sha256 不符".into())
                } else {
                    Ok(())
                }
            })
        }
        fn preflight_restart<'a>(&'a self, _session: &'a str, _install: &'a Path) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move { self.preflight_err.lock().unwrap().clone().map_or(Ok(()), Err) })
        }
        fn restart_server<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                let hook = self.before_restart.lock().unwrap().take();
                if let Some(hook) = hook {
                    hook.await;
                }
                self.restarts.lock().unwrap().push(session.to_string());
                self.check("restart")?;
                if self.fail_at.lock().unwrap().as_deref() == Some("restart_once") && self.restarts.lock().unwrap().len() == 1 {
                    return Err("fake restart command failed".into());
                }
                let v = if self.bad_restarts.load(Ordering::SeqCst) > 0 {
                    self.bad_restarts.fetch_sub(1, Ordering::SeqCst);
                    "0.0.1".to_string()
                } else {
                    cli_version_string(&self.installed()).unwrap()
                };
                // 只有「新版」那一次會帶新的 protocol；換回舊版就是正常的 22。
                let protocol = if v == "0.9.3" { self.new_protocol.load(Ordering::SeqCst) as u32 } else { 22 };
                *self.pong.lock().unwrap() = (v, protocol);
                Ok(())
            })
        }
        fn resume<'a>(&'a self, _app: &'a Arc<App>, bot_id: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                self.check("resume")?;
                self.resumed.lock().unwrap().push(bot_id.to_string());
                Ok(format!("run-{bot_id}"))
            })
        }
        fn notify_parent<'a>(&'a self, _app: &'a Arc<App>, parent: &'a str, text: &'a str, _rid: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.notified.lock().unwrap().push((parent.to_string(), text.to_string()));
                Ok(())
            })
        }
    }

    async fn child_of(env: &tt::Env, parent: &str, name: &str, title: &str) -> String {
        let id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, parent_bot_id, created_at)
             VALUES (?,?,?,'claude','[]',0,1,'tok','child',?,?)",
        )
        .bind(&id)
        .bind(&env.project_id)
        .bind(name)
        .bind(parent)
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let run = tt::fake_run(&env.app, &id).await;
        sqlx::query("UPDATE runs SET agent_title = ? WHERE id = ?").bind(title).bind(&run).execute(&env.app.db).await.unwrap();
        id
    }

    async fn top(env: &tt::Env, name: &str, status: &str) -> String {
        let b = tt::claude_bot(&env.app, &env.project_id, name).await;
        let run = tt::fake_run(&env.app, &b.id).await;
        sqlx::query("UPDATE runs SET agent_status = ? WHERE id = ?").bind(status).bind(&run).execute(&env.app.db).await.unwrap();
        b.id
    }

    async fn go(env: &tt::Env, fake: &Arc<Fake>, target: &str) -> (Value, Value) {
        let mut rx = env.app.subscribe();
        let accepted = start(&env.app, &HeaderMap::new(), "local", Some(target), Some(target.into()), fake.clone(), fast()).await.unwrap();
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if ev.kind == "herdr_update_done" {
                    return ev.data;
                }
            }
        })
        .await
        .expect("herdr_update_done");
        (accepted, done)
    }

    async fn window_open(app: &Arc<App>) -> bool {
        crate::runners::herdr_maintenance::active(app).await.unwrap().is_some()
    }

    #[tokio::test]
    async fn upgrades_restarts_resumes_and_tells_the_parent_about_lost_children() {
        let env = tt::env().await;
        sqlx::query("UPDATE projects SET label = ? WHERE id = ?")
            .bind("智選hub")
            .bind(&env.project_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let parent = top(&env, "boss", "idle").await;
        let kid = child_of(&env, &parent, "hub-dgs9j9-dev", "修 events_lost").await;
        let mut rx = env.app.subscribe();
        let (accepted, done) = go(&env, &fake, "0.9.3").await;

        assert_eq!(accepted["started"], true);
        assert_eq!(accepted["will_resume"], json!([{"bot_id": parent, "name": "boss"}]));
        assert_eq!(accepted["children_lost"], json!([{"bot_id": kid, "name": "dev", "parent_bot_id": parent}]));

        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["from"], "0.9.1");
        assert_eq!(done["to"], "0.9.3");
        assert_eq!(done["resumed"], json!([{"bot_id": parent, "name": "boss", "run_id": format!("run-{parent}")}]));
        assert_eq!(done["failed"], json!([]));
        assert_eq!(done["children_lost"][0]["name"], "dev");
        let stored_name: String = sqlx::query_scalar("SELECT name FROM bots WHERE id = ?").bind(&kid).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(stored_name, "hub-dgs9j9-dev", "the API display projection does not rewrite the database row");
        assert!(done.get("reason").is_none());
        assert!(!env.app.data_dir.join(STAGING_DIR).join("0.9.3").exists(), "升完不留 staging");

        assert_eq!(fake.installed(), "herdr 0.9.3");
        assert_eq!(std::fs::read_to_string(fake.install.with_file_name("herdr.bak-0.9.1")).unwrap().trim(), "herdr 0.9.1");
        let session = env.app.cfg.get().await.server.herdr_session.clone();
        assert_eq!(*fake.restarts.lock().unwrap(), vec![session], "只重啟這顆 daemon 的 session，一次");
        assert_eq!(*fake.resumed.lock().unwrap(), vec![parent.clone()], "子 agent 不由 daemon 接回");
        let notes = fake.notified.lock().unwrap().clone();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, parent);
        assert!(notes[0].1.contains("dev") && notes[0].1.contains("修 events_lost"), "{}", notes[0].1);
        assert!(!window_open(&env.app).await, "結束時關窗口");
        assert!(running_list(&env.app).is_empty());

        let mut phases = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if ev.kind == "herdr_update_progress" {
                phases.push(ev.data["phase"].as_str().unwrap().to_string());
            }
        }
        assert_eq!(phases, ["downloading", "waiting_idle", "stopping", "restarting", "resuming"]);
        let log = std::fs::read_to_string(env.app.data_dir.join(LOG_FILE)).unwrap();
        for p in ["downloading", "waiting_idle", "stopping", "restarting", "resuming", "done"] {
            assert!(log.contains(&format!(" 0.9.3 {p} ")), "log 少了 {p}：\n{log}");
        }
    }

    /// issue #893：複查閒置之後、重啟 server 之前進來的 prompt 不能打進即將被砍的 pane——回 409 `herdr_updating`、
    /// 一個字都沒送（沒有 turn），更新結束之後閘門就開了。
    #[tokio::test]
    async fn a_prompt_arriving_right_before_the_restart_is_refused_not_lost() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let bot = top(&env, "boss", "idle").await;
        let seen: Arc<Mutex<Option<Result<(), LcError>>>> = Arc::new(Mutex::new(None));
        let gate_phase: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        {
            let (app, bot, seen, gate_phase) = (env.app.clone(), bot.clone(), seen.clone(), gate_phase.clone());
            *fake.before_restart.lock().unwrap() = Some(Box::pin(async move {
                *gate_phase.lock().unwrap() = gate(&app).map(|p| p.phase);
                let r = crate::lifecycle::prompt(&app, &bot, "late prompt", "crid-893").await;
                *seen.lock().unwrap() = Some(r.map(|_| ()));
            }));
        }
        let (_accepted, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["ok"], true, "{done}");

        assert_eq!(gate_phase.lock().unwrap().as_deref(), Some("restarting"), "重啟那一刻閘門是關的");
        let got = seen.lock().unwrap().take().expect("hook 跑了");
        match got {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v["reason"], "herdr_updating", "{v}");
                assert_eq!(v["sent"], false);
                assert_eq!(v["retryable"], true);
            }
            other => panic!("要回 409 herdr_updating，拿到 {other:?}"),
        }
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE client_request_id = 'crid-893'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(turns, 0, "被擋下來的 prompt 沒有留下 turn");

        assert!(gate(&env.app).is_none(), "更新結束，閘門開了");
        let after = crate::lifecycle::prompt(&env.app, &bot, "after", "crid-893b").await;
        assert!(!matches!(&after, Err(LcError::Conflict(v)) if v["reason"] == "herdr_updating"), "結束後不再被這道閘門擋：{after:?}");
    }

    /// 閘門只在要砍 server 的階段關：下載、等閒置、接回都不擋。
    #[tokio::test]
    async fn only_the_destructive_phases_close_the_gate() {
        let env = tt::env().await;
        let _held = hold_slot_for_test(&env.app);
        assert!(is_running(&env.app, "local"));
        assert!(!is_running(&env.app, "remote"));
        for (phase, closed) in [("downloading", false), ("waiting_idle", false), ("stopping", true), ("restarting", true), ("rolling_back", true), ("resuming", false)] {
            set_phase(&env.app, "held", phase);
            assert_eq!(gate(&env.app).is_some(), closed, "{phase}");
        }
    }

    #[tokio::test]
    async fn a_download_reporting_the_wrong_version_changes_nothing() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.2");
        let bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["ok"], false);
        assert_eq!(done["reason"], "version_mismatch", "{done}");
        assert_eq!(fake.installed(), "herdr 0.9.1", "現行 binary 沒動");
        assert!(!fake.install.with_file_name("herdr.bak-0.9.1").exists());
        assert!(fake.restarts.lock().unwrap().is_empty() && fake.resumed.lock().unwrap().is_empty());
        assert!(!window_open(&env.app).await);
        assert!(crate::db::active_run(&env.app.db, &bot).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_busy_bot_times_out_without_touching_anything() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        top(&env, "worker", "working").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["reason"], "busy_timeout", "{done}");
        assert!(done["detail"].as_str().unwrap().contains("worker (working)"), "{done}");
        assert_eq!(fake.installed(), "herdr 0.9.1");
        assert!(fake.restarts.lock().unwrap().is_empty());
        assert!(!window_open(&env.app).await);
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_notes WHERE kind = 'herdr_maintenance_start'")
            .fetch_one(&env.app.db)
            .await
            .unwrap();
        assert_eq!(notes, 0, "等不到閒置就連窗口都沒開");
        assert!(!env.app.data_dir.join(STAGING_DIR).join("0.9.3").exists(), "沒升成也不留 staging");
    }

    #[tokio::test]
    async fn waits_for_a_busy_bot_to_go_idle_and_reports_progress_in_state() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let bot = top(&env, "worker", "working").await;
        let slow = Timing { idle_max: Duration::from_secs(20), ..fast() };
        let mut rx = env.app.subscribe();
        start(&env.app, &HeaderMap::new(), "local", Some("0.9.3"), Some("0.9.3".into()), fake.clone(), slow).await.unwrap();
        assert!(tt::eventually!(running_list(&env.app).first().is_some_and(|p| p.phase == "waiting_idle")));
        let p = running_list(&env.app)[0].clone();
        assert_eq!((p.host.as_str(), p.target_version.as_str()), ("local", "0.9.3"));
        let again = start(&env.app, &HeaderMap::new(), "local", Some("0.9.3"), Some("0.9.3".into()), fake.clone(), slow).await.unwrap_err();
        assert!(matches!(&again, LcError::Conflict(v) if v["reason"] == "herdr_update_in_progress" && v["update_id"] == p.update_id.as_str()), "{again:?}");
        assert!(fake.restarts.lock().unwrap().is_empty(), "忙的時候不重啟");
        sqlx::query("UPDATE runs SET agent_status = 'idle' WHERE bot_id = ?").bind(&bot).execute(&env.app.db).await.unwrap();
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if ev.kind == "herdr_update_done" {
                    return ev.data;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(*fake.resumed.lock().unwrap(), vec![bot]);
    }

    fn text(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap().trim().to_string()
    }

    /// issue #1752：launchd 跑的與 login shell／shim 解析到的 CLI 是兩份不同的檔——兩份一起換（server 只重啟一次），
    /// 已經是目標版的那份不動；結果逐份列出路徑、版本與誰舊（畫面據此講清楚）。
    #[tokio::test]
    async fn the_server_binary_and_a_separate_cli_binary_are_both_replaced_and_reported() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let cli = fake.add_cli(&env, "0.9.2");
        let _bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(text(&fake.install), "herdr 0.9.3");
        assert_eq!(text(&cli), "herdr 0.9.3", "CLI 那份也要換，不然 server／CLI 版本不一致");
        assert_eq!(text(&cli.with_file_name("herdr.bak-0.9.2")), "herdr 0.9.2");
        assert_eq!(fake.restarts.lock().unwrap().len(), 1, "server 只重啟一次");
        assert_eq!(
            done["binaries"],
            json!([
                {"role": "server", "path": fake.install.to_string_lossy(), "version": "0.9.1", "stale": true},
                {"role": "cli", "path": cli.to_string_lossy(), "version": "0.9.2", "stale": true},
            ]),
            "{done}"
        );
        assert!(!env.app.data_dir.join(STAGING_DIR).join("0.9.3").exists());
    }

    #[tokio::test]
    async fn a_cli_binary_that_is_already_the_target_is_left_alone() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let cli = fake.add_cli(&env, "0.9.3");
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["ok"], true, "{done}");
        assert!(!cli.with_file_name("herdr.bak-0.9.3").exists(), "已是目標版就不備份、不換");
        assert_eq!(done["binaries"][1]["stale"], false, "{done}");
        assert_eq!(text(&fake.install), "herdr 0.9.3");
    }

    /// 第二份換不上去：第一份已經換掉的要換回去，server 沒動過所以不重啟、什麼都沒壞。
    #[tokio::test]
    async fn a_failed_second_swap_restores_the_first_and_never_restarts() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let cli = fake.add_cli(&env, "0.9.2");
        *fake.fail_swap_path.lock().unwrap() = Some(cli.clone());
        let _bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!((done["ok"].clone(), done["reason"].clone()), (json!(false), json!("swap_failed")), "{done}");
        assert!(done["detail"].as_str().unwrap().contains(&cli.to_string_lossy().to_string()), "要講清楚是哪一份：{done}");
        assert_eq!(text(&fake.install), "herdr 0.9.1", "第一份換回去");
        assert_eq!(text(&cli), "herdr 0.9.2");
        assert!(fake.restarts.lock().unwrap().is_empty());
        assert!(!window_open(&env.app).await);
    }

    /// 新版 server 起不來：兩份都換回舊版，再重啟一次，bot 照樣接回。
    #[tokio::test]
    async fn a_failed_restart_rolls_back_both_binaries() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let cli = fake.add_cli(&env, "0.9.2");
        fake.bad_restarts.store(1, Ordering::SeqCst);
        let bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["reason"], "restart_failed", "{done}");
        assert_eq!(text(&fake.install), "herdr 0.9.1");
        assert_eq!(text(&cli), "herdr 0.9.2", "CLI 也要換回");
        assert_eq!(fake.restarts.lock().unwrap().len(), 2);
        assert_eq!(*fake.resumed.lock().unwrap(), vec![bot]);
    }

    /// CLI 那份是套件管理器的 symlink：什麼都不動（不碰 server 那份），reason 是 install_path_unsupported。
    #[tokio::test]
    async fn an_unswappable_cli_binary_refuses_before_anything_is_touched() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        *fake.cli_err.lock().unwrap() = Some("CLI／shim 解析到的 /opt/homebrew/bin/herdr is not a regular file (package manager install)".into());
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["reason"], "install_path_unsupported", "{done}");
        assert!(done["detail"].as_str().unwrap().contains("/opt/homebrew/bin/herdr"), "{done}");
        assert_eq!(text(&fake.install), "herdr 0.9.1");
        assert!(fake.restarts.lock().unwrap().is_empty() && !window_open(&env.app).await);
    }

    /// m4p 手動升級的教訓：Homebrew formula 被 `brew pin` 住時 `brew upgrade` 會直接拒絕，手動指令要 unpin → upgrade → pin。
    #[test]
    fn the_manual_command_unpins_upgrades_and_repins_a_homebrew_formula() {
        let cmd = manual_upgrade_command();
        let (unpin, upgrade, pin) = (cmd.find("brew unpin herdr"), cmd.find("brew upgrade herdr"), cmd.find("brew pin herdr"));
        assert!(unpin.is_some() && upgrade.is_some() && pin.is_some(), "{cmd}");
        assert!(unpin < upgrade && upgrade < pin, "順序要 unpin → upgrade → pin：{cmd}");
        assert!(cmd.contains("install.sh"), "沒有 Homebrew 時退到官方安裝腳本：{cmd}");
    }

    #[tokio::test]
    async fn a_server_that_comes_back_with_the_wrong_version_is_rolled_back_and_bots_still_resume() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        fake.bad_restarts.store(1, Ordering::SeqCst);
        let bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["ok"], false);
        assert_eq!(done["reason"], "restart_failed", "{done}");
        assert!(done["detail"].as_str().unwrap().contains("已換回 0.9.1"), "{done}");
        assert_eq!(fake.installed(), "herdr 0.9.1", "換回 .bak");
        assert_eq!(fake.restarts.lock().unwrap().len(), 2, "新版一次、換回後一次");
        assert_eq!(env.herdr.pong.lock().unwrap().0, "0.9.1");
        assert_eq!(*fake.resumed.lock().unwrap(), vec![bot], "失敗也要把 bot 接回");
        assert!(!window_open(&env.app).await);
        assert!(!env.app.data_dir.join(STAGING_DIR).join("0.9.3").exists(), "換回舊版也不留 staging（換回靠的是 .bak）");
    }

    /// 下載的 binary 對不上 release 公布的 sha256（被換掉、截斷）：在**執行它之前**就擋掉（驗 `--version` 就是執行它）。
    /// 現行 binary、server、維護窗口一個都沒碰。
    #[tokio::test]
    async fn a_download_that_does_not_match_the_published_checksum_is_never_run_or_installed() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        fake.bad_checksum.store(1, Ordering::SeqCst);
        let bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!((done["ok"].clone(), done["reason"].clone()), (json!(false), json!("checksum_mismatch")), "{done}");
        assert_eq!(fake.installed(), "herdr 0.9.1");
        assert_eq!(*fake.version_probes.lock().unwrap(), vec![fake.install.clone()], "校驗失敗不執行 staging --version");
        assert!(!fake.install.with_file_name("herdr.bak-0.9.1").exists());
        assert!(fake.restarts.lock().unwrap().is_empty() && fake.resumed.lock().unwrap().is_empty());
        assert!(!window_open(&env.app).await);
        assert!(!env.app.data_dir.join(STAGING_DIR).join("0.9.3").join("herdr").exists(), "不信任的檔案不留在 staging");
        assert!(crate::db::active_run(&env.app.db, &bot).await.unwrap().is_some());
    }

    /// server 不是 systemd／launchd 在管的（daemon 自己 spawn 的）：`systemctl restart` 會去**另外起**一顆搶 socket，
    /// 舊的還在跑舊版。換 binary 之前就要知道這條路走不通，而不是換完才失敗、再換回來。
    #[tokio::test]
    async fn a_server_nobody_can_restart_is_refused_before_anything_is_touched() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        *fake.preflight_err.lock().unwrap() = Some("herdr@agents-manager.service 不是 active".into());
        let bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["reason"], "restart_unsupported", "{done}");
        assert!(done["detail"].as_str().unwrap().contains("不是 active"), "{done}");
        assert_eq!(fake.installed(), "herdr 0.9.1");
        assert!(!fake.install.with_file_name("herdr.bak-0.9.1").exists(), "沒換就沒有備份");
        assert!(fake.restarts.lock().unwrap().is_empty() && fake.resumed.lock().unwrap().is_empty());
        assert!(!window_open(&env.app).await, "連維護窗口都不開");
        assert!(crate::db::active_run(&env.app.db, &bot).await.unwrap().is_some());
    }

    /// 新版 server 起來了、版本也對，但 protocol 是這顆 daemon 不認得的：之後每個 RPC 都會壞，bot 一顆也接不回。
    /// 當成沒起來，換回舊版。
    #[tokio::test]
    async fn a_new_server_speaking_an_unsupported_protocol_is_rolled_back() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        fake.new_protocol.store(99, Ordering::SeqCst);
        let bot = top(&env, "boss", "idle").await;
        let (_, done) = go(&env, &fake, "0.9.3").await;
        assert_eq!(done["reason"], "restart_failed", "{done}");
        assert!(done["detail"].as_str().unwrap().contains("protocol"), "{done}");
        assert!(done["detail"].as_str().unwrap().contains("已換回 0.9.1"), "{done}");
        assert_eq!(fake.installed(), "herdr 0.9.1");
        assert_eq!(fake.restarts.lock().unwrap().len(), 2);
        assert_eq!(*fake.resumed.lock().unwrap(), vec![bot]);
    }

    /// 同一台同時有 claude／codex CLI 安裝＋scoped 重啟在跑：herdr server 一重啟，它們正在重開的 pane 全部消失。
    #[tokio::test]
    async fn a_running_cli_install_on_this_host_blocks_a_herdr_update() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        sqlx::query(
            "INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at, host_target)
             VALUES ('cu1','local','claude','2.1.290','running','installing','b',?,?,'t')",
        )
        .bind(crate::db::now())
        .bind(crate::db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
        let err = start(&env.app, &HeaderMap::new(), "local", Some("0.9.3"), Some("0.9.3".into()), fake.clone(), fast()).await.unwrap_err();
        assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == "cli_update_in_progress" && v["update_id"] == "cu1"), "{err:?}");
        assert!(running_list(&env.app).is_empty(), "被擋的不佔位");
        assert!(fake.restarts.lock().unwrap().is_empty());
    }

    /// 下載用 https 且不接受被導到別的協定。
    #[test]
    fn the_download_only_speaks_https_even_through_redirects() {
        let args = curl_args("https://example.invalid/herdr", Path::new("/tmp/x.part"));
        let joined = args.join(" ");
        assert!(joined.contains("--proto =https") && joined.contains("--proto-redir =https"), "{joined}");
        assert!(args.contains(&"-f".to_string()) || joined.contains("-fsSL"), "{joined}");
        assert_eq!(args.last().map(String::as_str), Some("https://example.invalid/herdr"));
    }

    /// GitHub release API 每個 asset 都帶 `digest: "sha256:…"`；找不到那個 asset 或沒有 digest 都不能當成「驗過」。
    #[test]
    fn the_published_digest_comes_from_the_matching_asset_only() {
        let (up, low) = ("AB12".repeat(16), "ab12".repeat(16));
        let body = format!(r#"{{"assets":[{{"name":"herdr-linux-x86_64","digest":"sha256:{up}"}},{{"name":"herdr-macos-aarch64","digest":"sha256:{low}"}},{{"name":"herdr-linux-aarch64"}}]}}"#);
        let body = body.as_str();
        assert_eq!(published_digest(body, "herdr-linux-x86_64").as_deref(), Ok(low.as_str()), "大小寫統一成小寫");
        assert!(published_digest(body, "herdr-linux-aarch64").is_err(), "沒有 digest");
        assert!(published_digest(body, "herdr-windows").is_err(), "沒有這個 asset");
        assert!(published_digest("not json", "x").is_err());
        assert!(published_digest(r#"{"assets":[{"name":"x","digest":"md5:00"}]}"#, "x").is_err(), "只認 sha256");
        assert!(published_digest(r#"{"assets":[{"name":"x","digest":"sha256:ab12"}]}"#, "x").is_err(), "長度不對不算");
    }

    #[test]
    fn sha256_of_a_file_matches_the_known_vector() {
        let dir = crate::testing::scratch_dir("am-herdr-sha");
        let f = dir.join("f");
        std::fs::write(&f, "abc").unwrap();
        assert_eq!(sha256_hex(&f).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[tokio::test]
    async fn refuses_bots_bad_input_unknown_remote_shared_and_stale_targets() {
        let env = tt::env().await;
        let app = &env.app;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let ops = || -> Arc<dyn Ops> { fake.clone() };
        let call = |h: HeaderMap, host: &'static str, t: Option<&'static str>, cur: Option<&'static str>| {
            let ops = ops();
            async move { start(app, &h, host, t, cur.map(String::from), ops, fast()).await.unwrap_err() }
        };
        let mut bot = HeaderMap::new();
        bot.insert("X-AM-Bot-Id", "x".parse().unwrap());
        assert!(matches!(call(bot, "local", Some("0.9.3"), Some("0.9.3")).await, LcError::Forbidden(v) if v["reason"] == "ui_only"));
        assert!(matches!(call(HeaderMap::new(), "local", Some("latest"), Some("0.9.3")).await, LcError::Bad(_)));
        assert!(matches!(call(HeaderMap::new(), "local", None, Some("0.9.3")).await, LcError::Bad(_)));
        assert!(matches!(call(HeaderMap::new(), "nope", Some("0.9.3"), Some("0.9.3")).await, LcError::NotFound(_)));
        let cfg = |name: &str, shared: bool| crate::config::HostCfg { daemon_url: None,
            name: name.into(), ssh: "x".into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(),
            remote_path: String::new(), shared_session: shared,
        };
        app.hosts.insert_remote_for_test(cfg("build1", false)).await;
        assert!(matches!(call(HeaderMap::new(), "build1", Some("0.9.2"), Some("0.9.3")).await, LcError::Conflict(v) if v["reason"] == "stale_target"));
        app.hosts.insert_remote_for_test(cfg("m4p", true)).await;
        let m4p = cfg("m4p", true);
        app.cfg
            .update(move |c| {
                c.hosts.push(m4p);
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(call(HeaderMap::new(), "m4p", Some("0.9.3"), Some("0.9.3")).await, LcError::Conflict(v) if v["reason"] == "shared_session"));
        let stale = call(HeaderMap::new(), "local", Some("0.9.2"), Some("0.9.3")).await;
        assert!(matches!(&stale, LcError::Conflict(v) if v["reason"] == "stale_target" && v["current_target"] == "0.9.3"), "{stale:?}");
        let gone = call(HeaderMap::new(), "local", Some("0.9.3"), None).await;
        assert!(matches!(&gone, LcError::Conflict(v) if v["reason"] == "stale_target" && v["current_target"].is_null()), "{gone:?}");
        assert!(running_list(app).is_empty(), "被拒的不佔位");
        assert!(fake.restarts.lock().unwrap().is_empty());
    }

    async fn remote_env(env: &tt::Env, host: &str) -> tt::MockHerdr {
        let cfg = crate::config::HostCfg { daemon_url: None,
            name: host.into(), ssh: "never-connect.invalid".into(), ssh_port: 22, ssh_opts: vec![],
            herdr_session: "remote-owner".into(), remote_path: String::new(), shared_session: false,
        };
        let socket = env.dir.join("remote-herdr.sock");
        let remote = tt::MockHerdr::start(socket.clone());
        let client = crate::herdr::HerdrClient::new(socket);
        env.app.hosts.insert_remote_with_client_for_test(cfg, client).await;
        sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
        remote
    }

    fn remote_fake(env: &tt::Env, remote: &tt::MockHerdr, download: &str) -> Arc<Fake> {
        let mut fake = Fake::new(env, "0.9.1", download);
        *remote.pong.lock().unwrap() = ("0.9.1".into(), 22);
        Arc::get_mut(&mut fake).unwrap().pong = remote.pong.clone();
        fake
    }

    async fn go_host(env: &tt::Env, fake: &Arc<Fake>, host: &str) -> (Value, Value, Vec<String>) {
        let mut rx = env.app.subscribe();
        let accepted = start(&env.app, &HeaderMap::new(), host, Some("0.9.3"), Some("0.9.3".into()), fake.clone(), fast()).await.unwrap();
        let mut phases = vec![];
        let done = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if ev.kind == "herdr_update_progress" {
                    assert_eq!(ev.data["host"], host);
                    assert_eq!(ev.data["session"], "remote-owner");
                    phases.push(ev.data["phase"].as_str().unwrap().to_string());
                }
                if ev.kind == "herdr_update_done" { return ev.data; }
            }
        }).await.unwrap();
        (accepted, done, phases)
    }

    #[tokio::test]
    async fn remote_success_runs_all_phases_and_ignores_busy_bots_on_other_hosts() {
        let env = tt::env().await;
        let remote = remote_env(&env, "remote-test").await;
        let bot = top(&env, "remote-owner", "idle").await;
        let kid = child_of(&env, &bot, "remote-child", "remote task").await;
        let other = crate::db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/tmp/other-lx1752', 'other', 'local', ?)")
            .bind(&other).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        let local = tt::claude_bot(&env.app, &other, "busy-local").await;
        let run = tt::fake_run(&env.app, &local.id).await;
        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(run).execute(&env.app.db).await.unwrap();
        let fake = remote_fake(&env, &remote, "0.9.3");
        let (accepted, done, phases) = go_host(&env, &fake, "remote-test").await;
        assert_eq!(accepted["session"], "remote-owner");
        assert_eq!(accepted["will_resume"].as_array().unwrap().len(), 1);
        assert_eq!(accepted["children_lost"][0]["bot_id"], kid);
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["host"], "remote-test");
        assert_eq!(done["phase"], "done");
        assert!(done["failed_phase"].is_null());
        assert!(done["manual_upgrade_command"].as_str().unwrap().contains("brew upgrade herdr"));
        assert_eq!(phases, ["downloading", "waiting_idle", "stopping", "restarting", "resuming"]);
        assert_eq!(*fake.restarts.lock().unwrap(), ["remote-owner"]);
        assert_eq!(*fake.resumed.lock().unwrap(), [bot]);
        assert_eq!(fake.notified.lock().unwrap().len(), 1);
        assert!(!window_open(&env.app).await);
    }

    #[tokio::test]
    async fn remote_failure_matrix_preserves_or_restores_binary_and_closes_window() {
        for (failure, reason, phase, restarts, resumed) in [
            ("install", "install_path_unsupported", "downloading", 0, 0),
            ("preflight", "restart_unsupported", "downloading", 0, 0),
            ("download", "download_failed", "downloading", 0, 0),
            ("staging", "download_failed", "downloading", 0, 0),
            ("checksum", "checksum_mismatch", "downloading", 0, 0),
            ("version", "version_mismatch", "downloading", 0, 0),
            ("busy", "busy_timeout", "waiting_idle", 0, 0),
            ("maintenance", "maintenance_busy", "waiting_idle", 0, 0),
            ("swap", "swap_failed", "stopping", 0, 0),
            ("swap_ack", "swap_failed", "stopping", 1, 1),
            ("wrong_version", "restart_failed", "restarting", 2, 1),
            ("protocol", "restart_failed", "restarting", 2, 1),
            ("restart", "rollback_failed", "rolling_back", 2, 0),
            ("restart_once", "restart_failed", "restarting", 2, 1),
            ("rollback_ping", "rollback_failed", "rolling_back", 2, 0),
            ("rollback", "rollback_failed", "rolling_back", 1, 0),
            ("resume", "resume_failed", "resuming", 1, 0),
        ] {
            let env = tt::env().await;
            let remote = remote_env(&env, "remote-test").await;
            let bot = top(&env, "target-bot", if failure == "busy" { "working" } else { "idle" }).await;
            let fake = remote_fake(&env, &remote, if failure == "version" { "0.9.2" } else { "0.9.3" });
            let mut existing_window = None;
            match failure {
                "preflight" => *fake.preflight_err.lock().unwrap() = Some("job program mismatch".into()),
                "checksum" => { fake.bad_checksum.store(1, Ordering::SeqCst); }
                "wrong_version" | "rollback" => { fake.bad_restarts.store(1, Ordering::SeqCst); }
                "rollback_ping" => { fake.bad_restarts.store(2, Ordering::SeqCst); }
                "protocol" => { fake.new_protocol.store(99, Ordering::SeqCst); }
                "maintenance" => {
                    existing_window = crate::runners::herdr_maintenance::open_as(&env.app, 30, "test", "already held").await.unwrap();
                }
                _ => {}
            }
            if ["install", "download", "staging", "swap", "swap_ack", "restart", "restart_once", "rollback", "resume"].contains(&failure) {
                *fake.fail_at.lock().unwrap() = Some(failure.into());
            }
            let (_, done, phases) = go_host(&env, &fake, "remote-test").await;
            assert_eq!(done["reason"], reason, "{failure}: {done}");
            assert_eq!(done["failed_phase"], phase, "{failure}: {done}");
            if failure == "checksum" {
                assert_eq!(*fake.version_probes.lock().unwrap(), vec![fake.install.clone()], "不執行未通過校驗的遠端 staging");
            }
            assert_eq!(fake.restarts.lock().unwrap().len(), restarts, "{failure}");
            assert_eq!(fake.resumed.lock().unwrap().len(), resumed, "{failure}");
            let installed = if ["rollback", "resume"].contains(&failure) { "herdr 0.9.3" } else { "herdr 0.9.1" };
            assert_eq!(fake.installed(), installed, "{failure}");
            if ["swap_ack", "wrong_version", "protocol", "restart", "restart_once", "rollback", "rollback_ping"].contains(&failure) {
                assert!(phases.iter().any(|p| p == "rolling_back"), "{failure}");
            }
            if resumed == 1 { assert_eq!(fake.resumed.lock().unwrap()[0], bot); }
            if let Some(w) = existing_window {
                assert!(window_open(&env.app).await, "must not close someone else's window");
                crate::runners::herdr_maintenance::close_as(&env.app, &w, "test", None).await.unwrap();
            }
            assert!(!window_open(&env.app).await, "{failure}");
            assert!(!env.app.data_dir.join(STAGING_DIR).join("0.9.3").exists(), "{failure}: staging cleaned");
            assert!(running_list(&env.app).is_empty());
        }
    }

    #[test]
    fn swap_keeps_a_backup_and_restore_puts_it_back() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-swap-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let (install, staging) = (dir.join("herdr"), dir.join("staged"));
        std::fs::write(&install, "old").unwrap();
        std::fs::write(&staging, "new").unwrap();
        let bak = swap_in(&staging, &install, "0.9.1").unwrap();
        assert_eq!(bak, dir.join("herdr.bak-0.9.1"));
        assert_eq!(std::fs::read_to_string(&install).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(&bak).unwrap(), "old");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&install).unwrap().permissions().mode() & 0o777, 0o755);
        restore(&bak, &install).unwrap();
        assert_eq!(std::fs::read_to_string(&install).unwrap(), "old");
        assert!(bak.exists(), ".bak 留著");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).filter(|n| n.to_string_lossy().starts_with('.')).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launchd_restart_preflight_rejects_a_different_program() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-herdr-launchd-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let install = dir.join(".local/bin/herdr");
        let launched = dir.join("homebrew/bin/herdr");
        std::fs::create_dir_all(install.parent().unwrap()).unwrap();
        std::fs::create_dir_all(launched.parent().unwrap()).unwrap();
        std::fs::write(&install, "private copy").unwrap();
        std::fs::write(&launched, "package manager copy").unwrap();

        let job = format!("path = /tmp/job.plist\nprogram = {}\narguments = {{\n  {}\n}}\n", launched.display(), launched.display());
        let mismatch = ensure_launchd_program(&job, &install);
        assert!(mismatch.is_err(), "preflight must not restart a job that launches a different herdr binary: {mismatch:?}");

        let matching = format!("program = {}\n", install.display());
        assert!(ensure_launchd_program(&matching, &install).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn session_names_that_systemd_would_rewrite_are_refused() {
        assert!(session_ok("agents-manager") && session_ok("s_1.2"));
        for bad in ["", "a/b", "a b", ".x", "中文"] {
            assert!(!session_ok(bad), "{bad:?}");
        }
    }
}
