//! 一次升級的「安裝 → 重啟閒置 → 忙的排閒下來重啟」串接（2026-11-25，使用者：網頁關掉也要照跑）。
//!
//! `POST /api/update-chain`：User-only。對每個 `{host, kind, target_version}` 走 [`crate::cli_update::start_chained`] 開安裝，
//! 全部 `done` 才對每個 `(kind, host)` 走既有一鍵重啟的範圍批次（`Runner::restart`），再把仍在忙的 bot 排進
//! `bot_update_restart`（[`crate::update_restart::schedule_ids`]）；任一台沒裝成（或沒開起來）就一顆都不重啟、不排程，推 `update_chain_done`。
//! 串接的狀態在 `cli_update_chains`（durable）：daemon 重啟後 [`recover_at_startup`] 接手——還在裝的等安裝收尾，已進到重啟階段的
//! 重跑一次（候選與排程都由 bot 當下的更新通知決定，重跑不會重啟已套用的、也不會重複排）。
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

#[derive(Debug, Clone, Deserialize)]
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
           finished_at TEXT
         )",
    )
    .execute(pool)
    .await?;
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
    let chain_id = crate::db::ulid();
    let now = crate::db::now();
    sqlx::query("INSERT INTO cli_update_chains (id, status, members, started_at, updated_at) VALUES (?, 'running', '[]', ?, ?)")
        .bind(&chain_id)
        .bind(&now)
        .bind(&now)
        .execute(&app.db)
        .await
        .map_err(|e| LcError::Upstream(format!("記不下這次串接，沒有開始：{e}")))?;
    let mut members = Vec::new();
    let mut rejected = Vec::new();
    let mut first_err = None;
    for i in installs {
        match start_chained(app, headers, &i.host, Some(&i.kind), Some(&i.target_version), runner.clone(), Some(&chain_id)).await {
            Ok(v) => members.push(Member { update_id: v["update_id"].as_str().unwrap_or_default().to_string(), host: i.host, kind: i.kind }),
            Err(e) => {
                rejected.push(json!({"host": i.host, "kind": i.kind, "error": message_of(&e)}));
                first_err.get_or_insert(e);
            }
        }
    }
    if members.is_empty() {
        let _ = sqlx::query("DELETE FROM cli_update_chains WHERE id = ?").bind(&chain_id).execute(&app.db).await;
        return Err(first_err.unwrap_or_else(|| LcError::Bad("沒有可安裝的項目".into())));
    }
    let _ = sqlx::query("UPDATE cli_update_chains SET members = ? WHERE id = ?")
        .bind(serde_json::to_string(&members).unwrap_or_else(|_| "[]".into()))
        .bind(&chain_id)
        .execute(&app.db)
        .await;
    let out = json!({"chain_id": chain_id, "started": members, "rejected": rejected});
    if !rejected.is_empty() {
        finish(app, &chain_id, false, json!({"reason": "install_not_started", "failed": rejected})).await;
        return Ok(out);
    }
    app.emit("update_chain_progress", json!({"chain_id": chain_id, "phase": "installing", "members": members})).await;
    spawn_drive(app, runner, chain_id);
    Ok(out)
}

fn spawn_drive(app: &Arc<App>, runner: Arc<dyn Runner>, chain_id: String) {
    let app = app.clone();
    tokio::spawn(async move { drive(&app, runner.as_ref(), &chain_id).await });
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

async fn load(app: &Arc<App>, chain_id: &str) -> Option<(String, Vec<Member>)> {
    let (status, members): (String, String) = sqlx::query_as("SELECT status, members FROM cli_update_chains WHERE id = ?")
        .bind(chain_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten()?;
    Some((status, serde_json::from_str(&members).unwrap_or_default()))
}

/// 等安裝都收尾，再決定重啟或放棄。DB 暫時讀不到就等下一輪，不當成失敗。
pub async fn drive(app: &Arc<App>, runner: &dyn Runner, chain_id: &str) {
    let Some((status, members)) = load(app, chain_id).await else { return };
    if status == "failed" || status == "done" {
        return;
    }
    if status == "running" {
        loop {
            match failures(app, &members).await {
                Some(failed) if failed.is_empty() => break,
                Some(failed) => {
                    finish(app, chain_id, false, json!({"reason": "install_failed", "failed": failed})).await;
                    return;
                }
                None => tokio::time::sleep(POLL).await,
            }
        }
        let _ = sqlx::query("UPDATE cli_update_chains SET status = 'restarting', updated_at = ? WHERE id = ? AND status = 'running'")
            .bind(crate::db::now())
            .bind(chain_id)
            .execute(&app.db)
            .await;
    }
    app.emit("update_chain_progress", json!({"chain_id": chain_id, "phase": "restarting", "members": members})).await;
    let out = restart_phase(app, runner, &members).await;
    finish(app, chain_id, true, out).await;
}

/// `None`＝還有安裝沒收尾（或讀不到）；`Some(空)`＝全部成功；否則是沒裝成的那幾台。
async fn failures(app: &Arc<App>, members: &[Member]) -> Option<Vec<Value>> {
    let mut failed = Vec::new();
    for m in members {
        let row: Option<(String, Option<String>)> = sqlx::query_as("SELECT status, result FROM cli_updates WHERE id = ?")
            .bind(&m.update_id)
            .fetch_optional(&app.db)
            .await
            .ok()?;
        match row {
            Some((status, _)) if status == "running" => return None,
            Some((status, _)) if status == "done" => {}
            Some((_, result)) => {
                let r: Value = result.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(json!({}));
                failed.push(json!({"host": m.host, "kind": m.kind, "reason": r["reason"], "error": r["error"]}));
            }
            None => failed.push(json!({"host": m.host, "kind": m.kind, "reason": "missing", "error": "安裝紀錄不見了"})),
        }
    }
    Some(failed)
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

    /// 安裝成功之後才讀得到新版；重啟只記帳。
    struct Fake {
        before: &'static str,
        after: &'static str,
        install_ok: bool,
        installed: Mutex<bool>,
        restarts: Mutex<Vec<(String, String)>>,
    }

    impl Fake {
        fn new(before: &'static str, after: &'static str, install_ok: bool) -> Arc<Self> {
            Arc::new(Self { before, after, install_ok, installed: Mutex::new(false), restarts: Mutex::new(Vec::new()) })
        }
        fn restarts(&self) -> Vec<(String, String)> {
            self.restarts.lock().unwrap().clone()
        }
    }

    impl Runner for Fake {
        fn install<'a>(&'a self, _: &'a Arc<App>, _: &'a str, _: &'a str, _: &'a str, _: &'a HostFence) -> BoxFuture<'a, Result<String, InstallError>> {
            Box::pin(async move {
                if !self.install_ok {
                    return Err(InstallError::Failed("boom".into()));
                }
                *self.installed.lock().unwrap() = true;
                Ok("ok".into())
            })
        }
        fn installer_busy<'a>(&'a self, _: &'a Arc<App>, _: &'a str, _: &'a str, _: &'a HostFence) -> BoxFuture<'a, anyhow::Result<bool>> {
            Box::pin(async { Ok(false) })
        }
        fn version<'a>(&'a self, _: &'a Arc<App>, _: &'a str, _: &'a str) -> BoxFuture<'a, anyhow::Result<String>> {
            Box::pin(async move { Ok(if *self.installed.lock().unwrap() { self.after } else { self.before }.to_string()) })
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
        let pending = crate::codex_update::pending_text(Some("0.155.1"), "0.157.0");
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "cx").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET update_notice=? WHERE id=?").bind(&pending).bind(run).execute(&env.app.db).await.unwrap();
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
        assert_eq!(running_list(&env.app).await.len(), 1);
        let fake = Fake::new("x", "y", true);
        let mut rx = env.app.subscribe();
        drive(&env.app, fake.as_ref(), "c1").await;
        assert_eq!(done_event(&mut rx).await["ok"], true);
        assert_eq!(fake.restarts().len(), 1);
        assert_eq!(crate::db::update_restart::list(&env.app.db).await.unwrap(), vec![busy]);
        drive(&env.app, fake.as_ref(), "c1").await;
        assert_eq!(fake.restarts().len(), 1, "收尾過的串接再被接手也不會重做");
        assert!(running_list(&env.app).await.is_empty());
    }
}
