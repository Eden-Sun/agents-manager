use crate::{api, db, state::App};
use serde_json::Value;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn distinct_bot(e: &crate::testing::Env, project_id: &str, name: &str) -> db::Bot {
    let bot = crate::testing::claude_bot(&e.app, project_id, name).await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?")
        .bind(format!("read-scope-{}", bot.id))
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
    let router = api::router(app);
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

async fn get(app: Arc<App>, path: &str, auth: &str) -> String {
    raw(
        app,
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Connection: close\r\n\r\n"),
    )
    .await
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or("")
}

fn assert_role_gate(response: &str, path: &str) {
    assert!(
        response.starts_with("HTTP/1.1 403") && body(response).contains("\"reason\":\"role_required\""),
        "ordinary bot read was not rejected at the AGM boundary for {path}: {response}"
    );
}

#[tokio::test]
async fn bot_state_is_scoped_to_self_and_descendants_and_redacts_launch_secrets() {
    let e = crate::testing::env().await;
    let owner = distinct_bot(&e, &e.project_id, "state-owner").await;

    let victim_project = db::ulid();
    let victim_path = e.dir.join("victim-project");
    std::fs::create_dir_all(&victim_path).unwrap();
    sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES (?,?,?,'local',?)")
        .bind(&victim_project)
        .bind(victim_path.to_string_lossy().to_string())
        .bind("victim-project-label")
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let victim = distinct_bot(&e, &victim_project, "state-victim").await;

    let child_project = db::ulid();
    let child_path = e.dir.join("child-project");
    std::fs::create_dir_all(&child_path).unwrap();
    sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES (?,?,?,'local',?)")
        .bind(&child_project)
        .bind(child_path.to_string_lossy().to_string())
        .bind("owned-child-project")
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let child = distinct_bot(&e, &child_project, "state-child").await;
    sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?")
        .bind(&owner.id)
        .bind(&child.id)
        .execute(&e.app.db)
        .await
        .unwrap();

    sqlx::query("UPDATE bots SET persona='OWNER_PERSONA_SECRET', args_json='[\"OWNER_ARG_SECRET\"]', env_json='{\"OWNER_ENV_SECRET\":\"owner-secret\"}', identity='owner-identity', cwd='/private/owner-cwd' WHERE id=?")
        .bind(&owner.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE bots SET persona='VICTIM_PERSONA_SECRET', args_json='[\"VICTIM_ARG_SECRET\"]', env_json='{\"VICTIM_ENV_SECRET\":\"victim-secret\"}', identity='victim-identity', cwd='/private/victim-cwd' WHERE id=?")
        .bind(&victim.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    e.app
        .cfg
        .update(|cfg| {
            cfg.identities.push(crate::config::IdentityCfg {
                name: "state-secret-identity".into(),
                kind: "claude".into(),
                host: None,
                env: [("IDENTITY_ENV_SECRET".into(), "identity-secret".into())].into(),
                args: vec!["IDENTITY_ARG_SECRET".into()],
            });
            cfg.hosts.push(crate::config::HostCfg {
                name: "victim-host".into(),
                ssh: "victim-host.example".into(),
                ssh_port: 22,
                ssh_opts: Vec::new(),
                herdr_session: "victim-session".into(),
                remote_path: "/private/victim-host-path".into(),
                shared_session: false,
            });
            Ok(())
        })
        .await
        .unwrap();

    let bot_view_response = get(e.app.clone(), "/api/state", &bot_headers(&owner)).await;
    assert!(bot_view_response.starts_with("HTTP/1.1 200"), "bot-scoped state remains available: {bot_view_response}");
    let bot_view: Value = serde_json::from_str(body(&bot_view_response)).unwrap();
    let bot_json = serde_json::to_string(&bot_view).unwrap();
    assert!(bot_json.contains(&owner.id), "the caller remains visible: {bot_json}");
    assert!(bot_json.contains(&child.id), "the caller's child remains visible: {bot_json}");
    assert!(!bot_json.contains(&victim.id), "unrelated bot id leaked: {bot_json}");
    assert!(!bot_json.contains("victim-project-label"), "unrelated project leaked: {bot_json}");
    assert!(!bot_json.contains(victim_path.to_str().unwrap()), "unrelated project path leaked: {bot_json}");
    for secret in [
        "OWNER_PERSONA_SECRET",
        "OWNER_ARG_SECRET",
        "owner-secret",
        "VICTIM_PERSONA_SECRET",
        "VICTIM_ARG_SECRET",
        "victim-secret",
        "IDENTITY_ENV_SECRET",
        "identity-secret",
        "IDENTITY_ARG_SECRET",
        "victim-host.example",
        "/private/victim-host-path",
    ] {
        assert!(!bot_json.contains(secret), "Bot state leaked {secret}: {bot_json}");
    }
    assert!(bot_view["hosts"].is_null(), "Bot state must not enumerate host configuration: {bot_view}");
    assert!(bot_view["identities"].is_null(), "Bot state must not enumerate configured identities: {bot_view}");

    let user_view_response = get(
        e.app.clone(),
        "/api/state",
        &format!("X-AM-Token: {}\r\n", e.app.ui_token),
    )
    .await;
    assert!(user_view_response.starts_with("HTTP/1.1 200"), "UI state remains available: {user_view_response}");
    let user_view = body(&user_view_response);
    assert!(user_view.contains(&victim.id) && user_view.contains("VICTIM_ENV_SECRET"), "User retains full UI state: {user_view}");
}

#[tokio::test]
async fn global_message_search_and_evidence_are_user_or_agm_only() {
    let e = crate::testing::env().await;
    let caller = distinct_bot(&e, &e.project_id, "global-search-caller").await;
    let victim = distinct_bot(&e, &e.project_id, "global-search-victim").await;
    let conversation = db::conversation_id(&e.app.db, &victim.id).await.unwrap();
    sqlx::query("INSERT INTO messages (id,conversation_id,role,content,source,created_at) VALUES (?,?, 'assistant','GLOBAL_CROSS_READ_SENTINEL','terminal_fallback',?)")
        .bind(db::ulid())
        .bind(conversation)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();

    let search = get(e.app.clone(), "/api/search/messages?q=GLOBAL_CROSS_READ_SENTINEL", &bot_headers(&caller)).await;
    assert_role_gate(&search, "/api/search/messages");
    let evidence = get(e.app.clone(), "/api/supervisor/evidence?q=GLOBAL_CROSS_READ_SENTINEL", &bot_headers(&caller)).await;
    assert_role_gate(&evidence, "/api/supervisor/evidence");

    let user_auth = format!("X-AM-Token: {}\r\n", e.app.ui_token);
    for path in ["/api/search/messages?q=GLOBAL_CROSS_READ_SENTINEL", "/api/supervisor/evidence?q=GLOBAL_CROSS_READ_SENTINEL"] {
        let as_user = get(e.app.clone(), path, &user_auth).await;
        assert!(as_user.starts_with("HTTP/1.1 200") && body(&as_user).contains("GLOBAL_CROSS_READ_SENTINEL"), "User's existing history search remains intact at {path}: {as_user}");
    }

    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &caller.id, &e.project_id, "/tmp").await.unwrap();
    for path in ["/api/search/messages?q=GLOBAL_CROSS_READ_SENTINEL", "/api/supervisor/evidence?q=GLOBAL_CROSS_READ_SENTINEL"] {
        let as_agm = get(e.app.clone(), path, &bot_headers(&caller)).await;
        assert!(as_agm.starts_with("HTTP/1.1 200") && body(&as_agm).contains("GLOBAL_CROSS_READ_SENTINEL"), "verified AGM role retains global history access at {path}: {as_agm}");
    }
}

#[tokio::test]
async fn supervisor_management_reads_require_user_or_a_verified_agm_role() {
    let e = crate::testing::env().await;
    let caller = distinct_bot(&e, &e.project_id, "supervisor-read-caller").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    let now = db::now();
    let assignment_id = db::ulid();
    sqlx::query("INSERT INTO supervisor_assignments (id,supervisor_id,target_bot_id,client_request_id,text,created_at,updated_at) VALUES (?,'main',?,?, 'supervisor-private-assignment',?,?)")
        .bind(&assignment_id)
        .bind(&caller.id)
        .bind(format!("read-scope-{assignment_id}"))
        .bind(&now)
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    let paths = [
        "/api/supervisor",
        "/api/supervisor/health",
        "/api/supervisor/assignments",
        "/api/supervisor/handoff",
        "/api/claude-update/review",
        "/api/supervisor/incidents?all=1",
        "/api/supervisor/persona",
        "/api/supervisor/build-inputs",
        "/api/supervisor/remote",
        "/api/supervisor/approvals",
        "/api/supervisor/maintenance/safety",
        "/api/supervisor/leases",
        "/api/supervisor/inbox?all=1",
        "/api/supervisor/herdr-maintenance",
        "/api/supervisor/state",
        "/api/supervisor/cli",
        "/api/supervisor/responder",
        "/api/supervisor/responder/persona",
        "/api/supervisor/evidence?q=supervisor-private-assignment",
        "/api/intents",
    ];
    for path in paths {
        let response = get(e.app.clone(), path, &bot_headers(&caller)).await;
        assert_role_gate(&response, path);
    }
    let detail = get(
        e.app.clone(),
        &format!("/api/supervisor/assignments/{assignment_id}"),
        &bot_headers(&caller),
    )
    .await;
    assert_role_gate(&detail, "/api/supervisor/assignments/{id}");

    let user_auth = format!("X-AM-Token: {}\r\n", e.app.ui_token);
    for path in ["/api/supervisor/handoff", "/api/supervisor/inbox?all=1", "/api/supervisor/state"] {
        let as_user = get(e.app.clone(), path, &user_auth).await;
        assert!(!as_user.contains("role_required"), "User's management read remains available at {path}: {as_user}");
    }

    crate::supervisor::store::set_env(&e.app.db, &caller.id, &e.project_id, "/tmp").await.unwrap();
    for path in ["/api/supervisor/handoff", "/api/supervisor/inbox?all=1", "/api/supervisor/state"] {
        let as_agm = get(e.app.clone(), path, &bot_headers(&caller)).await;
        assert!(!as_agm.contains("role_required"), "AGM role retains required management read at {path}: {as_agm}");
    }
}
