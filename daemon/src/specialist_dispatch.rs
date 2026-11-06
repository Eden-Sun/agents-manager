//! 專家派工總開關（使用者 2026-11-06，取代逐對的執行者授權）：`[projects] specialist_dispatch`（預設開）。
//!
//! 開著時，同一 expert 專案的存活頂層 specialist 可直接派工給任何存活頂層 executor（含群集、peers 候選）；關掉＝回到只有
//! 管理者／代班中的副管理者能派。真相在 config.toml（`ProjectCfg.specialist_dispatch`），DB 的 `projects.specialist_dispatch`
//! 是投影；設定走 `PATCH /api/projects/{id}`（僅 User）。核權在 `relay_policy::specialist_dispatch_allows`，依據記成
//! `specialist:<專案 id>`。同一顆執行者被同時派工照現有佇列排隊。
//!
//! 關閉與派工的序列化邊界（Astra review3）：不用鎖。turn 變成可送出的兩個寫入（直接送出的 in_flight INSERT、排隊認領
//! `claim_queued`）在同一句 SQL 裡正向確認依據仍成立（`lifecycle::relay_grant`）。任何讓依據消失的投影（關掉開關、改角色、
//! 刪除、離開 expert）跟這兩句都由 SQLite 序列化：先提交的關閉讓直接送出回可重試的 409 `relay_dispatch_changed`、排隊的
//! 認領不到並在 flush 重驗時撤回；那一句先提交＝已開始，不中止。關掉開關時 PATCH 另外當場收回排隊中失去依據的 turn
//! （[`withdraw_disabled`]），撤回跟認領搶同一個 queued→ 轉換。
use std::sync::Arc;
use crate::state::App;

/// 這一對派工會不會受專家派工依據影響：目標與送件者都在同一個 expert 專案時回那個專案 id（跨專案、一般專案的派工不靠依據）。
/// 查不到時當作會（fail closed），回空字串當鍵。
pub(crate) async fn expert_internal(app: &Arc<App>, from: &str, to: &str) -> Option<String> {
    match sqlx::query_scalar::<_, String>("SELECT p.id FROM bots f JOIN bots t ON t.project_id=f.project_id JOIN projects p ON p.id=t.project_id WHERE f.id=? AND t.id=? AND p.mode='expert'")
        .bind(from).bind(to).fetch_optional(&app.db).await {
        Ok(pid) => pid,
        Err(_) => Some(String::new()),
    }
}

/// 開關關掉（或專案離開 expert）後，把本專案「已排隊、還沒認領」、送件者是本專案 specialist 的派工 turn（`queue_class='relay'`）收回：
/// 每筆先重驗（依據仍成立的——例如代班中——不動），撤回 CAS 與請求狀態同一交易（`relay_requests::withdraw_turn`），
/// 被 flush 先認領的就算已開始，不中止、不標失敗。
pub(crate) async fn withdraw_disabled(app: &Arc<App>, project_id: &str) {
    let turns: Vec<String> = match sqlx::query_scalar(
        "SELECT t.id FROM turns t JOIN conversations c ON c.id=t.conversation_id JOIN bots x ON x.id=c.bot_id
          WHERE x.project_id=? AND t.status='queued' AND t.queue_class='relay'
            AND EXISTS(SELECT 1 FROM messages m JOIN bots s ON s.id=m.relay_from WHERE m.turn_id=t.id AND m.role='user' AND s.role='specialist')
          ORDER BY t.created_at")
        .bind(project_id).fetch_all(&app.db).await {
        Ok(t) => t,
        Err(e) => { tracing::warn!(error = ?e, project_id, "關閉專家派工：查排隊中的派工失敗；送達時仍會重驗"); return; }
    };
    for turn in turns {
        let d = match crate::runners::relay_requests::denial(app, &turn).await {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => { tracing::warn!(error = ?e, turn = %turn, "關閉專家派工：重驗排隊中的派工失敗；送達時仍會重驗"); continue; }
        };
        match crate::runners::relay_requests::withdraw_turn(app, &turn, &d).await {
            Ok(true) => tracing::info!(turn = %turn, project_id, "關閉專家派工：收回排隊中的派工"),
            Ok(false) => {}
            Err(e) => tracing::warn!(error = ?e, turn = %turn, "關閉專家派工：收回排隊中的派工失敗；送達時仍會重驗"),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::{extract::{Path, State}, http::HeaderMap, Extension, Json};
    use serde_json::json;
    use crate::{api::RequestPrincipal, db, lifecycle::LcError};
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

    /// 專家專案（config 為真相，開關預設開）：管理者、兩位專家（track db／ui）、兩顆執行者；另一個 expert 專案有管理者與執行者。
    async fn fixture() -> Fx {
        let env = crate::testing::env().await;
        let other_path = env.dir.join("other");
        std::fs::create_dir_all(&other_path).unwrap();
        let bots = vec![
            bot_cfg("sd-manager", "manager", None), bot_cfg("sd-spec", "specialist", Some("db")), bot_cfg("sd-spec2", "specialist", Some("ui")),
            bot_cfg("sd-exec", "executor", None), bot_cfg("sd-exec2", "executor", None),
        ];
        let other_bots = vec![bot_cfg("sd-other-mgr", "manager", None), bot_cfg("sd-outsider", "executor", None)];
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

    /// 經 `PATCH /api/projects/{id}` 設開關（真實路徑：config → 投影 → 關掉時當場收回）。
    async fn patch(fx: &Fx, principal: RequestPrincipal, on: bool) -> Result<(), LcError> {
        let body: crate::api::PatchProject = serde_json::from_value(json!({"specialist_dispatch": on})).unwrap();
        crate::api::patch_project(State(fx.env.app.clone()), Path(fx.env.project_id.clone()), Extension(principal), Json(body)).await.map(|_| ())
    }

    async fn switch(fx: &Fx, on: bool) { patch(fx, RequestPrincipal::User, on).await.unwrap() }

    /// 只改 DB 投影（模擬「核權之後、config 還沒走完 PATCH 收回流程」的關閉）。
    async fn db_switch(fx: &Fx, on: bool) {
        sqlx::query("UPDATE projects SET specialist_dispatch=? WHERE id=?").bind(on).bind(&fx.env.project_id).execute(&fx.env.app.db).await.unwrap();
    }

    async fn stored(fx: &Fx) -> bool {
        db::project(&fx.env.app.db, &fx.env.project_id).await.unwrap().unwrap().specialist_dispatch
    }

    fn denied(r: Result<(), RelayDenied>) -> String {
        match r { Err(RelayDenied::Denied(m)) => m, other => panic!("expected denial, got {other:?}") }
    }

    #[tokio::test]
    async fn switch_on_lets_a_specialist_reach_every_top_level_executor() {
        let fx = fixture().await;
        let app = &fx.env.app;
        assert!(stored(&fx).await, "預設開");
        for (from, to) in [(&fx.spec, &fx.exec), (&fx.spec, &fx.exec2), (&fx.spec2, &fx.exec)] {
            assert!(relay_prompt_allowed(app, &from.id, &to.id, Admission::NewRequest).await.is_ok(), "{} → {}", from.name, to.name);
            assert!(relay_prompt_allowed(app, &from.id, &to.id, Admission::Deliver).await.is_ok());
        }
        let basis = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        assert_eq!(basis.as_deref(), Some(format!("specialist:{}", fx.env.project_id).as_str()));
        // 專家對專家：開關不涵蓋，訊息寫 specialist、不提開關。
        let msg = denied(relay_prompt_allowed(app, &fx.spec.id, &fx.spec2.id, Admission::NewRequest).await);
        assert!(msg.starts_with("specialist 只接受 manager 派工") && !msg.contains("開關"), "{msg}");
        // 跨專案：開關不讓專家出專案。
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.outsider.id, Admission::NewRequest).await.is_err());
        // child 執行者不算（只有頂層 executor）。
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&fx.manager.id).bind(&fx.exec2.id).execute(&app.db).await.unwrap();
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec2.id, Admission::NewRequest).await.is_err());
        sqlx::query("UPDATE bots SET parent_bot_id=NULL WHERE id=?").bind(&fx.exec2.id).execute(&app.db).await.unwrap();

        // 關掉：擋下，提示開關與管理者；/api/state 帶開關；config 寫 false。
        switch(&fx, false).await;
        assert!(!stored(&fx).await);
        let msg = denied(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await);
        assert!(msg.starts_with("executor 只接受 manager 派工") && msg.contains("開關已關閉") && msg.contains("sd-manager"), "{msg}");
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::Deliver).await.is_err());
        assert!(relay_prompt_allowed(app, &fx.manager.id, &fx.exec.id, Admission::NewRequest).await.is_ok(), "管理者照舊");
        let state = crate::api::state_json(app).await.unwrap();
        let proj = state["projects"].as_array().unwrap().iter().find(|p| p["id"] == fx.env.project_id.as_str()).unwrap().clone();
        assert_eq!(proj["specialist_dispatch"], false);
        assert!(proj.get("executor_grants").is_none());
        let cfg = app.cfg.get().await;
        assert!(!cfg.projects.iter().find(|p| p.id.as_deref() == Some(fx.env.project_id.as_str())).unwrap().specialist_dispatch);
        // 再打開：恢復。
        switch(&fx, true).await;
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_ok());
    }

    #[tokio::test]
    async fn only_the_user_may_flip_the_switch() {
        let fx = fixture().await;
        for caller in [&fx.manager, &fx.spec, &fx.exec] {
            let r = patch(&fx, RequestPrincipal::Bot(caller.id.clone()), false).await;
            assert!(matches!(&r, Err(LcError::Forbidden(b)) if b["reason"] == "user_only"), "{}: {r:?}", caller.name);
        }
        assert!(stored(&fx).await, "bot 改不動");
    }

    #[tokio::test]
    async fn switching_off_fails_a_pending_request_at_delivery() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,?,?,'[]','pending',?)")
            .bind(&rid).bind(&fx.spec.id).bind(&fx.exec.id).bind("sd-crid").bind("do it").bind(db::now()).execute(&app.db).await.unwrap();
        switch(&fx, false).await;
        crate::runners::relay_requests::sweep(app).await;
        let (state, error): (String, Option<String>) = sqlx::query_as("SELECT state,error FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "failed");
        assert!(error.unwrap_or_default().contains("只接受 manager 派工"));
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

    /// 沒有 relay_requests 的 queue_if_busy 派工：關掉開關時 PATCH 當場收回；已開始的不中止；其他失效來源（降級）在 flush 認領前擋下。
    #[tokio::test]
    async fn switching_off_withdraws_queued_prompts_but_not_started_ones() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let queued = queued_turn_from(&fx, &fx.spec, &fx.exec, "plain-q", "queued").await;
        let queued2 = queued_turn_from(&fx, &fx.spec2, &fx.exec2, "plain-q-2", "queued").await;
        let started = queued_turn_from(&fx, &fx.spec, &fx.exec, "plain-s", "in_flight").await;
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &queued).await.unwrap(), None, "開著：flush 的重驗放行");
        switch(&fx, false).await;
        assert_eq!(turn_status(&fx, &queued).await, "failed", "排隊中的派工被收回");
        assert_eq!(turn_status(&fx, &queued2).await, "failed", "另一位專家的也收回");
        assert_eq!(turn_status(&fx, &started).await, "in_flight", "已開始的不中止");
        let note: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&queued).fetch_one(&app.db).await.unwrap();
        assert!(note.contains("只接受 manager 派工"), "{note}");

        // 其他失效來源（專家被降級）不經 PATCH：flush 認領前的重驗擋下。
        switch(&fx, true).await;
        let later = queued_turn_from(&fx, &fx.spec, &fx.exec2, "plain-q2", "queued").await;
        set_role(&fx.env, &fx.spec, "advisor", None).await;
        assert!(crate::runners::relay_requests::queue_denial(app, &later).await.unwrap().is_some(), "降級：送達前擋下");
        assert_eq!(turn_status(&fx, &later).await, "failed", "queue_denial 當場撤回");
        // manager 的排隊派工照舊不受影響（開關關著也一樣）。
        switch(&fx, false).await;
        let from_mgr = queued_turn_from(&fx, &fx.manager, &fx.exec, "mgr-q", "queued").await;
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &from_mgr).await.unwrap(), None);
    }

    /// durable（expect_result）排隊中的 turn：關掉時 turn 收回、請求標 failed。專案離開 expert 也一樣收回。
    #[tokio::test]
    async fn switching_off_or_leaving_expert_withdraws_durable_queued_turns() {
        for leave_expert in [false, true] {
            let fx = fixture().await;
            let app = &fx.env.app;
            let rid = db::ulid();
            sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,'durable','do it','[]','pending',?)")
                .bind(&rid).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).execute(&app.db).await.unwrap();
            let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{rid}"), "queued").await;
            if leave_expert {
                let body: crate::api::PatchProject = serde_json::from_value(json!({"mode": "parent_child"})).unwrap();
                crate::api::patch_project(State(app.clone()), Path(fx.env.project_id.clone()), Extension(RequestPrincipal::User), Json(body)).await.unwrap();
            } else {
                switch(&fx, false).await;
            }
            assert_eq!(turn_status(&fx, &turn).await, "failed", "leave_expert={leave_expert}");
            let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
            assert_eq!(state, "failed");
        }
    }

    /// 開關不繞過 manager_use=false：停用的執行者也擋。
    #[tokio::test]
    async fn switch_does_not_bypass_member_disabled() {
        let fx = fixture().await;
        let app = &fx.env.app;
        sqlx::query("UPDATE bots SET manager_use=0 WHERE id=?").bind(&fx.exec.id).execute(&app.db).await.unwrap();
        assert!(matches!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await, Err(RelayDenied::MemberDisabled(message)) if message.contains(&fx.exec.name) && message.contains("manager_use=false")));
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

    /// 專家群集派工：開著時 resolve 可挑任一成員；關掉時排隊中的群集 turn 收回（請求 failed）、
    /// 還沒認領成員的 pending 群集請求下一輪 reconcile 因「送件者已不能對群集派工」標 failed、不改派；resolve 回 403。
    #[tokio::test]
    async fn switching_off_withdraws_specialist_cluster_dispatches() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let name = make_cluster(&fx).await;
        let resolved = crate::api::resolve_cluster_target(app, &fx.spec.id, &name).await.unwrap().0;
        let to = resolved["to_bot_id"].as_str().unwrap().to_string();
        assert!(to == fx.exec.id || to == fx.exec2.id, "{resolved}");
        let queued_req = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'cl-q','do it','[]','pending',?,?)")
            .bind(&queued_req).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).bind(&name).execute(&app.db).await.unwrap();
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{queued_req}"), "queued").await;
        let pending_req = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,cluster) VALUES(?,?,?,'cl-p','do it','[]','pending',?,?)")
            .bind(&pending_req).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).bind(&name).execute(&app.db).await.unwrap();
        switch(&fx, false).await;
        assert_eq!(turn_status(&fx, &turn).await, "failed", "排隊中的群集派工被收回");
        let db = fx.env.app.db.clone();
        let row = |rid: String| { let db = db.clone(); async move { sqlx::query_as::<_, (String, String, Option<String>)>("SELECT state,to_bot_id,error FROM relay_requests WHERE id=?").bind(rid).fetch_one(&db).await.unwrap() } };
        let (state, to, error) = row(queued_req.clone()).await;
        assert_eq!((state.as_str(), to.as_str()), ("failed", fx.exec.id.as_str()));
        assert!(error.as_deref().unwrap_or_default().contains("只接受 manager 派工"), "{error:?}");
        assert_eq!(crate::expert_cluster::claim_target(app, &pending_req, &fx.exec.id, &name).await.unwrap(), None);
        let (state, to, error) = row(pending_req.clone()).await;
        assert_eq!((state.as_str(), to.as_str()), ("failed", fx.exec.id.as_str()), "不改派給 exec2");
        assert!(error.as_deref().unwrap_or_default().contains("may no longer dispatch"), "{error:?}");
        match crate::api::resolve_cluster_target(app, &fx.spec.id, &name).await {
            Err(LcError::Forbidden(v)) => assert_eq!(v["reason"], "cluster_manager_only"),
            other => panic!("{other:?}"),
        }
        assert!(crate::api::resolve_cluster_target(app, &fx.manager.id, &name).await.is_ok(), "管理者照舊");
    }

    /// 專家忙碌時，執行者回來的 daemon 結果通知（queue_class=daemon）先排隊：開著也好、關掉後也好，都不被當成新派工撤回；撤回通知本身也不會再被撤回。
    #[tokio::test]
    async fn queued_daemon_notices_to_a_busy_specialist_are_not_rechecked_as_dispatches() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let notice = queued_turn_class(&fx, &fx.exec, &fx.spec, "relay-result:x:r0", "queued", "daemon").await;
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &notice).await.unwrap(), None, "開著：通知照送");
        let dispatched = queued_turn_from(&fx, &fx.spec, &fx.exec, "to-withdraw", "queued").await;
        switch(&fx, false).await;
        assert_eq!(turn_status(&fx, &dispatched).await, "failed");
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &notice).await.unwrap(), None, "關掉後：結果通知仍不重驗");
        assert_eq!(turn_status(&fx, &notice).await, "queued");
        let (rid, state, error): (String, String, Option<String>) = sqlx::query_as("SELECT id,state,error FROM relay_requests WHERE client_request_id=?").bind(format!("withdrawn:{dispatched}")).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "failed", "一般派工的撤回記成 failed 請求，由 reconcile 通知送件者");
        assert!(error.unwrap_or_default().contains("只接受 manager 派工"));
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
        fx.env.herdr.live_pane(&format!("pane-{}", fx.exec.id), crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        sqlx::query("UPDATE bots SET inject_hooks=0 WHERE id=?").bind(&fx.exec.id).execute(&fx.env.app.db).await.unwrap();
        crate::lifecycle::prompt_from_api_queue_if_busy(&fx.env.app, &fx.exec.id, "do it", crid, &[], crate::lifecycle::RelaySrc::trusted(Some(&fx.spec.id)).with_auth(auth), false, None).await
    }

    async fn claim(fx: &Fx, turn: &str) -> crate::lifecycle::turn_controller::Outcome {
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&fx.exec.id).fetch_one(&fx.env.app.db).await.unwrap();
        crate::lifecycle::turn_controller::claim_queued(&mut fx.env.app.db.acquire().await.unwrap(), turn, &run).await.unwrap()
    }

    fn basis_changed(r: &Result<crate::lifecycle::PromptOut, LcError>) -> bool {
        matches!(r, Err(LcError::Conflict(b)) if b["reason"] == "relay_dispatch_changed" && b["retryable"] == true && b["sent"] == false)
    }

    async fn turns(fx: &Fx, crid: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=? AND t.client_request_id=?").bind(&fx.exec.id).bind(crid).fetch_one(&fx.env.app.db).await.unwrap()
    }

    /// 核權通過後暫停（不論多久、沒有鎖可逾時），期間關掉開關（PATCH，或只有 DB 投影先變），恢復後：
    /// 直接送出寫不進 in_flight（可重試 409、沒有 turn）；已排隊的認領不到／被當場收回。不會「關掉之後才開始執行」。
    #[tokio::test]
    async fn a_dispatch_checked_before_switch_off_cannot_start_after_it() {
        let fx = fixture().await;
        let app = &fx.env.app;
        for via_patch in [true, false] {
            switch(&fx, true).await;
            let auth = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
            assert!(auth.is_some(), "核權通過");
            let queued = queued_turn_from(&fx, &fx.spec, &fx.exec, if via_patch { "q-patch" } else { "q-db" }, "queued").await;
            if via_patch { switch(&fx, false).await } else { db_switch(&fx, false).await }
            let crid = if via_patch { "now-patch" } else { "now-db" };
            let r = dispatch_with(&fx, crid, auth.as_deref()).await;
            assert!(basis_changed(&r), "{r:?}");
            assert_eq!(turns(&fx, crid).await, 0, "沒有建 turn、沒有送出");
            if !via_patch {
                assert!(matches!(claim(&fx, &queued).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)));
                assert_eq!(turn_status(&fx, &queued).await, "queued");
                assert!(crate::runners::relay_requests::queue_denial(app, &queued).await.unwrap().is_some());
            }
            assert_eq!(turn_status(&fx, &queued).await, "failed");
        }
        // 開著時同一條路照常建 in_flight turn（條件只擋失效）。
        switch(&fx, true).await;
        let r = dispatch_now(&fx, "now-ok").await;
        assert!(!basis_changed(&r), "{r:?}");
        assert_eq!(turns(&fx, "now-ok").await, 1);
        // manager 派工不受影響（關著也能送）。
        switch(&fx, false).await;
        let q = queued_turn_from(&fx, &fx.manager, &fx.exec2, "mgr-claim", "queued").await;
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=?").bind(&fx.exec2.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(crate::lifecycle::turn_controller::claim_queued(&mut app.db.acquire().await.unwrap(), &q, &run).await.unwrap(), crate::lifecycle::turn_controller::Outcome::Applied);
    }

    /// 開 → 派工排隊 → DB 投影關掉：認領不到；再打開，同一筆排隊的又可以認領（條件看當下，不看歷史）。
    #[tokio::test]
    async fn the_claim_follows_the_current_switch() {
        let fx = fixture().await;
        let queued = queued_turn_from(&fx, &fx.spec, &fx.exec, "interleave", "queued").await;
        db_switch(&fx, false).await;
        assert!(matches!(claim(&fx, &queued).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)), "認領不到");
        db_switch(&fx, true).await;
        assert_eq!(claim(&fx, &queued).await, crate::lifecycle::turn_controller::Outcome::Applied);
    }

    /// `withdrawn:` 是保留前綴（外部 prompt 拒用）；同鍵已有不相干的紀錄時撤回整筆退回（turn 留在佇列、不漏通知），同一個撤回事件重來則接受既有紀錄。
    #[tokio::test]
    async fn withdrawal_record_key_collisions_are_not_silently_ignored() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let turn = queued_turn_from(&fx, &fx.spec, &fx.exec, "collide", "queued").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,?,'unrelated','[]','pending',?)")
            .bind(db::ulid()).bind(&fx.spec.id).bind(&fx.exec.id).bind(format!("withdrawn:{turn}")).bind(db::now()).execute(&app.db).await.unwrap();
        db_switch(&fx, false).await;
        let d = crate::runners::relay_requests::denial(app, &turn).await.unwrap().expect("開關關了");
        assert!(crate::runners::relay_requests::withdraw_turn(app, &turn, &d).await.is_err(), "碰撞：不靜默略過");
        assert_eq!(turn_status(&fx, &turn).await, "queued", "整個交易退回，turn 沒被撤");
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE client_request_id=?").bind(format!("withdrawn:{turn}")).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "pending", "不誤用既有請求");
        db_switch(&fx, true).await;
        let turn2 = queued_turn_from(&fx, &fx.spec, &fx.exec2, "same-event", "queued").await;
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,?,'','[]','failed',?)")
            .bind(db::ulid()).bind(&fx.spec.id).bind(&fx.exec2.id).bind(format!("withdrawn:{turn2}")).bind(db::now()).execute(&app.db).await.unwrap();
        db_switch(&fx, false).await;
        let d = crate::runners::relay_requests::denial(app, &turn2).await.unwrap().expect("開關關了");
        assert!(crate::runners::relay_requests::withdraw_turn(app, &turn2, &d).await.unwrap());
        assert_eq!(turn_status(&fx, &turn2).await, "failed");
    }

    /// flush 先認領（turn 已不是 queued），撤回輸掉 CAS：請求與結果追蹤不動、不記撤回、不通知。
    #[tokio::test]
    async fn a_withdrawal_that_loses_to_the_claim_changes_nothing() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let rid = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,'claimed','do it','[]','pending',?)")
            .bind(&rid).bind(&fx.spec.id).bind(&fx.exec.id).bind(db::now()).execute(&app.db).await.unwrap();
        let durable = queued_turn_from(&fx, &fx.spec, &fx.exec, &format!("relay-request:{rid}"), "queued").await;
        let plain = queued_turn_from(&fx, &fx.spec, &fx.exec2, "plain-claimed", "queued").await;
        db_switch(&fx, false).await;
        let d = crate::runners::relay_requests::denial(app, &durable).await.unwrap().expect("開關關了");
        sqlx::query("UPDATE turns SET status='in_flight' WHERE id=?").bind(&durable).execute(&app.db).await.unwrap();
        assert!(!crate::runners::relay_requests::withdraw_turn(app, &durable, &d).await.unwrap(), "CAS 輸了");
        let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id=?").bind(&rid).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "pending", "已開始的不標失敗，結果照常追蹤");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'").bind(&durable).fetch_one(&app.db).await.unwrap();
        assert_eq!(notes, 0);
        let d = crate::runners::relay_requests::denial(app, &plain).await.unwrap().expect("開關關了");
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
        assert!(basis_changed(&r), "{label} 直送：{r:?}");
        let q = queued_with(fx, &format!("{label}-q"), auth).await;
        assert!(matches!(claim(fx, &q).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)), "{label} 認領不到");
        assert!(crate::runners::relay_requests::queue_denial(&fx.env.app, &q).await.unwrap().is_some(), "{label} 重驗擋下（不因搬專案略過）");
        assert_eq!(turn_status(fx, &q).await, "failed", "{label} 撤回");
    }

    /// 依開關核權通過（記下 `specialist:<專案>`）之後，專家被降級、變 child、搬專案；或目標被降級：直送與排隊認領都正向確認失敗（開關仍開著）。
    #[tokio::test]
    async fn a_specialist_basis_is_rechecked_positively_after_demotion_childing_or_moving() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let basis = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        let auth = basis.as_deref();
        assert!(!basis_changed(&dispatch_with(&fx, "ok-now", auth).await));
        let ok_q = queued_with(&fx, "ok-q", auth).await;
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE client_request_id='ok-now'").bind(db::now()).execute(&app.db).await.unwrap();
        assert_eq!(claim(&fx, &ok_q).await, crate::lifecycle::turn_controller::Outcome::Applied);
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(db::now()).bind(&ok_q).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "demoted", auth).await;
        sqlx::query("UPDATE bots SET role='specialist' WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&fx.exec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "target-demoted", auth).await;
        sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&fx.exec.id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&fx.manager.id).bind(&fx.spec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "childed", auth).await;
        sqlx::query("UPDATE bots SET parent_bot_id=NULL WHERE id=?").bind(&fx.spec.id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET project_id=? WHERE id=?").bind(&fx.other_mgr.project_id).bind(&fx.spec.id).execute(&app.db).await.unwrap();
        expect_blocked(&fx, "moved", auth).await;
    }

    /// 沒有依據（核權時是 manager）、之後才被改成 specialist 的送件者，仍被負向條件擋；舊版的 `grant:` 依據不再認。
    #[tokio::test]
    async fn no_basis_or_a_legacy_grant_basis_is_blocked() {
        let fx = fixture().await;
        expect_blocked(&fx, "no-basis", None).await;
        let legacy = format!("grant:{}", fx.env.project_id);
        expect_blocked(&fx, "legacy-grant", Some(&legacy)).await;
        expect_blocked(&fx, "bogus", Some("bogus:x")).await;
    }

    /// 被指定為副管理者 ≠ 代班中。指定但沒代班的專家，開關關掉後直送與認領都擋；真的代班中仍放行；代班結束後排隊的認領不到。
    #[tokio::test]
    async fn deputy_basis_requires_actually_acting() {
        let fx = fixture().await;
        let app = &fx.env.app;
        sqlx::query("UPDATE projects SET deputy_bot_id=? WHERE id=?").bind(&fx.spec.id).bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        let sp_auth = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        assert!(sp_auth.as_deref().is_some_and(|a| a.starts_with("specialist:")));
        db_switch(&fx, false).await;
        assert!(relay_prompt_allowed(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.is_err(), "沒代班：核權就擋");
        expect_blocked(&fx, "deputy-not-acting", sp_auth.as_deref()).await;
        let redirected = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,redirected_from) VALUES(?,?,?,'rd','p','[]','pending',?,?)")
            .bind(&redirected).bind(&fx.exec2.id).bind(&fx.spec.id).bind(db::now()).bind(&fx.manager.id).execute(&app.db).await.unwrap();
        let basis = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        assert_eq!(basis.as_deref(), Some(format!("deputy:{}", fx.env.project_id).as_str()));
        assert!(!basis_changed(&dispatch_with(&fx, "acting-now", basis.as_deref()).await), "真的代班中：直送放行");
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE client_request_id='acting-now'").bind(db::now()).execute(&app.db).await.unwrap();
        let q = queued_with(&fx, "acting-q", basis.as_deref()).await;
        assert_eq!(claim(&fx, &q).await, crate::lifecycle::turn_controller::Outcome::Applied, "代班中：認領放行");
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(db::now()).bind(&q).execute(&app.db).await.unwrap();
        let q_later = queued_with(&fx, "acting-q2", basis.as_deref()).await;
        sqlx::query("UPDATE relay_requests SET state='answered' WHERE id=?").bind(&redirected).execute(&app.db).await.unwrap();
        assert!(matches!(claim(&fx, &q_later).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)), "代班結束後認領不到");
        assert!(crate::runners::relay_requests::queue_denial(app, &q_later).await.unwrap().is_some());
        assert!(basis_changed(&dispatch_with(&fx, "acting-over-now", basis.as_deref()).await));
    }

    /// 同時靠開關又在代班的專家，兩個依據都記；關掉開關但仍在代班，直送、認領與 PATCH 的收回都照放行。
    #[tokio::test]
    async fn both_bases_are_kept_and_either_one_suffices() {
        let fx = fixture().await;
        let app = &fx.env.app;
        sqlx::query("UPDATE projects SET deputy_bot_id=? WHERE id=?").bind(&fx.spec.id).bind(&fx.env.project_id).execute(&app.db).await.unwrap();
        // config 也指定副管理者（否則之後的 PATCH 投影會把 DB 的 deputy 洗掉）。
        let (pid, sid) = (fx.env.project_id.clone(), fx.spec.id.clone());
        crate::projection::update_and_project(&app.cfg, &app.db, move |cfg| {
            let p = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(pid.as_str())).unwrap();
            p.deputy = Some(crate::config::DeputyCfg { bot_id: sid.clone(), after_sec: 60 });
            Ok(())
        }).await.unwrap();
        let redirected = db::ulid();
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at,redirected_from) VALUES(?,?,?,'rd-both','p','[]','pending',?,?)")
            .bind(&redirected).bind(&fx.exec2.id).bind(&fx.spec.id).bind(db::now()).bind(&fx.manager.id).execute(&app.db).await.unwrap();
        let basis = crate::relay_policy::relay_prompt_basis(app, &fx.spec.id, &fx.exec.id, Admission::NewRequest).await.unwrap();
        let pid = &fx.env.project_id;
        assert_eq!(basis.as_deref(), Some(format!("specialist:{pid},deputy:{pid}").as_str()));
        let q = queued_with(&fx, "both-q", basis.as_deref()).await;
        switch(&fx, false).await;
        assert_eq!(turn_status(&fx, &q).await, "queued", "仍在代班：PATCH 不收回");
        assert_eq!(crate::runners::relay_requests::queue_denial(app, &q).await.unwrap(), None, "仍在代班：重驗放行");
        assert_eq!(claim(&fx, &q).await, crate::lifecycle::turn_controller::Outcome::Applied, "仍在代班：認領放行");
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(db::now()).bind(&q).execute(&app.db).await.unwrap();
        assert!(!basis_changed(&dispatch_with(&fx, "both-now", basis.as_deref()).await), "仍在代班：直送放行");
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE client_request_id='both-now'").bind(db::now()).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE relay_requests SET state='answered' WHERE id=?").bind(&redirected).execute(&app.db).await.unwrap();
        assert!(basis_changed(&dispatch_with(&fx, "both-over", basis.as_deref()).await), "兩個依據都不成立才擋");
    }

    /// 外部請求夾帶 `relay_auth` 不生效——依據只由 daemon 的核權結果決定（body 沒有這個欄位，送進來的值被忽略）。
    #[tokio::test]
    async fn a_smuggled_relay_auth_in_the_prompt_body_is_ignored() {
        let fx = fixture().await;
        let app = &fx.env.app;
        fx.env.herdr.live_pane(&format!("pane-{}", fx.exec.id), crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        sqlx::query("UPDATE bots SET inject_hooks=0 WHERE id=?").bind(&fx.exec.id).execute(&app.db).await.unwrap();
        let pid = fx.env.project_id.clone();
        let send = |crid: &'static str| {
            let mut headers = HeaderMap::new();
            headers.insert("X-AM-Bot-Id", fx.spec.id.parse().unwrap());
            headers.insert("X-AM-Bot-Token", fx.spec.hook_token.parse().unwrap());
            let body = serde_json::from_value(json!({"text": "do it", "client_request_id": crid, "relay_auth": format!("specialist:{pid},deputy:{pid}")})).unwrap();
            crate::api::prompt_bot(State(app.clone()), Path(fx.exec.id.clone()), headers, Extension(RequestPrincipal::Bot(fx.spec.id.clone())), Json(body))
        };
        // 關著：夾帶的依據不會讓核權放行。
        switch(&fx, false).await;
        let r = send("smuggle-1").await;
        assert!(matches!(&r, Err(LcError::Forbidden(_))), "{:?}", r.as_ref().err());
        // 開著：存下來的依據是 daemon 判定的（只有 specialist，沒有夾帶的 deputy）。
        switch(&fx, true).await;
        sqlx::query("UPDATE bots SET inject_hooks=0 WHERE id=?").bind(&fx.exec.id).execute(&app.db).await.unwrap();
        let _ = send("smuggle-2").await;
        let stored_auth: Option<String> = sqlx::query_scalar("SELECT m.relay_auth FROM messages m JOIN turns t ON t.id=m.turn_id WHERE t.client_request_id='smuggle-2' AND m.role='user'").fetch_one(&app.db).await.unwrap();
        assert_eq!(stored_auth.as_deref(), Some(format!("specialist:{pid}").as_str()));
    }

    /// 一筆排隊 turn 有兩位送件者，只有一位的依據失效：整筆撤回；失效的那位收到它自己的原因，另一位收到「整筆撤回、請重送」。
    #[tokio::test]
    async fn multi_sender_turn_notifies_the_failed_sender_with_its_reason_and_the_others_to_resend() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let q = queued_turn_from(&fx, &fx.spec, &fx.exec, "multi", "queued").await;
        let conv = db::conversation_id(&app.db, &fx.exec.id).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,relay_from,created_at) VALUES(?,?,?,'user','also','web',?,?)")
            .bind(db::ulid()).bind(&conv).bind(&q).bind(&fx.manager.id).bind(db::now()).execute(&app.db).await.unwrap();
        db_switch(&fx, false).await;
        assert!(crate::runners::relay_requests::queue_denial(app, &q).await.unwrap().is_some());
        assert_eq!(turn_status(&fx, &q).await, "failed");
        let rows: Vec<(String, Option<String>)> = sqlx::query_as("SELECT from_bot_id,error FROM relay_requests WHERE client_request_id=? ORDER BY from_bot_id").bind(format!("withdrawn:{q}")).fetch_all(&app.db).await.unwrap();
        assert_eq!(rows.len(), 2, "{rows:?}");
        let why = |id: &str| rows.iter().find(|(f, _)| f == id).and_then(|(_, e)| e.clone()).unwrap_or_default();
        assert!(why(&fx.spec.id).contains("只接受 manager 派工"), "失效的那位：{}", why(&fx.spec.id));
        assert!(why(&fx.manager.id).contains("整筆撤回"), "另一位：{}", why(&fx.manager.id));
    }

    /// 升級前就排隊的專家派工沒有依據（`relay_auth` NULL）：即使開關開著也保守拒絕（認領不到、撤回並通知重送）。
    #[tokio::test]
    async fn pre_upgrade_queued_dispatch_without_a_basis_is_conservatively_refused() {
        let fx = fixture().await;
        let app = &fx.env.app;
        let q = queued_with(&fx, "pre-upgrade", None).await;
        assert!(matches!(claim(&fx, &q).await, crate::lifecycle::turn_controller::Outcome::Fenced(_)));
        let why = crate::runners::relay_requests::queue_denial(app, &q).await.unwrap().expect("保守拒絕");
        assert!(why.contains("請重送") || why.contains("改送管理者"), "{why}");
        assert_eq!(turn_status(&fx, &q).await, "failed");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_requests WHERE client_request_id=? AND from_bot_id=?").bind(format!("withdrawn:{q}")).bind(&fx.spec.id).fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1, "通知送件者重送");
    }
}
