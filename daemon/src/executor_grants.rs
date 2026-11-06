//! 專家模式執行者授權（使用者 2026-11-06 方案 c）：管理者（或使用者）把執行者授權給專家，專家就能直接派工給它。
//!
//! 真相在 config.toml 的 `[[projects.executor_grants]]`（`ProjectCfg.executor_grants`），DB 的
//! `projects.executor_grants_json` 只是投影。派工時的核對在 `relay_policy::granted`；不合格的授權由
//! `ProjectCfg::clear_invalid_grants` 在每次投影（含啟動、正式刪除）時清掉。授權只涵蓋派工，不給 start／restart／hire／stop／改角色／再授權。
//!
//! 撤銷與派工的序列化邊界（Astra review3）：不用鎖。turn 變成可送出的兩個寫入（直接送出的 in_flight INSERT、排隊認領
//! `claim_queued`）在同一句 SQL 裡核對授權仍成立（`lifecycle::relay_grant`）。任何讓授權消失的投影（PUT、改角色／track、
//! 刪除、離開 expert）跟這兩句都由 SQLite 序列化：撤銷先提交，直接送出回可重試的 409 `relay_grant_changed`、排隊的
//! 認領不到並在 flush 重驗時撤回；那一句先提交＝已開始，不中止。PUT 另外當場收回排隊中失去授權的 turn（[`withdraw_revoked`]），
//! 撤回跟認領搶同一個 queued→ 轉換。
use std::sync::Arc;
use axum::{extract::{Path, State}, http::HeaderMap, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{api::RequestPrincipal, config::ExecutorGrant, db, lifecycle::LcError, state::App};

/// 這一對派工會不會受授權影響：目標與送件者都在同一個 expert 專案時回那個專案 id（跨專案、一般專案的派工不靠授權）。
/// 查不到時當作會（fail closed），回空字串當鍵。
pub(crate) async fn expert_internal(app: &Arc<App>, from: &str, to: &str) -> Option<String> {
    match sqlx::query_scalar::<_, String>("SELECT p.id FROM bots f JOIN bots t ON t.project_id=f.project_id JOIN projects p ON p.id=t.project_id WHERE f.id=? AND t.id=? AND p.mode='expert'")
        .bind(from).bind(to).fetch_optional(&app.db).await {
        Ok(pid) => pid,
        Err(_) => Some(String::new()),
    }
}

/// 授權組的版本（樂觀鎖，Astra review #5）：授權內容的 sha256 前 16 個 hex。GET（`/api/state`）與 PUT 回應都帶，PUT 帶舊值回 409。
pub fn revision(grants: &[ExecutorGrant]) -> String {
    use sha2::{Digest, Sha256};
    let bytes = serde_json::to_vec(grants).unwrap_or_default();
    Sha256::digest(&bytes).iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[derive(Deserialize)]
pub(crate) struct PutGrants {
    grants: Vec<GrantInput>,
    /// 呼叫端看到的 `grants_revision`；也可用 `If-Match` header 帶。都沒帶＝不檢查（最後寫入者覆蓋）。
    #[serde(default)]
    revision: Option<String>,
}

#[derive(Deserialize)]
struct GrantInput {
    specialist: String,
    executor: String,
}

/// 從 config 閉包裡帶出來的拒絕（`reason` 寫明是哪一條）：400 違反不變式、403 呼叫者已不是 manager、409 版本過期。
#[derive(Debug)]
enum GrantRejected { Bad(Value), Forbidden(Value), Stale(Value) }
impl std::fmt::Display for GrantRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "executor grant rejected: {self:?}") }
}
impl std::error::Error for GrantRejected {}

fn reject(reason: &str, extra: Value) -> anyhow::Error {
    let mut body = json!({"error": "bad_request", "reason": reason});
    if let (Some(b), Value::Object(extra)) = (body.as_object_mut(), extra) { b.extend(extra); }
    anyhow::Error::new(GrantRejected::Bad(body))
}

fn manager_only() -> Value { json!({"error": "forbidden", "reason": "grant_manager_only"}) }

/// 呼叫者：使用者 → `"user"`；該專案前台 manager（存活頂層 role=manager）→ 它的 bot id；其他一律 403。
/// 這是鎖外的預檢；manager 在鎖內還會以最新 config 再驗一次（[`still_manager`]）。
async fn granted_by(app: &Arc<App>, principal: &RequestPrincipal, project_id: &str) -> Result<String, LcError> {
    let forbidden = || LcError::Forbidden(manager_only());
    match principal {
        RequestPrincipal::User => Ok("user".into()),
        RequestPrincipal::Bot(id) => {
            let bot = db::bot(&app.db, id).await.map_err(|e| LcError::Upstream(e.to_string()))?.ok_or_else(forbidden)?;
            let front = bot.deleted_at.is_none() && bot.project_id == project_id && bot.parent_bot_id.is_none() && bot.role.as_deref() == Some("manager");
            if front { Ok(bot.id) } else { Err(forbidden()) }
        }
        _ => Err(forbidden()),
    }
}

/// 鎖內重驗（Astra review #3）：預檢之後、拿到設定鎖之前 manager 可能被換掉；以當下 config 為準。config 的 `bots` 只有頂層 bot。
fn still_manager(p: &crate::config::ProjectCfg, by: &str) -> bool {
    by == "user" || p.bots.iter().any(|b| b.id.as_deref() == Some(by) && b.role.as_deref() == Some("manager"))
}

/// 套用整組授權（純函式，方便測試）：驗不變式、補 track／granted_by／granted_at，沿用既有同一對（且 track 未變）的紀錄。
pub(crate) fn apply(p: &mut crate::config::ProjectCfg, inputs: &[(String, String)], by: &str, now: &str) -> anyhow::Result<()> {
    if p.mode != "expert" { return Err(reject("grant_requires_expert_project", json!({}))); }
    let mut next: Vec<ExecutorGrant> = Vec::new();
    for (specialist, executor) in inputs {
        let spec = p.bots.iter().find(|b| b.id.as_deref() == Some(specialist.as_str()))
            .ok_or_else(|| reject("grant_specialist_not_found", json!({"specialist": specialist})))?;
        if spec.role.as_deref() != Some("specialist") { return Err(reject("grant_specialist_role_invalid", json!({"specialist": specialist}))); }
        let Some(track) = spec.track.clone().filter(|t| !t.trim().is_empty()) else { return Err(reject("grant_specialist_track_missing", json!({"specialist": specialist}))) };
        let exec = p.bots.iter().find(|b| b.id.as_deref() == Some(executor.as_str()))
            .ok_or_else(|| reject("grant_executor_not_found", json!({"executor": executor})))?;
        if exec.role.as_deref() != Some("executor") { return Err(reject("grant_executor_role_invalid", json!({"executor": executor}))); }
        if let Some(prev) = next.iter().find(|g| &g.executor == executor) {
            return Err(reject("executor_already_granted", json!({"executor": executor, "specialist": prev.specialist})));
        }
        let kept = p.executor_grants.iter().find(|g| &g.specialist == specialist && &g.executor == executor && g.track == track).cloned();
        next.push(kept.unwrap_or_else(|| ExecutorGrant { specialist: specialist.clone(), executor: executor.clone(), track, granted_by: by.into(), granted_at: now.into() }));
    }
    p.executor_grants = next;
    Ok(())
}

/// `PUT /api/projects/{id}/executor-grants`：body `{"grants":[{"specialist","executor"}],"revision"?}`（或 `If-Match: <revision>`），
/// 整組取代；回 `{"grants":[…],"revision":"…"}`。版本過期回 409 `grants_revision_conflict`（附目前的授權與版本）。
pub(crate) async fn put_http(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    headers: HeaderMap,
    Json(b): Json<PutGrants>,
) -> Result<Json<Value>, LcError> {
    db::project(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
    let by = granted_by(&app, &principal, &id).await?;
    let if_match = headers.get(axum::http::header::IF_MATCH).and_then(|v| v.to_str().ok()).map(|v| v.trim().trim_matches('"').to_string()).filter(|v| !v.is_empty() && v != "*");
    let expected = b.revision.clone().map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).or(if_match);
    let inputs: Vec<(String, String)> = b.grants.iter().map(|g| (g.specialist.trim().to_string(), g.executor.trim().to_string())).collect();
    let now = db::now();
    let pid = id.clone();
    #[cfg(test)]
    crate::lifecycle::race_point::hit("executor_grants_put_locked", &id).await;
    let out = crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
        let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).ok_or_else(|| anyhow::anyhow!("no-project"))?;
        if !still_manager(p, &by) { return Err(anyhow::Error::new(GrantRejected::Forbidden(manager_only()))); }
        let current = revision(&p.executor_grants);
        if expected.as_deref().is_some_and(|r| r != current) {
            return Err(anyhow::Error::new(GrantRejected::Stale(json!({"error": "conflict", "reason": "grants_revision_conflict", "revision": current, "grants": p.executor_grants}))));
        }
        let before = p.executor_grants.clone();
        apply(p, &inputs, &by, &now)?;
        Ok(before)
    }).await;
    let before = match out {
        Ok(g) => g,
        Err(e) => {
            match e.downcast_ref::<GrantRejected>() {
                Some(GrantRejected::Bad(body)) => return Err(LcError::BadValue(body.clone())),
                Some(GrantRejected::Forbidden(body)) => return Err(LcError::Forbidden(body.clone())),
                Some(GrantRejected::Stale(body)) => return Err(LcError::Conflict(body.clone())),
                None => {}
            }
            if e.to_string() == "no-project" { return Err(LcError::NotFound("project".into())); }
            return Err(crate::api::projection_err(e));
        }
    };
    // 投影後（clear_invalid_grants 也跑過）才是真正生效的授權。
    let grants = db::project(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.map(|p| p.executor_grants()).unwrap_or_default();
    let revoked: Vec<ExecutorGrant> = before.into_iter().filter(|g| !grants.iter().any(|n| n.specialist == g.specialist && n.executor == g.executor)).collect();
    withdraw_revoked(&app, &revoked).await;
    tracing::info!(project_id = %id, grants = grants.len(), revoked = revoked.len(), http = %crate::config_audit::http_caller(), "executor grants replaced");
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok(Json(json!({"grants": grants, "revision": revision(&grants)})))
}

/// 撤銷後把「已排隊、還沒認領」且送件者是被撤專家、目標是被撤執行者的派工 turn（`queue_class='relay'`）收回：
/// 撤回 CAS 與請求狀態同一交易（`relay_requests::withdraw_turn`），被 flush 先認領的就算已開始，不中止、不標失敗。
pub(crate) async fn withdraw_revoked(app: &Arc<App>, revoked: &[ExecutorGrant]) {
    for g in revoked {
        let turns: Vec<String> = match sqlx::query_scalar("SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status='queued' AND t.queue_class='relay' AND EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.role='user' AND m.relay_from=?) ORDER BY t.created_at")
            .bind(&g.executor).bind(&g.specialist).fetch_all(&app.db).await {
            Ok(t) => t,
            Err(e) => { tracing::warn!(error = ?e, executor = %g.executor, "撤銷授權：查排隊中的派工失敗；送達時仍會重驗"); continue; }
        };
        for turn in turns {
            let d = match crate::runners::relay_requests::denial(app, &turn).await {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => { tracing::warn!(error = ?e, turn = %turn, "撤銷授權：重驗排隊中的派工失敗；送達時仍會重驗"); continue; }
            };
            match crate::runners::relay_requests::withdraw_turn(app, &turn, &d).await {
                Ok(true) => tracing::info!(turn = %turn, specialist = %g.specialist, executor = %g.executor, "撤銷授權：收回排隊中的派工"),
                Ok(false) => {}
                Err(e) => tracing::warn!(error = ?e, turn = %turn, "撤銷授權：收回排隊中的派工失敗；送達時仍會重驗"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_policy::{relay_prompt_allowed, Admission, RelayDenied};

    struct Fx { env: crate::testing::Env, manager: db::Bot, spec: db::Bot, spec2: db::Bot, exec: db::Bot, exec2: db::Bot, outsider: db::Bot, other_mgr: db::Bot }

    async fn set_role(env: &crate::testing::Env, bot: &db::Bot, role: &str, track: Option<&str>) {
        let (id, role, track) = (bot.id.clone(), role.to_string(), track.map(str::to_string));
        crate::projection::update_and_project(&env.app.cfg, &env.app.db, move |cfg| {
            for p in &mut cfg.projects { for b in &mut p.bots { if b.id.as_deref() == Some(id.as_str()) { b.role = Some(role.clone()); b.track = track.clone(); } } }
            Ok(())
        }).await.unwrap();
    }

    fn bot_cfg(name: &str, role: &str, track: Option<&str>) -> crate::config::BotCfg {
        crate::config::BotCfg { id: Some(db::ulid()), name: name.into(), kind: "claude".into(), role: Some(role.into()), track: track.map(str::to_string), ..Default::default() }
    }

    /// 專家專案（config 為真相）：管理者、兩位專家（track db／ui）、兩顆執行者；另一個一般專案有管理者與執行者。
    async fn fixture() -> Fx {
        let env = crate::testing::env().await;
        let other_path = env.dir.join("other");
        std::fs::create_dir_all(&other_path).unwrap();
        let bots = vec![
            bot_cfg("grant-manager", "manager", None), bot_cfg("grant-spec", "specialist", Some("db")), bot_cfg("grant-spec2", "specialist", Some("ui")),
            bot_cfg("grant-exec", "executor", None), bot_cfg("grant-exec2", "executor", None),
        ];
        let other_bots = vec![bot_cfg("grant-other-mgr", "manager", None), bot_cfg("grant-outsider", "executor", None)];
        let ids: Vec<String> = bots.iter().chain(&other_bots).map(|b| b.id.clone().unwrap()).collect();
        let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
        crate::projection::update_and_project(&env.app.cfg, &env.app.db, move |cfg| {
            cfg.projects.push(crate::config::ProjectCfg { id: Some(pid), path: repo, label: "proj".into(), mode: "expert".into(), host: "local".into(), bots, ..Default::default() });
            cfg.projects.push(crate::config::ProjectCfg { id: Some(db::ulid()), path: other_path.to_string_lossy().to_string(), label: "other".into(), mode: "expert".into(), host: "local".into(), bots: other_bots, ..Default::default() });
            Ok(())
        }).await.unwrap();
        let mut out = Vec::new();
        for id in &ids {
            let bot = db::bot(&env.app.db, id).await.unwrap().unwrap();
            let run = crate::testing::fake_run(&env.app, id).await;
            sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:{}", bot.project_id, bot.role.as_deref().unwrap())).bind(run).execute(&env.app.db).await.unwrap();
            out.push(bot);
        }
        let mut it = out.into_iter();
        let mut next = || it.next().unwrap();
        Fx { manager: next(), spec: next(), spec2: next(), exec: next(), exec2: next(), other_mgr: next(), outsider: next(), env }
    }

    async fn put(fx: &Fx, principal: RequestPrincipal, grants: &[(&db::Bot, &db::Bot)]) -> Result<Value, LcError> {
        let body = PutGrants { grants: grants.iter().map(|(s, e)| GrantInput { specialist: s.id.clone(), executor: e.id.clone() }).collect(), revision: None };
        put_http(State(fx.env.app.clone()), Path(fx.env.project_id.clone()), Extension(principal), HeaderMap::new(), Json(body)).await.map(|j| j.0)
    }

    async fn stored(fx: &Fx) -> Vec<ExecutorGrant> {
        db::project(&fx.env.app.db, &fx.env.project_id).await.unwrap().unwrap().executor_grants()
    }

    fn denied(r: Result<(), RelayDenied>) -> String {
        match r { Err(RelayDenied::Denied(m)) => m, other => panic!("expected denial, got {other:?}") }
    }

    #[tokio::test]
    async fn specialist_reaches_only_the_executor_granted_to_it() {
        let fx = fixture().await;
        let app = &fx.env.app;
        // 未授權：擋下，訊息寫目標角色並提示授權。
        let msg = denied(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await);
        assert!(msg.starts_with("executor 只接受 manager 派工") && msg.contains("agm grant") && msg.contains("grant-manager"), "{msg}");
        // 專家對專家：寫 specialist，不提授權。
        let msg = denied(relay_prompt_allowed(app, &fx.spec.id, &fx.spec2.id, Admission::NewRequest).await);
        assert!(msg.starts_with("specialist 只接受 manager 派工") && !msg.contains("agm grant"), "{msg}");

        let v = put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        assert_eq!(v["grants"][0]["track"], "db");
        assert_eq!(v["grants"][0]["granted_by"], "user");
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_ok());
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::Deliver).await.is_ok());
        // 只授權那一顆：別的執行者、別的專家仍擋。
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec2.id, Admission::NewRequest).await.is_err());
        assert!(relay_prompt_allowed(app, &fx.spec2.id, &fx.exec.id, Admission::NewRequest).await.is_err());
        // /api/state 帶授權。
        let state = crate::api::state_json(app).await.unwrap();
        let proj = state["projects"].as_array().unwrap().iter().find(|p| p["id"] == fx.env.project_id.as_str()).unwrap().clone();
        assert_eq!(proj["executor_grants"][0]["executor"], fx.exec.id.as_str());

        // 錯 track：DB 上 track 被改（config 還沒跟上）也擋。
        sqlx::query("UPDATE bots SET track='ui' WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_err());
        sqlx::query("UPDATE bots SET track='db' WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();

        // 跨專案：就算 DB 裡被塞了一筆指向別專案執行者的授權，也不放行。
        let forged = serde_json::to_string(&vec![ExecutorGrant { specialist: fx.spec.id.clone(), executor: fx.outsider.id.clone(), track: "db".into(), granted_by: "user".into(), granted_at: db::now() }]).unwrap();
        let saved: String = sqlx::query_scalar("SELECT executor_grants_json FROM projects WHERE id=?").bind(&fx.env.project_id).fetch_one(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET executor_grants_json=? WHERE id=?").bind(&forged).bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.outsider.id, Admission::NewRequest).await.is_err());
        sqlx::query("UPDATE projects SET executor_grants_json=? WHERE id=?").bind(&saved).bind(&fx.env.project_id).execute(&app.db).await.unwrap();

        // 撤銷：之後送達（Deliver）也擋。
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        assert!(stored(&fx).await.is_empty());
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::Deliver).await.is_err());
    }

    #[tokio::test]
    async fn revoked_grant_fails_a_pending_request_at_delivery() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,?,?,'[]','pending',?)")
            .bind(&rid).bind(&fx.spec.id).bind(&fx.exec.id).bind("grant-crid").bind("do it").bind(db::now()).execute(&app.db).await.unwrap();
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        crate::runners::relay_requests::sweep(app).await;
        let (state, error): (String, Option<String>) = sqlx::query_as("SELECT state,error FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "failed");
        assert!(error.unwrap_or_default().contains("只接受 manager 派工"));
    }

    #[tokio::test]
    async fn put_permissions_and_invariants() {
        let fx = fixture().await;
        // 非 manager bot（專家自己、執行者）403；child 403。
        for caller in [&fx.spec, &fx.exec] {
            assert!(matches!(put(&fx, RequestPrincipal::Bot(caller.id.clone()), &[(&fx.spec, &fx.exec)]).await, Err(LcError::Forbidden(_))));
        }
        assert!(matches!(put(&fx, RequestPrincipal::Child { child_id: fx.spec.id.clone(), parent_bot_id: fx.manager.id.clone() }, &[]).await, Err(LcError::Forbidden(_))));
        // manager 可設，granted_by=manager id；同一對再送沿用 granted_at。
        let v = put(&fx, RequestPrincipal::Bot(fx.manager.id.clone()), &[(&fx.spec, &fx.exec)]).await.unwrap();
        assert_eq!(v["grants"][0]["granted_by"], fx.manager.id.as_str());
        let at = v["grants"][0]["granted_at"].clone();
        let v = put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec), (&fx.spec2, &fx.exec2)]).await.unwrap();
        assert_eq!(v["grants"][0]["granted_at"], at);
        assert_eq!(v["grants"][0]["granted_by"], fx.manager.id.as_str());
        // 排他：同一顆執行者給兩位專家 400，原授權不動。
        let reason = |r: Result<Value, LcError>| match r { Err(LcError::BadValue(b)) => b["reason"].as_str().unwrap().to_string(), other => panic!("{other:?}") };
        assert_eq!(reason(put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec), (&fx.spec2, &fx.exec)]).await), "executor_already_granted");
        assert_eq!(stored(&fx).await.len(), 2);
        // 角色不合：專家位置放執行者、執行者位置放專家。
        assert_eq!(reason(put(&fx, RequestPrincipal::User, &[(&fx.exec, &fx.exec2)]).await), "grant_specialist_role_invalid");
        assert_eq!(reason(put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.spec2)]).await), "grant_executor_role_invalid");
        // 跨專案：別專案的 bot 不在本專案 config 裡。
        assert_eq!(reason(put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.outsider)]).await), "grant_executor_not_found");
        // 跨專案的 manager 不能設本專案。
        assert!(matches!(put(&fx, RequestPrincipal::Bot(fx.other_mgr.id.clone()), &[]).await, Err(LcError::Forbidden(_))));
    }

    #[tokio::test]
    async fn grants_are_cleared_when_roles_change_or_bots_go_away() {
        let fx = fixture().await;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec), (&fx.spec2, &fx.exec2)]).await.unwrap();
        // 專家換 track → 它的授權清掉，另一筆不動。
        set_role(&fx.env, &fx.spec, "specialist", Some("api")).await;
        let g = stored(&fx).await;
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].specialist, fx.spec2.id);
        // 執行者改角色 → 清掉。
        set_role(&fx.env, &fx.exec2, "advisor", None).await;
        assert!(stored(&fx).await.is_empty());
        // 刪掉專家 → 清掉。
        set_role(&fx.env, &fx.exec2, "executor", None).await;
        put(&fx, RequestPrincipal::User, &[(&fx.spec2, &fx.exec2)]).await.unwrap();
        let sid = fx.spec2.id.clone();
        crate::projection::update_and_project(&fx.env.app.cfg, &fx.env.app.db, move |cfg| {
            for p in &mut cfg.projects { p.bots.retain(|b| b.id.as_deref() != Some(sid.as_str())); }
            Ok(())
        }).await.unwrap();
        assert!(stored(&fx).await.is_empty());
        // 專案離開 expert → 清掉。
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let pid = fx.env.project_id.clone();
        crate::projection::update_and_project(&fx.env.app.cfg, &fx.env.app.db, move |cfg| {
            cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).unwrap().mode = "parent_child".into();
            Ok(())
        }).await.unwrap();
        assert!(stored(&fx).await.is_empty());
    }

    async fn queued_turn_from(fx: &Fx, from: &db::Bot, to: &db::Bot, crid: &str, status: &str) -> String {
        queued_turn_class(fx, from, to, crid, status, "relay").await
    }

    /// 跟 daemon 建 turn 一樣寫 `queue_class`（bot 派工＝relay、daemon 結果通知＝daemon）。
    async fn queued_turn_class(fx: &Fx, from: &db::Bot, to: &db::Bot, crid: &str, status: &str, class: &str) -> String {
        let app = &fx.env.app;
        let conv = db::conversation_id(&app.db, &to.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,client_request_id,queue_class,created_at) VALUES(?,?,'web',?,'pending',?,?,?)")
            .bind(&turn).bind(&conv).bind(status).bind(crid).bind(class).bind(db::now()).execute(&app.db).await.unwrap();
        // 跟正式派工一樣記下核權當下的依據（`relay_prompt_basis`）；被擋的組合沒有依據。
        let auth = if class == "relay" { crate::relay_policy::relay_prompt_basis(app, &from.id, &to.id, Admission::Deliver).await.ok().flatten() } else { None };
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,relay_from,relay_auth,created_at) VALUES(?,?,?,'user','do it','web',?,?,?)")
            .bind(db::ulid()).bind(&conv).bind(&turn).bind(&from.id).bind(auth).bind(db::now()).execute(&app.db).await.unwrap();
        turn
    }

    async fn turn_status(fx: &Fx, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(&fx.env.app.db).await.unwrap()
    }

    /// Astra review #1：沒有 relay_requests 的 queue_if_busy 派工，撤銷後在送達認領前重驗；PUT 撤銷時直接收回。
    #[tokio::test]
    async fn revoking_withdraws_plain_queued_prompts_but_not_started_ones() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let queued = queued_turn_from(&fx, &fx.spec, &fx.exec, "plain-q", "queued").await;
        let started = queued_turn_from(&fx, &fx.spec, &fx.exec, "plain-s", "in_flight").await;
        // 授權還在：flush 的重驗放行。
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &queued).await.unwrap(), None);
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        assert_eq!(turn_status(&fx, &queued).await, "failed", "排隊中的派工被收回");
        assert_eq!(turn_status(&fx, &started).await, "in_flight", "已開始的不中止");
        let note: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&queued).fetch_one(&app.db).await.unwrap();
        assert!(note.contains("只接受 manager 派工"), "{note}");

        // 其他撤銷來源（例如專家改 track）不經 PUT：flush 認領前的重驗擋下。
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let later = queued_turn_from(&fx, &fx.spec, &fx.exec, "plain-q2", "queued").await;
        set_role(&fx.env, &fx.spec, "specialist", Some("api")).await;
        assert!(crate::runners::relay_requests::queue_denial(app, &later).await.unwrap().is_some(), "track 變了：送達前擋下");
        assert_eq!(turn_status(&fx, &later).await, "failed", "queue_denial 當場撤回");
        // manager 的排隊派工照舊不受影響。
        let from_mgr = queued_turn_from(&fx, &fx.manager, &fx.exec, "mgr-q", "queued").await;
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &from_mgr).await.unwrap(), None);
    }

    /// durable（expect_result）排隊中的 turn：撤銷時 turn 收回、請求標 failed。
    #[tokio::test]
    async fn revoking_withdraws_durable_queued_turns() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,'durable','do it','[]','pending',?)")
            .bind(&rid).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).execute(&app.db).await.unwrap();
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{rid}"), "queued").await;
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        assert_eq!(turn_status(&fx, &turn).await, "failed");
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "failed");
    }

    /// Astra review #3：鎖外預檢通過後 manager 被撤職，鎖內重驗擋下，授權不變。
    #[tokio::test]
    async fn demoted_manager_cannot_finish_an_in_flight_put() {
        let fx = fixture().await;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let before = stored(&fx).await;
        let (app, mid, new_mgr) = (fx.env.app.clone(), fx.manager.id.clone(), fx.exec2.id.clone());
        crate::lifecycle::race_point::arm("executor_grants_put_locked", &fx.env.project_id, move || async move {
            crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
                // 換人當 manager（expert 專案恰好一位 manager）：原 manager 改 advisor、另一顆執行者升 manager。
                for p in &mut cfg.projects { for b in &mut p.bots {
                    if b.id.as_deref() == Some(mid.as_str()) { b.role = Some("advisor".into()); }
                    else if b.id.as_deref() == Some(new_mgr.as_str()) { b.role = Some("manager".into()); }
                } }
                Ok(())
            }).await.unwrap();
        });
        let r = put(&fx, RequestPrincipal::Bot(fx.manager.id.clone()), &[]).await;
        assert!(matches!(&r, Err(LcError::Forbidden(b)) if b["reason"] == "grant_manager_only"), "{r:?}");
        assert_eq!(stored(&fx).await.len(), before.len());
        assert_eq!(stored(&fx).await[0].granted_at, before[0].granted_at);
    }

    /// Astra review #5：revision（body 或 If-Match）過期回 409，授權不動；對的 revision 照常寫入。
    #[tokio::test]
    async fn stale_revision_is_rejected() {
        let fx = fixture().await;
        let v = put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let rev = v["revision"].as_str().unwrap().to_string();
        let state = crate::api::state_json(&fx.env.app).await.unwrap();
        let proj = state["projects"].as_array().unwrap().iter().find(|p| p["id"] == fx.env.project_id.as_str()).unwrap().clone();
        assert_eq!(proj["grants_revision"], rev.as_str(), "GET 與 PUT 回應同一個版本");
        let send = |revision: Option<String>, header: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(h) = header { headers.insert(axum::http::header::IF_MATCH, h.parse().unwrap()); }
            let body = PutGrants { grants: vec![GrantInput { specialist: fx.spec2.id.clone(), executor: fx.exec2.id.clone() }], revision };
            put_http(State(fx.env.app.clone()), Path(fx.env.project_id.clone()), Extension(RequestPrincipal::User), headers, Json(body))
        };
        // 別人先改了（撤掉 spec→exec）：手上的舊 revision 過期。
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        let r = send(Some(rev.clone()), None).await;
        assert!(matches!(&r, Err(LcError::Conflict(b)) if b["reason"] == "grants_revision_conflict" && b["grants"].as_array().is_some_and(Vec::is_empty)), "{r:?}");
        let r = send(None, Some(&format!("\"{rev}\""))).await;
        assert!(matches!(&r, Err(LcError::Conflict(_))), "If-Match 也檢查");
        assert!(stored(&fx).await.is_empty(), "409 不寫入");
        let fresh = revision(&[]);
        let v = send(Some(fresh), None).await.unwrap().0;
        assert_eq!(v["grants"][0]["executor"], fx.exec2.id.as_str());
    }

    /// Astra review #2：正式刪除（`DELETE /api/bots/:id` → delete_from_config）也清授權，config 與 DB 一致。
    #[tokio::test]
    async fn real_bot_delete_clears_its_grants() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec), (&fx.spec2, &fx.exec2)]).await.unwrap();
        crate::api::delete_bot(State(app.clone()), Path(fx.exec.id.clone())).await.unwrap();
        let g = stored(&fx).await;
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].executor, fx.exec2.id);
        let cfg = app.cfg.get().await;
        let p = cfg.projects.iter().find(|p| p.id.as_deref() == Some(fx.env.project_id.as_str())).unwrap();
        assert_eq!(p.executor_grants.len(), 1, "config.toml 也清掉");
        let state = crate::api::state_json(app).await.unwrap();
        let proj = state["projects"].as_array().unwrap().iter().find(|p| p["id"] == fx.env.project_id.as_str()).unwrap().clone();
        assert_eq!(proj["executor_grants"].as_array().unwrap().len(), 1, "側欄依據的 state 也不再帶已刪執行者");
    }

    /// Astra review #2：啟動投影時手改出來的重複／失效授權被清掉（只留每顆執行者的第一筆），不原樣進 DB。
    #[tokio::test]
    async fn startup_projection_drops_duplicate_and_stale_grants() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let (pid, spec, spec2, exec, exec2) = (fx.env.project_id.clone(), fx.spec.id.clone(), fx.spec2.id.clone(), fx.exec.id.clone(), fx.exec2.id.clone());
        // 模擬手改 config.toml：不經 update_and_project。
        app.cfg.update(move |cfg| {
            let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).unwrap();
            let g = |s: &str, e: &str, track: &str| ExecutorGrant { specialist: s.into(), executor: e.into(), track: track.into(), granted_by: "user".into(), granted_at: "2026-11-06T00:00:00Z".into() };
            p.executor_grants = vec![g(&spec, &exec, "db"), g(&spec2, &exec, "ui"), g(&spec2, &exec2, "stale-track")];
            Ok(())
        }).await.unwrap();
        crate::projection::project_config_at_startup(&app.cfg, &app.db, false).await.unwrap();
        let g = stored(&fx).await;
        assert_eq!(g.len(), 1, "{g:?}");
        assert_eq!((g[0].specialist.as_str(), g[0].executor.as_str()), (fx.spec.id.as_str(), fx.exec.id.as_str()));
        let cfg = app.cfg.get().await;
        assert_eq!(cfg.projects.iter().find(|p| p.id.as_deref() == Some(fx.env.project_id.as_str())).unwrap().executor_grants.len(), 1);
        // DB 被直接塞重複時，派工只認第一筆（排他）。
        let dup = serde_json::to_string(&vec![
            ExecutorGrant { specialist: fx.spec.id.clone(), executor: fx.exec.id.clone(), track: "db".into(), granted_by: "user".into(), granted_at: db::now() },
            ExecutorGrant { specialist: fx.spec2.id.clone(), executor: fx.exec.id.clone(), track: "ui".into(), granted_by: "user".into(), granted_at: db::now() },
        ]).unwrap();
        sqlx::query("UPDATE projects SET executor_grants_json=? WHERE id=?").bind(&dup).bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_ok());
        assert!(relay_prompt_allowed(app, &fx.spec2.id, &fx.exec.id, Admission::NewRequest).await.is_err());
    }

    /// 授權不繞過 manager_use=false：停用的執行者就算有授權也擋。
    #[tokio::test]
    async fn grant_does_not_bypass_member_disabled() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        sqlx::query("UPDATE bots SET manager_use=0 WHERE id=?").bind(&fx.exec.id).execute(&app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await, Err(RelayDenied::Denied(r)) if r == "member_disabled"));
    }

    /// 兩顆執行者設成同一組（kind, model）成群集，回群集名稱（config 為真相，之後的投影不會把 model 洗掉）。
    async fn make_cluster(fx: &Fx) -> String {
        let ids = [fx.exec.id.clone(), fx.exec2.id.clone()];
        crate::projection::update_and_project(&fx.env.app.cfg, &fx.env.app.db, move |cfg| {
            for p in &mut cfg.projects { for b in &mut p.bots { if ids.iter().any(|i| b.id.as_deref() == Some(i.as_str())) { b.model = Some("claude-haiku-5-5".into()); } } }
            Ok(())
        }).await.unwrap();
        let clusters = crate::expert_cluster::project_clusters(&fx.env.app, &fx.env.project_id).await.unwrap();
        let c = clusters.iter().find(|c| c.members.iter().any(|m| m.id == fx.exec.id) && c.members.iter().any(|m| m.id == fx.exec2.id)).expect("兩顆執行者成群集");
        c.name.clone()
    }

    /// B 的專家群集派工：真實 resolve 只挑授權成員；排隊中的群集 turn 撤銷時收回（請求 failed、原因是授權）；
    /// 還沒認領成員的 pending 群集請求，下一輪 reconcile 因「沒有授權給送件者的成員」標 failed、不改派。
    #[tokio::test]
    async fn revoking_withdraws_specialist_cluster_dispatches() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let name = make_cluster(&fx).await;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let resolved = crate::api::resolve_cluster_target(app, &fx.spec.id, &name).await.unwrap().0;
        assert_eq!(resolved["to_bot_id"], fx.exec.id.as_str(), "專家只拿到授權成員：{resolved}");
        let queued_req = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'cl-q','do it','[]','pending',?,?)")
            .bind(&queued_req).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).bind(&name).execute(&app.db).await.unwrap();
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{queued_req}"), "queued").await;
        let pending_req = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'cl-p','do it','[]','pending',?,?)")
            .bind(&pending_req).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).bind(&name).execute(&app.db).await.unwrap();
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        assert_eq!(turn_status(&fx, &turn).await, "failed", "排隊中的群集派工被收回");
        assert!(crate::expert_cluster::project_clusters(app, &fx.env.project_id).await.unwrap().iter().any(|c| c.name == name), "群集還在：失敗不是因為群集消失");
        let db = fx.env.app.db.clone();
        let row = |rid: String| { let db = db.clone(); async move { sqlx::query_as::<_, (String, String, Option<String>)>("SELECT state,to_bot_id,error FROM relay_requests WHERE id=?").bind(rid).fetch_one(&db).await.unwrap() } };
        // 通知流程之後會改寫 error（送達確認等），所以在 sweep 前看撤銷當下寫的原因。
        let (state, to, error) = row(queued_req.clone()).await;
        assert_eq!((state.as_str(), to.as_str()), ("failed", fx.exec.id.as_str()));
        assert!(error.as_deref().unwrap_or_default().contains("只接受 manager 派工"), "{error:?}");
        // reconcile 的改派那一步：沒有授權給送件者的成員 → failed，不改派。
        assert_eq!(crate::expert_cluster::claim_target(app, &pending_req, &fx.exec.id, &name).await.unwrap(), None);
        let (state, to, error) = row(pending_req.clone()).await;
        assert_eq!((state.as_str(), to.as_str()), ("failed", fx.exec.id.as_str()), "不改派給未授權的 exec2");
        assert!(error.as_deref().unwrap_or_default().contains("no member granted"), "{error:?}");
        crate::runners::relay_requests::sweep(app).await;
        assert_eq!(row(pending_req.clone()).await.1, fx.exec.id, "sweep 之後也沒改派");
    }

    /// Astra review2 #1：專家忙碌時，執行者回來的 daemon 結果通知（queue_class=daemon，relay_from=執行者）先排隊，
    /// 授權仍有效也好、撤銷後也好，都不被當成新派工撤回；撤回通知本身也不會再被撤回。
    #[tokio::test]
    async fn queued_daemon_notices_to_a_busy_specialist_are_not_rechecked_as_dispatches() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let notice = queued_turn_class(&fx, &fx.exec, &fx.spec, "relay-result:x:r0", "queued", "daemon").await;
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &notice).await.unwrap(), None, "授權有效：通知照送");
        // 撤銷一筆排隊派工 → 產生撤回紀錄 → reconcile 給專家排一則 daemon 通知。
        let dispatched = queued_turn_from(&fx, &fx.spec, &fx.exec, "to-withdraw", "queued").await;
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        assert_eq!(turn_status(&fx, &dispatched).await, "failed");
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &notice).await.unwrap(), None, "撤銷後：結果通知仍不重驗");
        assert_eq!(turn_status(&fx, &notice).await, "queued");
        let (rid, state, error): (String, String, Option<String>) = sqlx::query_as("SELECT id,state,error FROM relay_requests WHERE client_request_id=?").bind(format!("withdrawn:{dispatched}")).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "failed", "一般派工的撤回記成 failed 請求，由 reconcile 通知送件者");
        assert!(error.unwrap_or_default().contains("只接受 manager 派工"));
        // 專家在跑但忙：撤回通知排進它的 daemon 佇列（佔用的那則先收掉，讓出 daemon 名額）。
        assert!(crate::lifecycle::revoke_queued_turn(app, &notice, "test: 讓出 daemon 名額").await.unwrap());
        sqlx::query("UPDATE runs SET state='running',agent_status='working' WHERE bot_id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        crate::runners::relay_requests::sweep(app).await;
        let withdrawal_notice: Option<String> = sqlx::query_scalar("SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.client_request_id=?")
            .bind(&fx.spec.id).bind(format!("relay-result:{rid}:r0")).fetch_optional(&app.db).await.unwrap();
        let t = withdrawal_notice.expect("撤回通知排進專家的 daemon 佇列");
        assert_eq!(turn_status(&fx, &t).await, "queued");
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &t).await.unwrap(), None, "撤回通知不會再被撤回");
        assert_eq!(turn_status(&fx, &t).await, "queued");
        let text: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='user'").bind(&t).fetch_one(&app.db).await.unwrap();
        assert!(text.contains("請求未投遞"), "{text}");
    }

    /// 直接送出（目標閒著）：真的走 lifecycle 建 in_flight turn 的那一句。
    async fn dispatch_now(fx: &Fx, crid: &str) -> Result<crate::lifecycle::PromptOut, LcError> {
        let auth = crate::relay_policy::relay_prompt_basis(&fx.env.app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.ok().flatten();
        dispatch_with(fx, crid, auth.as_deref()).await
    }

    /// 帶著指定的依據直接送出（模擬「核權時記下依據、之後狀態才變」）。
    async fn dispatch_with(fx: &Fx, crid: &str, auth: Option<&str>) -> Result<crate::lifecycle::PromptOut, LcError> {
        // 會反應的假 pane：輸入框讀得到、是空的（建 turn 之前的檢查都會過）。
        fx.env.herdr.live_pane(&format!("pane-{}", fx.exec.id), crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        // 沒掛 hook：不等 SessionStart 的 transcript（否則建 turn 之前就回 transcript_not_ready，走不到要測的那一句）。
        sqlx::query("UPDATE bots SET inject_hooks=0 WHERE id=?").bind(&fx.exec.id).execute(&fx.env.app.db).await.unwrap();
        crate::lifecycle::prompt_from_api_queue_if_busy(&fx.env.app, &fx.exec.id, "do it", crid, &[], crate::lifecycle::RelaySrc::trusted(Some(&fx.spec.id)).with_auth(auth), false, None).await
    }

    async fn claim(fx: &Fx, turn: &str) -> crate::lifecycle::turn_controller::Outcome {
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&fx.exec.id).fetch_one(&fx.env.app.db).await.unwrap();
        crate::lifecycle::turn_controller::claim_queued(&mut fx.env.app.db.acquire().await.unwrap(), turn, &run).await.unwrap()
    }

    fn grant_changed(r: &Result<crate::lifecycle::PromptOut, LcError>) -> bool {
        matches!(r, Err(LcError::Conflict(b)) if b["reason"] == "relay_grant_changed" && b["retryable"] == true && b["sent"] == false)
    }

    /// Astra review3 #1：核權通過後暫停（不論多久、沒有鎖可逾時），期間撤銷（PUT）或換 track，恢復後：
    /// 直接送出寫不進 in_flight（可重試 409、沒有 turn）；已排隊的認領不到，flush 重驗時撤回。不會「撤銷成功後才開始執行」。
    #[tokio::test]
    async fn a_dispatch_checked_before_revocation_cannot_start_after_it() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let turns = |crid: &'static str| { let db = app.db.clone(); let exec = fx.exec.id.clone(); async move {
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.client_request_id=?").bind(exec).bind(crid).fetch_one(&db).await.unwrap() } };
        for revoke_by_track in [false, true] {
            put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
            assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_ok(), "核權通過");
            // 已經排好隊的一筆（核權通過、撤銷前建好）。
            let queued = queued_turn_from(&fx, &fx.spec, &fx.exec, if revoke_by_track { "q-track" } else { "q-put" }, "queued").await;
            // 暫停期間撤銷：完成、立即生效，不等任何人。
            if revoke_by_track { set_role(&fx.env, &fx.spec, "specialist", Some("api")).await } else { put(&fx, RequestPrincipal::User, &[]).await.unwrap(); }
            assert!(stored(&fx).await.is_empty());
            // 恢復：直接送出被同一句 SQL 擋下。
            let crid = if revoke_by_track { "now-track" } else { "now-put" };
            let r = dispatch_now(&fx, crid).await;
            assert!(grant_changed(&r), "{r:?}");
            assert_eq!(turns(crid).await, 0, "沒有建 turn、沒有送出");
            // 排隊的那筆：PUT 已當場收回；換 track 的情況認領不到，flush 重驗撤回。
            if revoke_by_track {
                assert!(matches!(claim(&fx, &queued).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)));
                assert_eq!(turn_status(&fx, &queued).await, "queued");
                assert!(crate::runners::relay_requests::queue_denial(app, &queued).await.unwrap().is_some());
            }
            assert_eq!(turn_status(&fx, &queued).await, "failed");
            set_role(&fx.env, &fx.spec, "specialist", Some("db")).await;
        }
        // 授權有效時同一條路照常建 in_flight turn（條件只擋撤銷）。
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let r = dispatch_now(&fx, "now-ok").await;
        assert!(!grant_changed(&r), "{r:?}");
        assert_eq!(turns("now-ok").await, 1);
        // manager 派工不受影響（沒有授權也能送）。
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        let q = queued_turn_from(&fx, &fx.manager, &fx.exec2, "mgr-claim", "queued").await;
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=?").bind(&fx.exec2.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(crate::lifecycle::turn_controller::claim_queued(&mut app.db.acquire().await.unwrap(), &q, &run).await.unwrap(), crate::lifecycle::turn_controller::Outcome::Applied);
    }

    /// Astra review3 #2：空授權快照 → 新增授權 → 派工排隊 → 換 track 的交錯：沒有鎖集合可漏，認領那一句依當下授權判斷。
    #[tokio::test]
    async fn grant_added_then_dispatch_then_track_change_still_blocks_the_claim() {
        let fx = fixture().await;
        let app = &fx.env.app;
        assert!(stored(&fx).await.is_empty(), "起點：沒有授權");
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_err());
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_ok());
        let queued = queued_turn_from(&fx, &fx.spec, &fx.exec, "interleave", "queued").await;
        set_role(&fx.env, &fx.spec, "specialist", Some("api")).await;
        assert!(stored(&fx).await.is_empty());
        assert!(matches!(claim(&fx, &queued).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)), "認領不到");
        assert!(grant_changed(&dispatch_now(&fx, "interleave-now").await));
        // 再授權（新 track）之後同一筆排隊的又可以認領：條件看當下，不看歷史。
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        assert_eq!(claim(&fx, &queued).await, crate::lifecycle::turn_controller::Outcome::Applied);
    }

    /// Astra review3 #3：`withdrawn:` 是保留前綴（外部 prompt 拒用）；同鍵已有不相干的紀錄時撤回整筆退回（turn 留在佇列、不漏通知），
    /// 同一個撤回事件重來則接受既有紀錄。
    #[tokio::test]
    async fn withdrawal_record_key_collisions_are_not_silently_ignored() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, "collide", "queued").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,?,'unrelated','[]','pending',?)")
            .bind(db::ulid()).bind(&fx.spec.id).bind(&fx.exec.id).bind(format!("withdrawn:{turn}")).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET executor_grants_json='[]' WHERE id=?").bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        let d = crate::runners::relay_requests::denial(app, &turn).await.unwrap().expect("授權沒了");
        assert!(crate::runners::relay_requests::withdraw_turn(app, &turn, &d).await.is_err(), "碰撞：不靜默略過");
        assert_eq!(turn_status(&fx, &turn).await, "queued", "整個交易退回，turn 沒被撤");
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE client_request_id=?").bind(format!("withdrawn:{turn}")).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "pending", "不誤用既有請求");
        // 同一個撤回事件（同 turn）已記過：接受。
        let turn2 = queued_turn_from(&fx, &fx.spec, &fx.exec2, "same-event", "queued").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,to_turn_id,state,created_at) VALUES(?,?,?,?,'','[]',?,'failed',?)")
            .bind(db::ulid()).bind(&fx.spec.id).bind(&fx.exec2.id).bind(format!("withdrawn:{turn2}")).bind(&turn2).bind(db::now()).execute(&app.db).await.unwrap();
        let d = crate::runners::relay_requests::denial(app, &turn2).await.unwrap().expect("沒有授權");
        assert!(crate::runners::relay_requests::withdraw_turn(app, &turn2, &d).await.unwrap());
        assert_eq!(turn_status(&fx, &turn2).await, "failed");
    }

    /// Astra review2 #4：flush 先認領（turn 已不是 queued），撤回輸掉 CAS：請求與結果追蹤不動、不記撤回、不通知。
    #[tokio::test]
    async fn a_withdrawal_that_loses_to_the_claim_changes_nothing() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,'claimed','do it','[]','pending',?)")
            .bind(&rid).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).execute(&app.db).await.unwrap();
        let durable = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{rid}"), "queued").await;
        // 撤銷寫入後、撤回前，flush 搶先認領（race point 插在 PUT 投影之後的撤回之前不好掛，直接在判斷與撤回之間認領）。
        sqlx::query("UPDATE projects SET executor_grants_json='[]' WHERE id=?").bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        let d = crate::runners::relay_requests::denial(app, &durable).await.unwrap().expect("授權沒了");
        sqlx::query("UPDATE turns SET status='in_flight' WHERE id=?").bind(&durable).execute(&app.db).await.unwrap();
        assert!(!crate::runners::relay_requests::withdraw_turn(app, &durable, &d).await.unwrap(), "CAS 輸了");
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "pending", "已開始的不標失敗，結果照常追蹤");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&durable).fetch_one(&app.db).await.unwrap();
        assert_eq!(notes, 0);
        // 一般派工同樣：輸掉 CAS 不記撤回紀錄。
        let plain = queued_turn_from(&fx, &fx.spec, &fx.exec, "plain-claimed", "queued").await;
        let d = crate::runners::relay_requests::denial(app, &plain).await.unwrap().expect("授權沒了");
        sqlx::query("UPDATE turns SET status='in_flight' WHERE id=?").bind(&plain).execute(&app.db).await.unwrap();
        assert!(!crate::runners::relay_requests::withdraw_turn(app, &plain, &d).await.unwrap());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_requests WHERE client_request_id=?").bind(format!("withdrawn:{plain}")).fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0);
    }

    /// 排隊 turn，訊息帶指定依據（核權當下記的），不重算。
    async fn queued_with(fx: &Fx, crid: &str, auth: Option<&str>) -> String {
        let app = &fx.env.app;
        let conv = db::conversation_id(&app.db, &fx.exec.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,client_request_id,queue_class,created_at) VALUES(?,?,'web','queued','pending',?,'relay',?)")
            .bind(&turn).bind(&conv).bind(crid).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,relay_from,relay_auth,created_at) VALUES(?,?,?,'user','do it','web',?,?,?)")
            .bind(db::ulid()).bind(&conv).bind(&turn).bind(&fx.spec.id).bind(auth).bind(db::now()).execute(&app.db).await.unwrap();
        turn
    }

    async fn expect_blocked(fx: &Fx, label: &str, auth: Option<&str>) {
        let r = dispatch_with(fx, &format!("{label}-now"), auth).await;
        assert!(grant_changed(&r), "{label} 直送：{r:?}");
        let q = queued_with(fx, &format!("{label}-q"), auth).await;
        assert!(matches!(claim(fx, &q).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)), "{label} 認領不到");
        assert!(crate::runners::relay_requests::queue_denial(&fx.env.app, &q).await.unwrap().is_some(), "{label} 重驗擋下（不因搬專案略過）");
        assert_eq!(turn_status(fx, &q).await, "failed", "{label} 撤回");
    }

    /// Astra review4 A：依授權核權通過（記下 `grant:<專案>`）之後，專家被降級、變 child、搬專案：直送與排隊認領都正向確認失敗。
    #[tokio::test]
    async fn a_grant_basis_is_rechecked_positively_after_demotion_childing_or_moving() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let basis = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        assert_eq!(basis.as_deref(), Some(format!("grant:{}", fx.env.project_id).as_str()));
        let auth = basis.as_deref();
        // 依據仍成立時照常：直送建 turn、排隊可認領。
        assert!(!grant_changed(&dispatch_with(&fx, "ok-now", auth).await));
        let ok_q = queued_with(&fx, "ok-q", auth).await;
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE client_request_id='ok-now'").bind(db::now()).execute(&app.db).await.unwrap();
        assert_eq!(claim(&fx, &ok_q).await, crate::lifecycle::turn_controller::Outcome::Applied);
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(db::now()).bind(&ok_q).execute(&app.db).await.unwrap();
        // 降級成 advisor（DB 現況；授權 JSON 原樣留著也不放行）。
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "demoted", auth).await;
        sqlx::query("UPDATE bots SET role='specialist' WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        // 變成 child。
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&fx.manager.id).bind(&fx.spec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "childed", auth).await;
        sqlx::query("UPDATE bots SET parent_bot_id=NULL WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        // 搬到別的專案（舊的 plain 重驗會因不再同專案而略過）。
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&fx.other_mgr.project_id).bind(&fx.spec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "moved", auth).await;
    }

    /// Astra review4 A：沒有依據（核權時是 manager）、之後才被改成 specialist 的送件者，仍被負向條件擋。
    #[tokio::test]
    async fn no_basis_dispatch_from_a_sender_later_made_specialist_is_blocked() {
        let fx = fixture().await;
        expect_blocked(&fx, "no-basis", None).await;
    }

    /// Astra review4 B：被指定為副管理者 ≠ 代班中。指定但沒代班的專家，授權撤銷後直送與認領都擋；
    /// 真的代班中（名下有改派來的未結請求）仍放行；代班結束後排隊的認領不到。
    #[tokio::test]
    async fn deputy_basis_requires_actually_acting() {
        let fx = fixture().await;
        let app = &fx.env.app;
        sqlx::query("UPDATE projects SET deputy_bot_id=? WHERE id=?").bind(&fx.spec.id).bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        // 指定為副管理者、靠授權派工；撤銷授權之後不因「是副管理者」就放行。
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        sqlx::query("UPDATE projects SET deputy_bot_id=? WHERE id=?").bind(&fx.spec.id).bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        let grant_auth = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        assert!(grant_auth.as_deref().is_some_and(|a| a.starts_with("grant:")));
        sqlx::query("UPDATE projects SET executor_grants_json='[]' WHERE id=?").bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_err(), "沒代班：核權就擋");
        expect_blocked(&fx, "deputy-not-acting", grant_auth.as_deref()).await;
        // 開始代班：管理者的請求改派給它。
        let redirected = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,redirected_from) VALUES(?,?,?,'rd','p','[]','pending',?,?)")
            .bind(&redirected).bind(&fx.exec2.id).bind(&fx.spec.id).bind(db::now()).bind(&fx.manager.id).execute(&app.db).await.unwrap();
        let basis = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        assert_eq!(basis.as_deref(), Some(format!("deputy:{}", fx.env.project_id).as_str()));
        assert!(!grant_changed(&dispatch_with(&fx, "acting-now", basis.as_deref()).await), "真的代班中：直送放行");
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE client_request_id='acting-now'").bind(db::now()).execute(&app.db).await.unwrap();
        let q = queued_with(&fx, "acting-q", basis.as_deref()).await;
        assert_eq!(claim(&fx, &q).await, crate::lifecycle::turn_controller::Outcome::Applied, "代班中：認領放行");
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(db::now()).bind(&q).execute(&app.db).await.unwrap();
        // 代班中排好的下一筆（每個對話只能有一筆排隊的 relay turn）。
        let q_later = queued_with(&fx, "acting-q2", basis.as_deref()).await;
        // 代班結束（改派的請求做完）：之前排的認領不到、直送也擋。
        sqlx::query("UPDATE relay_requests SET state='answered' WHERE id=?").bind(&redirected).execute(&app.db).await.unwrap();
        assert!(matches!(claim(&fx, &q_later).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)), "代班結束後認領不到");
        assert!(crate::runners::relay_requests::queue_denial(app, &q_later).await.unwrap().is_some());
        assert!(grant_changed(&dispatch_with(&fx, "acting-over-now", basis.as_deref()).await));
    }
}
