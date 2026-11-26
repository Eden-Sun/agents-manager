//! 一次升級的「安裝 → 重啟閒置 → 忙的排閒下來重啟」串接（2026-11-25，使用者：網頁關掉也要照跑）。
//!
//! `POST /api/update-chain`：User-only。對每個 `{host, kind, target_version}` 走 [`crate::cli_update::start_chained`] 開安裝；
//! 同一台一次只開一筆（`cli_updates_one_running`），同台其餘的排在 `pending`，前一筆裝好才開下一筆。
//! 全部 `done` 才對每個 `(kind, host)` 走既有一鍵重啟的範圍批次（`Runner::restart`），再把仍在忙的 bot 排進
//! `bot_update_restart`（[`crate::update_restart::schedule_ids`]）。任一筆沒裝成（或沒開起來）：不再開後面排著的，
//! 等已開的收尾，只對已裝好的那幾個 `(kind, host)` 照樣重啟與排程（裝了新版不能沒人重啟），推 `update_chain_done` `ok:false`。
//! 串接的狀態在 `cli_update_chains`（durable），成員以 `cli_update_chain_members` 為準：daemon 重啟後 [`recover_at_startup`] 接手——
//! 還在裝的等安裝收尾，已進到重啟階段的重跑一次（候選與排程都由 bot 當下的更新通知決定，重跑不會重啟已套用的、也不會重複排）；
//! 一個成員都沒有的（建立途中 daemon 掛掉）判失敗，不當成功。
//! 串接從 `started_at` 起算有總時限（[`CHAIN_LIMIT`]）：超過還有安裝沒結束就收尾 `ok:false` `chain_timeout`，已 done 的照樣重啟。
//! 進度走 WS `update_chain_progress`／`update_chain_done`，`GET /api/state` 的 `update_chains` 供網頁對帳。

use crate::bulk_restart::{is_candidate, skip_reason, Scope, Skip};
use crate::cli_update::{start_chained, Real, Runner};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(500);

/// 串接從 `started_at`（DB）起算的總時限。超過還有安裝在跑就收尾為失敗；daemon 重啟後接手也以同一個開始時間計。
const CHAIN_LIMIT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallIn {
    pub host: String,
    pub kind: String,
    pub target_version: String,
}

#[derive(Debug, Deserialize, Default)]
pub struct ChainIn {
    #[serde(default)]
    pub installs: Vec<InstallIn>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Member {
    update_id: String,
    host: String,
    kind: String,
}

pub async fn migrate(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS cli_update_chains (
           id TEXT PRIMARY KEY,
           status TEXT NOT NULL,
           members TEXT NOT NULL,
           result TEXT,
           started_at TEXT NOT NULL,
           updated_at TEXT NOT NULL,
           finished_at TEXT,
           pending TEXT NOT NULL DEFAULT '[]',
           rejected TEXT NOT NULL DEFAULT '[]'
         )",
    )
    .execute(pool)
    .await?;
    for col in ["pending", "rejected"] {
        if !crate::db::has_column(pool, "cli_update_chains", col).await? {
            sqlx::query(&format!("ALTER TABLE cli_update_chains ADD COLUMN {col} TEXT NOT NULL DEFAULT '[]'"))
                .execute(pool)
                .await?;
        }
    }
    sqlx::query("CREATE TABLE IF NOT EXISTS cli_update_chain_members (update_id TEXT PRIMARY KEY, chain_id TEXT NOT NULL)")
        .execute(pool)
        .await?;
    Ok(())
}

/// `GET /api/state` 的 `update_chains`：還沒收尾的串接。讀不到 DB 回空清單。
pub async fn running_list(app: &impl crate::capabilities::Db) -> Vec<Value> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id, status, members FROM cli_update_chains WHERE status IN ('running', 'restarting') ORDER BY started_at")
            .fetch_all(app.db())
            .await
            .unwrap_or_default();
    rows.into_iter()
        .map(|(id, status, members)| json!({"chain_id": id, "status": status, "members": serde_json::from_str::<Value>(&members).unwrap_or(json!([]))}))
        .collect()
}

fn message_of(e: &LcError) -> String {
    match e {
        LcError::Conflict(v) | LcError::Forbidden(v) | LcError::Unavailable(v) | LcError::BadValue(v) => {
            v["message"].as_str().map(str::to_string).unwrap_or_else(|| v.to_string())
        }
        LcError::NotFound(m) | LcError::Bad(m) | LcError::Upstream(m) => m.clone(),
        other => format!("{other:?}"),
    }
}

fn rejection(i: &InstallIn, e: &LcError) -> Value {
    json!({"host": i.host, "kind": i.kind, "reason": "install_not_started", "error": message_of(e)})
}

/// `POST /api/update-chain`
pub async fn post_chain(State(app): State<Arc<App>>, headers: HeaderMap, Json(body): Json<ChainIn>) -> Result<Response, LcError> {
    let v = start(&app, &headers, body.installs, Arc::new(Real)).await?;
    Ok((StatusCode::ACCEPTED, Json(v)).into_response())
}

pub async fn start(app: &Arc<App>, headers: &HeaderMap, installs: Vec<InstallIn>, runner: Arc<dyn Runner>) -> Result<Value, LcError> {
    crate::cli_update::refuse_bot_caller(headers)?;
    if installs.is_empty() {
        return Err(LcError::Bad("installs 不能是空的".into()));
    }
    let (mut first, mut pending): (Vec<InstallIn>, Vec<InstallIn>) = (Vec::new(), Vec::new());
    for i in installs {
        if first.iter().any(|f| f.host == i.host) {
            pending.push(i);
        } else {
            first.push(i);
        }
    }
    let chain_id = crate::db::ulid();
    let now = crate::db::now();
    sqlx::query("INSERT INTO cli_update_chains (id, status, members, pending, started_at, updated_at) VALUES (?, 'running', '[]', ?, ?, ?)")
        .bind(&chain_id)
        .bind(serde_json::to_string(&pending).unwrap_or_else(|_| "[]".into()))
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .map_err(|e| LcError::Upstream(format!("記不下這次串接，沒有開始：{e}")))?;
    let mut members = Vec::new();
    let mut rejected = Vec::new();
    let mut first_err = None;
    for i in first {
        match start_chained(app, headers, &i.host, Some(&i.kind), Some(&i.target_version), runner.clone(), Some(&chain_id)).await {
            Ok(v) => members.push(Member { update_id: v["update_id"].as_str().unwrap_or_default().to_string(), host: i.host, kind: i.kind }),
            Err(e) => {
                rejected.push(rejection(&i, &e));
                first_err.get_or_insert(e);
            }
        }
    }
    if members.is_empty() {
        let _ = sqlx::query("DELETE FROM cli_update_chains WHERE id = ?").bind(&chain_id).execute(&app.db).await;
        return Err(first_err.unwrap_or_else(|| LcError::Bad("沒有可安裝的項目".into())));
    }
    if let Err(e) = sqlx::query("UPDATE cli_update_chains SET members = ?, rejected = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&members).unwrap_or_else(|_| "[]".into()))
        .bind(Value::from(rejected.clone()).to_string())
        .bind(crate::db::now())
        .bind(&chain_id)
        .execute(&app.db)
        .await
    {
        tracing::warn!(chain_id, error = %e, "could not record the update-chain members; the drive reads them from cli_update_chain_members");
    }
    let queued: Vec<Value> = pending.iter().map(|p| json!({"host": p.host, "kind": p.kind})).collect();
    let out = json!({"chain_id": chain_id, "started": members, "queued": queued, "rejected": rejected});
    app.emit("update_chain_progress", json!({"chain_id": chain_id, "phase": "installing", "members": members})).await;
    spawn_drive(app, runner, chain_id);
    Ok(out)
}

fn spawn_drive(app: &Arc<App>, runner: Arc<dyn Runner>, chain_id: String) {
    let app = app.clone();
    tokio::spawn(async move { drive(&app, runner, &chain_id).await });
}

async fn finish(app: &Arc<App>, chain_id: &str, ok: bool, extra: Value) {
    let mut v = json!({"chain_id": chain_id, "ok": ok});
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        o.extend(e);
    }
    let _ = sqlx::query("UPDATE cli_update_chains SET status = ?, result = ?, updated_at = ?, finished_at = ? WHERE id = ?")
        .bind(if ok { "done" } else { "failed" })
        .bind(v.to_string())
        .bind(crate::db::now())
        .bind(crate::db::now())
        .bind(chain_id)
        .execute(&app.db)
        .await;
    app.emit("update_chain_done", v).await;
}

enum Install {
    Running,
    Done,
    Failed(Value),
}

/// 串接當下的樣子：成員（以 `cli_update_chain_members` 為準）各自的安裝狀態、還排著的、開不起來的。
struct Snapshot {
    status: String,
    started_at: String,
    members: Vec<(Member, Install)>,
    pending: Vec<InstallIn>,
    rejected: Vec<Value>,
}

impl Snapshot {
    fn failures(&self) -> Vec<Value> {
        let mut out = self.rejected.clone();
        out.extend(self.members.iter().filter_map(|(_, s)| match s {
            Install::Failed(v) => Some(v.clone()),
            _ => None,
        }));
        out
    }
    fn running_on(&self, host: &str) -> bool {
        self.members.iter().any(|(m, s)| m.host == host && matches!(s, Install::Running))
    }
    fn any_running(&self) -> bool {
        self.members.iter().any(|(_, s)| matches!(s, Install::Running))
    }
    fn done(&self) -> Vec<Member> {
        self.members.iter().filter(|(_, s)| matches!(s, Install::Done)).map(|(m, _)| m.clone()).collect()
    }
    fn plain_members(&self) -> Vec<Member> {
        self.members.iter().map(|(m, _)| m.clone()).collect()
    }
}

/// 讀不到（DB 暫時不通）回 `None`，串接不存在回 `Some(None)`。
async fn snapshot(app: &Arc<App>, chain_id: &str) -> Option<Option<Snapshot>> {
    let row: Option<(String, String, String, String)> = sqlx::query_as("SELECT status, pending, rejected, started_at FROM cli_update_chains WHERE id = ?")
        .bind(chain_id)
        .fetch_optional(&app.db)
        .await
        .ok()?;
    let Some((status, pending, rejected, started_at)) = row else { return Some(None) };
    let rows: Vec<(String, Option<String>, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT m.update_id, u.host, u.kind, u.status, u.result FROM cli_update_chain_members m
         LEFT JOIN cli_updates u ON u.id = m.update_id WHERE m.chain_id = ? ORDER BY u.started_at, m.update_id",
    )
    .bind(chain_id)
    .fetch_all(&app.db)
    .await
    .ok()?;
    let members = rows
        .into_iter()
        .map(|(update_id, host, kind, status, result)| {
            let (host, kind) = (host.unwrap_or_default(), kind.unwrap_or_default());
            let state = match status.as_deref() {
                Some("running") => Install::Running,
                Some("done") => Install::Done,
                Some(_) => {
                    let r: Value = result.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
                    Install::Failed(json!({"host": host, "kind": kind, "reason": r["reason"], "error": r["error"]}))
                }
                None => Install::Failed(json!({"host": host, "kind": kind, "reason": "missing", "error": "安裝紀錄不見了"})),
            };
            (Member { update_id, host, kind }, state)
        })
        .collect();
    Some(Some(Snapshot {
        status,
        started_at,
        members,
        pending: serde_json::from_str(&pending).unwrap_or_default(),
        rejected: serde_json::from_str(&rejected).unwrap_or_default(),
    }))
}

async fn save_queue(app: &Arc<App>, chain_id: &str, s: &Snapshot) -> bool {
    sqlx::query("UPDATE cli_update_chains SET members = ?, pending = ?, rejected = ?, updated_at = ? WHERE id = ?")
        .bind(serde_json::to_string(&s.plain_members()).unwrap_or_else(|_| "[]".into()))
        .bind(serde_json::to_string(&s.pending).unwrap_or_else(|_| "[]".into()))
        .bind(Value::from(s.rejected.clone()).to_string())
        .bind(crate::db::now())
        .bind(chain_id)
        .execute(&app.db)
        .await
        .is_ok()
}

/// 還沒有失敗時，開某台排著的下一筆：那台已有成員裝好、目前沒有在裝。開了（或開不起來）就回 `true`，讓呼叫端重讀。
/// 同一台同一種已經是成員的（開了之後 daemon 在記下 `pending` 前掛掉）直接拿掉，不重開。
async fn start_next(app: &Arc<App>, runner: &Arc<dyn Runner>, chain_id: &str, s: &mut Snapshot) -> bool {
    let Some(idx) = s.pending.iter().position(|p| {
        !s.running_on(&p.host)
            && s.members.iter().any(|(m, st)| m.host == p.host && matches!(st, Install::Done))
    }) else {
        return false;
    };
    let p = s.pending.remove(idx);
    if s.members.iter().any(|(m, _)| m.host == p.host && m.kind == p.kind) {
        return save_queue(app, chain_id, s).await;
    }
    match start_chained(app, &HeaderMap::new(), &p.host, Some(&p.kind), Some(&p.target_version), runner.clone(), Some(chain_id)).await {
        Ok(v) => {
            let m = Member { update_id: v["update_id"].as_str().unwrap_or_default().to_string(), host: p.host, kind: p.kind };
            s.members.push((m, Install::Running));
        }
        Err(e) => s.rejected.push(rejection(&p, &e)),
    }
    if !save_queue(app, chain_id, s).await {
        tracing::warn!(chain_id, "could not record the update-chain queue; a restart will not reopen an install that is already a member");
    }
    app.emit("update_chain_progress", json!({"chain_id": chain_id, "phase": "installing", "members": s.plain_members()})).await;
    true
}

/// 同一台的安裝一筆接一筆開，等全部收尾（或有一筆失敗後已開的收尾），再對裝好的範圍重啟。DB 暫時讀不到就等下一輪，不當成失敗。
pub async fn drive(app: &Arc<App>, runner: Arc<dyn Runner>, chain_id: &str) {
    drive_with_limit(app, runner, chain_id, CHAIN_LIMIT).await
}

/// 同 [`drive`]，時限可換（測試用）。時限到時還有安裝沒結束：不再開新的，已 done 的照樣重啟，還在跑的與排著的都算失敗（`chain_timeout`）。
pub async fn drive_with_limit(app: &Arc<App>, runner: Arc<dyn Runner>, chain_id: &str, limit: Duration) {
    let (s, timed_out) = loop {
        let Some(read) = snapshot(app, chain_id).await else {
            tokio::time::sleep(POLL).await;
            continue;
        };
        let Some(mut s) = read else { return };
        if s.status == "failed" || s.status == "done" {
            return;
        }
        if s.members.is_empty() {
            let mut failed = s.rejected.clone();
            failed.extend(s.pending.iter().map(|p| json!({"host": p.host, "kind": p.kind, "reason": "install_not_started", "error": "串接建立途中中斷，這一項沒有開始安裝"})));
            finish(app, chain_id, false, json!({"reason": "no_members", "failed": failed})).await;
            return;
        }
        if s.status == "restarting" {
            break (s, false);
        }
        if s.failures().is_empty() && start_next(app, &runner, chain_id, &mut s).await {
            continue;
        }
        if s.any_running() {
            if past_limit(&s.started_at, limit) {
                break (s, true);
            }
            tokio::time::sleep(POLL).await;
            continue;
        }
        let _ = sqlx::query("UPDATE cli_update_chains SET status = 'restarting', updated_at = ? WHERE id = ? AND status = 'running'")
            .bind(crate::db::now())
            .bind(chain_id)
            .execute(&app.db)
            .await;
        break (s, false);
    };
    let mut failed = s.failures();
    if timed_out {
        failed.extend(s.members.iter().filter(|(_, st)| matches!(st, Install::Running)).map(|(m, _)| json!({"host": m.host, "kind": m.kind, "reason": "chain_timeout", "error": "安裝超過串接時限，還沒有結束"})));
    }
    failed.extend(s.pending.iter().map(|p| json!({"host": p.host, "kind": p.kind, "reason": "install_not_started", "error": if timed_out { "串接超過時限，這一項沒有開始" } else { "前面的安裝沒有完成，這一項沒有開始" }})));
    let done = s.done();
    app.emit("update_chain_progress", json!({"chain_id": chain_id, "phase": "restarting", "members": done})).await;
    let mut out = restart_phase(app, runner.as_ref(), &done).await;
    if failed.is_empty() && !timed_out {
        finish(app, chain_id, true, out).await;
        return;
    }
    let reason = if timed_out {
        "chain_timeout"
    } else if s.members.iter().any(|(_, st)| matches!(st, Install::Failed(_))) {
        "install_failed"
    } else {
        "install_not_started"
    };
    out["reason"] = json!(reason);
    out["failed"] = json!(failed);
    finish(app, chain_id, false, out).await;
}

/// `started_at`（RFC 3339，[`crate::db::now`] 寫的）到現在是否已經過了 `limit`。讀不懂時器不動，不當成逾時。
fn past_limit(started_at: &str, limit: Duration) -> bool {
    chrono::DateTime::parse_from_rfc3339(started_at)
        .ok()
        .and_then(|start| (chrono::Utc::now() - start.with_timezone(&chrono::Utc)).to_std().ok())
        .is_some_and(|elapsed| elapsed >= limit)
}

fn busy(skip: Skip) -> bool {
    matches!(skip, Skip::Working | Skip::Blocked | Skip::UnknownStatus | Skip::TurnInFlight | Skip::BackgroundJobs(_))
}

async fn restart_phase(app: &Arc<App>, runner: &dyn Runner, members: &[Member]) -> Value {
    let mut scopes: Vec<Scope> = Vec::new();
    for m in members {
        let s = Scope { kind: m.kind.clone(), host: m.host.clone() };
        if !scopes.contains(&s) {
            scopes.push(s);
        }
    }
    let (mut plans, mut errors, mut scheduled) = (Vec::new(), Vec::new(), Vec::new());
    for scope in &scopes {
        let Some(fence) = app.hosts.fence(&scope.host).await else {
            errors.push(json!({"host": scope.host, "kind": scope.kind, "error": "主機已經不在設定裡"}));
            continue;
        };
        match runner.restart(app, scope.clone(), &fence).await {
            Ok(plan) => plans.push(plan),
            Err(e) => errors.push(json!({"host": scope.host, "kind": scope.kind, "error": format!("{e:#}")})),
        }
        match crate::runners::bulk_restart::candidates(app, Some(scope)).await {
            Ok(cands) => scheduled.extend(cands.iter().filter(|c| is_candidate(c) && skip_reason(c).is_some_and(busy)).map(|c| c.bot_id.clone())),
            Err(e) => errors.push(json!({"host": scope.host, "kind": scope.kind, "error": format!("{e:#}")})),
        }
    }
    if !scheduled.is_empty() {
        if let Err(e) = crate::update_restart::schedule_ids(app, &scheduled).await {
            errors.push(json!({"error": message_of(&e)}));
        }
    }
    json!({"restarts": plans, "scheduled": scheduled, "errors": errors})
}

/// daemon 啟動時接手還沒收尾的串接。
pub async fn recover_at_startup(app: &Arc<App>) {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM cli_update_chains WHERE status IN ('running', 'restarting') ORDER BY started_at")
        .fetch_all(&app.db)
        .await
        .unwrap_or_default();
    for id in ids {
        spawn_drive(app, Arc::new(Real), id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli_update::InstallError;
    use crate::hosts::HostFence;
    use futures::future::BoxFuture;
    use std::sync::Mutex;

    /// 每種 CLI 各自安裝成功之後才讀得到新版；重啟只記帳。
    struct Fake {
        versions: Vec<(&'static str, &'static str, &'static str)>,
        failing: Vec<&'static str>,
        installed: Mutex<Vec<String>>,
        restarts: Mutex<Vec<(String, String)>>,
    }

    impl Fake {
        fn new(before: &'static str, after: &'static str, install_ok: bool) -> Arc<Self> {
            Self::per_kind(vec![("*", before, after)], if install_ok { vec![] } else { vec!["*"] })
        }
        fn per_kind(versions: Vec<(&'static str, &'static str, &'static str)>, failing: Vec<&'static str>) -> Arc<Self> {
            Arc::new(Self { versions, failing, installed: Mutex::new(Vec::new()), restarts: Mutex::new(Vec::new()) })
        }
        fn restarts(&self) -> Vec<(String, String)> {
            self.restarts.lock().unwrap().clone()
        }
    }

    impl Runner for Fake {
        fn install<'a>(&'a self, _: &'a Arc<App>, _: &'a str, kind: &'a str, _: &'a str, _: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>> {
            Box::pin(async move {
                if self.failing.iter().any(|k| *k == "*" || *k == kind) {
                    return Err(InstallError::Failed("boom".into()));
                }
                self.installed.lock().unwrap().push(kind.to_string());
                Ok("ok".into())
            })
        }
        fn installer_busy<'a>(&'a self, _: &'a Arc<App>, _: &'a str, _: &'a str, _: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async { Ok(false) })
        }
        fn version<'a>(&'a self, _: &'a Arc<App>, _: &'a str, kind: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async move {
                let (_, before, after) = *self.versions.iter().find(|(k, _, _)| *k == "*" || *k == kind).unwrap();
                Ok(if self.installed.lock().unwrap().iter().any(|k| k == kind) { after } else { before }.to_string())
            })
        }
        fn restart<'a>(&'a self, _: &'a Arc<App>, scope: Scope, _: &'a HostFence) -> BoxFuture<'a, anyhow::Result<Value>> {
            Box::pin(async move {
                self.restarts.lock().unwrap().push((scope.kind, scope.host));
                Ok(json!({"batch_id": "b-1", "total": 1, "planned": [], "skipped": []}))
            })
        }
    }

    const PENDING: &str = "claude 有新版 2.1.281 → 2.1.284，需安裝後重啟";

    async fn claude_bot_with_notice(env: &crate::testing::Env, name: &str, status: &str) -> String {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET update_notice=?, agent_status=?, status_json=? WHERE id=?")
            .bind(PENDING)
            .bind(status)
            .bind(r#"{"version":"2.1.281 (Claude Code)"}"#)
            .bind(run)
            .execute(&env.app.db)
            .await
            .unwrap();
        bot.id
    }

    async fn claude_snapshot(env: &crate::testing::Env) {
        crate::upstream_update::set_snapshot_for_test(
            &env.app.upstream_watch,
            crate::upstream_update::build_status("claude", &Ok("2.1.284".into()), &[("local".into(), Ok("2.1.281 (Claude Code)".into()))], None),
        )
        .await;
    }

    fn claude_install() -> Vec<InstallIn> {
        vec![InstallIn { host: "local".into(), kind: "claude".into(), target_version: "2.1.284".into() }]
    }

    async fn done_event(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Value {
        let mut out = None;
        assert!(
            crate::testing::eventually!({
                while let Ok(ev) = rx.try_recv() {
                    if ev.kind == "update_chain_done" {
                        out = Some(ev.data);
                    }
                }
                out.is_some()
            }),
            "update_chain_done should be published"
        );
        out.unwrap()
    }

    #[tokio::test]
    async fn a_successful_chain_restarts_each_scope_once_and_schedules_only_the_busy_bots() {
        let env = crate::testing::env().await;
        let _idle = claude_bot_with_notice(&env, "idle-one", "idle").await;
        let busy = claude_bot_with_notice(&env, "busy-one", "working").await;
        claude_snapshot(&env).await;
        let fake = Fake::new("2.1.281 (Claude Code)", "2.1.284 (Claude Code)", true);
        let mut rx = env.app.subscribe();
        let out = start(&env.app, &HeaderMap::new(), claude_install(), fake.clone()).await.unwrap();
        assert_eq!(out["started"].as_array().unwrap().len(), 1);
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(fake.restarts(), vec![("claude".to_string(), "local".to_string())]);
        assert_eq!(done["scheduled"], json!([busy]));
        assert_eq!(crate::db::update_restart::list(&env.app.db).await.unwrap(), vec![busy]);
        assert!(running_list(&env.app).await.is_empty(), "收尾後不再列在 state");
    }

    #[tokio::test]
    async fn a_failed_install_restarts_and_schedules_nothing() {
        let env = crate::testing::env().await;
        let _busy = claude_bot_with_notice(&env, "busy-one", "working").await;
        claude_snapshot(&env).await;
        let fake = Fake::new("2.1.281 (Claude Code)", "2.1.284 (Claude Code)", false);
        let mut rx = env.app.subscribe();
        start(&env.app, &HeaderMap::new(), claude_install(), fake.clone()).await.unwrap();
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], false, "{done}");
        assert_eq!(done["reason"], "install_failed");
        assert_eq!(done["failed"][0]["host"], "local");
        assert!(fake.restarts().is_empty(), "裝失敗就不重啟");
        assert!(crate::db::update_restart::list(&env.app.db).await.unwrap().is_empty(), "也不排程");
    }

    #[tokio::test]
    async fn a_chain_that_cannot_start_any_install_is_an_error_and_leaves_no_row() {
        let env = crate::testing::env().await;
        let fake = Fake::new("2.1.281 (Claude Code)", "2.1.284 (Claude Code)", true);
        let r = start(&env.app, &HeaderMap::new(), claude_install(), fake).await;
        assert!(matches!(r, Err(LcError::Conflict(ref v)) if v["reason"] == "stale_target"), "{r:?}");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM cli_update_chains").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 0);
        let mut h = HeaderMap::new();
        h.insert("X-AM-Bot-Id", "b1".parse().unwrap());
        let fake = Fake::new("2.1.281 (Claude Code)", "2.1.284 (Claude Code)", true);
        assert!(matches!(start(&env.app, &h, claude_install(), fake).await, Err(LcError::Forbidden(_))));
    }

    #[tokio::test]
    async fn a_codex_install_in_a_chain_leaves_its_restart_to_the_chain() {
        let env = crate::testing::env().await;
        codex_bot_with_notice(&env, "cx").await;
        let fake = Fake::new("codex-cli 0.155.1", "codex-cli 0.157.0", true);
        let mut rx = env.app.subscribe();
        let installs = vec![InstallIn { host: "local".into(), kind: "codex".into(), target_version: "0.157.0".into() }];
        start(&env.app, &HeaderMap::new(), installs, fake.clone()).await.unwrap();
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], true, "{done}");
        assert_eq!(fake.restarts(), vec![("codex".to_string(), "local".to_string())], "codex 只重啟一次（由串接統一做）");
    }

    #[tokio::test]
    async fn a_daemon_restart_during_the_restart_phase_resumes_once_and_never_repeats() {
        let env = crate::testing::env().await;
        let busy = claude_bot_with_notice(&env, "busy-one", "working").await;
        sqlx::query("UPDATE runs SET update_notice='已安裝，重啟套用' WHERE bot_id=?").bind(&busy).execute(&env.app.db).await.unwrap();
        let now = crate::db::now();
        sqlx::query("INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at, result) VALUES ('u1','local','claude','2.1.284','done','finishing','old',?,?,'{\"ok\":true}')")
            .bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let members = json!([{"update_id": "u1", "host": "local", "kind": "claude"}]).to_string();
        sqlx::query("INSERT INTO cli_update_chains (id, status, members, started_at, updated_at) VALUES ('c1','restarting',?,?,?)")
            .bind(members).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO cli_update_chain_members (update_id, chain_id) VALUES ('u1','c1')").execute(&env.app.db).await.unwrap();
        assert_eq!(running_list(&env.app).await.len(), 1);
        let fake = Fake::new("x", "y", true);
        let mut rx = env.app.subscribe();
        drive(&env.app, fake.clone(), "c1").await;
        assert_eq!(done_event(&mut rx).await["ok"], true);
        assert_eq!(fake.restarts().len(), 1);
        assert_eq!(crate::db::update_restart::list(&env.app.db).await.unwrap(), vec![busy]);
        drive(&env.app, fake.clone(), "c1").await;
        assert_eq!(fake.restarts().len(), 1, "收尾過的串接再被接手也不會重做");
        assert!(running_list(&env.app).await.is_empty());
    }

    async fn codex_bot_with_notice(env: &crate::testing::Env, name: &str) -> String {
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?").bind(&pending).bind(run).execute(&env.app.db).await.unwrap();
        bot.id
    }

    fn claude_and_codex_on_local() -> Vec<InstallIn> {
        let mut v = claude_install();
        v.push(InstallIn { host: "local".into(), kind: "codex".into(), target_version: "0.157.0".into() });
        v
    }

    #[tokio::test]
    async fn claude_and_codex_on_the_same_host_install_one_after_the_other_and_each_restart_once() {
        let env = crate::testing::env().await;
        claude_bot_with_notice(&env, "idle-one", "idle").await;
        codex_bot_with_notice(&env, "cx").await;
        claude_snapshot(&env).await;
        let fake = Fake::per_kind(vec![("claude", "2.1.281 (Claude Code)", "2.1.284 (Claude Code)"), ("codex", "codex-cli 0.155.1", "codex-cli 0.157.0")], vec![]);
        let mut rx = env.app.subscribe();
        let out = start(&env.app, &HeaderMap::new(), claude_and_codex_on_local(), fake.clone()).await.unwrap();
        assert_eq!(out["started"].as_array().unwrap().len(), 1, "同一台一次只開一筆：{out}");
        assert_eq!(out["queued"], json!([{"host": "local", "kind": "codex"}]));
        assert_eq!(out["rejected"], json!([]));
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], true, "{done}");
        let mut restarts = fake.restarts();
        restarts.sort();
        assert_eq!(restarts, vec![("claude".to_string(), "local".to_string()), ("codex".to_string(), "local".to_string())]);
        assert_eq!(*fake.installed.lock().unwrap(), vec!["claude".to_string(), "codex".to_string()]);
    }

    #[tokio::test]
    async fn when_the_second_install_on_a_host_fails_the_first_one_is_still_restarted() {
        let env = crate::testing::env().await;
        claude_bot_with_notice(&env, "idle-one", "idle").await;
        codex_bot_with_notice(&env, "cx").await;
        claude_snapshot(&env).await;
        let fake = Fake::per_kind(vec![("claude", "2.1.281 (Claude Code)", "2.1.284 (Claude Code)"), ("codex", "codex-cli 0.155.1", "codex-cli 0.157.0")], vec!["codex"]);
        let mut rx = env.app.subscribe();
        start(&env.app, &HeaderMap::new(), claude_and_codex_on_local(), fake.clone()).await.unwrap();
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], false, "{done}");
        assert_eq!(done["reason"], "install_failed");
        assert_eq!(done["failed"][0]["kind"], "codex");
        assert_eq!(fake.restarts(), vec![("claude".to_string(), "local".to_string())], "裝好的 claude 照樣重啟，沒裝成的 codex 不動");
        assert_eq!(done["restarts"].as_array().unwrap().len(), 1);
        assert!(running_list(&env.app).await.is_empty());
    }

    #[tokio::test]
    async fn a_chain_recovered_without_any_member_is_a_failure_not_a_success() {
        let env = crate::testing::env().await;
        let now = crate::db::now();
        let pending = json!([{"host": "local", "kind": "codex", "target_version": "0.157.0"}]).to_string();
        sqlx::query("INSERT INTO cli_update_chains (id, status, members, pending, started_at, updated_at) VALUES ('c0','running','[]',?,?,?)")
            .bind(pending).bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
        let fake = Fake::new("x", "y", true);
        let mut rx = env.app.subscribe();
        drive(&env.app, fake.clone(), "c0").await;
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], false, "{done}");
        assert_eq!(done["reason"], "no_members");
        assert_eq!(done["failed"][0]["kind"], "codex");
        assert!(fake.restarts().is_empty());
        assert!(running_list(&env.app).await.is_empty());
    }

    /// 直接寫一條串接與它的成員（host 都是 local）：`(update_id, kind, cli_updates 的 status)`。
    async fn seed_chain(env: &crate::testing::Env, chain: &str, started_at: &str, members: &[(&str, &str, &str)]) {
        let now = crate::db::now();
        let rows: Vec<Member> = members.iter().map(|(u, k, _)| Member { update_id: u.to_string(), host: "local".into(), kind: k.to_string() }).collect();
        sqlx::query("INSERT INTO cli_update_chains (id, status, members, started_at, updated_at) VALUES (?, 'running', ?, ?, ?)")
            .bind(chain)
            .bind(serde_json::to_string(&rows).unwrap())
            .bind(started_at)
            .bind(&now)
            .execute(&env.app.db)
            .await
            .unwrap();
        for (u, k, st) in members {
            let result = if *st == "done" { Some("{\"ok\":true}") } else { None };
            sqlx::query("INSERT INTO cli_updates (id, host, kind, target_version, status, phase, boot, started_at, updated_at, result) VALUES (?, 'local', ?, 'x', ?, 'finishing', 'old', ?, ?, ?)")
                .bind(u)
                .bind(k)
                .bind(st)
                .bind(&now)
                .bind(&now)
                .bind(result)
                .execute(&env.app.db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO cli_update_chain_members (update_id, chain_id) VALUES (?, ?)").bind(u).bind(chain).execute(&env.app.db).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_member_stuck_running_past_the_limit_finishes_chain_timeout_and_restarts_the_done_ones() {
        let env = crate::testing::env().await;
        let now = crate::db::now();
        seed_chain(&env, "c-timeout", &now, &[("u-claude", "claude", "done"), ("u-codex", "codex", "running")]).await;
        let fake = Fake::per_kind(vec![("claude", "x", "y"), ("codex", "x", "y")], vec![]);
        let mut rx = env.app.subscribe();
        drive_with_limit(&env.app, fake.clone(), "c-timeout", Duration::from_millis(50)).await;
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], false, "{done}");
        assert_eq!(done["reason"], "chain_timeout", "{done}");
        assert_eq!(done["failed"][0]["kind"], "codex");
        assert_eq!(done["failed"][0]["reason"], "chain_timeout");
        assert_eq!(fake.restarts(), vec![("claude".to_string(), "local".to_string())], "已 done 的照樣重啟，還在跑的不動");
        assert!(running_list(&env.app).await.is_empty(), "收尾後不再列在 state");
        let status: String = sqlx::query_scalar("SELECT status FROM cli_update_chains WHERE id = 'c-timeout'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(status, "failed");
    }

    #[tokio::test]
    async fn a_chain_recovered_after_its_persisted_start_passed_the_limit_times_out_at_once() {
        let env = crate::testing::env().await;
        seed_chain(&env, "c-old", "2020-01-01T00:00:00.000Z", &[("u-stuck", "claude", "running")]).await;
        let fake = Fake::new("x", "y", true);
        let mut rx = env.app.subscribe();
        drive(&env.app, fake.clone(), "c-old").await;
        let done = done_event(&mut rx).await;
        assert_eq!(done["ok"], false, "{done}");
        assert_eq!(done["reason"], "chain_timeout", "{done}");
        assert!(fake.restarts().is_empty(), "沒有已 done 的就不重啟");
        assert!(running_list(&env.app).await.is_empty());
    }
}
