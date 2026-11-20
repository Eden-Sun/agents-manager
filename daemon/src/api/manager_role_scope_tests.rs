use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

async fn distinct_bot(e: &crate::testing::Env, project_id: &str, name: &str) -> db::Bot {
    let bot = crate::testing::claude_bot(&e.app, project_id, name).await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?")
        .bind(format!("token-{}", bot.id))
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    db::bot(&e.app.db, &bot.id).await.unwrap().unwrap()
}

fn bot_headers(bot: &db::Bot) -> String {
    format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot.id, bot.hook_token)
}

async fn raw(app: Arc<App>, request: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    client.read_to_string(&mut response).await.unwrap();
    server.abort();
    response
}

async fn http_get(app: Arc<App>, path: &str, auth: &str) -> String {
    raw(
        app,
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"),
    )
    .await
}

async fn http_post(app: Arc<App>, path: &str, auth: &str, payload: &str) -> String {
    raw(
        app,
        format!(
            "POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        ),
    )
    .await
}

async fn http_delete(app: Arc<App>, path: &str, auth: &str) -> String {
    raw(
        app,
        format!("DELETE {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"),
    )
    .await
}

#[tokio::test]
async fn expert_manager_may_read_its_project_members_including_claude_api_bots() {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?")
        .bind(&env.project_id)
        .execute(&env.app.db)
        .await
        .unwrap();

    let manager = distinct_bot(&env, &env.project_id, "exp-manager").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?")
        .bind(&manager.id)
        .execute(&env.app.db)
        .await
        .unwrap();

    let executor = distinct_bot(&env, &env.project_id, "exp-executor").await;
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?")
        .bind(&executor.id)
        .execute(&env.app.db)
        .await
        .unwrap();

    let api_member = distinct_bot(&env, &env.project_id, "exp-api-member").await;
    sqlx::query("UPDATE bots SET kind='claude-api', role='advisor', model='claude-opus-5-5' WHERE id=?")
        .bind(&api_member.id)
        .execute(&env.app.db)
        .await
        .unwrap();

    let other_project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'expert')")
        .bind(&other_project)
        .bind(format!("/tmp/{other_project}"))
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();
    let other_manager = distinct_bot(&env, &other_project, "other-manager").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?")
        .bind(&other_manager.id)
        .execute(&env.app.db)
        .await
        .unwrap();

    // 1. Manager GETs member's messages -> 200 OK
    let resp = http_get(env.app.clone(), &format!("/api/bots/{}/messages", executor.id), &bot_headers(&manager)).await;
    assert!(resp.starts_with("HTTP/1.1 200"), "manager reading executor messages: {resp}");

    // 1b. Manager GETs claude-api member's messages -> 200 OK
    let resp_api = http_get(env.app.clone(), &format!("/api/bots/{}/messages", api_member.id), &bot_headers(&manager)).await;
    assert!(resp_api.starts_with("HTTP/1.1 200"), "manager reading claude-api member messages: {resp_api}");

    // 2. Manager of another project GETs member's messages -> 403 Forbidden with bot_resource_scope
    let resp_other = http_get(env.app.clone(), &format!("/api/bots/{}/messages", executor.id), &bot_headers(&other_manager)).await;
    assert!(resp_other.starts_with("HTTP/1.1 403"), "other project manager must get 403: {resp_other}");
    assert!(resp_other.contains("bot_resource_scope"), "must fail at bot_resource_scope: {resp_other}");

    // 3. Non-manager member reading another member -> 403 Forbidden with bot_resource_scope
    let resp_peer = http_get(env.app.clone(), &format!("/api/bots/{}/messages", api_member.id), &bot_headers(&executor)).await;
    assert!(resp_peer.starts_with("HTTP/1.1 403"), "peer member must get 403: {resp_peer}");
    assert!(resp_peer.contains("bot_resource_scope"), "must fail at bot_resource_scope: {resp_peer}");

    // 4. Manager POSTing to a write route on a member (other than allowed ones) -> still 403
    let resp_write = http_post(env.app.clone(), &format!("/api/bots/{}/fork", executor.id), &bot_headers(&manager), "{}").await;
    assert!(resp_write.starts_with("HTTP/1.1 403"), "manager POST to write route must get 403: {resp_write}");

    let err = authorize_bot_path(&env.app, &manager.id, "POST", &format!("/api/bots/{}/fork", executor.id)).await.unwrap_err();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["reason"] == "bot_resource_scope"), "authorize_bot_path must deny write with bot_resource_scope: {err:?}");

    // 5. Turns and attachments owned by a member (including claude-api member)
    // 5a. Executor turn & attachment
    let exec_conv = db::conversation_id(&env.app.db, &executor.id).await.unwrap();
    let exec_turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, created_at) VALUES (?, ?, 'web', 'completed', ?)")
        .bind(&exec_turn_id)
        .bind(&exec_conv)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();

    let exec_attachment = crate::attach::save(&env.app, &executor.id, "exec.txt", "text/plain", b"exec attachment data").await.unwrap();

    // 5b. Claude-api member turn & attachment
    let api_conv = db::conversation_id(&env.app.db, &api_member.id).await.unwrap();
    let api_turn_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, created_at) VALUES (?, ?, 'web', 'completed', ?)")
        .bind(&api_turn_id)
        .bind(&api_conv)
        .bind(db::now())
        .execute(&env.app.db)
        .await
        .unwrap();

    let api_attachment = crate::attach::save(&env.app, &api_member.id, "api.txt", "text/plain", b"api attachment data").await.unwrap();

    // Manager GET on turn -> Ok(())
    assert!(authorize_bot_path(&env.app, &manager.id, "GET", &format!("/api/turns/{exec_turn_id}")).await.is_ok());
    assert!(authorize_bot_path(&env.app, &manager.id, "GET", &format!("/api/turns/{api_turn_id}")).await.is_ok());

    // Other manager or peer GET on turn -> 403 Forbidden
    assert!(matches!(
        authorize_bot_path(&env.app, &other_manager.id, "GET", &format!("/api/turns/{exec_turn_id}")).await,
        Err(LcError::Forbidden(_))
    ));
    assert!(matches!(
        authorize_bot_path(&env.app, &executor.id, "GET", &format!("/api/turns/{api_turn_id}")).await,
        Err(LcError::Forbidden(_))
    ));

    // Manager GET on attachment -> Ok(())
    assert!(authorize_bot_path(&env.app, &manager.id, "GET", &format!("/api/attachments/{}", exec_attachment.id)).await.is_ok());
    assert!(authorize_bot_path(&env.app, &manager.id, "GET", &format!("/api/attachments/{}", api_attachment.id)).await.is_ok());

    // Other manager or peer GET on attachment -> 403 Forbidden
    assert!(matches!(
        authorize_bot_path(&env.app, &other_manager.id, "GET", &format!("/api/attachments/{}", exec_attachment.id)).await,
        Err(LcError::Forbidden(_))
    ));
    assert!(matches!(
        authorize_bot_path(&env.app, &executor.id, "GET", &format!("/api/attachments/{}", api_attachment.id)).await,
        Err(LcError::Forbidden(_))
    ));

    // get_attachment handler invocation -> 200 OK
    let res_exec = get_attachment(State(env.app.clone()), Path(exec_attachment.id.clone())).await.unwrap();
    assert_eq!(res_exec.status(), StatusCode::OK);
    let res_api = get_attachment(State(env.app.clone()), Path(api_attachment.id.clone())).await.unwrap();
    assert_eq!(res_api.status(), StatusCode::OK);

    // Note: GET /api/attachments/{id} over HTTP dies at bot_user_only because it is UserOnly in bot_route_policy
    let resp_att_http = http_get(env.app.clone(), &format!("/api/attachments/{}", exec_attachment.id), &bot_headers(&manager)).await;
    assert!(resp_att_http.starts_with("HTTP/1.1 403"), "UserOnly routes die at bot_user_only before authorize_bot_path: {resp_att_http}");
    assert!(resp_att_http.contains("user_only"), "UserOnly reason: {resp_att_http}");
}

#[tokio::test]
async fn expert_manager_deletes_paused_executor_in_its_project_only() {
    let env = crate::testing::env().await;
    let (pid, repo) = (env.project_id.clone(), env.repo.to_string_lossy().to_string());

    let manager = distinct_bot(&env, &env.project_id, "p-manager").await;
    let paused_executor = distinct_bot(&env, &env.project_id, "p-paused-exec").await;
    let paused_advisor = distinct_bot(&env, &env.project_id, "p-paused-adv").await;

    sqlx::query("UPDATE bots SET role='manager', manager_use=1 WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor', manager_use=0 WHERE id=?").bind(&paused_executor.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='advisor', manager_use=0 WHERE id=?").bind(&paused_advisor.id).execute(&env.app.db).await.unwrap();

    let other_project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at,mode) VALUES(?,?,'other','local',?,'parent_child')")
        .bind(&other_project).bind(format!("/tmp/{other_project}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let other_paused_exec = distinct_bot(&env, &other_project, "p-other-paused").await;
    sqlx::query("UPDATE bots SET role='executor', manager_use=0 WHERE id=?").bind(&other_paused_exec.id).execute(&env.app.db).await.unwrap();

    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();

    let (m_id, pe_id, pa_id, other_pid, other_pe_id) = (manager.id.clone(), paused_executor.id.clone(), paused_advisor.id.clone(), other_project.clone(), other_paused_exec.id.clone());
    // 把專案與 bot 寫進 config，以便完整測試 delete_bot
    env.app.cfg.update(move |cfg| {
        cfg.projects.push(crate::config::ProjectCfg {
            mode: "expert".into(),
            role_policy: Default::default(),
            deputy: None,
            specialist_dispatch: true,
            handed_off_to: None,
            id: Some(pid),
            path: repo,
            label: "proj".into(),
            host: LOCAL_HOST.into(),
            bots: vec![
                toml::from_str(&format!("id = '{m_id}'\nname = 'p-manager'\nkind = 'claude'\nrole = 'manager'\n")).unwrap(),
                toml::from_str(&format!("id = '{pe_id}'\nname = 'p-paused-exec'\nkind = 'claude'\nrole = 'executor'\nmanager_use = false\n")).unwrap(),
                toml::from_str(&format!("id = '{pa_id}'\nname = 'p-paused-adv'\nkind = 'claude'\nrole = 'advisor'\nmanager_use = false\n")).unwrap(),
            ],
        });
        cfg.projects.push(crate::config::ProjectCfg {
            mode: "parent_child".into(),
            role_policy: Default::default(),
            deputy: None,
            specialist_dispatch: true,
            handed_off_to: None,
            id: Some(other_pid.clone()),
            path: format!("/tmp/{other_pid}"),
            label: "other".into(),
            host: LOCAL_HOST.into(),
            bots: vec![
                toml::from_str(&format!("id = '{other_pe_id}'\nname = 'p-other-paused'\nkind = 'claude'\n")).unwrap(),
            ],
        });
        Ok(())
    }).await.unwrap();

    // 1. manager 刪自己專案停派執行者 -> 成功（manager_may_start 放行，HTTP DELETE 成功）
    assert!(manager_may_start(&env.app, &manager.id, "DELETE", &format!("/api/bots/{}", paused_executor.id)).await, "manager may delete paused executor");
    assert!(authorize_bot_path(&env.app, &manager.id, "DELETE", &format!("/api/bots/{}", paused_executor.id)).await.is_ok());

    let resp_del = http_delete(env.app.clone(), &format!("/api/bots/{}", paused_executor.id), &bot_headers(&manager)).await;
    assert!(resp_del.starts_with("HTTP/1.1 200"), "manager successfully deleted paused executor: {resp_del}");
    let deleted_at: Option<String> = sqlx::query_scalar("SELECT deleted_at FROM bots WHERE id=?").bind(&paused_executor.id).fetch_one(&env.app.db).await.unwrap();
    assert!(deleted_at.is_some(), "paused executor is soft-deleted");

    // 2. manager 刪別專案停派執行者 -> 403
    assert!(!manager_may_start(&env.app, &manager.id, "DELETE", &format!("/api/bots/{}", other_paused_exec.id)).await, "cannot delete paused executor in other project");
    let resp_del_other = http_delete(env.app.clone(), &format!("/api/bots/{}", other_paused_exec.id), &bot_headers(&manager)).await;
    assert!(resp_del_other.starts_with("HTTP/1.1 403"), "delete other project paused executor must return 403: {resp_del_other}");

    // 3. manager start 停派執行者 -> 仍 403
    let paused_exec2 = distinct_bot(&env, &env.project_id, "p-paused-exec2").await;
    sqlx::query("UPDATE bots SET role='executor', manager_use=0 WHERE id=?").bind(&paused_exec2.id).execute(&env.app.db).await.unwrap();

    assert!(!manager_may_start(&env.app, &manager.id, "POST", &format!("/api/bots/{}/start", paused_exec2.id)).await, "manager cannot start paused executor");
    assert!(!manager_may_start(&env.app, &manager.id, "POST", &format!("/api/bots/{}/restart", paused_exec2.id)).await, "manager cannot restart paused executor");

    let resp_start = http_post(env.app.clone(), &format!("/api/bots/{}/start", paused_exec2.id), &bot_headers(&manager), "{}").await;
    assert!(resp_start.starts_with("HTTP/1.1 403"), "start paused executor must return 403: {resp_start}");
    assert!(resp_start.contains("user_only"), "reason must be user_only: {resp_start}");

    // 4. manager 刪停派顧問 -> 403
    assert!(!manager_may_start(&env.app, &manager.id, "DELETE", &format!("/api/bots/{}", paused_advisor.id)).await, "manager cannot delete paused advisor");
    let resp_del_adv = http_delete(env.app.clone(), &format!("/api/bots/{}", paused_advisor.id), &bot_headers(&manager)).await;
    assert!(resp_del_adv.starts_with("HTTP/1.1 403"), "delete paused advisor must return 403: {resp_del_adv}");
    assert!(resp_del_adv.contains("user_only"), "reason must be user_only: {resp_del_adv}");

    // 5. PATCH 停派執行者（改名）放行
    assert!(manager_may_start(&env.app, &manager.id, "PATCH", &format!("/api/bots/{}", paused_exec2.id)).await, "manager may patch paused executor");
}
