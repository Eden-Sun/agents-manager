use super::*;

async fn seed(env: &crate::testing::Env) {
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        Ok(())
    }).await.unwrap();
}

async fn add(env: &crate::testing::Env, name: &str) -> String {
    let res = create_bot(State(env.app.clone()), Path(env.project_id.clone()), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
}

async fn make_expert(env: &crate::testing::Env, manager: &str) {
    let pid = env.project_id.clone();
    let mid = manager.to_string();
    env.app.cfg.update(move |cfg| {
        if let Some(p) = cfg.projects.iter_mut().find(|p| p.id.as_deref() == Some(&pid)) {
            p.mode = "expert".into();
            for b in &mut p.bots {
                if b.id.as_deref() == Some(&mid) {
                    b.role = Some("manager".into());
                }
            }
        }
        Ok(())
    }).await.unwrap();
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(manager).execute(&env.app.db).await.unwrap();
}

/// 專家（specialist）轉成員：role=specialist 必帶 track，缺 → 400 `track_required`；帶了才寫進 config 與 DB。
#[tokio::test]
async fn an_expert_child_becomes_a_specialist_only_with_a_track() {
    let env = crate::testing::env().await;
    seed(&env).await;
    let manager = add(&env, "spec-adopt-mgr").await;
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "spec-kid").await;
    sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=? WHERE id=?").bind(&manager).bind(&child.id).execute(&env.app.db).await.unwrap();
    make_expert(&env, &manager).await;
    let no_track = crate::expert_member::AdoptMember { role: Some("specialist".into()), effort: None, session_id: None, track: None };
    let err = crate::expert_member::adopt(State(env.app.clone()), Path(child.id.clone()), Extension(RequestPrincipal::User), Json(no_track)).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["error"] == "track_required"), "{err:?}");
    assert_eq!(db::bot(&env.app.db, &child.id).await.unwrap().unwrap().managed_by, "child", "a refused adopt changes nothing");
    let with_track = crate::expert_member::AdoptMember { role: Some("specialist".into()), effort: None, session_id: None, track: Some(" infra ".into()) };
    crate::expert_member::adopt(State(env.app.clone()), Path(child.id.clone()), Extension(RequestPrincipal::User), Json(with_track)).await.map_err(|e| format!("{e:?}")).unwrap();
    let b = db::bot(&env.app.db, &child.id).await.unwrap().unwrap();
    assert_eq!((b.managed_by.as_str(), b.role.as_deref(), b.track.as_deref()), ("user", Some("specialist"), Some("infra")));
    let in_cfg = env.app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|x| x.id.as_deref() == Some(child.id.as_str())).and_then(|x| x.track.clone());
    assert_eq!(in_cfg.as_deref(), Some("infra"), "track 的真相在 config.toml");
}

#[tokio::test]
async fn an_expert_child_becomes_a_member_in_place() {
    let env = crate::testing::env().await;
    seed(&env).await;
    let manager = add(&env, "mem-manager").await;
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "astra").await;
    sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=? WHERE id=?").bind(&manager).bind(&child.id).execute(&env.app.db).await.unwrap();
    let run = crate::testing::fake_run(&env.app, &child.id).await;
    make_expert(&env, &manager).await;
    let body = crate::expert_member::AdoptMember { role: Some("advisor".into()), effort: Some("high".into()), session_id: Some("sess-keep".into()), track: None };
    crate::expert_member::adopt(State(env.app.clone()), Path(child.id.clone()), Extension(RequestPrincipal::User), Json(body)).await.map_err(|e| format!("{e:?}")).unwrap();
    let b = db::bot(&env.app.db, &child.id).await.unwrap().unwrap();
    assert_eq!((b.managed_by.as_str(), b.parent_bot_id.as_deref(), b.role.as_deref(), b.deleted_at.is_none()), ("user", None, Some("advisor"), true));
    let session: Option<String> = sqlx::query_scalar("SELECT native_session_id FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(session.as_deref(), Some("sess-keep"), "the running pane keeps its session for the next resume");
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(state, "running", "the pane is not restarted");
    let again = crate::expert_member::adopt(State(env.app.clone()), Path(child.id.clone()), Extension(RequestPrincipal::User), Json(Default::default())).await;
    assert!(again.is_err(), "already a member");
}

#[tokio::test]
async fn patch_project_sets_validates_and_clears_the_deputy() {
    let env = crate::testing::env().await;
    seed(&env).await;
    let manager = add(&env, "dep-manager").await;
    let advisor = add(&env, "dep-advisor").await;
    let worker = add(&env, "dep-worker").await;
    make_expert(&env, &manager).await;
    let (advisor_id, worker_id) = (advisor.clone(), worker.clone());
    crate::projection::update_and_project(&env.app.cfg, &env.app.db, move |cfg| {
        for b in &mut cfg.projects.iter_mut().find(|p| p.mode == "expert").unwrap().bots {
            if b.id.as_deref() == Some(advisor_id.as_str()) { b.role = Some("advisor".into()); }
            if b.id.as_deref() == Some(worker_id.as_str()) { b.role = Some("executor".into()); }
        }
        Ok(())
    }).await.unwrap();
    let patch = |principal: RequestPrincipal, v: Value| patch_project(State(env.app.clone()), Path(env.project_id.clone()), Extension(principal), Json(serde_json::from_value::<PatchProject>(v).unwrap()));
    let reason = |e: LcError| match e { LcError::BadValue(v) => v["reason"].as_str().unwrap_or_default().to_string(), other => format!("{other:?}") };
    assert!(matches!(patch(RequestPrincipal::Bot(manager.clone()), json!({"deputy": {"bot_id": advisor}})).await, Err(LcError::Forbidden(_))), "User only");
    assert_eq!(reason(patch(RequestPrincipal::User, json!({"deputy": {"bot_id": advisor, "after_sec": 59}})).await.unwrap_err()), "deputy_after_sec_too_small");
    assert_eq!(reason(patch(RequestPrincipal::User, json!({"deputy": {"bot_id": worker}})).await.unwrap_err()), "deputy_role_invalid", "executor");
    assert_eq!(reason(patch(RequestPrincipal::User, json!({"deputy": {"bot_id": manager}})).await.unwrap_err()), "deputy_role_invalid", "manager");
    let col = || sqlx::query_as::<_, (Option<String>, Option<i64>)>("SELECT deputy_bot_id,deputy_after_sec FROM projects WHERE id=?").bind(env.project_id.clone()).fetch_one(&env.app.db);
    assert_eq!(col().await.unwrap(), (None, None), "400 的都沒寫進去");
    patch(RequestPrincipal::User, json!({"deputy": {"bot_id": advisor}})).await.unwrap();
    assert_eq!(col().await.unwrap(), (Some(advisor.clone()), Some(180)), "預設 180 秒");
    patch(RequestPrincipal::User, json!({"deputy": {"bot_id": advisor, "after_sec": 60}})).await.unwrap();
    assert_eq!(col().await.unwrap().1, Some(60));
    // 副管理者不能增聘（hire 只認管理者）。
    let hire = hire_member(State(env.app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::Bot(advisor.clone())),
        Json(HireMember { name: "worker-x".into(), kind: Some("claude".into()), model: None, effort: None, identity: None, role: None, track: None, fork_from: None })).await;
    assert!(matches!(hire, Err(LcError::Forbidden(ref v)) if v["reason"] == "manager_role_scope"), "{hire:?}");
    patch(RequestPrincipal::User, json!({"deputy": null})).await.unwrap();
    assert_eq!(col().await.unwrap(), (None, None));
}

#[tokio::test]
async fn expert_manager_hires_with_user_selected_settings_and_ignores_retired_pool_fields() {
    let env = crate::testing::env().await;
    seed(&env).await;
    let manager = add(&env, "hire-manager").await;
    let peer = add(&env, "hire-peer").await;
    let hire = |caller: &str| hire_member(State(env.app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::Bot(caller.to_string())),
        Json(HireMember { name: "worker-1".into(), kind: Some("claude".into()), model: None, effort: None, identity: None, role: None, track: None, fork_from: None }));
    assert!(hire(&manager).await.is_err(), "parent_child project");
    make_expert(&env, &manager).await;
    assert!(hire(&peer).await.is_err(), "non-manager");

    let request: HireMember = serde_json::from_value(json!({
        "name": "worker-codex", "kind": "codex", "model": "gpt-6.1-sol", "effort": "high", "pool_entry": "retired-entry"
    })).unwrap();
    let response = hire_member(State(env.app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::Bot(manager.clone())), Json(request)).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    let bot = db::bot(&env.app.db, &id).await.unwrap().unwrap();
    assert_eq!((bot.role.as_deref(), bot.kind.as_str(), bot.model.as_deref(), bot.effort.as_deref()), (Some("executor"), "codex", Some("gpt-6.1-sol"), Some("high")));

    // The retired project key is accepted as an unknown field and has no effect.
    let ignored: PatchProject = serde_json::from_value(json!({"executor_pool": "not a pool"})).unwrap();
    patch_project(State(env.app.clone()), Path(env.project_id.clone()), Extension(RequestPrincipal::User), Json(ignored)).await.unwrap();
    let state = state_json(&env.app.clone()).await.unwrap();
    let project = state["projects"].as_array().unwrap().iter().find(|p| p["id"] == env.project_id.as_str()).unwrap();
    assert!(project.get("executor_pool").is_none());
}

