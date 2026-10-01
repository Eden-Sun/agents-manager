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
//! 這版只做 local：遠端（尤其 `shared_session` 的 m4p，別顆 daemon 也在用那台 herdr）一律 409。

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
    /// 現行 herdr binary 的路徑（server 重啟時 exec 的那一個）。
    fn install_path(&self) -> Result<PathBuf, String>;
    /// 把 `version` 的 release asset 下載到 `dest`（可執行）。
    fn download<'a>(&'a self, version: &'a str, dest: &'a Path) -> BoxFuture<'a, Result<(), String>>;
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
}

/// 以 data_dir 分（測試同一個行程裡有很多顆 App）。同時只跑一個。
fn running() -> &'static Mutex<HashMap<PathBuf, Progress>> {
    static M: OnceLock<Mutex<HashMap<PathBuf, Progress>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// `GET /api/state` 的 `herdr_updates`：進行中的那一筆（沒有就空陣列）。
pub fn running_list(app: &App) -> Vec<Progress> {
    running().lock().unwrap_or_else(|e| e.into_inner()).get(&app.data_dir).cloned().into_iter().collect()
}

fn set_phase(app: &App, update_id: &str, phase: &str) {
    if let Some(p) = running().lock().unwrap_or_else(|e| e.into_inner()).get_mut(&app.data_dir) {
        if p.update_id == update_id {
            p.phase = phase.to_string();
        }
    }
}

fn release_slot(app: &App, update_id: &str) {
    let mut g = running().lock().unwrap_or_else(|e| e.into_inner());
    if g.get(&app.data_dir).is_some_and(|p| p.update_id == update_id) {
        g.remove(&app.data_dir);
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

/// 這台（local）上會被 server 重啟打斷的 bot。使用者自己的 `default` session 不在這個 server 上，不算。
pub async fn affected(app: &Arc<App>, host: &str) -> anyhow::Result<Affected> {
    let mut out = Affected::default();
    for run in crate::db::all_active_runs(&app.db).await? {
        let Some(bot) = crate::db::bot(&app.db, &run.bot_id).await? else { continue };
        if bot.deleted_at.is_some() || crate::db::bot_host(&app.db, &bot.id).await? != host {
            continue;
        }
        if crate::lifecycle::in_default_session(&run) || bot.herdr_session.as_deref() == Some("default") {
            continue;
        }
        let parent = bot.parent_bot_id.clone().filter(|p| !p.is_empty());
        if bot.managed_by == "child" || parent.is_some() {
            out.children.push(LostChild { bot_id: bot.id, name: bot.name, parent_bot_id: parent, title: run.agent_title.clone() });
            continue;
        }
        let why = if run.agent_status == "working" {
            Some("working")
        } else if run.agent_status == "blocked" {
            Some("blocked")
        } else if crate::db::in_flight_turn(&app.db, &run.id).await?.is_some() {
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
        .fetch_optional(&app.db)
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
    let v = start(&app, &headers, &host, b.target_version.as_deref(), current, Arc::new(Real), Timing::default()).await?;
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
            "message": format!("{host} 的 herdr session 也是別顆 daemon 在用的，這裡不能重啟它")})));
    }
    if host != crate::config::LOCAL_HOST {
        return Err(LcError::conflict("unsupported_host", json!({"host": host,
            "message": "herdr 一鍵更新目前只支援本機"})));
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
    let progress = Progress {
        update_id: update_id.clone(),
        host: host.to_string(),
        target_version: target.clone(),
        phase: "downloading".into(),
        started_at: crate::db::now(),
    };
    {
        let mut g = running().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(cur) = g.get(&app.data_dir) {
            return Err(LcError::conflict("herdr_update_in_progress", json!({"update_id": cur.update_id, "host": cur.host,
                "message": format!("herdr 已經在更新（{}），這一下沒有再開一次", cur.phase)})));
        }
        g.insert(app.data_dir.clone(), progress);
    }
    let session = app.cfg.get().await.server.herdr_session.clone();
    let ctx = Ctx { update_id: update_id.clone(), host: host.to_string(), session, target: target.clone() };
    let app2 = app.clone();
    tokio::spawn(async move {
        let done = run(&app2, ops.as_ref(), timing, &ctx).await;
        release_slot(&app2, &ctx.update_id);
        app2.emit("herdr_update_done", done).await;
    });
    Ok(json!({
        "update_id": update_id,
        "host": host,
        "target_version": target,
        "started": true,
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
    from: Option<String>,
    reason: Option<&'static str>,
    detail: Option<String>,
    resumed: Vec<Value>,
    failed: Vec<Value>,
    children_lost: Vec<LostChild>,
}

impl Outcome {
    fn fail(mut self, reason: &'static str, detail: impl Into<String>) -> Self {
        self.reason = Some(reason);
        self.detail = Some(detail.into());
        self
    }
}

fn log_line(app: &App, ctx: &Ctx, phase: &str, msg: &str) {
    tracing::info!(update = %ctx.update_id, phase, "herdr update: {msg}");
    let line = format!("{} {} {} {} {}\n", crate::db::now(), ctx.update_id, ctx.target, phase, msg.replace('\n', " "));
    let path = app.data_dir.join(LOG_FILE);
    if let Err(e) = std::fs::OpenOptions::new().create(true).append(true).open(&path).and_then(|mut f| f.write_all(line.as_bytes())) {
        tracing::warn!(path = %path.display(), error = %e, "cannot append to herdr-update.log");
    }
}

async fn phase(app: &Arc<App>, ctx: &Ctx, phase: &str, msg: &str) {
    set_phase(app, &ctx.update_id, phase);
    log_line(app, ctx, phase, msg);
    app.emit("herdr_update_progress", json!({"update_id": ctx.update_id, "host": ctx.host, "target_version": ctx.target, "phase": phase}))
        .await;
}

async fn run(app: &Arc<App>, ops: &dyn Ops, timing: Timing, ctx: &Ctx) -> Value {
    let out = steps(app, ops, timing, ctx).await;
    let ok = out.reason.is_none();
    let msg = match (&out.reason, &out.detail) {
        (None, _) => format!("ok：resumed {}，failed {}，children_lost {}", out.resumed.len(), out.failed.len(), out.children_lost.len()),
        (Some(r), d) => format!("{r}：{}", d.as_deref().unwrap_or("")),
    };
    log_line(app, ctx, "done", &msg);
    if ok {
        crate::upstream_update::note_installed(app, "herdr", &ctx.host, &ctx.target).await;
    }
    // 版本快取（hosts[].herdr）不等下一輪 60 秒巡邏。測試不跑：它會去執行本機真的 `herdr --version`。
    if !cfg!(test) {
        crate::herdr_version::refresh(app, &ctx.host).await;
    }
    let mut v = json!({
        "update_id": ctx.update_id, "host": ctx.host, "ok": ok, "from": out.from, "to": ctx.target,
        "resumed": out.resumed, "failed": out.failed, "children_lost": out.children_lost,
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
    let install = match ops.install_path() {
        Ok(p) => p,
        Err(e) => return out.fail("install_path_unsupported", e),
    };
    out.from = ops.binary_version(&install).await.ok();
    let staging = app.data_dir.join(STAGING_DIR).join(&ctx.target).join("herdr");
    if let Some(dir) = staging.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            return out.fail("download_failed", format!("{}: {e}", dir.display()));
        }
    }
    if let Err(e) = ops.download(&ctx.target, &staging).await {
        return out.fail("download_failed", e);
    }
    match ops.binary_version(&staging).await {
        Ok(v) if parse_version(&v) == parse_version(&ctx.target) => {
            log_line(app, ctx, "downloading", &format!("staging {} = {v}；現行 {} = {:?}", staging.display(), install.display(), out.from));
        }
        got => {
            let _ = std::fs::remove_file(&staging);
            return out.fail("version_mismatch", format!("下載的 binary 回報 {got:?}，不是 {}；現行 binary 沒動", ctx.target));
        }
    }

    // 2–3. 等閒置 → 開維護窗口 → 窗口裡再看一次（開窗口之前剛好有人送出一則就回去等）。
    phase(app, ctx, "waiting_idle", "等頂層 bot 都閒置").await;
    let deadline = Instant::now() + timing.idle_max;
    let (window, plan) = loop {
        match affected(app, &ctx.host).await {
            Ok(a) if a.busy.is_empty() => {
                match crate::herdr_maintenance::open_as(app, crate::herdr_maintenance::MAX_MINUTES, "herdr_update", &format!("herdr 一鍵更新 → {}", ctx.target)).await {
                    Ok(Some(w)) => match affected(app, &ctx.host).await {
                        Ok(a) if a.busy.is_empty() => break (w, a),
                        recheck => {
                            log_line(app, ctx, "waiting_idle", &format!("開窗口時又忙起來了：{:?}", recheck.map(|a| a.busy)));
                            let _ = crate::herdr_maintenance::close_as(app, &w, "herdr_update", Some("又忙起來了，晚點再試")).await;
                        }
                    },
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
    let backup = match swap_in(&staging, &install, out.from.as_deref().unwrap_or("unknown")) {
        Ok(b) => b,
        Err(e) => {
            let _ = crate::herdr_maintenance::close_as(app, &window, "herdr_update", Some("換 binary 失敗，沒有重啟")).await;
            return out.fail("swap_failed", format!("{e}；server 沒有重啟"));
        }
    };
    phase(app, ctx, "restarting", &format!("{} 換成 {}（舊的在 {}），重啟 herdr@{}", install.display(), ctx.target, backup.display(), ctx.session)).await;
    let restarted = match ops.restart_server(&ctx.session).await {
        Ok(()) => wait_server(app, &ctx.target, timing).await,
        Err(e) => Err(e),
    };
    // 5. 失敗就換回 `.bak` 再重啟一次；bot 照樣接回（舊版也比沒有 bot 好）。
    if let Err(e) = restarted {
        out.reason = Some("restart_failed");
        phase(app, ctx, "rolling_back", &format!("新版沒起來：{e}；換回 {}", backup.display())).await;
        let rolled = match restore(&backup, &install) {
            Ok(()) => match ops.restart_server(&ctx.session).await {
                Ok(()) => match out.from.as_deref() {
                    Some(from) => wait_server(app, from, timing).await,
                    None => wait_server_any(app, timing).await,
                },
                Err(e) => Err(e),
            },
            Err(e) => Err(e),
        };
        out.detail = Some(match rolled {
            Ok(v) => format!("新版沒起來（{e}）；已換回 {v}"),
            Err(e2) => format!("新版沒起來（{e}）；換回舊版也沒起來（{e2}），要人工處理 {}", backup.display()),
        });
    }

    // 6. 接回頂層 bot；子 agent 通知母 bot。
    phase(app, ctx, "resuming", &format!("接回 {} 顆", plan.resume.len())).await;
    for r in &plan.resume {
        match ops.resume(app, &r.bot_id).await {
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
    for (parent, kids) in group_by_parent(&plan.children) {
        let text = parent_notice(&ctx.target, out.reason.is_none(), &kids);
        let request_id = format!("herdr-update-{}-{parent}", ctx.update_id);
        if let Err(e) = ops.notify_parent(app, &parent, &text, &request_id).await {
            log_line(app, ctx, "resuming", &format!("通知母 bot {parent} 失敗：{e}"));
        }
    }

    // 7. 關窗口（沒接回的子 agent 由它照原規則退休）。
    match crate::herdr_maintenance::close_as(app, &window, "herdr_update", Some("herdr 一鍵更新結束")).await {
        Ok(retired) => log_line(app, ctx, "resuming", &format!("維護窗口關了；退休的子 agent：{retired:?}")),
        Err(e) => log_line(app, ctx, "resuming", &format!("關維護窗口失敗（{e:#}），會在截止時間自己結束")),
    }
    out
}

/// 等 local 的 herdr ping 回來且版本＝`expected`，回讀到的版本。
async fn wait_server(app: &Arc<App>, expected: &str, timing: Timing) -> Result<String, String> {
    let deadline = Instant::now() + timing.restart_max;
    loop {
        let last = match ping_version(app).await {
            Ok(v) if parse_version(&v) == parse_version(expected) => return Ok(v),
            Ok(v) => format!("server 回報 {v}，不是 {expected}"),
            Err(e) => e,
        };
        if Instant::now() >= deadline {
            return Err(format!("{:?} 內沒等到 {expected}：{last}", timing.restart_max));
        }
        tokio::time::sleep(timing.restart_poll).await;
    }
}

/// 舊版讀不到版本時，換回之後只要 server 回來就算。
async fn wait_server_any(app: &Arc<App>, timing: Timing) -> Result<String, String> {
    let deadline = Instant::now() + timing.restart_max;
    loop {
        match ping_version(app).await {
            Ok(v) => return Ok(v),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(timing.restart_poll).await,
        }
    }
}

async fn ping_version(app: &Arc<App>) -> Result<String, String> {
    let conn = app.hosts.get(crate::config::LOCAL_HOST).await.ok_or("沒有 local 主機")?;
    let pong = conn.client.ping().await.map_err(|e| format!("ping：{e:#}"))?;
    Ok(cli_version_string(&pong.version).or_else(|| version_string(&pong.version)).unwrap_or(pong.version))
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

pub struct Real;

fn asset_name() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("herdr-linux-x86_64"),
        ("linux", "aarch64") => Some("herdr-linux-aarch64"),
        ("macos", "x86_64") => Some("herdr-macos-x86_64"),
        ("macos", "aarch64") => Some("herdr-macos-aarch64"),
        _ => None,
    }
}

/// systemd 實例名／launchd label 要原樣可用（同 `herdr_unit`）。
fn session_ok(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('.') && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

async fn command_output(mut cmd: tokio::process::Command, timeout: Duration) -> Result<String, String> {
    cmd.stdin(std::process::Stdio::null()).kill_on_drop(true);
    let out = tokio::time::timeout(timeout, cmd.output())
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

impl Ops for Real {
    /// herdr 自己的安裝器裝在 `~/.local/bin/herdr`（systemd unit 的 PATH 也是它排第一）。symlink 是套件管理器
    /// （Homebrew 之類）裝的，換它會跟套件管理器打架，不自動換。
    fn install_path(&self) -> Result<PathBuf, String> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or("沒有 HOME")?;
        let candidates = [home.join(".local/bin/herdr"), PathBuf::from("/usr/local/bin/herdr"), PathBuf::from("/opt/homebrew/bin/herdr")];
        let p = candidates.into_iter().find(|p| p.symlink_metadata().is_ok()).ok_or("找不到 herdr binary")?;
        let meta = p.symlink_metadata().map_err(|e| e.to_string())?;
        if !meta.file_type().is_file() {
            return Err(format!("{} 不是一般檔案（套件管理器裝的？），不自動換", p.display()));
        }
        Ok(p)
    }

    fn download<'a>(&'a self, version: &'a str, dest: &'a Path) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let asset = asset_name().ok_or_else(|| format!("{}/{} 沒有對應的 release asset", std::env::consts::OS, std::env::consts::ARCH))?;
            let url = format!("{RELEASE_BASE}/v{version}/{asset}");
            let part = dest.with_extension("part");
            let mut cmd = tokio::process::Command::new("curl");
            cmd.args(["-fsSL", "--retry", "2", "--max-time", "300", "-o"]).arg(&part).arg(&url);
            command_output(cmd, Duration::from_secs(320)).await.map_err(|e| format!("curl {url}: {e}"))?;
            set_exec(&part)?;
            std::fs::rename(&part, dest).map_err(|e| format!("{}: {e}", dest.display()))
        })
    }

    fn binary_version<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new(path);
            cmd.arg("--version");
            let out = command_output(cmd, Duration::from_secs(15)).await?;
            cli_version_string(&out).ok_or_else(|| format!("看不懂 `{} --version`：{}", path.display(), out.trim()))
        })
    }

    fn restart_server<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            if !session_ok(session) {
                return Err(format!("session 名 `{session}` 不能當 systemd/launchd 名稱"));
            }
            // SAFETY: getuid 沒有前置條件，也不會失敗。
            let uid = unsafe { libc::getuid() };
            let mut cmd;
            if cfg!(target_os = "macos") {
                cmd = tokio::process::Command::new("launchctl");
                cmd.args(["kickstart", "-k", &format!("gui/{uid}/dev.agents-manager.herdr-{session}")]);
            } else {
                cmd = tokio::process::Command::new("systemctl");
                cmd.args(["--user", "restart", &format!("herdr@{session}.service")]);
                if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
                    cmd.env("XDG_RUNTIME_DIR", format!("/run/user/{uid}"));
                }
            }
            command_output(cmd, Duration::from_secs(60)).await.map(|_| ())
        })
    }

    fn resume<'a>(&'a self, app: &'a Arc<App>, bot_id: &'a str) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let opts = crate::lifecycle::StartOpts { resume_native: true, ..Default::default() };
            crate::lifecycle::restart_bot_with(app, bot_id, opts).await.map_err(|e| format!("{e:?}"))
        })
    }

    /// 母 bot 剛被接回、多半還在啟動：走 AGM 派工那條會排隊的路，失敗再等一下重試（最多約 2 分鐘）。
    fn notify_parent<'a>(&'a self, app: &'a Arc<App>, parent_bot_id: &'a str, text: &'a str, request_id: &'a str) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut last = String::new();
            for _ in 0..12 {
                match crate::lifecycle::prompt_relayed_queueable(app, parent_bot_id, text, request_id, Some(crate::agent_relay::DAEMON_SENDER)).await {
                    Ok(_) => return Ok(()),
                    Err(e) => last = format!("{e:?}"),
                }
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
            Err(last)
        })
    }
}

#[cfg(test)]
mod tests {
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
        install: PathBuf,
        download_as: String,
        /// 前 N 次重啟起來的不是現行檔案寫的版本（模擬新版起不來／起成別的）。
        bad_restarts: AtomicUsize,
        pong: Arc<std::sync::Mutex<(String, u32)>>,
        restarts: Mutex<Vec<String>>,
        resumed: Mutex<Vec<String>>,
        notified: Mutex<Vec<(String, String)>>,
    }

    impl Fake {
        fn new(env: &tt::Env, installed: &str, download_as: &str) -> Arc<Fake> {
            let bin = env.dir.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let install = bin.join("herdr");
            std::fs::write(&install, format!("herdr {installed}\n")).unwrap();
            *env.herdr.pong.lock().unwrap() = (installed.to_string(), 22);
            Arc::new(Fake {
                install,
                download_as: download_as.into(),
                bad_restarts: AtomicUsize::new(0),
                pong: env.herdr.pong.clone(),
                restarts: Mutex::new(vec![]),
                resumed: Mutex::new(vec![]),
                notified: Mutex::new(vec![]),
            })
        }
        fn installed(&self) -> String {
            std::fs::read_to_string(&self.install).unwrap().trim().to_string()
        }
    }

    impl Ops for Fake {
        fn install_path(&self) -> Result<PathBuf, String> {
            Ok(self.install.clone())
        }
        fn download<'a>(&'a self, _v: &'a str, dest: &'a Path) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move { std::fs::write(dest, format!("herdr {}\n", self.download_as)).map_err(|e| e.to_string()) })
        }
        fn binary_version<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
                let s = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
                cli_version_string(&s).ok_or(s)
            })
        }
        fn restart_server<'a>(&'a self, session: &'a str) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.restarts.lock().unwrap().push(session.to_string());
                let v = if self.bad_restarts.load(Ordering::SeqCst) > 0 {
                    self.bad_restarts.fetch_sub(1, Ordering::SeqCst);
                    "0.0.1".to_string()
                } else {
                    cli_version_string(&self.installed()).unwrap()
                };
                *self.pong.lock().unwrap() = (v, 22);
                Ok(())
            })
        }
        fn resume<'a>(&'a self, _app: &'a Arc<App>, bot_id: &'a str) -> BoxFuture<'a, Result<String, String>> {
            Box::pin(async move {
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
        crate::herdr_maintenance::active(app).await.unwrap().is_some()
    }

    #[tokio::test]
    async fn upgrades_restarts_resumes_and_tells_the_parent_about_lost_children() {
        let env = tt::env().await;
        let fake = Fake::new(&env, "0.9.1", "0.9.3");
        let parent = top(&env, "boss", "idle").await;
        let kid = child_of(&env, &parent, "boss-fix", "修 events_lost").await;
        let mut rx = env.app.subscribe();
        let (accepted, done) = go(&env, &fake, "0.9.3").await;

        assert_eq!(accepted["started"], true);
        assert_eq!(accepted["will_resume"], json!([{"bot_id": parent, "name": "boss"}]));
        assert_eq!(accepted["children_lost"], json!([{"bot_id": kid, "name": "boss-fix", "parent_bot_id": parent}]));

        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(done["from"], "0.9.1");
        assert_eq!(done["to"], "0.9.3");
        assert_eq!(done["resumed"], json!([{"bot_id": parent, "name": "boss", "run_id": format!("run-{parent}")}]));
        assert_eq!(done["failed"], json!([]));
        assert_eq!(done["children_lost"][0]["name"], "boss-fix");
        assert!(done.get("reason").is_none());

        assert_eq!(fake.installed(), "herdr 0.9.3");
        assert_eq!(std::fs::read_to_string(fake.install.with_file_name("herdr.bak-0.9.1")).unwrap().trim(), "herdr 0.9.1");
        let session = env.app.cfg.get().await.server.herdr_session.clone();
        assert_eq!(*fake.restarts.lock().unwrap(), vec![session], "只重啟這顆 daemon 的 session，一次");
        assert_eq!(*fake.resumed.lock().unwrap(), vec![parent.clone()], "子 agent 不由 daemon 接回");
        let notes = fake.notified.lock().unwrap().clone();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].0, parent);
        assert!(notes[0].1.contains("boss-fix") && notes[0].1.contains("修 events_lost"), "{}", notes[0].1);
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
        let cfg = |name: &str, shared: bool| crate::config::HostCfg {
            name: name.into(), ssh: "x".into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(),
            remote_path: String::new(), shared_session: shared,
        };
        app.hosts.insert_remote_for_test(cfg("build1", false)).await;
        assert!(matches!(call(HeaderMap::new(), "build1", Some("0.9.3"), Some("0.9.3")).await, LcError::Conflict(v) if v["reason"] == "unsupported_host"));
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

    #[test]
    fn swap_keeps_a_backup_and_restore_puts_it_back() {
        let dir = std::env::temp_dir().join(format!("am-herdr-swap-{}", crate::db::ulid()));
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
    fn session_names_that_systemd_would_rewrite_are_refused() {
        assert!(session_ok("agents-manager") && session_ok("s_1.2"));
        for bad in ["", "a/b", "a b", ".x", "中文"] {
            assert!(!session_ok(bad), "{bad:?}");
        }
    }
}
