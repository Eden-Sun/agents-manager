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

async fn send(app: Arc<App>, method: &str, path: &str, auth: &str, payload: &str) -> String {
    raw(
        app,
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
            payload.len()
        ),
    )
    .await
}

fn body(response: &str) -> &str {
    response.split_once("\r\n\r\n").map(|(_, body)| body).unwrap_or("")
}

fn assert_role_gate(response: &str, path: &str) {
    assert!(
        // AGM 角色閘（role_required），或更嚴的只限網頁（user_only，例如 claude-update review）都算擋下。
        response.starts_with("HTTP/1.1 403")
            && (body(response).contains("\"reason\":\"role_required\"")
                || body(response).contains("\"reason\":\"user_only\"")),
        "ordinary bot read was not rejected at the AGM boundary for {path}: {response}"
    );
}

fn assert_user_only(response: &str, path: &str) {
    assert!(
        response.starts_with("HTTP/1.1 403") && body(response).contains("\"reason\":\"user_only\""),
        "Bot principal must be rejected as user_only at {path}: {response}"
    );
}

fn assert_no_browser_state_events(events: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) {
    loop {
        match events.try_recv() {
            Ok(event) => assert!(
                !matches!(event.kind.as_str(), "draft_updated" | "bot_read" | "group_read"),
                "rejected Bot request emitted a browser-state event: {event:?}"
            ),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => return,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(count)) => {
                panic!("test event receiver unexpectedly lagged by {count}")
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Closed) => return,
        }
    }
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
    ];
    for path in paths {
        let response = get(e.app.clone(), path, &bot_headers(&caller)).await;
        assert_role_gate(&response, path);
    }
    let intents = get(e.app.clone(), "/api/intents", &bot_headers(&caller)).await;
    assert_user_only(&intents, "/api/intents");
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

#[tokio::test]
async fn browser_drafts_and_shared_read_marks_are_user_only() {
    let e = crate::testing::env().await;
    let caller = distinct_bot(&e, &e.project_id, "browser-state-caller").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();

    let own_draft_key = format!("bot:{}", caller.id);
    let private_draft_key = "shell:local/private-pane";
    crate::drafts::put(&e.app.db, &own_draft_key, "unsent-private-draft").await.unwrap();
    crate::drafts::put(&e.app.db, private_draft_key, "unsent-shell-command").await.unwrap();
    let conversation = db::conversation_id(&e.app.db, &caller.id).await.unwrap();
    let at = db::now();
    sqlx::query("INSERT INTO messages (id,conversation_id,role,content,source,created_at) VALUES (?,?, 'assistant','unread bot reply','hook',?)")
        .bind(db::ulid())
        .bind(&conversation)
        .bind(&at)
        .execute(&e.app.db)
        .await
        .unwrap();
    let group_turn = db::ulid();
    sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,created_at,completed_at) VALUES (?,?,'web','completed',?,?)")
        .bind(&group_turn)
        .bind(&conversation)
        .bind(&at)
        .bind(&at)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,group_id,created_at) VALUES (?,?,?,'user','group prompt','web','shared-group',?)")
        .bind(db::ulid())
        .bind(&conversation)
        .bind(&group_turn)
        .bind(&at)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO messages (id,conversation_id,turn_id,role,content,source,created_at) VALUES (?,?,?,'assistant','group reply','hook',?)")
        .bind(db::ulid())
        .bind(&conversation)
        .bind(&group_turn)
        .bind(&at)
        .execute(&e.app.db)
        .await
        .unwrap();
    assert_eq!(crate::read_marks::unread_counts(&e.app.db).await.unwrap().get(&caller.id), Some(&2));
    assert_eq!(crate::read_marks::group_unread_counts(&e.app.db).await.unwrap().get(&e.project_id), Some(&1));
    let mut events = e.app.subscribe();
    let bot_auth = bot_headers(&caller);
    let user_auth = format!("X-AM-Token: {}\r\n", e.app.ui_token);

    for is_agm in [false, true] {
        if is_agm {
            crate::supervisor::store::set_env(&e.app.db, &caller.id, &e.project_id, "/tmp").await.unwrap();
        }
        let list = get(e.app.clone(), "/api/drafts", &bot_auth).await;
        assert_user_only(&list, "GET /api/drafts");
        let write = send(
            e.app.clone(),
            "PUT",
            &format!("/api/drafts/bot%3A{}", caller.id),
            &bot_auth,
            r#"{"text":"attacker-overwrite","client_id":"worker"}"#,
        )
        .await;
        assert_user_only(&write, "PUT /api/drafts/{key}");
        let response = send(
            e.app.clone(),
            "POST",
            &format!("/api/bots/{}/read", caller.id),
            &bot_auth,
            "{}",
        )
        .await;
        assert_user_only(&response, if is_agm { "POST /api/bots/{id}/read (AGM)" } else { "POST /api/bots/{id}/read" });
        let response = send(
            e.app.clone(),
            "POST",
            &format!("/api/projects/{}/group/read", e.project_id),
            &bot_auth,
            "{}",
        )
        .await;
        assert_user_only(&response, if is_agm { "POST /api/projects/{id}/group/read (AGM)" } else { "POST /api/projects/{id}/group/read" });
        assert_no_browser_state_events(&mut events);
    }
    for (key, expected) in [(own_draft_key.as_str(), "unsent-private-draft"), (private_draft_key, "unsent-shell-command")] {
        let text: String = sqlx::query_scalar("SELECT text FROM composer_drafts WHERE key=?")
            .bind(key)
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(text, expected, "denied Bot draft writes must not change the row");
    }
    let bot_mark: Option<String> = sqlx::query_scalar("SELECT read_at FROM bot_reads WHERE bot_id=?")
        .bind(&caller.id)
        .fetch_optional(&e.app.db)
        .await
        .unwrap();
    let group_mark: Option<String> = sqlx::query_scalar("SELECT read_at FROM project_group_reads WHERE project_id=?")
        .bind(&e.project_id)
        .fetch_optional(&e.app.db)
        .await
        .unwrap();
    assert_eq!(bot_mark, None, "denied Bot request must not advance the bot read mark");
    assert_eq!(group_mark, None, "denied Bot request must not advance the group read mark");
    assert_eq!(crate::read_marks::unread_counts(&e.app.db).await.unwrap().get(&caller.id), Some(&2), "denied Bot request leaves the bot's unread badge unchanged");
    assert_eq!(crate::read_marks::group_unread_counts(&e.app.db).await.unwrap().get(&e.project_id), Some(&1), "denied Bot request leaves the project's group unread badge unchanged");
    assert_no_browser_state_events(&mut events);

    let user_list = get(e.app.clone(), "/api/drafts", &user_auth).await;
    assert!(user_list.starts_with("HTTP/1.1 200") && body(&user_list).contains("unsent-private-draft"), "User retains shared draft reads: {user_list}");
    let user_write = send(
        e.app.clone(),
        "PUT",
        &format!("/api/drafts/bot%3A{}", caller.id),
        &user_auth,
        r#"{"text":"user-updated-draft","client_id":"browser"}"#,
    )
    .await;
    assert!(user_write.starts_with("HTTP/1.1 200"), "User retains draft writes: {user_write}");
    for path in [format!("/api/bots/{}/read", caller.id), format!("/api/projects/{}/group/read", e.project_id)] {
        let marked = send(e.app.clone(), "POST", &path, &user_auth, "{}").await;
        assert!(marked.starts_with("HTTP/1.1 200"), "User retains read-mark updates at {path}: {marked}");
    }
    assert_eq!(crate::read_marks::unread_counts(&e.app.db).await.unwrap().get(&caller.id), None, "User read marks clear the bot unread badge as before");
    assert_eq!(crate::read_marks::group_unread_counts(&e.app.db).await.unwrap().get(&e.project_id), None, "User read marks clear the group unread badge as before");
}

#[tokio::test]
async fn memory_and_pane_preview_reads_are_user_only_for_plain_and_agm_bots() {
    let e = crate::testing::env().await;
    let caller = distinct_bot(&e, &e.project_id, "memory-read-caller").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    let pane_id = "unrelated-private-pane";
    e.herdr.screens.lock().unwrap().insert(pane_id.into(), "private terminal sentinel".into());
    let bot_auth = bot_headers(&caller);
    for is_agm in [false, true] {
        if is_agm {
            crate::supervisor::store::set_env(&e.app.db, &caller.id, &e.project_id, "/tmp").await.unwrap();
        }
        for path in [
            "/api/mem".to_string(),
            "/api/mem/processes?host=unknown-host".to_string(),
            format!("/api/mem/processes/pane?host=local&pane_id={pane_id}"),
        ] {
            let before = e.herdr.calls.lock().unwrap().len();
            let response = get(e.app.clone(), &path, &bot_auth).await;
            assert_user_only(&response, &path);
            assert_eq!(e.herdr.calls.lock().unwrap().len(), before, "denied Bot memory read must not issue a Herdr RPC at {path}");
        }
    }

    let user_auth = format!("X-AM-Token: {}\r\n", e.app.ui_token);
    let preview = get(e.app.clone(), &format!("/api/mem/processes/pane?host=local&pane_id={pane_id}"), &user_auth).await;
    assert!(preview.starts_with("HTTP/1.1 200") && body(&preview).contains("private terminal sentinel"), "User retains pane preview: {preview}");
    assert!(e.herdr.calls.lock().unwrap().iter().any(|(method, params)| method == "pane.read" && params["pane_id"] == pane_id), "User pane preview still invokes the configured Herdr session");
}

#[tokio::test]
async fn bot_project_timeline_and_bot_tree_reads_do_not_cross_to_siblings_or_ancestors() {
    let e = crate::testing::env().await;
    let parent = distinct_bot(&e, &e.project_id, "tree-parent").await;
    let child = distinct_bot(&e, &e.project_id, "tree-child").await;
    let grandchild = distinct_bot(&e, &e.project_id, "tree-grandchild").await;
    let sibling = distinct_bot(&e, &e.project_id, "tree-sibling").await;
    sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?")
        .bind(&parent.id)
        .bind(&child.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?")
        .bind(&child.id)
        .bind(&grandchild.id)
        .execute(&e.app.db)
        .await
        .unwrap();

    let mut markers = Vec::new();
    for (bot, marker) in [
        (&parent, "TREE_PARENT_MESSAGE_SECRET"),
        (&child, "TREE_CHILD_MESSAGE_SECRET"),
        (&grandchild, "TREE_GRANDCHILD_MESSAGE_SECRET"),
        (&sibling, "TREE_SIBLING_MESSAGE_SECRET"),
    ] {
        let conversation = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?, 'assistant', ?, 'terminal_fallback', ?)")
            .bind(db::ulid())
            .bind(conversation)
            .bind(marker)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        markers.push(marker);
    }

    let parent_timeline = get(
        e.app.clone(),
        &format!("/api/projects/{}/messages", e.project_id),
        &bot_headers(&parent),
    )
    .await;
    assert!(parent_timeline.starts_with("HTTP/1.1 200"), "parent retains its scoped group timeline: {parent_timeline}");
    for marker in [markers[0], markers[1], markers[2]] {
        assert!(parent_timeline.contains(marker), "parent should read its tree's group messages ({marker}): {parent_timeline}");
    }
    assert!(!parent_timeline.contains(markers[3]), "project timeline leaked a sibling bot message: {parent_timeline}");

    // A sibling's newest row must not consume the Bot's page limit or become a usable cursor.
    let limited = get(
        e.app.clone(),
        &format!("/api/projects/{}/messages?limit=1", e.project_id),
        &bot_headers(&parent),
    )
    .await;
    assert!(limited.starts_with("HTTP/1.1 200") && limited.contains(markers[2]), "scope filtering happens before the page limit: {limited}");
    assert!(!limited.contains(markers[3]), "the limited page leaked a sibling: {limited}");
    let sibling_message_id: String = sqlx::query_scalar(
        "SELECT m.id FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE c.bot_id=? AND m.content=?",
    )
    .bind(&sibling.id)
    .bind(markers[3])
    .fetch_one(&e.app.db)
    .await
    .unwrap();
    let sibling_cursor = get(
        e.app.clone(),
        &format!("/api/projects/{}/messages?before={sibling_message_id}", e.project_id),
        &bot_headers(&parent),
    )
    .await;
    assert!(sibling_cursor.starts_with("HTTP/1.1 404"), "a sibling message cannot be used as a timeline cursor: {sibling_cursor}");
    assert!(!sibling_cursor.contains(markers[3]), "foreign cursor error disclosed its message: {sibling_cursor}");

    let child_state_response = get(e.app.clone(), "/api/state", &bot_headers(&child)).await;
    assert!(child_state_response.starts_with("HTTP/1.1 200"), "child can read its scoped state: {child_state_response}");
    let child_state = body(&child_state_response);
    assert!(child_state.contains(&child.id) && child_state.contains(&grandchild.id), "child sees itself and descendants: {child_state}");
    assert!(!child_state.contains(&parent.id) && !child_state.contains(&sibling.id), "child sees neither ancestor nor sibling: {child_state}");

    for (target, should_read) in [(&child, true), (&grandchild, true), (&parent, false), (&sibling, false)] {
        let response = get(
            e.app.clone(),
            &format!("/api/bots/{}/messages", target.id),
            &bot_headers(&child),
        )
        .await;
        if should_read {
            assert!(response.starts_with("HTTP/1.1 200"), "child should read its own tree resource {}: {response}", target.id);
        } else {
            assert!(response.starts_with("HTTP/1.1 403"), "child must not read ancestor/sibling {}: {response}", target.id);
            assert!(!response.contains("TREE_PARENT_MESSAGE_SECRET") && !response.contains("TREE_SIBLING_MESSAGE_SECRET"), "denial must not disclose message contents: {response}");
        }
    }

    let user_timeline = get(
        e.app.clone(),
        &format!("/api/projects/{}/messages", e.project_id),
        &format!("X-AM-Token: {}\r\n", e.app.ui_token),
    )
    .await;
    assert!(user_timeline.starts_with("HTTP/1.1 200"), "UI timeline remains available: {user_timeline}");
    assert!(user_timeline.contains(markers[3]), "UI keeps the full project timeline: {user_timeline}");
}

#[tokio::test]
async fn global_memory_and_arbitrary_pane_previews_are_user_only() {
    let e = crate::testing::env().await;
    let caller = distinct_bot(&e, &e.project_id, "memory-read-caller").await;
    let pane_preview = "/api/mem/processes/pane?host=local&pane_id=known-victim-pane";
    let project_panes = format!("/api/projects/{}/panes", e.project_id);
    let now = db::now();
    sqlx::query(
        "INSERT INTO panes (pane_id, host, workspace_id, tab_id, cwd, kind, project_id, purpose, foreground, last_output_at, first_seen, last_seen, owned_by, label)
         VALUES ('w1:pPrivate','local','w1','t1','/private/PANE_CWD_SECRET','shell',?,'private-purpose','zsh',?,?,?,'user','PANE_LABEL_SECRET')",
    )
    .bind(&e.project_id)
    .bind(&now)
    .bind(&now)
    .bind(&now)
    .execute(&e.app.db)
    .await
    .unwrap();
    let mut unexpected = Vec::new();
    for path in ["/api/mem", pane_preview, project_panes.as_str()] {
        let denied = get(e.app.clone(), path, &bot_headers(&caller)).await;
        if !denied.starts_with("HTTP/1.1 403") || !body(&denied).contains("\"reason\":\"user_only\"") {
            unexpected.push(format!("{path}: {denied}"));
        }
    }

    let user_auth = format!("X-AM-Token: {}\r\n", e.app.ui_token);
    let memory = get(e.app.clone(), "/api/mem", &user_auth).await;
    assert!(memory.starts_with("HTTP/1.1 200"), "UI retains machine-wide memory data: {memory}");
    let pane = get(e.app.clone(), pane_preview, &user_auth).await;
    assert!(!body(&pane).contains("\"reason\":\"user_only\""), "UI pane preview path must remain accessible: {pane}");
    let project_panes_for_user = get(e.app.clone(), &project_panes, &user_auth).await;
    assert!(project_panes_for_user.starts_with("HTTP/1.1 200") && project_panes_for_user.contains("PANE_LABEL_SECRET"), "UI retains the project pane inventory: {project_panes_for_user}");
    assert!(unexpected.is_empty(), "bot diagnostics leaked through unguarded read paths: {unexpected:#?}");
}
