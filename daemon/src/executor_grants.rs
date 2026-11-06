//! 專家模式執行者授權（使用者 2026-11-06 方案 c）：管理者（或使用者）把執行者授權給專家，專家就能直接派工給它。
//!
//! 真相在 config.toml 的 `[[projects.executor_grants]]`（`ProjectCfg.executor_grants`），DB 的
//! `projects.executor_grants_json` 只是投影。派工時的核對在 `relay_policy::granted`；不合格的授權由
//! `ProjectCfg::clear_invalid_grants` 在每次 config 變更後清掉。授權只涵蓋派工，不給 start／restart／hire／stop／改角色／再授權。
use std::sync::Arc;
use axum::{extract::{Path, State}, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{api::RequestPrincipal, config::ExecutorGrant, db, lifecycle::LcError, state::App};

#[derive(Deserialize)]
pub(crate) struct PutGrants {
    grants: Vec<GrantInput>,
}

#[derive(Deserialize)]
struct GrantInput {
    specialist: String,
    executor: String,
}

/// 驗證不過時從 config 閉包裡帶出來的 400 body（`reason` 寫明是哪一條不變式）。
#[derive(Debug)]
struct GrantRejected(Value);
impl std::fmt::Display for GrantRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "executor grant rejected: {}", self.0) }
}
impl std::error::Error for GrantRejected {}

fn reject(reason: &str, extra: Value) -> anyhow::Error {
    let mut body = json!({"error": "bad_request", "reason": reason});
    if let (Some(b), Value::Object(extra)) = (body.as_object_mut(), extra) { b.extend(extra); }
    anyhow::Error::new(GrantRejected(body))
}

/// 呼叫者：使用者 → `"user"`；該專案前台 manager（存活頂層 role=manager）→ 它的 bot id；其他一律 403。
async fn granted_by(app: &Arc<App>, principal: &RequestPrincipal, project_id: &str) -> Result<String, LcError> {
    let forbidden = || LcError::Forbidden(json!({"error": "forbidden", "reason": "grant_manager_only"}));
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

/// `PUT /api/projects/{id}/executor-grants`：body `{"grants":[{"specialist","executor"}]}`，整組取代；回 `{"grants":[…]}`（更新後的授權）。
pub(crate) async fn put_http(
    State(app): State<Arc<App>>,
    Path(id): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(b): Json<PutGrants>,
) -> Result<Json<Value>, LcError> {
    db::project(&app.db, &id).await.map_err(|e| LcError::Upstream(e.to_string()))?.filter(|p| p.deleted_at.is_none()).ok_or_else(|| LcError::NotFound("project".into()))?;
    let by = granted_by(&app, &principal, &id).await?;
    let inputs: Vec<(String, String)> = b.grants.iter().map(|g| (g.specialist.trim().to_string(), g.executor.trim().to_string())).collect();
    let now = db::now();
    let pid = id.clone();
    let out = crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
        let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).ok_or_else(|| anyhow::anyhow!("no-project"))?;
        apply(p, &inputs, &by, &now)?;
        Ok(p.executor_grants.clone())
    }).await;
    let grants = match out {
        Ok(g) => g,
        Err(e) => {
            if let Some(GrantRejected(body)) = e.downcast_ref::<GrantRejected>() { return Err(LcError::BadValue(body.clone())); }
            if e.to_string() == "no-project" { return Err(LcError::NotFound("project".into())); }
            return Err(crate::api::projection_err(e));
        }
    };
    tracing::info!(project_id = %id, grants = grants.len(), http = %crate::config_audit::http_caller(), "executor grants replaced");
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok(Json(json!({"grants": grants})))
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
        let body = PutGrants { grants: grants.iter().map(|(s, e)| GrantInput { specialist: s.id.clone(), executor: e.id.clone() }).collect() };
        put_http(State(fx.env.app.clone()), Path(fx.env.project_id.clone()), Extension(principal), Json(body)).await.map(|j| j.0)
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
}
