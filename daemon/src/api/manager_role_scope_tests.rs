use super::*;

#[tokio::test]
async fn expert_manager_sets_executor_and_advisor_roles_in_its_project_only() {
    let env = crate::testing::env().await;
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "role-manager").await;
    let peer = crate::testing::claude_bot(&env.app, &env.project_id, "role-peer").await;
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "role-child").await;
    sqlx::query("UPDATE bots SET managed_by='child',parent_bot_id=? WHERE id=?").bind(&manager.id).bind(&child.id).execute(&env.app.db).await.unwrap();
    let other_project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'expert')")
        .bind(&other_project).bind(format!("/tmp/{other_project}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let outsider = crate::testing::claude_bot(&env.app, &other_project, "role-outsider").await;
    let path = |id: &str| format!("/api/bots/{id}/role");
    let denied = |r: Result<(), LcError>| matches!(r, Err(LcError::Forbidden(_)));
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    assert!(denied(authorize_bot_path(&env.app, &manager.id, "PUT", &path(&peer.id)).await), "parent_child project: no exception");
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    assert!(authorize_bot_path(&env.app, &manager.id, "PUT", &path(&peer.id)).await.is_ok());
    assert!(authorize_bot_path(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", peer.id)).await.is_ok(), "model／effort／fast／name are allowed");
    let identity = patch_bot(State(env.app.clone()), Path(peer.id.clone()), Extension(RequestPrincipal::Bot(manager.id.clone())), Json(serde_json::from_value::<PatchBot>(json!({"identity": "x"})).unwrap())).await;
    assert!(matches!(identity, Err(LcError::Forbidden(_))), "identity stays user only");
    assert!(denied(authorize_bot_path(&env.app, &manager.id, "PUT", &path(&outsider.id)).await), "other project");
    assert!(denied(authorize_bot_path(&env.app, &peer.id, "PUT", &path(&manager.id)).await), "non-manager");
    let call = |caller: &str, target: &str, role: Option<&str>| {
        put_bot_role(State(env.app.clone()), Path(target.to_string()), Extension(RequestPrincipal::Bot(caller.to_string())), Json(PutBotRole { role: role.map(str::to_string), track: None }))
    };
    assert!(call(&manager.id, &peer.id, Some("manager")).await.is_err(), "manager cannot appoint another manager");
    assert!(expert_manager_over(&env.app, &manager.id, &db::bot(&env.app.db, &child.id).await.unwrap().unwrap()).await.unwrap(), "the manager's own child");
    assert!(!expert_manager_over(&env.app, &peer.id, &db::bot(&env.app.db, &child.id).await.unwrap().unwrap()).await.unwrap(), "someone else's child");
    let child_run = crate::testing::fake_run(&env.app, &child.id).await;
    put_bot_role(State(env.app.clone()), Path(child.id.clone()), Extension(RequestPrincipal::Bot(manager.id.clone())), Json(PutBotRole { role: Some("advisor".into()), track: None })).await.map_err(|e| format!("{e:?}")).unwrap();
    let policy: Option<String> = sqlx::query_scalar("SELECT runtime_role_policy FROM runs WHERE id=?").bind(&child_run).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(policy, Some(format!("{}:local:expert:advisor", env.project_id)), "a child's role applies without a restart");
    assert!(manager_may_start(&env.app, &manager.id, "POST", &format!("/api/bots/{}/restart", peer.id)).await);
    assert!(!manager_may_start(&env.app, &manager.id, "POST", &format!("/api/bots/{}/stop", peer.id)).await, "only start/restart");
    assert!(!manager_may_start(&env.app, &peer.id, "POST", &format!("/api/bots/{}/restart", manager.id)).await, "non-manager");
    assert!(!manager_may_start(&env.app, &manager.id, "POST", &format!("/api/bots/{}/restart", outsider.id)).await, "other project");
    assert!(call(&manager.id, &manager.id, None).await.is_err(), "cannot change itself");
}

#[tokio::test]
async fn expert_manager_soft_deletes_only_top_level_members_of_its_project() {
    let env = crate::testing::env().await;
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "del-manager").await;
    let peer = crate::testing::claude_bot(&env.app, &env.project_id, "del-peer").await;
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "del-child").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&peer.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor',managed_by='child',parent_bot_id=? WHERE id=?").bind(&manager.id).bind(&child.id).execute(&env.app.db).await.unwrap();
    let other_project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'expert')")
        .bind(&other_project).bind(format!("/tmp/{other_project}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let outsider = crate::testing::claude_bot(&env.app, &other_project, "del-outsider").await;
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&outsider.id).execute(&env.app.db).await.unwrap();
    let path = |id: &str| format!("/api/bots/{id}");
    let denied = |r: Result<(), LcError>| matches!(r, Err(LcError::Forbidden(_)));
    assert!(denied(authorize_bot_path(&env.app, &manager.id, "DELETE", &path(&peer.id)).await), "parent_child project: no exception");
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    assert!(authorize_bot_path(&env.app, &manager.id, "DELETE", &path(&peer.id)).await.is_ok(), "top-level executor in its project");
    assert!(denied(authorize_bot_path(&env.app, &manager.id, "DELETE", &path(&outsider.id)).await), "other project");
    assert!(denied(authorize_bot_path(&env.app, &manager.id, "DELETE", &path(&manager.id)).await), "cannot delete itself");
    assert!(denied(authorize_bot_path(&env.app, &peer.id, "DELETE", &path(&manager.id)).await), "non-manager");
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&peer.id).execute(&env.app.db).await.unwrap();
    assert!(denied(authorize_bot_path(&env.app, &manager.id, "DELETE", &path(&peer.id)).await), "advisor is not deleted by a manager");
}

/// 管理者真的改成員的模型／強度（走 handler 與設定檔）：專家、顧問、執行者都行；改名也行。
#[tokio::test]
async fn expert_manager_changes_model_effort_and_name_through_the_handler() {
    let env = crate::testing::env().await;
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        Ok(())
    }).await.unwrap();
    let add = |name: &'static str| {
        let app = env.app.clone();
        let pid = env.project_id.clone();
        async move {
            let res = create_bot(State(app), Path(pid), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
        }
    };
    let manager = add("h-manager").await;
    let specialist = add("h-specialist").await;
    let advisor = add("h-advisor").await;
    let peer = add("h-peer").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='specialist', track='shared-assistant' WHERE id=?").bind(&specialist).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&advisor).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&peer).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let patch = |target: &str, body: Value| {
        patch_bot(State(env.app.clone()), Path(target.to_string()), Extension(RequestPrincipal::Bot(manager.clone())), Json(serde_json::from_value::<PatchBot>(body).unwrap()))
    };
    let res = patch(&specialist, json!({"model": "claude-opus-5-5", "effort": "medium"})).await;
    assert!(res.is_ok(), "specialist model and effort: {:?}", res.err());
    assert!(patch(&advisor, json!({"effort": "high"})).await.is_ok(), "advisor effort");
    assert!(patch(&peer, json!({"model": "claude-haiku-5-5", "fast": false})).await.is_ok(), "executor model and fast");
    assert!(patch(&peer, json!({"name": "h-renamed"})).await.is_ok(), "rename");
    let effort: Option<String> = sqlx::query_scalar("SELECT effort FROM bots WHERE id=?").bind(&specialist).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(effort.as_deref(), Some("medium"));
}

#[tokio::test]
async fn expert_manager_patches_model_effort_fast_and_name_of_top_level_members_only() {
    let env = crate::testing::env().await;
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "pat-manager").await;
    let peer = crate::testing::claude_bot(&env.app, &env.project_id, "pat-peer").await;
    let other_manager = crate::testing::claude_bot(&env.app, &env.project_id, "pat-other-mgr").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&peer.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&other_manager.id).execute(&env.app.db).await.unwrap();
    let other_project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'expert')")
        .bind(&other_project).bind(format!("/tmp/{other_project}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let outsider = crate::testing::claude_bot(&env.app, &other_project, "pat-outsider").await;
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&outsider.id).execute(&env.app.db).await.unwrap();
    let patch = |caller: &str, target: &str, body: Value| {
        patch_bot(State(env.app.clone()), Path(target.to_string()), Extension(RequestPrincipal::Bot(caller.to_string())), Json(serde_json::from_value::<PatchBot>(body).unwrap()))
    };
    assert!(!manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", peer.id)).await, "parent_child project: no exception");
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    assert!(manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", peer.id)).await, "top-level executor in its project");
    assert!(!manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", outsider.id)).await, "other project");
    assert!(!manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", manager.id)).await, "cannot change itself");
    assert!(!manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", other_manager.id)).await, "another manager");
    assert!(!manager_may_start(&env.app, &peer.id, "PATCH", &format!("/api/bots/{}", manager.id)).await, "non-manager");
    // 顧問與專家也是成員：管理者能調模型／強度（wits lead 的實際需求：專家「OO共享助理」）。
    let advisor = crate::testing::claude_bot(&env.app, &env.project_id, "pat-advisor").await;
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&advisor.id).execute(&env.app.db).await.unwrap();
    let specialist = crate::testing::claude_bot(&env.app, &env.project_id, "pat-specialist").await;
    sqlx::query("UPDATE bots SET role='specialist', track='shared-assistant' WHERE id=?").bind(&specialist.id).execute(&env.app.db).await.unwrap();
    assert!(manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", advisor.id)).await, "advisor");
    assert!(manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", specialist.id)).await, "specialist");
    assert!(matches!(patch(&manager.id, &peer.id, json!({"identity": "x"})).await, Err(LcError::Forbidden(_))), "identity stays user only");
    assert!(matches!(patch(&manager.id, &peer.id, json!({"persona": "x"})).await, Err(LcError::Forbidden(_))), "persona is outside the manager scope");
    assert!(matches!(patch(&manager.id, &peer.id, json!({"autostart": true})).await, Err(LcError::Forbidden(_))), "autostart is outside the manager scope");
}

#[tokio::test]
async fn manager_role_change_is_written_like_the_web_setting() {
    let env = crate::testing::env().await;
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, handed_off_to: None, id: Some(pid), path: repo, label: "proj".into(), host: LOCAL_HOST.into(), bots: vec![] });
        Ok(())
    }).await.unwrap();
    let add = |name: &'static str| {
        let app = env.app.clone();
        let pid = env.project_id.clone();
        async move {
            let res = create_bot(State(app), Path(pid), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
            let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
            serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
        }
    };
    let manager = add("cfg-manager").await;
    let peer = add("cfg-peer").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let call = |role: &str| put_bot_role(State(env.app.clone()), Path(peer.clone()), Extension(RequestPrincipal::Bot(manager.clone())), Json(PutBotRole { role: Some(role.into()), track: None }));
    call("executor").await.map_err(|e| format!("{e:?}")).unwrap();
    assert_eq!(db::bot(&env.app.db, &peer).await.unwrap().unwrap().role.as_deref(), Some("executor"));
}
