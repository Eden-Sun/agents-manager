use super::*;

/// 專家模式專案（一位 manager、池裡有 claude 與 codex 條目）。回傳 manager 的 bot id。
async fn expert_project(env: &crate::testing::Env) -> String {
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        Ok(())
    }).await.unwrap();
    let manager = add_bot(env, &env.project_id, "claude", false).await;
    let set_manager = serde_json::from_value::<PatchBot>(json!({"role": "manager"})).unwrap();
    patch_bot(State(env.app.clone()), Path(manager.clone()), Extension(RequestPrincipal::User), Json(set_manager)).await.map_err(|e| format!("{e:?}")).unwrap();
    crate::projection::update_and_project(&env.app.cfg, &env.app.db, |cfg| {
        for p in cfg.projects.iter_mut() {
            p.mode = "expert".into();
        }
        Ok(())
    }).await.map_err(|e| format!("{e:?}")).unwrap();
    manager
}

/// 排程專屬 bot（role = scheduler）：建立時就帶角色、不進 peers／群集、不是派工目標。
#[tokio::test]
async fn a_scheduler_bot_is_not_a_peer_a_cluster_member_or_a_dispatch_target() {
    let env = crate::testing::env().await;
    let manager = expert_project(&env).await;
    let create = |body: Value| create_bot(State(env.app.clone()), Path(env.project_id.clone()), Json(serde_json::from_value(body).unwrap()));
    let err = create(json!({"name": "bad-role", "kind": "claude", "role": "manager"})).await.err().unwrap();
    assert!(matches!(err, LcError::Bad(_)), "{err:?}");
    // 專家專案沒帶 role＝執行者；也可指定顧問。
    let res = create(json!({"name": "no-role", "kind": "claude"})).await.unwrap();
    let nr = serde_json::from_slice::<Value>(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap()["bot_id"].as_str().unwrap().to_string();
    assert_eq!(db::bot(&env.app.db, &nr).await.unwrap().unwrap().role.as_deref(), Some("executor"));
    let res = create(json!({"name": "adv-new", "kind": "claude", "role": "advisor"})).await.unwrap();
    let av = serde_json::from_slice::<Value>(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap()["bot_id"].as_str().unwrap().to_string();
    assert_eq!(db::bot(&env.app.db, &av).await.unwrap().unwrap().role.as_deref(), Some("advisor"));
    let res = create(json!({"name": "sched-a", "kind": "claude", "role": "scheduler"})).await.unwrap();
    let sched = serde_json::from_slice::<Value>(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap()["bot_id"].as_str().unwrap().to_string();
    assert_eq!(db::bot(&env.app.db, &sched).await.unwrap().unwrap().role.as_deref(), Some("scheduler"));
    // 管理者也能增聘專屬 bot（走既有 hire 權限）。
    let res = hire(&env, &manager, json!({"name": "sched-b", "kind": "claude", "role": "scheduler"})).await.unwrap();
    let hired = serde_json::from_slice::<Value>(&axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap()).unwrap()["bot_id"].as_str().unwrap().to_string();
    assert_eq!(db::bot(&env.app.db, &hired).await.unwrap().unwrap().role.as_deref(), Some("scheduler"));
    // 也有一般的執行者作對照。
    let worker = add_bot(&env, &env.project_id, "claude", false).await;
    patch_bot(State(env.app.clone()), Path(worker.clone()), Extension(RequestPrincipal::User), Json(serde_json::from_value(json!({"role": "executor"})).unwrap())).await.map_err(|e| format!("{e:?}")).unwrap();

    let Json(peers) = crate::expert_peers::list(State(env.app.clone()), Path(env.project_id.clone()), Query(crate::expert_peers::PeersQuery { for_track: None }), Extension(RequestPrincipal::Bot(manager.clone()))).await.unwrap();
    let ids: Vec<&str> = peers["peers"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
    assert!(ids.contains(&worker.as_str()) && !ids.contains(&sched.as_str()) && !ids.contains(&hired.as_str()), "{ids:?}");
    let clusters = crate::expert_cluster::project_clusters(&env.app, &env.project_id).await.unwrap();
    assert!(clusters.iter().flat_map(|c| c.members.iter()).all(|m| m.id != sched && m.id != hired), "排程專屬不進群集");
    for to in [&sched, &hired] {
        let denied = crate::relay_policy::relay_prompt_allowed(&env.app, &manager, to, crate::relay_policy::Admission::NewRequest).await;
        assert!(matches!(denied, Err(crate::relay_policy::RelayDenied::Denied(ref m)) if m == "schedule_bot_not_dispatchable"), "{denied:?}");
    }
}

/// 建一顆 bot（名字唯一）；`session` 為真就給它一段已結束的 run 與 codex session。
async fn add_bot(env: &crate::testing::Env, project_id: &str, kind: &str, session: bool) -> String {
    let name = format!("b{}", db::ulid().chars().rev().take(10).collect::<String>().to_lowercase());
    let res = create_bot(State(env.app.clone()), Path(project_id.to_string()), Json(serde_json::from_value(json!({"name": name, "kind": kind})).unwrap())).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    if session {
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at) VALUES (?,?,'stopped','idle','sid-src','2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')")
            .bind(db::ulid())
            .bind(&id)
            .execute(&env.app.db)
            .await
            .unwrap();
    }
    id
}

fn hire(env: &crate::testing::Env, manager: &str, body: Value) -> impl std::future::Future<Output = Result<Response, LcError>> {
    let app = env.app.clone();
    let pid = env.project_id.clone();
    let caller = RequestPrincipal::Bot(manager.to_string());
    let body = serde_json::from_value::<HireMember>(body).unwrap();
    async move { hire_member(State(app), Path(pid), Extension(caller), Json(body)).await }
}

#[tokio::test]
async fn fork_source_is_checked_and_refused_with_a_reason() {
    let env = crate::testing::env().await;
    let manager = expert_project(&env).await;
    let claude_src = add_bot(&env, &env.project_id, "claude", true).await;
    let codex_src = add_bot(&env, &env.project_id, "codex", true).await;
    let codex_nosess = add_bot(&env, &env.project_id, "codex", false).await;
    // 來源不是 codex。
    let err = hire(&env, &manager, json!({"name": "fk1", "kind": "codex", "fork_from": claude_src})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "fork_source_not_codex"), "{err:?}");
    // 新成員不是 codex。
    let err = hire(&env, &manager, json!({"name": "fk2", "kind": "claude", "fork_from": codex_src})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "fork_target_not_codex"), "{err:?}");
    // 來源沒有 session。
    let err = hire(&env, &manager, json!({"name": "fk3", "kind": "codex", "fork_from": codex_nosess})).await.err().unwrap();
    assert!(matches!(err, LcError::Conflict(ref v) if v["reason"] == "fork_source_no_session"), "{err:?}");
    // 來源不存在。
    let err = hire(&env, &manager, json!({"name": "fk4", "kind": "codex", "fork_from": "nope"})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "fork_source_not_found"), "{err:?}");
}

#[tokio::test]
async fn fork_source_from_another_project_is_refused() {
    let env = crate::testing::env().await;
    let manager = expert_project(&env).await;
    let other = crate::db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES (?,?,?,?)").bind(&other).bind(format!("/tmp/{other}")).bind("other").bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    let foreign = crate::testing::claude_bot(&env.app, &other, "foreign").await.id;
    sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&foreign).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at) VALUES (?,?,'stopped','idle','sid-f','2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')")
        .bind(crate::db::ulid()).bind(&foreign).execute(&env.app.db).await.unwrap();
    let err = hire(&env, &manager, json!({"name": "fk5", "kind": "codex", "fork_from": foreign})).await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "fork_source_other_project"), "{err:?}");
}

#[tokio::test]
async fn fork_source_on_another_host_is_refused() {
    // 來源 bot 的主機由它的專案決定（`bot_host` 就是專案的 host），所以這裡直接用「專案的 host 跟來源不同」驗證檢查本身。
    let env = crate::testing::env().await;
    let _manager = expert_project(&env).await;
    let source = add_bot(&env, &env.project_id, "codex", true).await;
    let mut project = db::project(&env.app.db, &env.project_id).await.unwrap().unwrap();
    project.host = "mac_builder".into();
    let err = validate_fork_source(&env.app, &project, &source, "codex").await.err().unwrap();
    assert!(matches!(err, LcError::BadValue(ref v) if v["reason"] == "fork_source_other_host"), "{err:?}");
}

#[tokio::test]
async fn a_fork_member_remembers_its_source_and_manager_can_hire_another() {
    let env = crate::testing::env().await;
    let manager = expert_project(&env).await;
    let source = add_bot(&env, &env.project_id, "codex", true).await;

    let response = hire(&env, &manager, json!({"name": "forked", "kind": "codex", "fork_from": source})).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    let cfg_bot = env.app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|b| b.id.as_deref() == Some(id.as_str())).cloned().unwrap();
    assert_eq!(cfg_bot.fork_from.as_deref(), Some(source.as_str()));

    let another = hire(&env, &manager, json!({"name": "again", "kind": "codex", "model": "gpt-6.1-sol", "effort": "high"})).await.unwrap();
    assert_eq!(another.status(), StatusCode::OK);
}

/// 2026-11-08（wits tr3）：fork hire 之後 /api/state 必須看得到這顆 bot（側欄與群集計數都從 state 來）。
/// fork hire 沒帶 model／effort／identity：從來源 bot 繼承；有帶的照給（不被來源蓋掉）。
#[tokio::test]
async fn a_fork_hire_inherits_model_effort_and_identity_it_was_not_given() {
    let env = crate::testing::env().await;
    let manager = expert_project(&env).await;
    env.app.cfg.update(|f| {
        f.identities.push(crate::config::IdentityCfg { name: "co0".into(), kind: "codex".into(), host: None, env: Default::default(), args: vec![] });
        Ok(())
    }).await.unwrap();
    // 來源要經由 create_bot 建（設定寫進 config.toml）：直接改 DB 會被之後的投影蓋回去。
    let res = create_bot(
        State(env.app.clone()),
        Path(env.project_id.clone()),
        Json(serde_json::from_value(json!({"name": "src-terra", "kind": "codex", "model": "gpt-6.1-terra", "effort": "medium", "identity": "co0"})).unwrap()),
    )
    .await
    .unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let source = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at) VALUES (?,?,'stopped','idle','sid-src','2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')")
        .bind(db::ulid())
        .bind(&source)
        .execute(&env.app.db)
        .await
        .unwrap();

    let inherited = hire(&env, &manager, json!({"name": "tr3", "kind": "codex", "fork_from": source})).await.unwrap();
    let bytes = axum::body::to_bytes(inherited.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    let bot = db::bot(&env.app.db, &id).await.unwrap().unwrap();
    assert_eq!(bot.model.as_deref(), Some("gpt-6.1-terra"));
    assert_eq!(bot.effort.as_deref(), Some("medium"));
    assert_eq!(bot.identity.as_deref(), Some("co0"));

    let overridden = hire(&env, &manager, json!({"name": "tr4", "kind": "codex", "fork_from": source, "effort": "high"})).await.unwrap();
    let bytes = axum::body::to_bytes(overridden.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();
    let bot = db::bot(&env.app.db, &id).await.unwrap().unwrap();
    assert_eq!(bot.effort.as_deref(), Some("high"), "明確給的 effort 不被來源蓋掉");
    assert_eq!(bot.model.as_deref(), Some("gpt-6.1-terra"), "沒給的 model 仍繼承");
}

#[tokio::test]
async fn a_forked_hire_shows_up_in_state() {
    let env = crate::testing::env().await;
    let manager = expert_project(&env).await;
    let source = add_bot(&env, &env.project_id, "codex", true).await;
    let response = hire(&env, &manager, json!({"name": "tr3", "kind": "codex", "model": "gpt-6.1-terra", "fork_from": source})).await.unwrap();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let id = serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string();

    let state = state_json(&env.app).await.unwrap();
    let mine = state["projects"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["bots"].as_array().into_iter().flatten())
        .find(|b| b["id"] == id.as_str())
        .unwrap_or_else(|| panic!("forked bot missing from /api/state: {state}"));
    assert_eq!(mine["role"], "executor");
    assert_eq!(mine["name"], "tr3");
}
