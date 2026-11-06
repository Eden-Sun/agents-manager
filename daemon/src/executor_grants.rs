//! 專家模式執行者授權（使用者 2026-11-06 方案 c）：管理者（或使用者）把執行者授權給專家，專家就能直接派工給它。
//!
//! 真相在 config.toml 的 `[[projects.executor_grants]]`（`ProjectCfg.executor_grants`），DB 的
//! `projects.executor_grants_json` 只是投影。派工時的核對在 `relay_policy::granted`；不合格的授權由
//! `ProjectCfg::clear_invalid_grants` 在每次投影（含啟動、正式刪除）時清掉。授權只涵蓋派工，不給 start／restart／hire／stop／改角色／再授權。
//!
//! 撤銷與派工的序列化邊界（Astra review #1）：專家模式專案內部的派工從「核對授權」到「建好 turn（排隊或已認領）」
//! 全程拿著 [`GATE`] 的讀鎖（[`dispatch_gate`]）；PUT 拿寫鎖改設定，並在放開前把已經排隊、送件者已失去授權的 turn
//! 用 CAS 撤回（[`withdraw_revoked`]）。flush 認領前也會重驗（`relay_requests::queue_denial`），跟撤回搶同一個 queued→
//! 狀態轉換，只會有一邊成功：認領成功＝已開始，不中止；撤回成功＝不送。
use std::sync::{Arc, LazyLock};
use axum::{extract::{Path, State}, http::HeaderMap, Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{api::RequestPrincipal, config::ExecutorGrant, db, lifecycle::LcError, state::App};

/// 授權派工與撤銷之間的閘門：派工拿讀鎖（彼此不互斥），撤銷拿寫鎖。
static GATE: LazyLock<tokio::sync::RwLock<()>> = LazyLock::new(|| tokio::sync::RwLock::new(()));

/// 這一對派工會不會受授權影響：目標與送件者都在同一個 expert 專案（跨專案、一般專案的派工不靠授權）。查不到時當作會（fail closed）。
pub(crate) async fn expert_internal(app: &Arc<App>, from: &str, to: &str) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM bots f JOIN bots t ON t.project_id=f.project_id JOIN projects p ON p.id=t.project_id WHERE f.id=? AND t.id=? AND p.mode='expert')")
        .bind(from).bind(to).fetch_one(&app.db).await.unwrap_or(true)
}

/// 派工路徑在「核對授權」之前拿，握到 turn 建好（排隊或已認領）為止；跟授權無關的派工回 `None`、不排隊等鎖。
pub(crate) async fn dispatch_gate(app: &Arc<App>, from: &str, to: &str) -> Option<tokio::sync::RwLockReadGuard<'static, ()>> {
    if expert_internal(app, from, to).await { Some(GATE.read().await) } else { None }
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
    // 寫鎖：等手上正在「核對→建 turn」的授權派工做完；放開前把排隊中、已失去授權的 turn 撤回。
    let _gate = GATE.write().await;
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
    drop(_gate);
    tracing::info!(project_id = %id, grants = grants.len(), revoked = revoked.len(), http = %crate::config_audit::http_caller(), "executor grants replaced");
    app.emit("project_changed", json!({"project_id": id})).await;
    Ok(Json(json!({"grants": grants, "revision": revision(&grants)})))
}

/// 撤銷後把「已排隊、還沒認領」且送件者是被撤專家、目標是被撤執行者的 turn 收回（CAS：被 flush 先認領的就算已開始，不中止）。
/// 帶 `relay_requests` 的會一併標 failed（由 reconcile 通知送件者）；其他的另送一則 daemon 通知給送件者。
pub(crate) async fn withdraw_revoked(app: &Arc<App>, revoked: &[ExecutorGrant]) {
    for g in revoked {
        let turns: Vec<String> = match sqlx::query_scalar("SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.status='queued' AND EXISTS(SELECT 1 FROM messages m WHERE m.turn_id=t.id AND m.role='user' AND m.relay_from=?) ORDER BY t.created_at")
            .bind(&g.executor).bind(&g.specialist).fetch_all(&app.db).await {
            Ok(t) => t,
            Err(e) => { tracing::warn!(error = ?e, executor = %g.executor, "撤銷授權：查排隊中的派工失敗；送達時仍會重驗"); continue; }
        };
        for turn in turns {
            let why = match crate::runners::relay_requests::queue_denial(app, &turn).await {
                Ok(Some(why)) => why,
                Ok(None) => continue,
                Err(e) => { tracing::warn!(error = ?e, turn = %turn, "撤銷授權：重驗排隊中的派工失敗；送達時仍會重驗"); continue; }
            };
            match crate::lifecycle::revoke_queued_turn(app, &turn, &why).await {
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
        let app = &fx.env.app;
        let conv = db::conversation_id(&app.db, &to.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,client_request_id,created_at) VALUES(?,?,'web',?,'pending',?,?)")
            .bind(&turn).bind(&conv).bind(status).bind(crid).bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,relay_from,created_at) VALUES(?,?,?,'user','do it','web',?,?)")
            .bind(db::ulid()).bind(&conv).bind(&turn).bind(&from.id).bind(db::now()).execute(&app.db).await.unwrap();
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
        let why = crate::runners::relay_requests::queue_denial(app, &later).await.unwrap().expect("track 變了：送達前擋下");
        assert!(crate::lifecycle::revoke_queued_turn(app, &later, &why).await.unwrap());
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

    /// 序列化邊界：派工握著讀鎖（已過 NewRequest、turn 還沒建）時，撤銷等它建好 turn 才生效，而且會把那筆排隊的收回。
    #[tokio::test]
    async fn revocation_waits_for_an_in_progress_dispatch_and_then_withdraws_it() {
        let fx = fixture().await;
        let app = fx.env.app.clone();
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let gate = dispatch_gate(&app, &fx.spec.id, &fx.exec.id).await;
        assert!(gate.is_some(), "專家模式內部的派工要拿閘門");
        assert!(relay_prompt_allowed(&app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_ok());
        let (app2, pid) = (app.clone(), fx.env.project_id.clone());
        let revoke = tokio::spawn(async move {
            put_http(State(app2), Path(pid), Extension(RequestPrincipal::User), HeaderMap::new(), Json(PutGrants { grants: vec![], revision: None })).await.map(|j| j.0)
        });
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(!revoke.is_finished(), "派工還握著讀鎖：撤銷要等");
        assert_eq!(stored(&fx).await.len(), 1);
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, "racing", "queued").await;
        drop(gate);
        revoke.await.unwrap().unwrap();
        assert!(stored(&fx).await.is_empty());
        assert_eq!(turn_status(&fx, &turn).await, "failed", "撤銷前排進去的，撤銷時收回");
        // 跟授權無關的派工（跨專案）不拿閘門。
        assert!(dispatch_gate(&app, &fx.spec.id, &fx.outsider.id).await.is_none());
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

    /// B 的專家群集派工：排隊中的群集 turn 撤銷時一樣收回；還沒認領成員的 pending 群集請求，下一輪 reconcile 標 failed、不改派。
    #[tokio::test]
    async fn revoking_withdraws_specialist_cluster_dispatches() {
        let fx = fixture().await;
        let app = &fx.env.app;
        put(&fx, RequestPrincipal::User, &[(&fx.spec, &fx.exec)]).await.unwrap();
        let queued_req = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'cl-q','do it','[]','pending',?,'any')")
            .bind(&queued_req).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).execute(&app.db).await.unwrap();
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{queued_req}"), "queued").await;
        let pending_req = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'cl-p','do it','[]','pending',?,'any')")
            .bind(&pending_req).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).execute(&app.db).await.unwrap();
        put(&fx, RequestPrincipal::User, &[]).await.unwrap();
        assert_eq!(turn_status(&fx, &turn).await, "failed", "排隊中的群集派工被收回");
        crate::runners::relay_requests::sweep(app).await;
        for rid in [&queued_req, &pending_req] {
            let (state, to): (String, String) = sqlx::query_as("SELECT state,to_bot_id FROM relay_requests WHERE id=?").bind(rid).fetch_one(&app.db).await.unwrap();
            assert_eq!(state, "failed", "{rid}");
            assert_eq!(to, fx.exec.id, "不改派給未授權的成員");
        }
    }
}

