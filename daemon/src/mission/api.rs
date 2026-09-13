//! `/api/projects/{id}/missions`、`/api/missions/*`、`/api/identities/{name}/disabled`。
//! 契約寫在 `docs/API.md` 的「群組任務」一節；這支檔案改了那一節要跟著改。

use super::{deliver, pick, store};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;

fn up<E: std::fmt::Display>(e: E) -> LcError {
    LcError::Upstream(e.to_string())
}

async fn emit(app: &Arc<App>, m: &store::Mission) {
    app.emit("mission_updated", json!({"mission_id": m.id, "project_id": m.project_id, "status": m.status()})).await;
}

async fn load(app: &Arc<App>, id: &str) -> Result<store::Mission, LcError> {
    store::get(&app.db, id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("mission".into()))
}

/// 已結案的任務不能再動：一律 409 `already_closed`，讓呼叫端知道要開新任務而不是重試。
fn ensure_open(m: &store::Mission) -> Result<(), LcError> {
    if m.completed_at.is_some() || m.cancelled_at.is_some() {
        return Err(LcError::conflict("already_closed", json!({"mission_id": m.id, "status": m.status()})));
    }
    Ok(())
}

/// `relay_from` 跟 `POST /api/bots/{id}/prompt` 同一套：省略＝使用者本人，否則必須是存在中的 bot 或 `daemon`。
async fn check_relay_from(app: &Arc<App>, relay_from: Option<&str>) -> Result<Option<String>, LcError> {
    match relay_from.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(crate::agent_relay::DAEMON_SENDER) => Ok(Some(crate::agent_relay::DAEMON_SENDER.into())),
        Some(id) => match crate::db::bot(&app.db, id).await.map_err(up)? {
            Some(b) if b.deleted_at.is_none() => Ok(Some(b.id)),
            _ => Err(LcError::Bad(format!("relay_from must be a live bot id or `{}`", crate::agent_relay::DAEMON_SENDER))),
        },
    }
}

fn one_of(field: &str, v: &str, allowed: &[&str]) -> Result<(), LcError> {
    if allowed.contains(&v) {
        Ok(())
    } else {
        Err(LcError::Bad(format!("{field} must be one of {}", allowed.join(" | "))))
    }
}

#[derive(Deserialize)]
pub struct NewMissionIn {
    text: String,
    #[serde(default)]
    client_request_id: Option<String>,
    delivery_mode: String,
    executor_kind: String,
    on_5h_limit: String,
    #[serde(default)]
    max_rounds: Option<i64>,
}

/// 群組的「交給 AGM」。建任務、在群組時間軸記下使用者的指示，並放進 AGM 的 inbox 叫它起來調度。
pub async fn post_mission(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Json(b): Json<NewMissionIn>,
) -> Result<Json<Value>, LcError> {
    let text = b.text.trim();
    if text.is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    one_of("delivery_mode", &b.delivery_mode, &["push_main", "pr"])?;
    one_of("executor_kind", &b.executor_kind, &["claude", "codex", "grok"])?;
    one_of("on_5h_limit", &b.on_5h_limit, &["wait", "switch"])?;
    let max_rounds = b.max_rounds.unwrap_or(2);
    if !(0..=10).contains(&max_rounds) {
        return Err(LcError::Bad("max_rounds must be 0..=10".into()));
    }
    let project = crate::db::project(&app.db, &project_id)
        .await
        .map_err(up)?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    if project.host != crate::config::LOCAL_HOST {
        // team 的 worktree helper 同樣本機限定；遠端要另外設計交付路徑，先明確拒絕。
        return Err(LcError::BadValue(json!({"error": "remote_not_supported", "host": project.host})));
    }
    let crid = b.client_request_id.clone().unwrap_or_else(crate::db::ulid);
    let (m, created) = store::create(
        &app.db,
        &store::NewMission {
            project_id: &project.id,
            client_request_id: &crid,
            text,
            delivery_mode: &b.delivery_mode,
            executor_kind: &b.executor_kind,
            on_5h_limit: &b.on_5h_limit,
            max_rounds,
        },
    )
    .await
    .map_err(up)?;
    if created {
        store::add_event(&app.db, &m.id, "instruction", text, None, &json!({})).await.map_err(up)?;
        let payload = json!({
            "mission_id": m.id,
            "project_id": project.id,
            "project": project.label,
            "cwd": project.path,
            "text": text,
            "delivery_mode": m.delivery_mode,
            "executor_kind": m.executor_kind,
            "on_5h_limit": m.on_5h_limit,
            "max_rounds": m.max_rounds,
        });
        crate::supervisor::store::push_inbox(&app.db, &format!("mission:{}:created", m.id), "mission_created", None, None, None, &payload)
            .await
            .map_err(up)?;
        emit(&app, &m).await;
    }
    let mut out = m.json();
    out["created"] = created.into();
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

/// 「已完成任務」＝ `status=done`。
pub async fn get_missions(
    State(app): State<Arc<App>>,
    Path(project_id): Path<String>,
    Query(q): Query<ListQuery>,
) -> Result<Json<Value>, LcError> {
    let status = q.status.as_deref().unwrap_or("all");
    one_of("status", status, &["all", "open", "done", "cancelled"])?;
    let rows = store::list(&app.db, &project_id, status, q.limit.unwrap_or(100)).await.map_err(up)?;
    Ok(Json(json!({"project_id": project_id, "missions": rows.iter().map(|m| m.json()).collect::<Vec<_>>()})))
}

pub async fn get_mission(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    let events = store::events(&app.db, &id).await.map_err(up)?;
    let mut out = m.json();
    out["events"] = json!(events);
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct EventIn {
    kind: String,
    text: String,
    #[serde(default)]
    relay_from: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

/// AGM／bot 往群組時間軸回報（`report`、`note`），或記下驗證通過（`verified`，交付前必須有）。
pub async fn post_event(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<EventIn>) -> Result<Json<Value>, LcError> {
    one_of("kind", &b.kind, &["report", "note", "verified"])?;
    if b.text.trim().is_empty() {
        return Err(LcError::Bad("text is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    let ev = store::add_event(&app.db, &id, &b.kind, b.text.trim(), from.as_deref(), &b.payload.unwrap_or_else(|| json!({})))
        .await
        .map_err(up)?;
    emit(&app, &load(&app, &id).await?).await;
    Ok(Json(json!(ev)))
}

#[derive(Deserialize)]
pub struct PauseIn {
    reason: String,
    #[serde(default)]
    detail: Option<String>,
}

pub async fn post_pause(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<PauseIn>) -> Result<Json<Value>, LcError> {
    if b.reason.trim().is_empty() {
        return Err(LcError::Bad("reason is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    store::pause(&app.db, &id, b.reason.trim(), b.detail.as_deref()).await.map_err(up)?;
    let text = match b.detail.as_deref() {
        Some(d) => format!("暫停：{}（{}）", b.reason.trim(), d),
        None => format!("暫停：{}", b.reason.trim()),
    };
    store::add_event(&app.db, &id, "paused", &text, Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": b.reason}))
        .await
        .map_err(up)?;
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(m.json()))
}

pub async fn post_resume(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    if store::resume(&app.db, &id).await.map_err(up)? {
        store::add_event(&app.db, &id, "resumed", "繼續", Some(crate::agent_relay::DAEMON_SENDER), &json!({})).await.map_err(up)?;
    }
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(m.json()))
}

pub async fn post_cancel(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    store::cancel(&app.db, &id).await.map_err(up)?;
    store::add_event(&app.db, &id, "cancelled", "已取消", Some(crate::agent_relay::DAEMON_SENDER), &json!({})).await.map_err(up)?;
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(m.json()))
}

#[derive(Deserialize)]
pub struct CompleteIn {
    result_summary: String,
    #[serde(default)]
    relay_from: Option<String>,
}

pub async fn post_complete(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<CompleteIn>) -> Result<Json<Value>, LcError> {
    if b.result_summary.trim().is_empty() {
        return Err(LcError::Bad("result_summary is empty".into()));
    }
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    store::complete(&app.db, &id, b.result_summary.trim()).await.map_err(up)?;
    store::add_event(&app.db, &id, "completed", b.result_summary.trim(), from.as_deref(), &json!({})).await.map_err(up)?;
    let m = load(&app, &id).await?;
    emit(&app, &m).await;
    Ok(Json(m.json()))
}

/// 用掉一輪（review 退回或驗證失敗）。到上限就把任務停下來（`max_rounds`）並回 409。
pub async fn post_round(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    match store::use_round(&app.db, &id).await.map_err(up)? {
        Ok(used) => {
            store::add_event(&app.db, &id, "round", &format!("第 {used} 輪退回（上限 {}）", m.max_rounds), Some(crate::agent_relay::DAEMON_SENDER), &json!({"rounds_used": used}))
                .await
                .map_err(up)?;
            let m = load(&app, &id).await?;
            emit(&app, &m).await;
            Ok(Json(m.json()))
        }
        Err(used) => {
            let detail = format!("review／驗證已退回 {used} 輪，達到上限 {}", m.max_rounds);
            store::pause(&app.db, &id, "max_rounds", Some(&detail)).await.map_err(up)?;
            store::add_event(&app.db, &id, "paused", &format!("暫停：{detail}，等使用者決定"), Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": "max_rounds"}))
                .await
                .map_err(up)?;
            emit(&app, &load(&app, &id).await?).await;
            Err(LcError::conflict("max_rounds", json!({"mission_id": id, "rounds_used": used, "max_rounds": m.max_rounds})))
        }
    }
}

/// 照任務的設定挑身分（D3–D7）。`role=verifier` 找不到 Fable 額度時回 `ask_user`，並把任務停下來（D6）。
pub async fn get_pick(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    let role = q.get("role").and_then(|r| pick::Role::parse(r)).ok_or_else(|| LcError::Bad("role must be executor | reviewer | verifier".into()))?;
    let project = crate::db::project(&app.db, &m.project_id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("project".into()))?;
    // 驗證者一律 claude＋Fable（D3）；reviewer 跟執行者同 kind。
    let kind = if role == pick::Role::Verifier { "claude" } else { m.executor_kind.as_str() };
    let raw = super::candidates(&app, &project.host, kind).await;
    let cands: Vec<pick::Candidate> = raw.iter().map(|(n, d, q)| pick::Candidate { name: n, disabled: *d, quota: q.as_ref() }).collect();
    let on_5h = if m.on_5h_limit == "switch" { pick::On5hLimit::Switch } else { pick::On5hLimit::Wait };
    let decision = pick::pick(role, &cands, on_5h, q.get("exclude").map(String::as_str), chrono::Utc::now());
    if role == pick::Role::Verifier && m.completed_at.is_none() && m.cancelled_at.is_none() {
        if let pick::Pick::AskUser { reason, .. } = &decision {
            if m.paused_reason.as_deref() != Some("no_fable_for_verifier") {
                store::pause(&app.db, &id, "no_fable_for_verifier", Some(reason)).await.map_err(up)?;
                store::add_event(&app.db, &id, "paused", &format!("暫停：{reason}，等使用者決定"), Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": "no_fable_for_verifier", "decision": decision}))
                    .await
                    .map_err(up)?;
                emit(&app, &load(&app, &id).await?).await;
            }
        }
    }
    Ok(Json(json!({"mission_id": id, "role": q.get("role"), "kind": kind, "pick": decision})))
}

#[derive(Deserialize)]
pub struct DeliverIn {
    /// 要交付的 worktree（本機絕對路徑），HEAD 就是要交的 commit。
    worktree: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    relay_from: Option<String>,
}

/// 依任務的 `delivery_mode` 推 main（fast-forward only）或開 PR。必須先有 `verified` 事件。
/// 任何失敗都把任務停下來問人（D8），回 409 帶機器碼。
pub async fn post_deliver(State(app): State<Arc<App>>, Path(id): Path<String>, Json(b): Json<DeliverIn>) -> Result<Json<Value>, LcError> {
    let m = load(&app, &id).await?;
    ensure_open(&m)?;
    let from = check_relay_from(&app, b.relay_from.as_deref()).await?;
    if !store::has_event(&app.db, &id, "verified").await.map_err(up)? {
        return Err(LcError::conflict("not_verified", json!({"mission_id": id})));
    }
    let dir = std::path::PathBuf::from(&b.worktree);
    if !dir.is_absolute() || !dir.is_dir() {
        return Err(LcError::Bad("worktree must be an existing absolute path".into()));
    }
    let result = if m.delivery_mode == "push_main" {
        deliver::push_main(&dir, "origin", "main").await.map(|sha| json!({"mode": "push_main", "sha": sha}))
    } else {
        let branch = format!("mission/{}", m.id.to_lowercase());
        let title = b.title.clone().unwrap_or_else(|| m.text.chars().take(72).collect());
        let body = b.body.clone().unwrap_or_else(|| format!("群組任務 {}\n\n{}", m.id, m.text));
        deliver::open_pr(&dir, "origin", "main", &branch, &title, &body).await.map(|url| json!({"mode": "pr", "branch": branch, "url": url}))
    };
    match result {
        Ok(out) => {
            let text = match out["mode"].as_str() {
                Some("push_main") => format!("已推上 main：{}", out["sha"].as_str().unwrap_or_default()),
                _ => format!("已開 PR：{}", out["url"].as_str().unwrap_or_default()),
            };
            store::add_event(&app.db, &id, "delivered", &text, from.as_deref(), &out).await.map_err(up)?;
            emit(&app, &load(&app, &id).await?).await;
            Ok(Json(out))
        }
        Err(f) => {
            let reason = if m.delivery_mode == "push_main" { "push_main_failed" } else { "pr_failed" };
            store::pause(&app.db, &id, reason, Some(&format!("{}：{}", f.code, f.detail))).await.map_err(up)?;
            store::add_event(&app.db, &id, "paused", &format!("交付失敗（{}），等使用者決定：{}", f.code, f.detail), Some(crate::agent_relay::DAEMON_SENDER), &json!({"reason": reason, "code": f.code}))
                .await
                .map_err(up)?;
            emit(&app, &load(&app, &id).await?).await;
            Err(LcError::conflict(f.code, json!({"mission_id": id, "detail": f.detail})))
        }
    }
}

#[derive(Deserialize)]
pub struct DisableIn {
    kind: String,
    disabled: bool,
    #[serde(default)]
    host: Option<String>,
}

/// 身分停用搬進 daemon（原本只在瀏覽器 localStorage），挑身分時才看得到。
pub async fn put_identity_disabled(
    State(app): State<Arc<App>>,
    Path(name): Path<String>,
    Json(b): Json<DisableIn>,
) -> Result<Json<Value>, LcError> {
    one_of("kind", &b.kind, &["claude", "codex", "grok"])?;
    if name.trim().is_empty() {
        return Err(LcError::Bad("identity is empty".into()));
    }
    let host = b.host.clone().unwrap_or_else(|| crate::config::LOCAL_HOST.to_string());
    store::set_identity_disabled(&app.db, &host, &b.kind, name.trim(), b.disabled).await.map_err(up)?;
    app.emit("identity_prefs_changed", json!({"host": host, "kind": b.kind, "identity": name, "disabled": b.disabled})).await;
    Ok(Json(json!({"host": host, "kind": b.kind, "identity": name, "disabled": b.disabled})))
}

pub async fn get_identity_prefs(State(app): State<Arc<App>>) -> Result<Json<Value>, LcError> {
    let rows: Vec<(String, String, String)> =
        sqlx::query_as("SELECT host, kind, identity FROM identity_prefs WHERE disabled = 1 ORDER BY host, kind, identity")
            .fetch_all(&app.db)
            .await
            .map_err(up)?;
    Ok(Json(json!({"disabled": rows.iter().map(|(h, k, i)| json!({"host": h, "kind": k, "identity": i})).collect::<Vec<_>>()})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{Quota, Window};

    fn new_mission(crid: &str, mode: &str) -> NewMissionIn {
        NewMissionIn {
            text: "把設定頁的錯字修掉".into(),
            client_request_id: Some(crid.into()),
            delivery_mode: mode.into(),
            executor_kind: "claude".into(),
            on_5h_limit: "switch".into(),
            max_rounds: Some(2),
        }
    }

    fn quota(fable_used: f64) -> Quota {
        let w = |u: f64| Some(Window { used_pct: u, resets_at: Some("2026-09-18T06:00:00Z".into()) });
        Quota {
            five_hour: w(10.0),
            seven_day: w(10.0),
            fable: w(fable_used),
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: "local".into(),
        }
    }

    fn status(v: &Value) -> &str {
        v["status"].as_str().unwrap_or_default()
    }

    fn conflict_reason(e: LcError) -> String {
        match e {
            LcError::Conflict(v) => v["reason"].as_str().unwrap_or_default().to_string(),
            other => panic!("expected a conflict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_mission_runs_through_its_gates_end_to_end() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let pid = env.project_id.clone();

        // 建立，且同一個 request id 不會建第二筆。
        let Json(m) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "push_main"))).await.unwrap();
        assert_eq!(m["created"], true);
        assert_eq!(status(&m), "open");
        let id = m["id"].as_str().unwrap().to_string();
        let Json(again) = post_mission(State(app.clone()), Path(pid.clone()), Json(new_mission("r1", "push_main"))).await.unwrap();
        assert_eq!(again["created"], false);
        assert_eq!(again["id"], m["id"]);

        // AGM 的 inbox 收到一則，只有一則。
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'mission_created' AND event_key = ?")
            .bind(format!("mission:{id}:created"))
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1);

        // 驗證者找不到 Fable 額度 → ask_user，任務停下來（D6）。
        let q = |role: &str| HashMap::from([("role".to_string(), role.to_string())]);
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(q("verifier"))).await.unwrap();
        assert_eq!(p["pick"]["decision"], "ask_user");
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(status(&cur), "paused");
        assert_eq!(cur["paused_reason"], "no_fable_for_verifier");

        // 有 Fable 額度之後挑得到；cc2 被停用就往下挑 cc1。
        post_resume(State(app.clone()), Path(id.clone())).await.unwrap();
        {
            let mut qs = app.quotas.lock().await;
            qs.insert("claude:cc2".into(), quota(10.0));
            qs.insert("claude:cc1".into(), quota(20.0));
        }
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(q("verifier"))).await.unwrap();
        assert_eq!(p["pick"]["identity"], "cc2");
        assert_eq!(p["pick"]["model"], "fable");
        put_identity_disabled(State(app.clone()), Path("cc2".into()), Json(DisableIn { kind: "claude".into(), disabled: true, host: None }))
            .await
            .unwrap();
        let Json(p) = get_pick(State(app.clone()), Path(id.clone()), Query(q("executor"))).await.unwrap();
        assert_eq!(p["pick"]["identity"], "cc1");

        // 輪數上限：兩輪之後第三輪停下來。
        post_round(State(app.clone()), Path(id.clone())).await.unwrap();
        post_round(State(app.clone()), Path(id.clone())).await.unwrap();
        let err = post_round(State(app.clone()), Path(id.clone())).await.unwrap_err();
        assert_eq!(conflict_reason(err), "max_rounds");
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(cur["paused_reason"], "max_rounds");
        post_resume(State(app.clone()), Path(id.clone())).await.unwrap();

        // 沒有驗證通過不能交付。
        let deliver = || DeliverIn { worktree: env.repo.to_string_lossy().to_string(), title: None, body: None, relay_from: None };
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver())).await.unwrap_err();
        assert_eq!(conflict_reason(err), "not_verified");

        // 來源不能冒名；daemon 哨符可以。
        let bogus = EventIn { kind: "verified".into(), text: "ok".into(), relay_from: Some("no-such-bot".into()), payload: None };
        assert!(matches!(post_event(State(app.clone()), Path(id.clone()), Json(bogus)).await, Err(LcError::Bad(_))));
        let ok = EventIn { kind: "verified".into(), text: "cargo test 全過".into(), relay_from: Some("daemon".into()), payload: None };
        post_event(State(app.clone()), Path(id.clone()), Json(ok)).await.unwrap();

        // 測試 repo 沒有 origin：交付失敗 → 停下來問人（D8），不是靜靜吞掉。
        let err = post_deliver(State(app.clone()), Path(id.clone()), Json(deliver())).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)));
        let Json(cur) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        assert_eq!(cur["paused_reason"], "push_main_failed");

        // 完成之後就關起來；已完成任務清單查得到。
        post_complete(State(app.clone()), Path(id.clone()), Json(CompleteIn { result_summary: "修好了".into(), relay_from: None })).await.unwrap();
        let err = post_pause(State(app.clone()), Path(id.clone()), Json(PauseIn { reason: "late".into(), detail: None })).await.unwrap_err();
        assert_eq!(conflict_reason(err), "already_closed");
        let Json(done) = get_missions(State(app.clone()), Path(pid.clone()), Query(ListQuery { status: Some("done".into()), limit: None })).await.unwrap();
        assert_eq!(done["missions"].as_array().unwrap().len(), 1);

        let Json(full) = get_mission(State(app.clone()), Path(id.clone())).await.unwrap();
        let kinds: Vec<&str> = full["events"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
        assert_eq!(kinds.first(), Some(&"instruction"));
        assert!(kinds.contains(&"verified") && kinds.contains(&"completed"));
        let instruction = &full["events"][0];
        assert!(instruction["relay_from"].is_null(), "使用者下的指示不帶來源標");
    }

    #[tokio::test]
    async fn bad_options_and_remote_projects_are_rejected() {
        let env = crate::team::testing::env().await;
        let app = env.app.clone();
        let mut bad = new_mission("r2", "push_main");
        bad.delivery_mode = "force".into();
        assert!(matches!(post_mission(State(app.clone()), Path(env.project_id.clone()), Json(bad)).await, Err(LcError::Bad(_))));

        sqlx::query("UPDATE projects SET host = 'build-box' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();
        match post_mission(State(app.clone()), Path(env.project_id.clone()), Json(new_mission("r3", "pr"))).await {
            Err(LcError::BadValue(v)) => assert_eq!(v["error"], "remote_not_supported"),
            other => panic!("expected remote_not_supported, got {:?}", other.map(|j| j.0)),
        }
    }
}
