use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// `testing::claude_bot` gives every bot the same `hook_token` ('tok'); identity tests need
/// each bot's token to be its own, or "someone else's token" is indistinguishable from "mine".
async fn distinct_bot(e: &crate::testing::Env, name: &str) -> db::Bot {
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind(format!("tok-{}", bot.id)).bind(&bot.id).execute(&e.app.db).await.unwrap();
    db::bot(&e.app.db, &bot.id).await.unwrap().unwrap()
}

async fn spawn_ready_bot(e: &crate::testing::Env, name: &str) -> db::Bot {
    let bot = distinct_bot(e, name).await;
    crate::testing::fake_run(&e.app, &bot.id).await;
    bot
}

async fn raw(app: Arc<App>, request: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    // 超過 body 上限的請求：伺服器在讀完之前就回 413 並關線，寫入端可能撞 BrokenPipe／ECONNRESET。
    // 回應其實已經在收到的位元組裡（實測：寫入失敗與讀取 RST 都照樣收得到 413），所以這裡不把連線錯誤當成測試失敗，照收到的內容回。
    let wrote = c.write_all(request.as_bytes()).await;
    let mut bytes = Vec::new();
    let read = c.read_to_end(&mut bytes).await;
    note_raw_io(&wrote, &read, bytes.len());
    server.abort();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 連線錯誤照樣不算失敗，只留證據：libtest 只在測試紅時才顯示這行（#1270：紅的那次沒留下寫入／讀取的錯誤種類）。
/// 放在獨立的 `#[inline(never)]` 函式裡：格式化的暫存不進 `raw` 的 future，不然 debug 建置的 stack 會撐爆（per_principal 整批 SIGABRT）。
#[inline(never)]
fn note_raw_io(wrote: &std::io::Result<()>, read: &std::io::Result<usize>, n: usize) {
    if wrote.is_err() || read.is_err() {
        eprintln!("raw(): write={wrote:?} read={read:?} response_bytes={n}");
    }
}

async fn raw_many(app: Arc<App>, requests: Vec<String>) -> Vec<String> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut responses = Vec::with_capacity(requests.len());
    for request in requests {
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(request.as_bytes()).await.unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).await.unwrap();
        responses.push(out);
    }
    server.abort();
    responses
}

async fn raw_head(app: Arc<App>, request: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(request.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let mut chunk = [0; 2048];
    while !out.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = c.read(&mut chunk).await.unwrap();
        if n == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..n]);
    }
    server.abort();
    String::from_utf8_lossy(&out).into_owned()
}

async fn raw_at(addr: std::net::SocketAddr, request: &str) -> String {
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(request.as_bytes()).await.unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    out
}

#[tokio::test]
async fn registered_agm_roles_are_still_denied_on_browser_state_routes() {
    let e = crate::testing::env().await;
    let patrol = distinct_bot(&e, "strict-user-only-patrol").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &patrol.id, &e.project_id, "/tmp").await.unwrap();

    let rpc_calls_before = e.herdr.methods();
    let headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", patrol.id, patrol.hook_token);
    let requests = [
        ("GET", "/api/bots/deleted", ""),
        ("HEAD", "/api/bots/deleted", ""),
        ("GET", "/api/intents", ""),
        ("GET", "/api/mem", ""),
        ("HEAD", "/api/mem", ""),
        ("GET", "/api/drafts", ""),
        ("HEAD", "/api/drafts", ""),
        ("PUT", "/api/drafts/bot:victim", r#"{"text":"overwrite","client_id":"test"}"#),
        ("POST", &format!("/api/bots/{}/read", patrol.id), "{}"),
        (
            "POST",
            &format!("/api/bots/{}/suggestion/accept", patrol.id),
            r#"{"suggestion":"test","expect_run_id":"r1","client_request_id":"c1"}"#,
        ),
        ("GET", "/api/mem/processes/pane?host=local&pane_id=w1-1&lines=20", ""),
        ("HEAD", "/api/mem/processes/pane?host=local&pane_id=w1-1&lines=20", ""),
    ];
    let responses = futures::future::join_all(requests.iter().map(|(method, path, body)| {
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        raw(e.app.clone(), request)
    }))
    .await;

    for ((method, path, _), response) in requests.iter().zip(responses) {
        assert!(
            response.starts_with("HTTP/1.1 403") && (method == &"HEAD" || response.contains("user_only")),
            "registered AGM role must not act as User for {method} {path}: {response}"
        );
    }
    assert_eq!(e.herdr.methods(), rpc_calls_before, "strict auth must reject before any host/session/pane RPC");
    assert!(e.herdr.calls_to("pane.read").is_empty(), "central auth policy must reject before pane_read");
    assert!(e.herdr.calls_to("pane.size").is_empty(), "central auth policy must reject before pane_size");
}

#[test]
fn route_policy_matches_percent_decoded_static_segments() {
    let uri = |path: &str| path.parse::<axum::http::Uri>().unwrap();
    assert_eq!(bot_route_policy("POST", &uri("/api/projects/p1/%67it/push")), Some(BotRoutePolicy::UserOrAgm));
    assert_eq!(bot_route_policy("GET", &uri("/api/draft%73")), Some(BotRoutePolicy::UserOnly));
    assert_eq!(bot_route_policy("HEAD", &uri("/api/drafts")), Some(BotRoutePolicy::UserOnly));
    assert!(!route_path_matches("/api/projects/{id}/git/push", "/api/projects/p1/%ZZit/push"));
}

#[test]
fn ui_only_routes_have_one_strict_central_bot_policy() {
    let mut registered = std::collections::BTreeSet::new();
    for (method, path, _) in BOT_ROUTE_POLICIES {
        assert!(registered.insert((*method, *path)), "duplicate Bot policy entry: {method} {path}");
    }
    for (method, path) in [
        ("GET", "/api/panes"),
        ("POST", "/api/panes/w1:p1/adopt"),
        ("GET", "/api/projects/p1/panes"),
        ("GET", "/api/hosts/local/shells"),
        ("GET", "/api/hosts/local/shells/w1:p1/terminal"),
        ("POST", "/api/hosts/local/shells"),
        ("DELETE", "/api/hosts/local/shells/w1:p1"),
        ("POST", "/api/hosts/local/shells/w1:p1/text"),
        ("POST", "/api/hosts/local/shells/w1:p1/keys"),
        ("POST", "/api/bots/b1/keys"),
        ("POST", "/api/bots/b1/text"),
        ("POST", "/api/bots/b1/attachments"),
        ("POST", "/api/bots/b1/suggestion/accept"),
        ("GET", "/api/attachments/a1"),
        ("GET", "/api/bots/b1/local-image"),
        ("GET", "/api/bots/b1/outbox"),
        ("GET", "/api/bots/b1/outbox/file"),
        ("GET", "/api/mem/processes"),
        ("GET", "/api/intents"),
        ("GET", "/api/build-slots"),
    ] {
        let uri = path.parse::<axum::http::Uri>().unwrap();
        assert_eq!(bot_route_policy(method, &uri), Some(BotRoutePolicy::UserOnly), "{method} {path} must be covered once by the central strict Bot policy");
    }
}

#[tokio::test]
async fn patrol_and_responder_keep_their_scoped_agm_cli_management_routes() {
    let e = crate::testing::env().await;
    let patrol = distinct_bot(&e, "user-only-patrol-caller").await;
    let responder = distinct_bot(&e, "user-only-responder-caller").await;
    let patrol_target = distinct_bot(&e, "user-only-patrol-target").await;
    let responder_target = distinct_bot(&e, "user-only-responder-target").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &patrol.id, &e.project_id, "/tmp").await.unwrap();
    crate::supervisor::roles::set_env(
        &e.app.db,
        crate::supervisor::roles::Role::Responder,
        &responder.id,
        &e.project_id,
        "/tmp",
    )
    .await
    .unwrap();
    for (parent, child) in [(&patrol, &patrol_target), (&responder, &responder_target)] {
        sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?")
            .bind(&parent.id)
            .bind(&child.id)
            .execute(&e.app.db)
            .await
            .unwrap();
    }
    let configured_bots = [&patrol, &responder, &patrol_target, &responder_target]
        .into_iter()
        .map(|bot| crate::config::BotCfg { role: None, manager_use: true, track: None, runtime_override: None, fork_from: None,
            id: Some(bot.id.clone()),
            name: bot.name.clone(),
            kind: bot.kind.clone(),
            model: bot.model.clone(),
            effort: bot.effort.clone(),
            fast: bot.fast != 0,
            persona: bot.persona.clone(),
            args: bot.args(),
            autostart: bot.autostart != 0,
            inject_hooks: bot.inject_hooks != 0,
            auto_approve: bot.auto_approve != 0,
            identity: bot.identity.clone(),
            env: bot.env(),
            herdr_session: bot.herdr_session.clone(),
            create_request_id: None,
            create_fingerprint: None,
            })
        .collect();
    let (project_id, repo) = (e.project_id.clone(), e.repo.to_string_lossy().to_string());
    e.app
        .cfg
        .update(move |cfg| {
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true,
                handed_off_to: None,
                id: Some(project_id),
                path: repo,
                label: "proj".into(),
                host: LOCAL_HOST.into(),
                bots: configured_bots,
            });
            Ok(())
        })
        .await
        .unwrap();

    for (role, bot, target) in [("patrol", &patrol, &patrol_target), ("responder", &responder, &responder_target)] {
        let headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot.id, bot.hook_token);
        let request = |method: &str, path: &str, body: &str| {
            format!(
                "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            )
        };
        let created_body = json!({"name": format!("agm-{role}-created"), "kind": "claude"}).to_string();
        let created = raw(e.app.clone(), request("POST", &format!("/api/projects/{}/bots", e.project_id), &created_body)).await;
        assert!(created.starts_with("HTTP/1.1 200"), "{role} agm bot create remains available in its project: {created}");

        // Use the role bot itself here: its invalid resume option reaches the handler, while
        // the separate child target below exercises the existing child-stop permission.
        let started = raw(e.app.clone(), request("POST", &format!("/api/bots/{}/start?resume=invalid", bot.id), "{}")).await;
        assert!(started.starts_with("HTTP/1.1 400") && !started.contains("user_only"), "{role} agm bot start reaches the existing validation for its child: {started}");
        let stopped = raw(e.app.clone(), request("POST", &format!("/api/bots/{}/stop", target.id), "{}")).await;
        assert!(stopped.starts_with("HTTP/1.1 204"), "{role} agm bot stop remains available for its child: {stopped}");

        let quota = raw(e.app.clone(), request("POST", "/api/quota/probe?kind=codex", "{}")).await;
        assert!(quota.starts_with("HTTP/1.1 400") && !quota.contains("user_only"), "{role} agm quota --probe reaches its existing validation: {quota}");

        let shell = raw(e.app.clone(), request("POST", "/api/hosts/local/shells", "{}")).await;
        assert!(shell.starts_with("HTTP/1.1 403") && shell.contains("user_only"), "AGM role does not bypass the handler's explicit shell restriction: {shell}");
    }

    // Changing the DB role mapping immediately removes the shared-fence exception.
    let replacement = distinct_bot(&e, "user-only-patrol-replacement").await;
    crate::supervisor::store::set_env(&e.app.db, &replacement.id, &e.project_id, "/tmp").await.unwrap();
    let body = json!({"name": "stale-patrol", "kind": "claude"}).to_string();
    let stale = raw(
        e.app.clone(),
        format!(
            "POST /api/projects/{}/bots HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            e.project_id, patrol.id, patrol.hook_token, body.len()
        ),
    )
    .await;
    assert!(stale.starts_with("HTTP/1.1 403") && stale.contains("user_only"), "a bot that lost the patrol role must lose the exception: {stale}");
}

async fn spawn_begin(app: Arc<App>, bot_id: &str, token: &str) -> String {
    let body = format!("bot_id={bot_id}");
    raw(
        app,
        format!(
            "POST /relay/spawn/begin HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

async fn spawn_finish(app: Arc<App>, bot_id: &str, token: &str, permit_id: &str, pane_id: &str) -> String {
    let body = format!("bot_id={bot_id}&permit_id={permit_id}&pane_id={pane_id}&purpose=child");
    raw(
        app,
        format!(
            "POST /relay/spawn/finish HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

fn response_json(response: &str) -> Value {
    serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap_or("{}")).unwrap()
}

/// Every routed path whose identifier resolves to a bot, project, run/turn,
/// attachment, assignment, or bot-owned pane is exercised here. A valid Bot A credential
/// must not cross into Bot B's project or bot-owned data; the prompt route is the one
/// explicit cross-bot exception because relay authentication authorizes its recipient.
#[tokio::test]
async fn bot_path_resources_are_scoped_to_the_authenticated_bot() {
    let e = crate::testing::env().await;
    let bot_a = distinct_bot(&e, "scope-a").await;
    let project_b = db::ulid();
    let project_b_path = e.dir.join("project-b");
    std::fs::create_dir_all(&project_b_path).unwrap();
    std::fs::write(project_b_path.join("x.png"), b"fixture image").unwrap();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,'local',?)")
        .bind(&project_b)
        .bind(project_b_path.to_string_lossy().to_string())
        .bind("project-b")
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let bot_b = crate::testing::claude_bot(&e.app, &project_b, "scope-b").await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?")
        .bind(format!("tok-{}", bot_b.id))
        .bind(&bot_b.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    let bot_b = db::bot(&e.app.db, &bot_b.id).await.unwrap().unwrap();
    let now = db::now();
    let run_b = crate::testing::fake_run(&e.app, &bot_b.id).await;
    let turn_b = db::ulid();
    let conversation_b = db::conversation_id(&e.app.db, &bot_b.id).await.unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'assistant','B-private-scope-fixture','terminal_fallback',?)")
        .bind(db::ulid())
        .bind(&conversation_b)
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','queued','pending',?)")
        .bind(&turn_b)
        .bind(&conversation_b)
        .bind(&run_b)
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    let attachment_b = db::ulid();
    let attachment_path = project_b_path.join(".agents-manager").join("attachments").join("x.txt");
    std::fs::create_dir_all(attachment_path.parent().unwrap()).unwrap();
    std::fs::write(&attachment_path, b"B attachment").unwrap();
    sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, state, created_at) VALUES (?,?,'x.txt','text/plain',1,?,'/tmp/x','local','ready',?)")
        .bind(&attachment_b)
        .bind(&bot_b.id)
        .bind(attachment_path.to_string_lossy().to_string())
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    let outbox_b = crate::outbox::ensure(&e.app.data_dir, &bot_b.id).unwrap();
    std::fs::write(outbox_b.join("x.txt"), b"B outbox fixture").unwrap();
    let assignment_b = db::ulid();
    sqlx::query("INSERT INTO supervisor_assignments (id, supervisor_id, target_bot_id, client_request_id, text, created_at, updated_at) VALUES (?,'main',?,?, 'scope fixture',?,?)")
        .bind(&assignment_b)
        .bind(&bot_b.id)
        .bind(format!("scope-{}", assignment_b))
        .bind(&now)
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    let pane_b = format!("scope-pane-{}", &bot_b.id[..8]);
    sqlx::query("INSERT INTO panes (pane_id, host, kind, owner_bot_id, project_id, last_output_at, first_seen, last_seen) VALUES (?,'local','shell',?,?,?, ?,?)")
        .bind(&pane_b)
        .bind(&bot_b.id)
        .bind(&project_b)
        .bind(&now)
        .bind(&now)
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();
    let draft_key_b = format!("bot:{}", bot_b.id);
    crate::drafts::put(&e.app.db, &draft_key_b, "B-private-draft").await.unwrap();

    // Prove an actual disclosure before running the full route matrix. This stays first so
    // an unguarded destructive handler cannot make later cases appear protected by 404.
    let bot_headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot_a.id, bot_a.hook_token);
    let leaked = raw(
        e.app.clone(),
        format!(
            "GET /api/bots/{}/messages HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n",
            bot_b.id
        ),
    )
    .await;
    assert!(leaked.starts_with("HTTP/1.1 403") || leaked.starts_with("HTTP/1.1 404"), "Bot A read Bot B's messages: {leaked}");
    let encoded_bot = format!("%{:02X}{}", bot_b.id.as_bytes()[0], &bot_b.id[1..]);
    let encoded_leak = raw(
        e.app.clone(),
        format!(
            "GET /api/bots/{encoded_bot}/messages HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n"
        ),
    )
    .await;
    assert!(encoded_leak.starts_with("HTTP/1.1 403") || encoded_leak.starts_with("HTTP/1.1 404"), "percent-encoded Bot B id bypassed scope: {encoded_leak}");
    let drafts_leak = raw(
        e.app.clone(),
        format!("GET /api/drafts HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n"),
    )
    .await;
    assert!(drafts_leak.starts_with("HTTP/1.1 403") || drafts_leak.starts_with("HTTP/1.1 404"), "Bot A read the shared drafts list: {drafts_leak}");

    // This is also the review table: all handlers registered against an identifier that
    // resolves to another bot/project are listed, including mixed-method route families.
    let paths = [
        ("GET", "/api/drafts"), ("PUT", "/api/drafts/bot%3A{bot}"),
        ("PATCH", "/api/projects/{project}"), ("DELETE", "/api/projects/{project}"),
        ("POST", "/api/projects/{project}/bots"),
        ("GET", "/api/projects/{project}/panes"),
        ("POST", "/api/projects/{project}/github/refresh"), ("GET", "/api/projects/{project}/submodules"),
        ("GET", "/api/projects/{project}/git"), ("POST", "/api/projects/{project}/git/commit"),
        ("POST", "/api/projects/{project}/git/push"), ("POST", "/api/projects/{project}/git/pull"),
        ("GET", "/api/projects/{project}/issues"), ("GET", "/api/projects/{project}/issues/1"),
        ("PATCH", "/api/bots/{bot}"), ("DELETE", "/api/bots/{bot}"),
        ("POST", "/api/bots/{bot}/start"), ("POST", "/api/bots/{bot}/restart"),
        ("POST", "/api/bots/{bot}/credential/rotate"), ("POST", "/api/bots/{bot}/fork"),
        ("POST", "/api/bots/{bot}/promote"), ("POST", "/api/bots/{bot}/stop"),
        ("POST", "/api/bots/{bot}/rewind"), ("GET", "/api/bots/{bot}/preview"),
        ("POST", "/api/bots/{bot}/preview"), ("DELETE", "/api/bots/{bot}/preview"),
        ("POST", "/api/bots/{bot}/interrupt"), ("POST", "/api/bots/{bot}/login"),
        ("POST", "/api/bots/{bot}/pane/move-to-tab"), ("POST", "/api/bots/{bot}/attachments"),
        ("POST", "/api/bots/{bot}/keys"), ("POST", "/api/bots/{bot}/text"),
        ("GET", "/api/bots/{bot}/messages"), ("GET", "/api/bots/{bot}/terminal"),
        ("GET", "/api/bots/{bot}/pending-question"), ("GET", "/api/bots/{bot}/local-image?path=x.png"),
        ("GET", "/api/bots/{bot}/outbox"), ("GET", "/api/bots/{bot}/outbox/file?path=x.txt"),
        ("GET", "/api/bots/{bot}/scratchpad"), ("GET", "/api/bots/{bot}/scratchpad/file?path=x.txt"),
        ("POST", "/api/bots/{bot}/read"), ("POST", "/api/bots/{bot}/abort"),
        ("POST", "/api/bots/{bot}/restore"), ("GET", "/api/attachments/{attachment}"),
        ("POST", "/api/turns/{turn}/abandon"), ("POST", "/api/turns/{turn}/withdraw"),
        ("POST", "/api/turns/{encoded_turn}/withdraw"),
        ("GET", "/api/supervisor/assignments/{assignment}"),
        ("GET", "/api/supervisor/assignments/{encoded_assignment}"),
        ("POST", "/api/supervisor/assignments/{assignment}/review"),
        ("POST", "/api/panes/{pane}/adopt"), ("POST", "/api/panes/{pane}/close"),
        ("POST", "/api/panes/{pane}/focus"),
        ("POST", "/api/services/daemon-swap/probe/{bot}"), ("POST", "/api/services/herdr-upgrade/resume/{bot}"),
        ("GET", "/api/attachments/{encoded_attachment}"), ("GET", "/api/projects/{encoded_project}/git"),
    ];
    let encoded_project = format!("%{:02X}{}", project_b.as_bytes()[0], &project_b[1..]);
    let encoded_turn = format!("%{:02X}{}", turn_b.as_bytes()[0], &turn_b[1..]);
    let encoded_assignment = format!("%{:02X}{}", assignment_b.as_bytes()[0], &assignment_b[1..]);
    let encoded_attachment = format!("%{:02X}{}", attachment_b.as_bytes()[0], &attachment_b[1..]);
    let requests = paths.into_iter().map(|(method, template)| {
        let path = template
            .replace("{encoded_project}", &encoded_project)
            .replace("{encoded_turn}", &encoded_turn)
            .replace("{encoded_assignment}", &encoded_assignment)
            .replace("{encoded_attachment}", &encoded_attachment)
            .replace("{bot}", &bot_b.id)
            .replace("{project}", &project_b)
            .replace("{attachment}", &attachment_b)
            .replace("{turn}", &turn_b)
            .replace("{assignment}", &assignment_b)
            .replace("{pane}", &pane_b);
        let body = if matches!(method, "GET" | "DELETE") {
            String::new()
        } else if path.starts_with("/api/drafts/") {
            r#"{"text":"cross-bot overwrite","client_id":"scope"}"#.into()
        } else {
            "{}".into()
        };
        (method, format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
    }).collect::<Vec<_>>();
    let responses = raw_many(e.app.clone(), requests.iter().map(|(_, request)| request.clone()).collect()).await;
    assert_eq!(responses.len(), requests.len());
    for ((method, request), response) in requests.iter().zip(&responses) {
        assert!(response.starts_with("HTTP/1.1 403") || response.starts_with("HTTP/1.1 404"), "Bot A crossed into Bot B resource ({method} {}): {response}", request.lines().next().unwrap_or(""));
    }
    let draft_after: String = sqlx::query_scalar("SELECT text FROM composer_drafts WHERE key=?")
        .bind(&draft_key_b)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(draft_after, "B-private-draft", "cross-bot draft writes must be rejected before mutation");

    // The same resource remains visible to the User principal, and a parent may access a
    // child resource that carries the parent's delegated authority.
    let as_user = raw(
        e.app.clone(),
        format!(
            "GET /api/bots/{}/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\n\r\n",
            bot_b.id, e.app.ui_token
        ),
    )
    .await;
    assert!(as_user.starts_with("HTTP/1.1 200") && as_user.contains("B-private-scope-fixture"), "User behavior stays unchanged: {as_user}");
    let drafts_as_user = raw(
        e.app.clone(),
        format!("GET /api/drafts HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\n\r\n", e.app.ui_token),
    )
    .await;
    assert!(drafts_as_user.starts_with("HTTP/1.1 200") && drafts_as_user.contains("B-private-draft"), "User can still read shared drafts: {drafts_as_user}");
    let child = crate::testing::claude_bot(&e.app, &e.project_id, "scope-child").await;
    sqlx::query("UPDATE bots SET parent_bot_id=?, managed_by='child' WHERE id=?")
        .bind(&bot_a.id)
        .bind(&child.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    let child_read = raw(
        e.app.clone(),
        format!(
            "GET /api/bots/{}/messages HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n",
            child.id
        ),
    )
    .await;
    assert!(child_read.starts_with("HTTP/1.1 200"), "parent may access its child resource: {child_read}");
    let bot_b_headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot_b.id, bot_b.hook_token);
    let own_read = raw(
        e.app.clone(),
        format!("GET /api/bots/{}/messages HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_b_headers}Connection: close\r\n\r\n", bot_b.id),
    )
    .await;
    assert!(own_read.starts_with("HTTP/1.1 200") && own_read.contains("B-private-scope-fixture"), "Bot B can still access its own resource: {own_read}");
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    sqlx::query("UPDATE supervisors SET bot_id=? WHERE id='AGM'")
        .bind(&bot_a.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    let role_read = raw(
        e.app.clone(),
        format!("GET /api/bots/{}/messages HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n", bot_b.id),
    )
    .await;
    assert!(role_read.starts_with("HTTP/1.1 403") || role_read.starts_with("HTTP/1.1 404"), "AGM role token must not inherit generic cross-bot message access: {role_read}");
    let role_project_write = raw(
        e.app.clone(),
        format!("POST /api/projects/{project_b}/bots HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Content-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}"),
    )
    .await;
    assert!(role_project_write.starts_with("HTTP/1.1 403") || role_project_write.starts_with("HTTP/1.1 404"), "AGM role must not inherit generic cross-project bot creation: {role_project_write}");
    let role_assignment = raw(
        e.app.clone(),
        format!("GET /api/supervisor/assignments/{assignment_b} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n"),
    )
    .await;
    assert!(role_assignment.starts_with("HTTP/1.1 200"), "AGM role may read supervisory assignments across bots: {role_assignment}");

    // A different bot may intentionally prompt B; that route performs its own relay proof.
    let relay = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\n{}Content-Type: application/json\r\nConnection: close\r\nContent-Length: 15\r\n\r\n{{\"text\":\"ping\"}}",
            bot_b.id, bot_headers
        ),
    )
    .await;
    assert!(!response_json(&relay).get("reason").is_some_and(|r| r == "bot_resource_scope"), "authorized relay must reach the prompt handler: {relay}");
}

fn encode_path_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// Raw HTTP targets keep encoded slashes and dot segments intact until Axum routes them.
/// Aliases must not reach Bot B's resource even when routing treats an encoded separator
/// as part of a different path parameter and returns Method Not Allowed.
#[tokio::test]
async fn bot_resource_paths_reject_router_normalization_aliases() {
    let e = crate::testing::env().await;
    let bot_a = distinct_bot(&e, "path-norm-a").await;
    let project_b = db::ulid();
    let project_b_path = e.dir.join("path-norm-project-b");
    std::fs::create_dir_all(&project_b_path).unwrap();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,'local',?)")
        .bind(&project_b)
        .bind(project_b_path.to_string_lossy().to_string())
        .bind("path-norm-project-b")
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let bot_b = crate::testing::claude_bot(&e.app, &project_b, "path-norm-b").await;
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?")
        .bind(format!("tok-{}", bot_b.id))
        .bind(&bot_b.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    let bot_b = db::bot(&e.app.db, &bot_b.id).await.unwrap().unwrap();
    let now = db::now();
    let conversation_b = db::conversation_id(&e.app.db, &bot_b.id).await.unwrap();
    let secret = "path-normalization-private-marker";
    sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'assistant',?,'terminal_fallback',?)")
        .bind(db::ulid())
        .bind(&conversation_b)
        .bind(secret)
        .bind(&now)
        .execute(&e.app.db)
        .await
        .unwrap();

    let unicode_project = "project-e\u{301}";
    let unicode_project_path = e.dir.join("path-norm-unicode-project");
    std::fs::create_dir_all(&unicode_project_path).unwrap();
    sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,'local',?)")
        .bind(unicode_project)
        .bind(unicode_project_path.to_string_lossy().to_string())
        .bind("path-norm-unicode-project")
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();
    let unicode_bot = crate::testing::claude_bot(&e.app, unicode_project, "path-norm-unicode").await;
    let unicode_conversation = db::conversation_id(&e.app.db, &unicode_bot.id).await.unwrap();
    sqlx::query("INSERT INTO messages (id, conversation_id, role, content, source, created_at) VALUES (?,?,'assistant','unicode-normalization-private-marker','terminal_fallback',?)")
        .bind(db::ulid())
        .bind(&unicode_conversation)
        .bind(db::now())
        .execute(&e.app.db)
        .await
        .unwrap();

    let (alpha_at, alpha_byte) = bot_b.id.bytes().enumerate().find(|(_, b)| b.is_ascii_alphabetic()).expect("ULID has an alphabetic byte");
    let encoded_upper = format!("{}%{alpha_byte:02X}{}", &bot_b.id[..alpha_at], &bot_b.id[alpha_at + 1..]);
    let encoded_lower = format!("{}%{alpha_byte:02x}{}", &bot_b.id[..alpha_at], &bot_b.id[alpha_at + 1..]);
    let nfc_project = "project-é";
    let nfd_path = encode_path_segment(unicode_project);
    let nfc_path = encode_path_segment(nfc_project);
    let cases = [
        ("canonical", "GET", format!("/api/bots/{}/messages", bot_b.id), true),
        ("encoded id uppercase hex", "GET", format!("/api/bots/{encoded_upper}/messages"), true),
        ("encoded id lowercase hex", "GET", format!("/api/bots/{encoded_lower}/messages"), true),
        ("encoded slash folds route segments", "GET", format!("/api/bots/{}%2Fmessages", bot_b.id), true),
        ("encoded lowercase slash", "GET", format!("/api/bots/{}%2fmessages", bot_b.id), true),
        ("double encoded slash", "GET", format!("/api/bots/{}%252Fmessages", bot_b.id), true),
        ("dot dot segment", "GET", format!("/api/bots/../bots/{}/messages", bot_b.id), true),
        ("encoded dot dot segment", "GET", format!("/api/bots/%2e%2e/bots/{}/messages", bot_b.id), true),
        ("double encoded dot dot", "GET", format!("/api/bots/%252e%252e/bots/{}/messages", bot_b.id), true),
        ("trailing slash", "GET", format!("/api/bots/{}/messages/", bot_b.id), true),
        ("duplicate slash", "GET", format!("/api//bots//{}//messages", bot_b.id), true),
        ("case changed static route", "GET", format!("/api/BOTS/{}/messages", bot_b.id), true),
        ("encoded static route", "GET", format!("/api/%62ots/{}/messages", bot_b.id), true),
        ("query id cannot override path", "GET", format!("/api/bots/{}/messages?id={}&bot_id={}", bot_b.id, bot_a.id, bot_a.id), true),
        ("dot traversal across API prefix", "GET", format!("/foo/../api/bots/{}/messages", bot_b.id), false),
        ("encoded dot traversal across API prefix", "GET", format!("/foo/%2e%2e/api/bots/{}/messages", bot_b.id), false),
        ("dot traversal inside API prefix", "GET", format!("/api/../api/bots/{}/messages", bot_b.id), true),
        ("encoded dot traversal inside API prefix", "GET", format!("/api/%2e%2e/api/bots/{}/messages", bot_b.id), true),
        ("uppercase API prefix", "GET", format!("/API/bots/{}/messages", bot_b.id), false),
        ("encoded API prefix", "GET", format!("/%61pi/bots/{}/messages", bot_b.id), false),
        ("HEAD uses bot scope", "HEAD", format!("/api/bots/{}/messages", bot_b.id), true),
        ("OPTIONS uses bot scope", "OPTIONS", format!("/api/bots/{}/messages", bot_b.id), true),
    ];
    let bot_headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot_a.id, bot_a.hook_token);
    let requests = cases
        .iter()
        .map(|(_, method, path, _)| format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\nContent-Length: 0\r\n\r\n"))
        .collect::<Vec<_>>();
    let responses = raw_many(e.app.clone(), requests).await;
    for ((name, method, path, in_api), response) in cases.iter().zip(&responses) {
        assert!(!response.contains(secret), "{name} ({method} {path}) disclosed Bot B's message: {response}");
        assert!(!response.contains("unicode-normalization-private-marker"), "{name} ({method} {path}) disclosed the NFD project's message: {response}");
        if *in_api {
            let status = response.split_whitespace().nth(1).unwrap_or("");
            assert!(matches!(status, "403" | "404" | "405"), "{name} ({method} {path}) should be denied, unmatched, or method-mismatched: {response}");
        }
    }

    let patch_body = r#"{"name":"must-not-change"}"#;
    let encoded_separator_patch = raw(
        e.app.clone(),
        format!(
            "PATCH /api/bots/{}%2Fmessages HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{patch_body}",
            bot_b.id,
            patch_body.len()
        ),
    )
    .await;
    assert!(
        !encoded_separator_patch.starts_with("HTTP/1.1 2"),
        "encoded separator must not patch Bot B: {encoded_separator_patch}"
    );
    let bot_b_after: String = sqlx::query_scalar("SELECT name FROM bots WHERE id=?")
        .bind(&bot_b.id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(bot_b_after, bot_b.name, "encoded separator must not mutate Bot B");

    let own_with_query_override = raw(
        e.app.clone(),
        format!("GET /api/bots/{}/messages?bot_id={} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n", bot_a.id, bot_b.id),
    )
    .await;
    assert!(own_with_query_override.starts_with("HTTP/1.1 200"), "query parameters must not block the actual path owner: {own_with_query_override}");
    assert!(!own_with_query_override.contains(secret), "query bot_id must not replace the path id: {own_with_query_override}");
}

/// `/ws` is a separate upgrade route outside the `/api` auth layer. It must not ignore a
/// Bot/Service principal when a query string also supplies the User token.
#[tokio::test]
async fn websocket_upgrade_rejects_mixed_bot_and_user_credentials() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "ws-path-principal").await;
    let upgrade = |path: &str, token: &str, bot_headers: &str| {
        format!(
            "GET {path}?token={token} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
    };
    let bot_headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot.id, bot.hook_token);
    let service_headers = "X-AM-Service-Id: unknown\r\nX-AM-Service-Token: ignored\r\n";
    let bot_token_alone = raw_head(e.app.clone(), upgrade("/ws", &bot.hook_token, "")).await;
    assert!(bot_token_alone.starts_with("HTTP/1.1 401"), "a Bot token alone cannot subscribe to User WebSocket events: {bot_token_alone}");
    for (identity, headers) in [
        ("Bot", bot_headers.as_str()),
        ("partial Bot", "X-AM-Bot-Token: ignored\r\n"),
        ("Child id", "X-AM-Child-Spawn-Id: ignored\r\n"),
        ("Child token", "X-AM-Child-Token: ignored\r\n"),
        ("Service", service_headers),
    ] {
        let mixed = raw_head(e.app.clone(), upgrade("/ws", &e.app.ui_token, headers)).await;
        assert!(mixed.starts_with("HTTP/1.1 401"), "WebSocket must reject {identity} plus User credentials: {mixed}");
    }

    let aliases = [
        "/ws/",
        "//ws",
        "/w%73",
        "/ws%2f",
        "/ws%252f",
        "/WS",
        "/ws/../ws",
        "/ws/%2e%2e/ws",
        "/foo/../ws",
        "/api/ws",
    ];
    for path in aliases {
        let response = raw_head(e.app.clone(), upgrade(path, &e.app.ui_token, "")).await;
        assert!(!response.starts_with("HTTP/1.1 101"), "User token must not upgrade unmatched WebSocket alias {path}: {response}");
    }
    let user = raw_head(e.app.clone(), upgrade("/ws", &e.app.ui_token, "")).await;
    assert!(user.starts_with("HTTP/1.1 101"), "browser User-token WebSocket remains available: {user}");
}

/// 原始按鍵／文字／開關 shell 是給人用的（網頁的鍵盤同步、shell 面板）：bot 的 hook token 驗得過身份，
/// 但不該拿它直接對任何 pane 打字或按鍵（受 prompt injection 的 bot 可以替自己開一顆 shell 再打指令進去，
/// 或對別顆 bot 按鍵繞過回合那條線）。只有 UI token 的使用者可以。
#[tokio::test]
async fn raw_pane_text_keys_and_shell_endpoints_are_for_the_user_only() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "raw-pane-bot").await;
    let victim = distinct_bot(&e, "raw-pane-victim").await;
    let call = |method: &str, path: &str, body: &str, who: &[(&str, String)]| {
        let extra: String = who.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let as_bot = [("X-AM-Bot-Id", bot.id.clone()), ("X-AM-Bot-Token", bot.hook_token.clone())];
    let as_user = [("X-AM-Token", e.app.ui_token.clone())];
    let victim_text = format!("/api/bots/{}/text", victim.id);
    let victim_keys = format!("/api/bots/{}/keys", victim.id);
    let cases: Vec<(&str, &str, String)> = vec![
        ("POST", "/api/hosts/local/shells", "{}".into()),
        ("POST", "/api/hosts/local/shells/w1:p1/text", r#"{"text":"id"}"#.into()),
        ("POST", "/api/hosts/local/shells/w1:p1/keys", r#"{"keys":["enter"]}"#.into()),
        ("DELETE", "/api/hosts/local/shells/w1:p1", String::new()),
    ];
    for (method, path, body) in cases.iter().map(|(m, p, b)| (*m, p.to_string(), b.clone())).chain([
        ("POST", victim_text.clone(), r#"{"text":"hi"}"#.to_string()),
        ("POST", victim_keys.clone(), r#"{"keys":["ctrl+c"]}"#.to_string()),
    ]) {
        let denied = raw(e.app.clone(), call(method, &path, &body, &as_bot)).await;
        assert!(denied.starts_with("HTTP/1.1 403"), "bot 不能打 {method} {path}：{denied}");
        assert!(denied.contains("user_only"), "{denied}");
        let user = raw(e.app.clone(), call(method, &path, &body, &as_user)).await;
        assert!(!user.starts_with("HTTP/1.1 401") && !user.contains("user_only"), "使用者本人照常（可以是別的錯，但不是被擋）：{method} {path}: {user}");
    }
}

/// Host management runs SSH with the daemon user's credentials, and the host shell GETs can
/// expose a user's terminal. These are UI operations, so a valid bot hook token is not enough.
#[tokio::test]
async fn host_management_and_shell_read_endpoints_are_user_only() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "host-api-bot").await;
    let as_bot = [("X-AM-Bot-Id", bot.id.clone()), ("X-AM-Bot-Token", bot.hook_token.clone())];
    let as_user = [("X-AM-Token", e.app.ui_token.clone())];
    let cases = [
        ("GET", "/api/hosts/local/shells", ""),
        ("GET", "/api/hosts/local/shells/w1:p1/terminal?source=visible&lines=5", ""),
        ("POST", "/api/hosts", r#"{"name":"blocked","ssh":""}"#),
        ("DELETE", "/api/hosts/no-such-host", ""),
        ("POST", "/api/hosts/no-such-host/reconnect", ""),
        ("POST", "/api/hosts/no-such-host/tools/refresh", ""),
        ("POST", "/api/hosts/no-such-host/tools/install", r#"{"kind":"claude","via_bot_id":"none"}"#),
        ("POST", "/api/hosts/no-such-host/tools/install", r#"{"kind":"claude"}"#),
        ("POST", "/api/hosts/no-such-host/identities/cc1/login", ""),
        ("POST", "/api/hosts/no-such-host/identities/cc1/logout", ""),
        ("GET", "/api/hosts/no-such-host/gh", ""),
        ("POST", "/api/hosts/no-such-host/gh/login", "{}"),
        ("POST", "/api/hosts/no-such-host/gh/cancel", ""),
    ];
    let request = |method: &str, path: &str, body: &str, who: &[(&str, String)]| {
        let extra: String = who.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        format!(
            "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    for (method, path, body) in cases {
        let denied = raw(e.app.clone(), request(method, path, body, &as_bot)).await;
        assert!(denied.starts_with("HTTP/1.1 403"), "bot 不能呼叫 {method} {path}: {denied}");
        assert!(denied.contains("user_only"), "{denied}");

        let user = raw(e.app.clone(), request(method, path, body, &as_user)).await;
        assert!(!user.contains("user_only"), "使用者本人應通過 principal gate: {method} {path}: {user}");
    }
}

#[tokio::test]
async fn every_user_only_route_rejects_an_ordinary_bot_before_the_handler() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "user-only-route-caller").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(e.app.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    for (method, pattern, _) in BOT_ROUTE_POLICIES {
        let path = pattern
            .replace("{id}", &bot.id)
            .replace("{name}", "local")
            .replace("{identity}", "cc1")
            .replace("{pane_id}", "w1:p1")
            .replace("{key}", "bot%3Atest");
        let methods = if *method == "GET" { vec![*method, "HEAD"] } else { vec![*method] };
        for request_method in methods {
            let request = format!(
                "{request_method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{{}}",
                bot.id, bot.hook_token
            );
            let response = raw_at(addr, &request).await;
            assert!(
                response.starts_with("HTTP/1.1 403")
                    && (request_method == "HEAD" || response.contains("user_only")),
                "ordinary Bot passed User-only route {request_method} {path}: {response}"
            );
        }
    }
    server.abort();
}

#[tokio::test]
async fn quota_and_model_refresh_require_a_user_or_registered_agm_role() {
    let e = crate::testing::env().await;
    let ordinary = distinct_bot(&e, "refresh-ordinary-bot").await;
    let patrol = distinct_bot(&e, "refresh-patrol").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &patrol.id, &e.project_id, "/tmp")
        .await
        .unwrap();
    let bot_headers = |bot: &db::Bot| {
        format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot.id, bot.hook_token)
    };
    let request = |path: &str, headers: &str| {
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Connection: close\r\n\r\n")
    };

    for path in [
        "/api/quota?refresh=%31&host=missing-host",
        "/api/models?kind=codex&host=missing-host&refresh=1",
        "/api/quota?refresh=true&host=missing-host",
        "/api/quota?refresh=yes&host=missing-host",
        "/api/models?kind=codex&host=missing-host&refresh=true",
        "/api/models?kind=codex&host=missing-host&refresh=yes",
    ] {
        let denied = raw(e.app.clone(), request(path, &bot_headers(&ordinary))).await;
        assert!(
            denied.starts_with("HTTP/1.1 403") && denied.contains("user_only"),
            "ordinary Bot refresh must be rejected before handler side effects: {path}: {denied}"
        );
    }

    let cached = raw(e.app.clone(), request("/api/models?kind=codex&host=missing-host", &bot_headers(&ordinary))).await;
    assert!(cached.starts_with("HTTP/1.1 404"), "ordinary Bot may still read cached models; unknown host reaches validation: {cached}");

    for path in [
        "/api/quota?refresh=1&host=missing-host",
        "/api/models?kind=codex&host=missing-host&refresh=1",
    ] {
        let allowed = raw(e.app.clone(), request(path, &bot_headers(&patrol))).await;
        assert!(allowed.starts_with("HTTP/1.1 404") && !allowed.contains("user_only"), "registered AGM role retains the old refresh path: {path}: {allowed}");
        let user = raw(e.app.clone(), request(path, &format!("X-AM-Token: {}\r\n", e.app.ui_token))).await;
        assert!(user.starts_with("HTTP/1.1 404") && !user.contains("user_only"), "User behavior is unchanged: {path}: {user}");
    }
}

/// 矩陣只打 router 列出來的方法與樣板。這裡補 HEAD／OPTIONS、點路徑、編碼、
/// 重覆的 refresh，以及 service 憑證走別條路徑。2xx／101 才算繞過。
#[tokio::test]
async fn bot_and_service_cannot_bypass_user_only_by_method_path_or_query_shape() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "shape-bypass-bot").await;
    e.app.service_tokens.write().unwrap().insert("daemon-swap".into(), "swap-secret".into());
    e.app.service_tokens.write().unwrap().insert("herdr-upgrade".into(), "herdr-secret".into());
    let bot_headers = format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", bot.id, bot.hook_token);
    let head = |request: String| {
        let app = e.app.clone();
        async move { raw_head(app, request).await }
    };
    let opened = |status: &str| status.starts_with("HTTP/1.1 2") || status.starts_with("HTTP/1.1 101");
    let bot_shapes = [
        "OPTIONS /api/intents HTTP/1.1",
        "HEAD /api/intents HTTP/1.1",
        "GET /api/intents/ HTTP/1.1",
        "GET /api/./intents HTTP/1.1",
        "GET /api/intents/. HTTP/1.1",
        "GET /api/nope/../intents HTTP/1.1",
        "GET /api/%2e%2e/intents HTTP/1.1",
        "GET /api/intents%2f../mem HTTP/1.1",
        "GET /api/%69ntents HTTP/1.1",
        "GET //api/intents HTTP/1.1",
        "GET /api/drafts%3fbot=1 HTTP/1.1",
        "GET /api/models/../intents HTTP/1.1",
        "POST /api/intents HTTP/1.1",
        "GET /api/mem?refresh=1 HTTP/1.1",
        "GET /ws?token=not-the-user-token HTTP/1.1",
    ];
    for start in bot_shapes {
        let response = head(format!("{start}\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\nContent-Length: 0\r\n\r\n")).await;
        let line = response.lines().next().unwrap_or("");
        assert!(!opened(line), "bot shape opened a fenced route: {start} -> {line}");
        assert!(!response.contains(&e.app.ui_token), "bot shape leaked the UI token: {start}");
    }
    for start in [
        "GET /api/models?kind=codex&refresh=0&refresh=1 HTTP/1.1",
        "GET /api/quota?refresh=&refresh=1 HTTP/1.1",
    ] {
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            raw(e.app.clone(), format!("{start}\r\nHost: 127.0.0.1\r\n{bot_headers}Connection: close\r\n\r\n")),
        )
        .await
        .unwrap_or_else(|_| panic!("duplicate refresh hung, likely a live probe: {start}"));
        assert!(response.contains("user_only"), "duplicate refresh must still be user_only: {start} -> {response}");
    }
    let upgrade = format!(
        "GET /ws?token={} HTTP/1.1\r\nHost: 127.0.0.1\r\n{bot_headers}Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        e.app.ui_token
    );
    let mixed_ws = head(upgrade).await;
    assert!(mixed_ws.starts_with("HTTP/1.1 401"), "websocket upgrade with bot headers plus user token: {mixed_ws}");

    let service_shapes = [
        ("daemon-swap", "swap-secret", "HEAD /api/supervisor/health HTTP/1.1"),
        ("daemon-swap", "swap-secret", "OPTIONS /api/supervisor/health HTTP/1.1"),
        ("daemon-swap", "swap-secret", "GET /api/supervisor/health/ HTTP/1.1"),
        ("daemon-swap", "swap-secret", "GET /api/supervisor/./health HTTP/1.1"),
        ("daemon-swap", "swap-secret", "GET /api/supervisor/health/../../state HTTP/1.1"),
        ("daemon-swap", "swap-secret", "GET /api/supervisor%2fhealth HTTP/1.1"),
        ("daemon-swap", "swap-secret", "GET /api/state HTTP/1.1"),
        ("daemon-swap", "swap-secret", "POST /api/deploy/now HTTP/1.1"),
        ("daemon-swap", "swap-secret", "POST /api/services/daemon-swap/probe/abc/../restart-window HTTP/1.1"),
        ("daemon-swap", "swap-secret", "POST /api/supervisor/leases/restart/release/../acquire HTTP/1.1"),
        ("herdr-upgrade", "herdr-secret", "GET /api/panes/../state HTTP/1.1"),
        ("herdr-upgrade", "herdr-secret", "GET /api/capabilities/../drafts HTTP/1.1"),
        ("herdr-upgrade", "herdr-secret", "GET /ws?token=herdr-secret HTTP/1.1"),
    ];
    for (id, token, start) in service_shapes {
        let response = head(format!(
            "{start}\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: {id}\r\nX-AM-Service-Token: {token}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"
        ))
        .await;
        let line = response.lines().next().unwrap_or("");
        assert!(!opened(line), "service shape was not confined: {id} {start} -> {line} {response}");
        assert!(!response.contains(&e.app.ui_token), "service shape leaked the UI token: {start}");
    }
}

/// A Bot cache miss never probes on the request path: the 503 returns at once and the daemon schedules
/// a background refresh (#1890, `runners::models_refresh`). `refresh=1` is fenced in middleware, so
/// omitting it must not bypass the same policy.
#[tokio::test]
async fn a_plain_bot_model_cache_miss_does_not_spawn_the_cli() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "model-cache-miss-bot").await;
    let missing_codex = e.dir.join("does-not-exist-codex").to_string_lossy().to_string();
    e.app.tools.lock().await.insert(
        LOCAL_HOST.to_string(),
        crate::tools::HostTools {
            tools: BTreeMap::from([("codex".into(), crate::tools::ToolInfo {
                installed: true,
                path: Some(missing_codex),
                version: None,
                logged_in: Some(true),
            })]),
            identities: BTreeMap::new(),
            shell_identities: Vec::new(),
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: db::now(),
        },
    );
    let response = raw(
        e.app.clone(),
        format!(
            "GET /api/models?kind=codex HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\n\r\n",
            bot.id, bot.hook_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 503"), "a Bot cache miss must return 503 at once: {response}");
    assert!(response.contains("model_cache_miss"), "the response should explain that the daemon is refreshing: {response}");
    assert!(response.contains("\"refresh\"") && !response.contains("\"state\":\"none\""), "a miss must schedule a background refresh: {response}");
    assert!(e.app.models_cache.lock().await.is_empty(), "the request path must not populate the cache");
    // The background refresh uses the stubbed tool path (a file that does not exist), so it fails at once
    // instead of starting a real codex, and leaves nothing in the cache.
    let failed = crate::testing::eventually!(
        crate::runners::models_refresh::on_cache_miss(&e.app, LOCAL_HOST, "codex", None).await["state"] == "failed"
    );
    assert!(failed, "the background refresh must fail fast against the missing stub binary");
    assert!(e.app.models_cache.lock().await.is_empty(), "a failed background refresh must not populate the cache");

    let cached = json!({"kind":"codex","host":"local","source":"codex-app-server","models":[{"id":"cached-model"}]});
    e.app.models_cache.lock().await.insert(
        "local/codex/".into(),
        (std::time::Instant::now(), cached.clone()),
    );
    let response = raw(
        e.app.clone(),
        format!(
            "GET /api/models?kind=codex HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\n\r\n",
            bot.id, bot.hook_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200") && response.contains("cached-model"), "a Bot may read a fresh cached model list: {response}");

    e.app.models_cache.lock().await.insert(
        "local/codex/".into(),
        (std::time::Instant::now() - crate::models::CACHE_TTL - std::time::Duration::from_secs(1), cached),
    );
    let response = raw(
        e.app.clone(),
        format!(
            "GET /api/models?kind=codex HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\n\r\n",
            bot.id, bot.hook_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 503") && response.contains("model_cache_miss"), "a stale cache must not trigger an implicit probe: {response}");
}

/// Deployment status is the browser's global host/repo and all-Bot activity panel.
#[tokio::test]
async fn a_plain_bot_cannot_read_global_deployment_status() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "deploy-status-bot").await;
    let response = raw(
        e.app.clone(),
        format!(
            "GET /api/deploy/status HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\n\r\n",
            bot.id, bot.hook_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403") && response.contains("user_only"), "a Bot must not read the global deploy panel: {response}");
}

#[tokio::test]
async fn a_bot_state_snapshot_does_not_expose_global_deploy_wait_details() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "deploy-wait-scope-bot").await;
    *e.app.deploy_wait.lock().unwrap() = Some(crate::deploy_wait::Wait {
        id: "private-wait-id".into(),
        owner: "daemon-update-kick".into(),
        approval_id: "private-approval-id".into(),
        commit: "private-deploy-commit-sha".into(),
        since: db::now(),
        last_attempt_at: db::now(),
        blockers: vec![crate::deploy_wait::Blocker {
            bot_id: Some("private-blocker-bot-id".into()),
            name: "private-blocker-bot-name".into(),
            why: "working".into(),
        }],
        escalates_at: None,
        user_escalated: false,
        auto_escalated: false,
        notified_at: Some(db::now()),
        pending_first: None,
        pending_events: Vec::new(),
        dismissed: false,
        phase: crate::deploy_wait::Phase::Waiting,
        rev: 1,
        ended_at: None,
        swap_announced_at: std::collections::BTreeMap::new(),
    });

    let bot_response = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &bot.hook_token)]).await;
    assert!(bot_response.starts_with("HTTP/1.1 200"), "the Bot may still read its scoped state: {bot_response}");
    for marker in ["private-wait-id", "private-deploy-commit-sha", "private-blocker-bot-id", "private-blocker-bot-name"] {
        assert!(!bot_response.contains(marker), "global deployment detail leaked to Bot state: {marker}: {bot_response}");
    }

    let user_response = state(e.app.clone(), &[("X-AM-Token", &e.app.ui_token)]).await;
    assert!(user_response.contains("private-blocker-bot-name"), "the UI user keeps the deployment wait details: {user_response}");
}

/// #810 #811 #812：行程清單、recovery journal、build 佇列是 UI 診斷面。一般 bot 讀得到別顆 bot 的 argv、
/// session／路徑，以及別人的 build holder。使用者仍看完整內容；pane 的 acquire 不在 `/api` 底下，維持原樣。
#[tokio::test]
async fn process_inventory_intents_and_build_slots_are_user_only() {
    let e = crate::testing::env().await;
    let worker = distinct_bot(&e, "scope-worker").await;
    let victim = distinct_bot(&e, "scope-victim").await;
    let marker = "victim-session-SECRET-PATH-/tmp/only-b";
    sqlx::query(
        "INSERT INTO intents (id, kind, subject_id, host, payload_json, status, last_error, created_at, updated_at, expires_at)
             VALUES (?,'promote',?, 'local', ?, 'pending', 'victim-last-error', ?, ?, ?)",
    )
    .bind(db::ulid())
    .bind(&victim.id)
    .bind(format!(r#"{{"session_id":"{marker}","dest":"{marker}"}}"#))
    .bind(db::now())
    .bind(db::now())
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO build_slots (holder, token, bot_id, purpose, host, status, since, last_seen, expires_at)
             VALUES ('victim-holder-UNIQUE', 'tok', ?, 'victim-purpose-UNIQUE', 'local', 'waiting', ?, ?, NULL)",
    )
    .bind(&victim.id)
    .bind(db::now())
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();

    let call = |path: &str, who: &[(&str, String)]| {
        let extra: String = who.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
        format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\n\r\n")
    };
    let as_bot = [("X-AM-Bot-Id", worker.id.clone()), ("X-AM-Bot-Token", worker.hook_token.clone())];
    let as_user = [("X-AM-Token", e.app.ui_token.clone())];
    let status = |r: &str| r.split_whitespace().nth(1).unwrap_or("").to_string();
    // 失敗訊息附回應：先把 token 換掉再截 300 字，不讓憑證進測試輸出。
    let redact = |r: &str| {
        r.replace(&e.app.ui_token, "<ui_token>").replace(&worker.hook_token, "<hook_token>").chars().take(300).collect::<String>()
    };
    for path in [
        "/api/mem/processes?host=local",
        "/api/mem/processes?host=remote-nope",
        "/api/intents",
        "/api/build-slots",
    ] {
        let denied = raw(e.app.clone(), call(path, &as_bot)).await;
        assert_eq!(status(&denied), "403", "bot 不能讀 {path}");
        assert!(denied.contains("\"reason\":\"user_only\""), "{path} 應是 user_only");
        assert!(
            !denied.contains(marker)
                && !denied.contains("victim-holder-UNIQUE")
                && !denied.contains("victim-purpose-UNIQUE")
                && !denied.contains("victim-last-error"),
            "{path} 的 403 不該帶受害者內容"
        );
        let user = raw(e.app.clone(), call(path, &as_user)).await;
        if path.contains("remote-nope") {
            assert_eq!(status(&user), "404", "未知主機仍由 handler 回 404");
        } else {
            assert_eq!(status(&user), "200", "使用者仍讀得到 {path}：{}", redact(&user));
        }
    }
    let user_intents = raw(e.app.clone(), call("/api/intents", &as_user)).await;
    assert!(user_intents.contains(marker), "使用者仍看得到 recovery journal");
    let user_slots = raw(e.app.clone(), call("/api/build-slots", &as_user)).await;
    assert!(user_slots.contains("victim-holder-UNIQUE"), "使用者仍看得到全域 build 佇列");

    let acquire = format!(
        "POST /build-slots/acquire HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
        worker.id,
        worker.hook_token,
        format!("holder=scope-worker&bot_id={}&purpose=self&host=local", worker.id).len(),
        format!("holder=scope-worker&bot_id={}&purpose=self&host=local", worker.id),
    );
    let acquired = raw(e.app.clone(), acquire).await;
    assert_ne!(status(&acquired), "403", "非 /api 的 acquire 仍給 bot");
}

/// `/hook/{provider}` 的入口：畸形／缺欄位／型別錯／過大的 body 都是乾淨的 4xx（不是 5xx、不 panic、不留一列在收件匣），
/// 認證失敗一律 401，provider 跟 bot 的 kind 不符 409；正常的一則收下一列，重送不長第二列。
#[tokio::test]
async fn the_hook_endpoint_rejects_bad_requests_cleanly_and_stores_a_good_one_once() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "hook-edge").await;
    let post = |provider: &str, token: Option<&str>, body: String| {
        let tok = token.map(|t| format!("X-AM-Bot-Token: {t}\r\n")).unwrap_or_default();
        format!(
            "POST /hook/{provider} HTTP/1.1\r\nHost: 127.0.0.1\r\n{tok}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    };
    let rows = || async { sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM hook_events").fetch_one(&e.app.db).await.unwrap() };
    let status = |r: &str| r.split_whitespace().nth(1).unwrap_or("").to_string();
    let good = |extra: &str| format!(r#"{{"bot_id":"{}","provider":"claude","payload":{{"hook_event_name":"PostToolUse","tool_use_id":"t1"{extra}}},"received_at":"2026-10-02T00:00:00.000Z"}}"#, bot.id);

    for (what, body, want) in [
        ("畸形 JSON", "{".to_string(), "400"),
        ("缺 bot_id", r#"{"payload":{}}"#.to_string(), "422"),
        ("bot_id 型別錯", r#"{"bot_id":5,"payload":{}}"#.to_string(), "422"),
        ("payload 是字串也收得下（之後的處理不 panic）", format!(r#"{{"bot_id":"{}","payload":"x"}}"#, bot.id), "200"),
    ] {
        let r = raw(e.app.clone(), post("claude", Some(&bot.hook_token), body)).await;
        assert_eq!(status(&r), want, "{what}: {r}");
    }
    assert_eq!(rows().await, 1, "只有那則型別怪但合法的 body 進了收件匣");

    // 認證：沒有 token、錯的 token、不存在的 bot，一律 401 且分不出差別。
    for (what, token, bot_id) in [("沒有 token", None, bot.id.as_str()), ("錯 token", Some("wrong"), bot.id.as_str()), ("沒這顆 bot", Some("x"), "no-such-bot")] {
        let body = format!(r#"{{"bot_id":"{bot_id}","payload":{{}}}}"#);
        let r = raw(e.app.clone(), post("claude", token, body)).await;
        assert_eq!(status(&r), "401", "{what}: {r}");
    }
    // 未知 provider（跟這顆 bot 的 kind 不符）。
    let r = raw(e.app.clone(), post("nonesuch", Some(&bot.hook_token), good(""))).await;
    let r2 = raw(e.app.clone(), post("claude", Some(&bot.hook_token), good("").replace(r#""provider":"claude""#, r#""provider":"codex""#))).await;
    assert_eq!(status(&r2), "409", "{r2}");
    assert!(status(&r) == "200" || status(&r) == "409", "URL 上的 provider 被 body 的蓋過：{r}");

    // 太大：2 MiB 以上整個擋在 body 限制，不進 JSON 解析、不進收件匣。
    let before = rows().await;
    let huge = good(&format!(r#","tool_response":"{}""#, "x".repeat(3 * 1024 * 1024)));
    let r = raw(e.app.clone(), post("claude", Some(&bot.hook_token), huge)).await;
    assert_eq!(status(&r), "413", "{}", &r[..r.len().min(200)]);
    assert_eq!(rows().await, before);

    // 好的一則：收下一列，同一則重送回 stored:false、不長第二列。
    let fresh = || good("").replace("\"t1\"", "\"t2\"");
    let before = rows().await;
    let one = raw(e.app.clone(), post("claude", Some(&bot.hook_token), fresh())).await;
    assert_eq!(status(&one), "200", "{one}");
    assert!(one.contains(r#""stored":true"#), "{one}");
    let again = raw(e.app.clone(), post("claude", Some(&bot.hook_token), fresh())).await;
    assert!(again.contains(r#""stored":false"#), "{again}");
    assert_eq!(rows().await, before + 1, "重送不長第二列");
}

async fn state(app: Arc<App>, headers: &[(&str, &str)]) -> String {
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    raw(
        app,
        format!("GET /api/state HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\nContent-Length: 0\r\n\r\n"),
    )
    .await
}

#[tokio::test]
async fn a_bot_id_and_its_token_authenticate_without_the_ui_token() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "bot-auth").await;
    let response = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &bot.hook_token)]).await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
}

#[tokio::test]
async fn a_bot_id_without_its_token_cannot_fall_back_to_a_valid_ui_token() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "missing-bot-token").await;
    let response = state(e.app.clone(), &[("X-AM-Token", &e.app.ui_token), ("X-AM-Bot-Id", &bot.id)]).await;
    assert!(response.starts_with("HTTP/1.1 401") || response.starts_with("HTTP/1.1 403"), "{response}");
}

#[tokio::test]
async fn a_wrong_bot_token_cannot_fall_back_to_a_valid_ui_token() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "wrong-bot-token").await;
    let response = state(
        e.app.clone(),
        &[("X-AM-Token", &e.app.ui_token), ("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", "wrong")],
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 401"), "invalid Bot proof must not downgrade to User: {response}");
}

#[tokio::test]
async fn an_authenticated_bot_with_no_relay_from_is_stored_as_that_bot() {
    let e = crate::testing::env().await;
    let sender = distinct_bot(&e, "sender").await;
    let target = distinct_bot(&e, "target").await;
    sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&target.id).execute(&e.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-principal','target','test',1,?)",
    )
    .bind(db::ulid())
    .bind(&target.id)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    e.herdr.live_pane("pane-principal", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

    let body = json!({ "text": "bot-authored", "client_request_id": "bot-authored-no-relay" });
    let bytes = serde_json::to_vec(&body).unwrap();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            target.id,
            sender.id,
            sender.hook_token,
            bytes.len(),
            String::from_utf8(bytes).unwrap(),
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let payload = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    let payload: Value = serde_json::from_str(payload).unwrap();
    let message_id = payload["message_id"].as_str().expect("prompt response message id");
    let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(relay.as_deref(), Some(sender.id.as_str()));
}

#[tokio::test]
async fn an_empty_relay_from_cannot_override_the_authenticated_bot_identity() {
    let e = crate::testing::env().await;
    let sender = distinct_bot(&e, "empty-relay-sender").await;
    let target = distinct_bot(&e, "empty-relay-target").await;
    sqlx::query("UPDATE bots SET kind='grok' WHERE id=?").bind(&target.id).execute(&e.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-empty-relay','target','test',1,?)",
    )
    .bind(db::ulid())
    .bind(&target.id)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    e.herdr.live_pane("pane-empty-relay", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

    let body = serde_json::to_vec(&json!({ "text": "empty-relay-authored", "client_request_id": "empty-relay-authored", "relay_from": "" })).unwrap();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            target.id,
            sender.id,
            sender.hook_token,
            body.len(),
            String::from_utf8(body).unwrap(),
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let payload = response.split("\r\n\r\n").nth(1).unwrap_or_default();
    let payload: Value = serde_json::from_str(payload).unwrap();
    let message_id = payload["message_id"].as_str().expect("prompt response message id");
    let relay: Option<String> = sqlx::query_scalar("SELECT relay_from FROM messages WHERE id=?")
        .bind(message_id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(relay.as_deref(), Some(sender.id.as_str()), "empty body claim must not erase the authenticated bot identity");
}

#[tokio::test]
async fn a_service_credential_authenticates_only_inside_its_scope() {
    let e = crate::testing::env().await;
    e.app.service_tokens.write().unwrap().insert(crate::service_auth::HERDR_UPGRADE.into(), "service-secret".into());
    let allowed = raw(
        e.app.clone(),
        "GET /api/capabilities HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: herdr-upgrade\r\nX-AM-Service-Token: service-secret\r\nConnection: close\r\nContent-Length: 0\r\n\r\n".into(),
    )
    .await;
    assert!(allowed.starts_with("HTTP/1.1 200"), "{allowed}");

    let denied = state(e.app.clone(), &[("X-AM-Service-Id", "herdr-upgrade"), ("X-AM-Service-Token", "service-secret")]).await;
    assert!(denied.starts_with("HTTP/1.1 403"), "service must not read arbitrary state: {denied}");

    let fallback = state(
        e.app.clone(),
        &[("X-AM-Token", &e.app.ui_token), ("X-AM-Service-Id", "herdr-upgrade"), ("X-AM-Service-Token", "wrong")],
    )
    .await;
    assert!(fallback.starts_with("HTTP/1.1 401"), "invalid service credentials must not downgrade to User: {fallback}");
}

async fn service_post(app: Arc<App>, id: &str, token: &str, path: &str) -> String {
    raw(
        app,
        format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Service-Id: {id}\r\nX-AM-Service-Token: {token}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n"),
    )
    .await
}

#[tokio::test]
async fn daemon_swap_opens_its_own_restart_window_without_an_approval_from_anyone_else() {
    let e = crate::testing::env().await;
    let body = || SwapWindowIn { owner: "daemon-update-kick".into(), commit: "abc1234".into(), ttl_secs: Some(600) };
    let svc = || Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into()));
    // 只有 daemon-swap 服務身份；使用者與 bot 都不行。
    for other in [RequestPrincipal::User, RequestPrincipal::Bot("b1".into()), RequestPrincipal::Service(crate::service_auth::HERDR_UPGRADE.into())] {
        let r = service_daemon_swap_restart_window(State(e.app.clone()), Extension(other), Json(body())).await;
        assert!(matches!(r, Err(LcError::Forbidden(_))));
    }
    let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(body())).await.unwrap();
    assert_eq!(v["lease"]["held"], true, "{v}");
    assert!(v["lease_token"].as_str().is_some_and(|t| !t.is_empty()));
    assert_eq!(v["approval"]["decided_by"], "service(daemon-swap)");
    assert_eq!(v["approval"]["purpose"], "restart");
    // 窗口被握著時別人拿不到，而且不留下 approved 的殘單。
    let mut other = body();
    other.owner = "someone-else".into();
    let r = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(other)).await;
    assert!(matches!(r, Err(LcError::Conflict(_))), "{r:?}");
    let live = crate::supervisor::store::approvals(&e.app.db, 100).await.unwrap().into_iter().filter(|a| a.status == "approved").count();
    assert_eq!(live, 1, "只剩握著窗口的那一筆");
}

#[tokio::test]
async fn daemon_swap_window_is_refused_while_a_bot_is_working() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "swap-busy").await;
    let run = crate::testing::fake_run(&e.app, &bot.id).await;
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
    let body = SwapWindowIn { owner: "daemon-update-kick".into(), commit: "abc1234".into(), ttl_secs: None };
    let r = service_daemon_swap_restart_window(
        State(e.app.clone()),
        Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into())),
        Json(body),
    )
    .await;
    assert!(matches!(&r, Err(LcError::Conflict(v)) if v["detail"]["reason"] == "not_idle" || v["reason"] == "not_idle"), "{r:?}");
}

/// commit 是自由字串會被寫進核准、租約與稽核紀錄、也會被腳本拿去用：三個入口（申請、acquire、daemon-swap 的窗口）都只收 7～64 碼十六進位。
/// 被擋的什麼都不寫（不留 pending 核准、不留租約）。
#[tokio::test]
async fn a_commit_that_is_not_a_sha_is_refused_at_every_entrance_and_nothing_is_written() {
    let e = crate::testing::env().await;
    let bad = ["main", "abc12", "abc1234; rm -rf /", "abc1234\n--force"];
    for commit in bad {
        let r = crate::runners::supervisor::api::post_approval(
            State(e.app.clone()),
            HeaderMap::new(),
            Extension(RequestPrincipal::User),
            Json(crate::runners::supervisor::api::ApprovalIn {
                requester: "ops".into(), purpose: "rebuild".into(), scope: "x".into(), target_commit: Some(commit.into()),
                expires_in_secs: None, request_id: None, supersedes: None, reason: None,
            }),
        )
        .await;
        assert!(matches!(r, Err(LcError::Bad(_))), "{commit:?}: {r:?}");
        let r = service_daemon_swap_restart_window(
            State(e.app.clone()),
            Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into())),
            Json(SwapWindowIn { owner: "ops".into(), commit: commit.into(), ttl_secs: None }),
        )
        .await;
        assert!(matches!(r, Err(LcError::Bad(_))), "{commit:?}: {r:?}");
    }
    let approvals: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_approvals").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(approvals, 0, "被擋的申請不留列");
    let good = crate::runners::supervisor::api::post_approval(
        State(e.app.clone()),
        HeaderMap::new(),
        Extension(RequestPrincipal::User),
        Json(crate::runners::supervisor::api::ApprovalIn {
            requester: "ops".into(), purpose: "rebuild".into(), scope: "x".into(), target_commit: Some("0123456789abcdef0123456789abcdef01234567".into()),
            expires_in_secs: None, request_id: None, supersedes: None, reason: None,
        }),
    )
    .await;
    assert!(good.is_ok(), "{good:?}");
}

/// 2026-10-01：一直有 bot 在忙時，每一輪都開新核准、拿不到就撤，升級計時每輪歸零，自動部署卡了一整晚。
/// 現在同 commit 沿用同一張（`not_idle` 不撤），換 commit 開新的並接續舊的等待；等滿門檻 working 就不再擋。
#[tokio::test]
async fn daemon_swap_keeps_one_approval_across_rounds_so_a_busy_fleet_still_gets_a_window() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "swap-forever-busy").await;
    let run = crate::testing::fake_run(&e.app, &bot.id).await;
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
    let svc = || Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into()));
    let ask = |commit: &str| SwapWindowIn { owner: "daemon-update-kick".into(), commit: commit.into(), ttl_secs: None };
    let approval_of = |r: &Result<Json<Value>, LcError>| match r {
        Err(LcError::Conflict(v)) => v["safety"]["escalation_approval_id"].as_str().or(v["detail"]["safety"]["escalation_approval_id"].as_str()).map(String::from),
        _ => None,
    };
    let r1 = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask("c1c1c1c1"))).await;
    let r2 = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask("c1c1c1c1"))).await;
    let (a1, a2) = (approval_of(&r1), approval_of(&r2));
    assert!(a1.is_some(), "{r1:?}");
    assert_eq!(a1, a2, "同 commit 下一輪沿用同一張核准");
    let a1 = a1.unwrap();
    assert_eq!(crate::supervisor::store::approval(&e.app.db, &a1).await.unwrap().unwrap().status, "approved", "not_idle 不撤");

    // 那張核准已經等了 31 分鐘（把核准時間往前推）；main 又動了，換 commit 也接得下去。
    let old = crate::db::iso_in(-31 * 60);
    sqlx::query("UPDATE supervisor_approvals SET decided_at=?, created_at=? WHERE id=?").bind(&old).bind(&old).bind(&a1).execute(&e.app.db).await.unwrap();
    let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask("c2c2c2c2"))).await.expect("等滿門檻之後 working 不再擋");
    assert_eq!(v["lease"]["held"], true, "{v}");
    assert_eq!(crate::supervisor::store::approval(&e.app.db, &a1).await.unwrap().unwrap().status, "superseded");
}

/// 2026-10-04 08:41～09:22：每一輪都在瞬間閒置時拿到窗口、§3a 換版前複查又看到有人 working 而交還；交還消耗了核准，
/// 下一輪重開一張，`escalates_at` 每輪往後推，忙碌的機群永遠升級不了。現在交還後的下一張接續等待（計時不歸零），
/// 等滿門檻之後 working 不再擋——acquire 與 3a 複查（同一張核准的 `safety`）都一樣。
#[tokio::test]
async fn a_window_given_back_by_the_swap_recheck_does_not_reset_the_escalation_clock() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "swap-flicker").await;
    let run = crate::testing::fake_run(&e.app, &bot.id).await;
    let set = |status: &'static str| {
        let (db, run) = (e.app.db.clone(), run.clone());
        async move { sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(status).bind(&run).execute(&db).await.unwrap() }
    };
    let svc = || Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into()));
    let owner = "daemon-update-kick";
    let ask = || SwapWindowIn { owner: owner.into(), commit: "d4d4d4d4".into(), ttl_secs: None };
    let approval = |id: &str| {
        let (db, id) = (e.app.db.clone(), id.to_string());
        async move { crate::supervisor::store::approval(&db, &id).await.unwrap().unwrap() }
    };
    let recheck = |id: &str| {
        let (app, id) = (e.app.clone(), id.to_string());
        async move { crate::supervisor::maintenance::safety_as(&app, &[], Some(&id), Some(owner)).await.unwrap() }
    };

    // 第一輪：瞬間閒置，拿到窗口；3a 複查時又有人在忙、還沒放寬 → 交還。
    let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask())).await.expect("閒置時拿得到");
    let a1 = v["approval"]["id"].as_str().unwrap().to_string();
    set("working").await;
    let s = recheck(&a1).await;
    assert_eq!(s["safe"], false, "{s}");
    let fence = v["lease"]["fence"].as_i64().unwrap();
    let token = v["lease_token"].as_str().unwrap();
    assert!(crate::supervisor::maintenance::release(&e.app, "restart", owner, fence, crate::supervisor::store::LeaseProof::Token(token)).await.unwrap());
    let a1 = approval(&a1).await;
    assert_eq!(a1.status, "consumed", "一次核准一個窗口：交還照樣消耗");
    let since = a1.waiting_since().unwrap().to_string();

    // 第二輪：同一個 commit 開新的一張，等待起點接續第一張；escalates_at 沒有往後推。
    let r = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask())).await;
    let Err(LcError::Conflict(d)) = &r else { panic!("{r:?}") };
    let d = if d.get("detail").is_some() { &d["detail"] } else { d };
    assert_eq!(d["reason"], "not_idle", "{d}");
    let a2 = d["safety"]["escalation_approval_id"].as_str().unwrap().to_string();
    assert_ne!(a2, a1.id);
    assert_eq!(approval(&a2).await.waiting_since(), Some(since.as_str()), "接續第一張的等待");
    let eta = chrono::DateTime::parse_from_rfc3339(d["escalates_at"].as_str().unwrap()).unwrap();
    let want = chrono::DateTime::parse_from_rfc3339(&since).unwrap() + chrono::Duration::seconds(crate::supervisor::maintenance::escalate_after_secs());
    assert!((eta - want).num_seconds().abs() <= 2, "escalates_at {eta} 應該還是 {want}");

    // 等滿門檻（把第一張的等待往前推 31 分鐘）：即使還有人 working，窗口給得出來，3a 複查也放行。
    let old = crate::db::iso_in(-31 * 60);
    sqlx::query("UPDATE supervisor_approvals SET wait_since=? WHERE id=?").bind(&old).bind(&a2).execute(&e.app.db).await.unwrap();
    let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask())).await.expect("等滿門檻後 working 不擋");
    assert_eq!(v["approval"]["id"], a2.as_str(), "同一張沿用");
    assert_eq!(v["safety"]["escalated"], true, "{v}");
    assert!(e.app.deploy_wait.lock().unwrap().as_ref().unwrap().auto_escalated, "成功 acquire 的 safety 也要更新 deploy_wait");
    let s = recheck(&a2).await;
    assert_eq!(s["safe"], true, "3a 複查吃得到同一張核准的放寬：{s}");
    assert_eq!(s["escalated"], true, "{s}");
    assert!(!s["working"].as_array().unwrap().is_empty(), "還有人在忙，放行是因為放寬：{s}");
}

/// 2026-10-02 風暴的尾巴：拿窗口那一刻 daemon 自己的 DB 讀寫失敗（`database is locked`，正好是 daemon 重啟中）不是「窗口拒絕」，
/// 核准沒有任何問題；撤掉它，下一輪重開的核准就把 30 分鐘的升級計時歸零，一直忙的機群又等不到放寬。基礎設施暫時失效要留著核准、
/// 下一輪帶同一張再試。
#[tokio::test]
async fn daemon_swap_keeps_its_approval_when_the_window_attempt_fails_on_the_database() {
    let e = crate::testing::env().await;
    let svc = || Extension(RequestPrincipal::Service(crate::service_auth::DAEMON_SWAP.into()));
    let ask = || SwapWindowIn { owner: "daemon-update-kick".into(), commit: "abc1234".into(), ttl_secs: None };
    let live = || async { crate::supervisor::store::approvals(&e.app.db, 100).await.unwrap().into_iter().filter(|a| a.status == "approved").collect::<Vec<_>>() };

    crate::testing::make_table_unreadable(&e.app, "supervisor_leases").await;
    let r = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask())).await;
    assert!(matches!(r, Err(LcError::Upstream(_) | LcError::Unavailable(_))), "{r:?}");
    let kept = live().await;
    assert_eq!(kept.len(), 1, "DB 暫時出錯不撤核准");

    crate::testing::make_table_readable(&e.app, "supervisor_leases").await;
    let Json(v) = service_daemon_swap_restart_window(State(e.app.clone()), svc(), Json(ask())).await.unwrap();
    assert_eq!(v["lease"]["held"], true, "{v}");
    assert_eq!(v["approval"]["id"], kept[0].id, "下一輪沿用同一張核准");
    let (total,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM supervisor_approvals").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(total, 1, "沒有多開核准");
}

#[tokio::test]
async fn daemon_swap_service_scope_accepts_only_its_fixed_probe_route() {
    let e = crate::testing::env().await;
    e.app.service_tokens.write().unwrap().insert(crate::service_auth::DAEMON_SWAP.into(), "swap-secret".into());
    let target = distinct_bot(&e, "swap-probe-target").await;
    // An unknown bot is 404 from the handler: auth and scope let the request through.
    let response = service_post(e.app.clone(), "daemon-swap", "swap-secret", "/api/services/daemon-swap/probe/01NOSUCHBOT").await;
    assert!(response.starts_with("HTTP/1.1 404"), "{response}");
    // The same credential cannot use the generic prompt, bot control, or the other service's route.
    for path in [
        format!("/api/bots/{}/prompt", target.id),
        format!("/api/bots/{}/stop", target.id),
        format!("/api/services/herdr-upgrade/resume/{}", target.id),
        "/api/services/herdr-upgrade/notify".to_string(),
    ] {
        let denied = service_post(e.app.clone(), "daemon-swap", "swap-secret", &path).await;
        assert!(denied.starts_with("HTTP/1.1 403"), "{path}: {denied}");
    }
    // Another service's token under this id, or an unknown service id, is not a credential.
    e.app.service_tokens.write().unwrap().insert(crate::service_auth::HERDR_UPGRADE.into(), "herdr-secret".into());
    let swapped = service_post(e.app.clone(), "daemon-swap", "herdr-secret", "/api/services/daemon-swap/probe/01NOSUCHBOT").await;
    assert!(swapped.starts_with("HTTP/1.1 401"), "{swapped}");
    let unknown = service_post(e.app.clone(), "root", "swap-secret", "/api/services/daemon-swap/probe/01NOSUCHBOT").await;
    assert!(unknown.starts_with("HTTP/1.1 401"), "{unknown}");
}

/// 被證明身份的一般 bot 打 AGM 的管理面：setup／start／stop／fallback、交辦與裁示、管理摘要、ops-alert、CLI 更新、
/// 協調者的 setup／start／stop——一律 403 `role_required`，而且什麼都不動。使用者（沒帶 bot 標頭）與角色 bot 不受影響；
/// 角色換人之後舊 bot 立刻失效。
#[tokio::test]
async fn a_plain_bot_cannot_use_the_agm_management_endpoints_and_a_role_swap_takes_effect_at_once() {
    plain_bot_gate_body().await;
}

/// 堆疊守門的預算：1 MiB（#1465；#1461 時是 1.5 MiB）。測試執行緒預設 2 MiB，debug 建置下 async fn 的 poll 框與 tokio 的
/// spawn 都會在堆疊上留下 future 的多份副本，路徑一深就貼近上限；預算收到一半，餘裕才看得見。
const STACK_BUDGET: usize = 1024 * 1024;

/// 在 [`STACK_BUDGET`] 的執行緒上跑一段 async 內容；溢位會整個行程 SIGABRT（stack overflow，沒有任何 FAILED 行）。
fn on_budget_stack<Fut: std::future::Future<Output = ()>>(body: impl FnOnce() -> Fut + Send + 'static) {
    std::thread::Builder::new()
        .name("stack-budget".into())
        .stack_size(STACK_BUDGET)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(body())
        })
        .expect("spawn")
        .join()
        .expect("the path panicked on the stack-budget thread");
}

/// #1461／#1465 守門：同一條路徑（`POST /api/supervisor/start` 經 start_requested → start_manager → 啟動 bot、送握手、
/// 起控制迴圈、對帳）在 1 MiB 的執行緒上要跑得完。餘裕不足就在這裡紅，不等整批 `per_principal_auth_tests` 以 SIGABRT 溢位。
#[test]
fn the_plain_bot_gate_path_fits_in_a_one_mib_stack() {
    on_budget_stack(plain_bot_gate_body);
}

/// #1465 守門：控制迴圈的一整拍（二十幾段）在 1 MiB 的執行緒上跑得完。以前所有段落寫在同一個 async fn 裡，
/// debug 建置下那個迴圈的 poll 框有 900 KiB 以上，光是被 poll 一次就吃掉預算的大半。
#[test]
fn a_controller_tick_fits_in_a_one_mib_stack() {
    on_budget_stack(controller_tick_body);
}

/// #1465 守門：重啟（鎖 → 停 → 起；restart 的脫離任務跑的就是這條）在 1 MiB 的執行緒上跑得完。
/// 它比單純的啟動多疊了 `restart_bot_with_authority`／`restart_stop_and_start`／`restart_start` 三層大框。
#[test]
fn a_restart_fits_in_a_one_mib_stack() {
    on_budget_stack(restart_body);
}

async fn restart_body() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "restart-gate").await;
    lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    lifecycle::restart_bot_with(&e.app, &bot.id, Default::default()).await.unwrap();
}

async fn controller_tick_body() {
    let e = crate::testing::env().await;
    let manager = distinct_bot(&e, "tick-gate").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &manager.id, &e.project_id, "/tmp").await.unwrap();
    crate::runners::supervisor::runtime::tick_once(&e.app).await;
}

/// #1461 守門：關鍵 future 的大小上限。debug 建置下 future 會在呼叫端的 poll 框裡留一份暫存，超過這個大小就該用 Box::pin 切開。
#[tokio::test]
async fn the_supervisor_start_futures_stay_small() {
    const LIMIT: usize = 64 * 1024;
    let e = crate::testing::env().await;
    let sizes = [
        ("post_start", std::mem::size_of_val(&crate::runners::supervisor::api::post_start(State(e.app.clone())))),
        ("start_requested", std::mem::size_of_val(&crate::runners::supervisor::start_requested(&e.app))),
        ("start_manager", std::mem::size_of_val(&crate::runners::supervisor::start_manager(&e.app, None))),
        ("status_json", std::mem::size_of_val(&crate::runners::supervisor::status_json(&e.app))),
    ];
    for (name, size) in sizes {
        assert!(size <= LIMIT, "{name} 的 future 有 {size} 位元組，超過 {LIMIT}：用 Box::pin 切開");
    }
}

async fn plain_bot_gate_body() {
    let e = crate::testing::env().await;
    let plain = distinct_bot(&e, "plain-gate").await;
    let patrol = distinct_bot(&e, "patrol-gate").await;
    let other = distinct_bot(&e, "patrol-gate-2").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &patrol.id, &e.project_id, "/tmp").await.unwrap();
    let call = |method: &'static str, path: &'static str, who: Option<(&str, &str)>| {
        let app = e.app.clone();
        let ui = e.app.ui_token.clone();
        let who = who.map(|(i, t)| (i.to_string(), t.to_string()));
        async move {
            let body = "{}";
            let auth = match &who {
                Some((id, tok)) => format!("X-AM-Bot-Id: {id}\r\nX-AM-Bot-Token: {tok}\r\n"),
                None => format!("X-AM-Token: {ui}\r\n"),
            };
            raw(app, format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len())).await
        }
    };
    let endpoints: &[(&'static str, &'static str)] = &[
        ("POST", "/api/supervisor/setup"),
        ("POST", "/api/supervisor/start"),
        ("POST", "/api/supervisor/stop"),
        ("POST", "/api/supervisor/fallback"),
        ("POST", "/api/supervisor/assignments/nope/review"),
        ("PUT", "/api/supervisor/handoff"),
        ("POST", "/api/supervisor/ops-alerts"),
        ("POST", "/api/supervisor/cli"),
        ("POST", "/api/supervisor/responder/setup"),
        ("POST", "/api/supervisor/responder/start"),
        ("POST", "/api/supervisor/responder/stop"),
    ];
    let denied = |r: &str| r.starts_with("HTTP/1.1 403") && r.contains("role_required");
    let plain_creds = (plain.id.as_str(), plain.hook_token.as_str());
    let patrol_creds = (patrol.id.as_str(), patrol.hook_token.as_str());
    for (m, p) in endpoints {
        let r = call(m, p, Some(plain_creds)).await;
        assert!(denied(&r), "{m} {p}: 一般 bot 要 403 role_required：{r}");
        let r = call(m, p, None).await;
        assert!(!denied(&r), "{m} {p}: 使用者不能被擋：{r}");
    }
    // 角色 bot：過了這道閘（後面可能因為別的原因 4xx/5xx，但不是 role_required）。
    for (m, p) in endpoints {
        let r = call(m, p, Some(patrol_creds)).await;
        assert!(!denied(&r), "{m} {p}: 角色 bot 不能被擋：{r}");
    }
    // 角色換人：原本的巡檢立刻變成一般 bot，新的那顆立刻有權限。
    crate::supervisor::store::set_env(&e.app.db, &other.id, &e.project_id, "/tmp").await.unwrap();
    let r = call("POST", "/api/supervisor/stop", Some(patrol_creds)).await;
    assert!(denied(&r), "換人之後舊的巡檢沒有權限了：{r}");
    let r = call("POST", "/api/supervisor/stop", Some((other.id.as_str(), other.hook_token.as_str()))).await;
    assert!(!denied(&r), "{r}");
}

/// 審批只能由使用者收回，或由 AGM 角色處理；一般 bot 的有效 token 不能借 `actor_role(None)` 冒充使用者去 deny／revoke。
#[tokio::test]
async fn a_plain_bot_cannot_deny_or_revoke_supervisor_approvals() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "approval-decision-bot").await;
    let pending = crate::supervisor::store::create_approval(&e.app.db, "requester-a", "rebuild", "daemon", Some("abc1234"), None, None)
        .await
        .unwrap()
        .approval;
    let approved = crate::supervisor::store::create_approval(&e.app.db, "requester-b", "restart", "daemon", Some("def5678"), None, None)
        .await
        .unwrap()
        .approval;
    crate::supervisor::store::decide_approval(&e.app.db, &approved.id, "approved", "AGM", None, None)
        .await
        .unwrap();

    for (id, decision, expected_status) in [(&pending.id, "deny", "pending"), (&approved.id, "revoke", "approved")] {
        let body = json!({"decision": decision}).to_string();
        let response = raw(
            e.app.clone(),
            format!(
                "POST /api/supervisor/approvals/{id}/decide HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                bot.id,
                bot.hook_token,
                body.len()
            ),
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "bot 的 {decision} 應被拒絕：{response}");
        assert!(response.contains("role_required"), "{response}");
        let row = crate::supervisor::store::approval(&e.app.db, id).await.unwrap().unwrap();
        assert_eq!(row.status, expected_status, "被拒絕的 {decision} 不能改審批狀態");
    }
}

/// acquire 的 owner 不能只靠 body 自稱：否則另一顆 bot 能拿受害者已核准的 approval 開出它的 lease。
#[tokio::test]
async fn a_bot_can_only_acquire_a_lease_for_its_own_approval() {
    let e = crate::testing::env().await;
    let owner = distinct_bot(&e, "lease-owner").await;
    let attacker = distinct_bot(&e, "lease-attacker").await;
    let approval = crate::supervisor::store::create_approval(&e.app.db, &owner.id, "rebuild", "daemon", None, None, None)
        .await
        .unwrap()
        .approval;
    crate::supervisor::store::decide_approval(&e.app.db, &approval.id, "approved", "AGM", None, None)
        .await
        .unwrap();

    let body = json!({"owner": owner.id, "approval_id": approval.id, "require_idle": false}).to_string();
    let call = |bot: &db::Bot| {
        let app = e.app.clone();
        let body = body.clone();
        let (id, token) = (bot.id.clone(), bot.hook_token.clone());
        async move {
            raw(
                app,
                format!(
                    "POST /api/supervisor/leases/rebuild/acquire HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {id}\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
            .await
        }
    };

    let denied = call(&attacker).await;
    assert!(denied.starts_with("HTTP/1.1 403"), "一般 bot 不能冒用核准的 requester：{denied}");
    assert!(denied.contains("requester_not_the_caller"), "{denied}");
    assert!(crate::supervisor::store::lease(&e.app.db, "rebuild").await.unwrap().is_none(), "拒絕時不能開 lease");

    let allowed = call(&owner).await;
    assert!(allowed.starts_with("HTTP/1.1 200"), "申請者本人仍能開自己的 lease：{allowed}");
}

/// Before lease tokens were introduced, a legacy row can still accept a missing token.
/// That compatibility path must not let Bot A renew or release Bot B's lease with public owner/fence values.
#[tokio::test]
async fn a_bot_can_only_renew_or_release_its_own_legacy_lease() {
    let e = crate::testing::env().await;
    let owner = distinct_bot(&e, "legacy-lease-owner").await;
    let attacker = distinct_bot(&e, "legacy-lease-attacker").await;
    let mut leases = Vec::new();
    for resource in ["rebuild", "restart"] {
        let approval = crate::supervisor::store::create_approval(
            &e.app.db,
            &owner.id,
            resource,
            "daemon",
            None,
            None,
            None,
        )
        .await
        .unwrap()
        .approval;
        crate::supervisor::store::decide_approval(&e.app.db, &approval.id, "approved", "AGM", None, None)
            .await
            .unwrap();
        let lease = crate::supervisor::store::acquire_lease(
            &e.app.db,
            resource,
            &owner.id,
            Some(&approval.id),
            None,
            &db::iso_in(900),
            false,
            None,
            &json!({}),
        )
        .await
        .unwrap()
        .unwrap();
        sqlx::query("UPDATE supervisor_leases SET lease_token=NULL WHERE resource=?")
            .bind(resource)
            .execute(&e.app.db)
            .await
            .unwrap();
        leases.push((resource.to_string(), lease.fence, lease.expires_at.clone().unwrap()));
    }

    let call = |method: &str, resource: &str, body: Value, bot: &db::Bot| {
        let app = e.app.clone();
        let body = body.to_string();
        let (id, token) = (bot.id.clone(), bot.hook_token.clone());
        let operation = if resource == "rebuild" { "renew" } else { "release" }.to_string();
        let method = method.to_string();
        let resource = resource.to_string();
        async move {
            raw(
                app,
                format!(
                    "{method} /api/supervisor/leases/{resource}/{operation} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {id}\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                ),
            )
            .await
        }
    };
    for (resource, fence, old_expiry) in &leases {
        let operation = if resource == "rebuild" { "renew" } else { "release" };
        let method = "POST";
        let body = json!({"owner":owner.id,"fence":fence});
        let denied = call(method, resource, body.clone(), &attacker).await;
        assert!(denied.starts_with("HTTP/1.1 403") && denied.contains("requester_not_the_caller"), "Bot A must not {operation} Bot B's tokenless lease: {denied}");
        let held = crate::supervisor::store::lease(&e.app.db, resource).await.unwrap().unwrap();
        assert!(held.released_at.is_none(), "the rejected cross-Bot {operation} must not release the lease");
        if operation == "renew" {
            assert_eq!(held.expires_at.as_deref(), Some(old_expiry.as_str()), "the rejected cross-Bot renewal must not extend the lease");
        }

        let owner_call = call(method, resource, body, &owner).await;
        assert!(owner_call.starts_with("HTTP/1.1 200"), "the actual Bot owner retains legacy lease {operation}: {owner_call}");
    }
}

/// 角色閘的相容性：release／herdr 更新任務的 assignee 可以是專用的一般 bot（`release_bot_id`），任務正文叫它跑
/// `agm assign --notice --bot <巡檢>` 把結論交回去——一般 bot 對 AGM 角色 bot 送 `notice` 要放行；派工、要驗收的交辦、
/// 掛任務的交辦、送給別的一般 bot 的 notice 仍然 403。
#[tokio::test]
async fn a_plain_bot_may_send_a_notice_to_an_agm_role_but_not_dispatch_work() {
    let e = crate::testing::env().await;
    let plain = distinct_bot(&e, "release-bot").await;
    let patrol = distinct_bot(&e, "patrol-notice").await;
    let victim = distinct_bot(&e, "victim").await;
    crate::supervisor::store::get_or_init(&e.app.db).await.unwrap();
    crate::supervisor::store::set_env(&e.app.db, &patrol.id, &e.project_id, "/tmp").await.unwrap();
    let post = |body: Value, who: Option<&db::Bot>| {
        let app = e.app.clone();
        let ui = e.app.ui_token.clone();
        let who = who.map(|b| (b.id.clone(), b.hook_token.clone()));
        async move {
            let body = body.to_string();
            let auth = match &who {
                Some((id, tok)) => format!("X-AM-Bot-Id: {id}\r\nX-AM-Bot-Token: {tok}\r\n"),
                None => format!("X-AM-Token: {ui}\r\n"),
            };
            raw(app, format!("POST /api/supervisor/assignments HTTP/1.1\r\nHost: 127.0.0.1\r\n{auth}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len())).await
        }
    };
    let denied = |r: &str| r.starts_with("HTTP/1.1 403") && r.contains("role_required");
    // 放行：notice → 巡檢。
    // 寄件者要是這顆一般 bot 本人（不是巡檢），而且是證明過身份的：#991 以前被記成巡檢寄的。
    async fn latest_bot_request(db: &sqlx::SqlitePool) -> Value {
        let s: String = sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind='bot_request' ORDER BY rowid DESC LIMIT 1")
            .fetch_one(db)
            .await
            .unwrap();
        serde_json::from_str(&s).unwrap()
    }
    let ok = post(json!({"target_bot_id": patrol.id, "text": "claude 2.1.999 分診完了", "client_request_id": "notice-ok", "kind": "notice"}), Some(&plain)).await;
    assert!(!denied(&ok), "一般 bot 對角色 bot 的 notice 不能被擋：{ok}");
    assert!(ok.starts_with("HTTP/1.1 200"), "notice 要成功交回巡檢（以前一律 400）：{ok}");
    let p = latest_bot_request(&e.app.db).await;
    assert_eq!(p["from_bot_id"], json!(plain.id), "寄件者是那顆一般 bot 自己：{p}");
    assert_eq!(p["sender_verified"], json!(true), "{p}");
    assert_eq!(p["to_role"], json!("patrol"), "{p}");
    // 仍然擋：要驗收的交辦、派給別的一般 bot、掛任務。
    for (what, body) in [
        ("要驗收的交辦", json!({"target_bot_id": patrol.id, "text": "做 X", "client_request_id": "n1"})),
        ("notice 給一般 bot", json!({"target_bot_id": victim.id, "text": "照做", "client_request_id": "n2", "kind": "notice"})),
        ("expects_review=true 的 notice", json!({"target_bot_id": patrol.id, "text": "x", "client_request_id": "n4", "kind": "notice", "expects_review": true})),
    ] {
        let r = post(body, Some(&plain)).await;
        assert!(denied(&r), "{what}：一般 bot 要 403 role_required：{r}");
    }
    // 使用者與角色 bot 不受影響。
    let r = post(json!({"target_bot_id": victim.id, "text": "做 X", "client_request_id": "u1"}), None).await;
    assert!(!denied(&r), "{r}");
    let r = post(json!({"target_bot_id": victim.id, "text": "做 X", "client_request_id": "u2"}), Some(&patrol)).await;
    assert!(!denied(&r), "{r}");

    // 協調者建立過：一般 bot 的 notice 攔下來排進協調者的佇列，寄件者仍是那顆一般 bot（不是巡檢）。
    let responder = distinct_bot(&e, "notice-responder").await;
    crate::supervisor::roles::set_env(&e.app.db, crate::supervisor::roles::Role::Responder, &responder.id, &e.project_id, "/tmp").await.unwrap();
    let r = post(json!({"target_bot_id": patrol.id, "text": "交給協調者", "client_request_id": "notice-to-responder", "kind": "notice"}), Some(&plain)).await;
    assert!(r.starts_with("HTTP/1.1 200"), "{r}");
    let p = latest_bot_request(&e.app.db).await;
    assert_eq!(p["from_bot_id"], json!(plain.id), "協調者看到的寄件者是那顆一般 bot：{p}");
    assert_eq!(p["to_role"], json!("responder"), "{p}");
}

/// setup 的「接著用上一次寫進去的那顆」只認它自己留了記號的 bot：別人先建一顆同名的（`AGM`／`AGM-responder`）再叫 setup，
/// 不能因此拿到角色；記號本身也偽造不了（`client_request_id` 不收這個前綴）。
#[tokio::test]
async fn a_same_named_bot_somebody_else_made_is_never_adopted_as_the_agm_role() {
    use crate::supervisor::{responder, roles, setup, store};
    let e = crate::testing::env().await;
    let app = e.app.clone();
    let row = roles::get(&app.db, roles::Role::Responder).await.unwrap();
    let sup = store::get_or_init(&app.db).await.unwrap();
    app.tools.lock().await.insert(
        LOCAL_HOST.to_string(),
        crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: vec![
                crate::config::IdentityCfg { name: row.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] },
                crate::config::IdentityCfg { name: sup.identity.clone(), kind: "claude".into(), host: None, env: Default::default(), args: vec![] },
            ],
            utc_offset_secs: None, herdr_cli: None, checked_at: db::now(),
        },
    );
    // 巡檢：總管目錄對應的專案先被別人建好、裡面有一顆叫 `AGM` 的普通 bot。
    let agm_dir = setup::agm_dir(&app);
    std::fs::create_dir_all(&agm_dir).unwrap();
    let canonical = crate::config::canonical_path(&agm_dir.to_string_lossy()).unwrap();
    let (pid, planted) = (db::ulid(), db::ulid());
    let (pid2, planted2) = (pid.clone(), planted.clone());
    app.cfg
        .update(move |cfg| {
            let mut bot: crate::config::BotCfg = toml::from_str(&format!("id = '{planted2}'\nname = 'AGM'\nkind = 'claude'\n")).unwrap();
            bot.persona = Some("我是普通 bot".into());
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, id: Some(pid2), path: canonical, label: "planted".into(), host: LOCAL_HOST.into(), bots: vec![bot], handed_off_to: None });
            Ok(())
        })
        .await
        .unwrap();
    let err = setup::ensure_env(&app).await.expect_err("同名但不是 setup 建的：不能接著用");
    assert!(format!("{err:?}").contains("name_taken"), "{err:?}");
    assert_eq!(store::get_or_init(&app.db).await.unwrap().bot_id, None, "角色沒有被綁到那顆 bot");
    assert_eq!(app.cfg.get().await.projects.iter().flat_map(|p| p.bots.iter()).find(|b| b.id.as_deref() == Some(planted.as_str())).unwrap().persona.as_deref(), Some("我是普通 bot"), "它的設定一個字都沒被改");

    // 協調者：同一個形狀。
    let resp_dir = responder::dir(&app);
    std::fs::create_dir_all(&resp_dir).unwrap();
    let resp_canonical = crate::config::canonical_path(&resp_dir.to_string_lossy()).unwrap();
    let planted_resp = db::ulid();
    let planted_resp2 = planted_resp.clone();
    app.cfg
        .update(move |cfg| {
            let bot: crate::config::BotCfg = toml::from_str(&format!("id = '{planted_resp2}'\nname = 'AGM-responder'\nkind = 'claude'\n")).unwrap();
            cfg.projects.push(crate::config::ProjectCfg { mode: "parent_child".into(), role_policy: Default::default(), deputy: None, specialist_dispatch: true, id: Some(db::ulid()), path: resp_canonical, label: "planted-resp".into(), host: LOCAL_HOST.into(), bots: vec![bot], handed_off_to: None });
            Ok(())
        })
        .await
        .unwrap();
    let err = responder::ensure_env(&app, None, None, None).await.expect_err("協調者也一樣");
    assert!(format!("{err:?}").contains("name_taken"), "{err:?}");
    assert_eq!(roles::get(&app.db, roles::Role::Responder).await.unwrap().bot_id, None);

    // 記號偽造不了：`client_request_id` 帶保留前綴 → 400。
    let r = create_bot(
        State(app.clone()),
        Path(e.project_id.clone()),
        Json(serde_json::from_value(json!({"name": "AGM-responder", "kind": "claude", "client_request_id": "agm-role-setup:responder"})).unwrap()),
    )
    .await;
    assert!(matches!(r, Err(LcError::Bad(_))), "{r:?}");
}

#[tokio::test]
async fn a_bot_principal_cannot_call_a_service_route() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "not-a-service").await;
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/services/herdr-upgrade/resume/{} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, bot.id, bot.hook_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403") && response.contains("service_only"), "{response}");
}

#[tokio::test]
async fn a_child_credential_is_not_rotated_because_its_pane_uses_the_parents() {
    let e = crate::testing::env().await;
    let child = distinct_bot(&e, "rotate-child").await;
    sqlx::query("UPDATE bots SET managed_by='child' WHERE id=?").bind(&child.id).execute(&e.app.db).await.unwrap();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            child.id, e.app.ui_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 409") && response.contains("child_uses_parent_credential"), "{response}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&child.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(current, child.hook_token);
}

#[tokio::test]
async fn a_bot_principal_cannot_rotate_a_credential() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "bot-cannot-rotate").await;
    let old = bot.hook_token.clone();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, bot.id, bot.hook_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"), "only User may rotate bot credentials: {response}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(current, old);
}

#[tokio::test]
async fn relay_from_cannot_override_the_authenticated_bot_identity() {
    let e = crate::testing::env().await;
    let sender = distinct_bot(&e, "authenticated-sender").await;
    let claimed = distinct_bot(&e, "forged-relay").await;
    let target = distinct_bot(&e, "relay-target").await;
    let body = serde_json::to_vec(&json!({"text": "must-not-send", "relay_from": claimed.id})).unwrap();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            target.id,
            sender.id,
            sender.hook_token,
            body.len(),
            String::from_utf8(body).unwrap(),
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE content='must-not-send'")
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(count, 0, "mismatched relay identity must be rejected before message creation");
}

/// Defense in depth: even if two bots ever shared a token, a Bot principal still may only
/// claim its own id as `relay_from`. (`testing::claude_bot` gives both bots the same 'tok'.)
#[tokio::test]
async fn relay_from_is_compared_by_id_even_when_tokens_collide() {
    let e = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&e.app, &e.project_id, "collide-sender").await;
    let claimed = crate::testing::claude_bot(&e.app, &e.project_id, "collide-claimed").await;
    let target = crate::testing::claude_bot(&e.app, &e.project_id, "collide-target").await;
    assert_eq!(sender.hook_token, claimed.hook_token, "precondition: the helper hands out one shared token");
    let body = serde_json::to_vec(&json!({"text": "collide-must-not-send", "relay_from": claimed.id})).unwrap();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            target.id,
            sender.id,
            sender.hook_token,
            body.len(),
            String::from_utf8(body).unwrap(),
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 403") && response.contains("relay_from_mismatch"), "{response}");
}

/// #410 對抗式審查：HTTP 層（含認證中介層）上所有「沒有證明卻想讓 `relay_from` 記上別顆 bot」的送法，
/// 一律被擋、什麼都不寫；擋掉的錯誤碼／內容不能洩漏某顆 bot 存不存在。
#[tokio::test]
async fn no_header_trick_gets_an_unproven_relay_from_stored() {
    let e = crate::testing::env().await;
    let sender = distinct_bot(&e, "adv-sender").await;
    let victim = distinct_bot(&e, "adv-victim").await;
    let target = distinct_bot(&e, "adv-target").await;
    let gone = distinct_bot(&e, "adv-gone").await;
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&gone.id).execute(&e.app.db).await.unwrap();
    let ui = e.app.ui_token.clone();
    let send = |marker: &str, relay: &str, headers: Vec<(String, String)>| {
        let app = e.app.clone();
        let (target, marker, relay) = (target.id.clone(), marker.to_string(), relay.to_string());
        async move {
            let body = serde_json::to_vec(&json!({"text": marker, "client_request_id": marker, "relay_from": relay})).unwrap();
            let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
            let resp = raw(
                app.clone(),
                format!(
                    "POST /api/bots/{target}/prompt HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    String::from_utf8(body).unwrap()
                ),
            )
            .await;
            let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE content=?").bind(&marker).fetch_one(&app.db).await.unwrap();
            assert_eq!(stored, 0, "{marker}: nothing may be written\n{resp}");
            resp
        }
    };
    let h = |k: &str, v: &str| (k.to_string(), v.to_string());
    let status = |r: &str| r.lines().next().unwrap_or_default().to_string();

    // UI token（User）自稱 bot、沒有任何 bot 憑證。
    let r = send("adv-ui-only", &victim.id, vec![h("X-AM-Token", &ui)]).await;
    assert!(status(&r).contains("403") && r.contains("relay_from_token_required"), "{r}");
    // UI token 加上受害者的 token（混合憑證不得降級／升級）：401。
    for (name, headers) in [
        ("adv-ui-plus-token", vec![h("X-AM-Token", &ui), h("X-AM-Bot-Token", &victim.hook_token)]),
        ("adv-ui-plus-empty-token", vec![h("X-AM-Token", &ui), h("X-AM-Bot-Token", "")]),
        ("adv-token-only", vec![h("X-AM-Bot-Token", &victim.hook_token)]),
        ("adv-empty-token", vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", "")]),
        ("adv-victim-id-sender-token", vec![h("X-AM-Bot-Id", &victim.id), h("X-AM-Bot-Token", &sender.hook_token)]),
        ("adv-lowercased-id", vec![h("X-AM-Bot-Id", &sender.id.to_lowercase()), h("X-AM-Bot-Token", &sender.hook_token)]),
        ("adv-deleted-bot", vec![h("X-AM-Bot-Id", &gone.id), h("X-AM-Bot-Token", &gone.hook_token)]),
    ] {
        let r = send(name, &victim.id, headers).await;
        assert!(status(&r).contains("401"), "{name}: {r}");
    }
    // 自己的憑證、claim 別顆：不管大小寫的 header 名、前後空白、重複 header，都是 403 mismatch。
    let mut bodies = Vec::new();
    for (name, relay, headers) in [
        ("adv-plain", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token)]),
        ("adv-lower-names", victim.id.clone(), vec![h("x-am-bot-id", &sender.id), h("x-am-bot-token", &sender.hook_token)]),
        ("adv-padded", format!("  {}  ", victim.id), vec![h("X-AM-Bot-Id", &format!(" {} ", sender.id)), h("X-AM-Bot-Token", &format!(" {} ", sender.hook_token))]),
        ("adv-dup-id", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Id", &victim.id), h("X-AM-Bot-Token", &sender.hook_token)]),
        ("adv-dup-token", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token), h("X-AM-Bot-Token", &victim.hook_token)]),
        ("adv-dup-token-rev", victim.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &victim.hook_token), h("X-AM-Bot-Token", &sender.hook_token)]),
        ("adv-deleted-claim", gone.id.clone(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token)]),
        ("adv-nosuch-claim", "01MNOSUCHBOTIDATALL0000000".to_string(), vec![h("X-AM-Bot-Id", &sender.id), h("X-AM-Bot-Token", &sender.hook_token)]),
    ] {
        let r = send(name, &relay, headers).await;
        let ok_status = if name == "adv-dup-token-rev" { "401" } else { "403" };
        assert!(status(&r).contains(ok_status), "{name}: {r}");
        if ok_status == "403" {
            assert!(r.contains("relay_from_mismatch"), "{name}: {r}");
            bodies.push(r.split("\r\n\r\n").nth(1).unwrap_or_default().to_string());
        }
    }
    // 活的、已刪的、根本不存在的 claim，回的內容完全一樣（不能當 bot id 神諭）。
    assert!(bodies.windows(2).all(|w| w[0] == w[1]), "{bodies:?}");
    // 自己轉給自己：400，同樣不寫。
    let r = send(
        "adv-self",
        &target.id,
        vec![h("X-AM-Bot-Id", &target.id), h("X-AM-Bot-Token", &target.hook_token)],
    )
    .await;
    assert!(status(&r).contains("400") && r.contains("relay_self"), "{r}");
}

#[tokio::test]
async fn rotating_a_bot_credential_invalidates_the_old_token_without_returning_the_new_one() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "rotate-auth").await;
    let old = bot.hook_token.clone();
    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, e.app.ui_token
        ),
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(!response.contains(&old), "response must never disclose either credential");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(current, old, "old bot credential is invalidated immediately");
    let stale = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &old)]).await;
    assert!(stale.starts_with("HTTP/1.1 401"), "the old proof must already be unusable: {stale}");
}

#[tokio::test]
async fn rotating_a_default_session_bot_is_refused_before_changing_or_stopping_it() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "rotate-default-session").await;
    sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let run_id = crate::testing::fake_run(&e.app, &bot.id).await;
    sqlx::query("UPDATE runs SET herdr_session='default' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();

    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, e.app.ui_token
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 409") && response.contains("\"reason\":\"default_session\""), "{response}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(current, bot.hook_token, "default-session credentials are not rotated");
    assert_eq!(db::run(&e.app.db, &run_id).await.unwrap().unwrap().state, "running", "the imported pane is untouched");
    let intents: i64 = sqlx::query_scalar("SELECT count(*) FROM intents WHERE kind='restart' AND subject_id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(intents, 0, "a default-session rotation cannot leave a restart intent");
    assert!(e.herdr.calls_to("agent.send_keys").is_empty(), "the user's pane must not receive ctrl+c");
}

#[tokio::test]
async fn resuming_credential_rotation_refuses_default_session_before_sending_keys() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "resume-rotate-default-session").await;
    sqlx::query("UPDATE bots SET herdr_session='default' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let run_id = crate::testing::fake_run(&e.app, &bot.id).await;
    sqlx::query("UPDATE runs SET herdr_session='default' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();

    let lock = e.app.bot_lock(&bot.id).await;
    let _guard = lock.lock().await;
    let result = lifecycle::resume_credential_rotation_locked(&e.app, &bot.id, lifecycle::StartOpts::default(), &run_id).await;

    assert!(matches!(result, Err(LcError::Conflict(ref body)) if body["reason"] == "default_session"));
    assert_eq!(db::run(&e.app.db, &run_id).await.unwrap().unwrap().state, "running", "recovery must leave the imported run active");
    assert!(e.herdr.calls_to("agent.send_keys").is_empty(), "the user's pane must not receive ctrl+c");
}

#[tokio::test]
async fn an_unreadable_intents_table_does_not_commit_credential_rotation() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "rotate-read-failure").await;
    crate::testing::fake_run(&e.app, &bot.id).await;
    let old = bot.hook_token.clone();
    crate::testing::make_table_unreadable(&e.app, "intents").await;

    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, e.app.ui_token
        ),
    )
    .await;

    assert!(!response.starts_with("HTTP/1.1 200"), "an unreadable intent table must fail before rotation: {response}");
    crate::testing::make_table_readable(&e.app, "intents").await;
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(current, old, "failed preflight must leave the old credential valid");
    assert!(!response.contains("\"credential_rotated\":true"), "a pre-commit failure must not claim the credential changed: {response}");
    let old_proof = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &old)]).await;
    assert!(old_proof.starts_with("HTTP/1.1 200"), "old credential must still authenticate: {old_proof}");
}

#[tokio::test]
async fn a_postcommit_restart_failure_reports_rotation_and_keeps_recovery_intent() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "rotate-restart-failure").await;
    crate::lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    sqlx::query("UPDATE bots SET identity='no-such-identity' WHERE id=?").bind(&bot.id).execute(&e.app.db).await.unwrap();
    let old = bot.hook_token.clone();

    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, e.app.ui_token
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 409"), "restart failure must be reported after rotation: {response}");
    assert!(response.contains("\"credential_rotated\":true"), "post-commit errors must say the old credential is invalid: {response}");
    assert!(response.contains("restart_pending"), "the caller needs to know recovery remains scheduled: {response}");
    assert!(!response.contains(&old), "the response must not disclose the old proof");
    let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind='restart' AND subject_id=? ORDER BY created_at DESC LIMIT 1")
        .bind(&bot.id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert!(payload.contains("\"credential_rotation\":true"), "the restart intent must preserve its rotation-specific recovery rule: {payload}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(current, old, "the token update is durable even though the restart must be retried");
}

#[tokio::test]
async fn rotating_a_live_bot_restarts_it_and_completes_its_durable_intent() {
    let e = crate::testing::env().await;
    let bot = distinct_bot(&e, "rotate-live").await;
    let old_run = crate::lifecycle::start_bot(&e.app, &bot.id).await.unwrap();
    let old_token = bot.hook_token.clone();

    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            bot.id, e.app.ui_token
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200"), "live credential rotation should restart successfully: {response}");
    assert!(response.contains("\"credential_rotated\":true") && response.contains("\"restarted\":true"), "success response must report rotation and restart: {response}");
    assert!(!response.contains(&old_token), "the response must not disclose the old credential");
    let new_token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&bot.id).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(new_token, old_token, "the bot token is rotated");
    let stale = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &old_token)]).await;
    assert!(stale.starts_with("HTTP/1.1 401"), "old proof is invalid after commit: {stale}");
    let fresh = state(e.app.clone(), &[("X-AM-Bot-Id", &bot.id), ("X-AM-Bot-Token", &new_token)]).await;
    assert!(fresh.starts_with("HTTP/1.1 200"), "new proof is available to the restarted run: {fresh}");
    assert_ne!(db::active_run(&e.app.db, &bot.id).await.unwrap().unwrap().id, old_run, "the pane was restarted");
    let payload: String = sqlx::query_scalar("SELECT payload_json FROM intents WHERE kind='restart' AND subject_id=? ORDER BY created_at DESC LIMIT 1")
        .bind(&bot.id)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert!(payload.contains("\"credential_rotation\":true"), "completed restart keeps its rotation semantics in the intent");
}

#[tokio::test]
async fn rotation_refuses_a_live_grandchild_that_inherited_the_parent_credential() {
    let e = crate::testing::env().await;
    let parent = distinct_bot(&e, "rotate-parent-with-live-descendants").await;
    let child = distinct_bot(&e, "rotate-child-with-live-descendant").await;
    let grandchild = distinct_bot(&e, "rotate-grandchild").await;
    for (id, parent_id) in [(&child.id, &parent.id), (&grandchild.id, &child.id)] {
        sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=?, hook_token=? WHERE id=?")
            .bind(parent_id)
            .bind(&parent.hook_token)
            .bind(id)
            .execute(&e.app.db)
            .await
            .unwrap();
    }
    let parent_run = crate::testing::fake_run(&e.app, &parent.id).await;
    let child_run = crate::testing::fake_run(&e.app, &child.id).await;
    let grandchild_run = crate::testing::fake_run(&e.app, &grandchild.id).await;

    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            parent.id, e.app.ui_token
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 409"), "rotation must be refused while an inherited credential is live: {response}");
    assert!(response.contains("live_children_use_credential"), "the conflict must identify the credential dependency: {response}");
    assert!(response.contains(&child.id) && response.contains(&grandchild.id), "the live descendant set must include both levels: {response}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(current, parent.hook_token, "a refused rotation must leave the old token valid");
    let old_proof = state(e.app.clone(), &[("X-AM-Bot-Id", &parent.id), ("X-AM-Bot-Token", &parent.hook_token)]).await;
    assert!(old_proof.starts_with("HTTP/1.1 200"), "the refusal must keep the inherited proof usable: {old_proof}");
    for (bot_id, run_id) in [(&parent.id, &parent_run), (&child.id, &child_run), (&grandchild.id, &grandchild_run)] {
        assert_eq!(crate::db::active_run(&e.app.db, bot_id).await.unwrap().unwrap().id, *run_id, "refusal must not restart or touch any dependent pane");
    }
}

#[tokio::test]
async fn stopped_or_deleted_children_without_live_runs_do_not_block_parent_rotation() {
    let e = crate::testing::env().await;
    let parent = distinct_bot(&e, "rotate-parent-with-stopped-child").await;
    let stopped = distinct_bot(&e, "rotate-stopped-child").await;
    let deleted = distinct_bot(&e, "rotate-deleted-child").await;
    for child in [&stopped, &deleted] {
        sqlx::query("UPDATE bots SET managed_by='child', parent_bot_id=?, hook_token=? WHERE id=?")
            .bind(&parent.id)
            .bind(&parent.hook_token)
            .bind(&child.id)
            .execute(&e.app.db)
            .await
            .unwrap();
    }
    let stopped_run = crate::testing::fake_run(&e.app, &stopped.id).await;
    sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(crate::db::now())
        .bind(&stopped_run)
        .execute(&e.app.db)
        .await
        .unwrap();
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(crate::db::now()).bind(&deleted.id).execute(&e.app.db).await.unwrap();

    let response = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            parent.id, e.app.ui_token
        ),
    )
    .await;

    assert!(response.starts_with("HTTP/1.1 200"), "retired descendants do not hold the credential: {response}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(current, parent.hook_token, "the parent can rotate once all dependent panes are gone");
}

#[tokio::test]
async fn a_child_spawn_permit_blocks_rotation_and_the_registered_pane_keeps_the_old_proof() {
    let e = crate::testing::env().await;
    let parent = spawn_ready_bot(&e, "rotate-parent-with-spawn-permit").await;
    let old = parent.hook_token.clone();
    let begin = spawn_begin(e.app.clone(), &parent.id, &old).await;
    assert!(begin.starts_with("HTTP/1.1 200"), "the parent may reserve before opening a child pane: {begin}");
    let permit = response_json(&begin)["permit_id"].as_str().unwrap().to_string();

    let rotate = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            parent.id, e.app.ui_token
        ),
    )
    .await;
    assert!(rotate.starts_with("HTTP/1.1 409") && rotate.contains("child_spawn_in_progress"), "an in-flight external spawn must prevent rotation: {rotate}");
    assert_eq!(db::bot(&e.app.db, &parent.id).await.unwrap().unwrap().hook_token, old, "refusal keeps the old proof valid");

    e.herdr.set_agent("spawned-child", "w1:p-unadopted-child", false);
    e.herdr.live_pane("w1:p-unadopted-child", crate::testing::LivePane { rows: Some(40), ..Default::default() });
    let finish = spawn_finish(e.app.clone(), &parent.id, &old, &permit, "w1:p-unadopted-child").await;
    assert!(finish.starts_with("HTTP/1.1 200"), "successful pane creation must be registered before releasing its permit: {finish}");
    let rotate = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            parent.id, e.app.ui_token
        ),
    )
    .await;
    assert!(rotate.starts_with("HTTP/1.1 409") && rotate.contains("w1:p-unadopted-child"), "an unadopted sibling pane still carries the old credential: {rotate}");
    assert_eq!(db::bot(&e.app.db, &parent.id).await.unwrap().unwrap().hook_token, old, "the sibling pane keeps a valid proof until closed");
}

#[tokio::test]
async fn concurrent_replay_of_a_spawn_permit_registers_only_one_pane() {
    let e = crate::testing::env().await;
    let parent = spawn_ready_bot(&e, "concurrent-spawn-permit-replay").await;
    let begin = spawn_begin(e.app.clone(), &parent.id, &parent.hook_token).await;
    assert!(begin.starts_with("HTTP/1.1 200"), "{begin}");
    let permit = response_json(&begin)["permit_id"].as_str().unwrap().to_string();
    e.herdr.set_agent("replay-a", "w1:p-replay-a", false);
    e.herdr.live_pane("w1:p-replay-a", crate::testing::LivePane { rows: Some(40), ..Default::default() });
    e.herdr.set_agent("replay-b", "w1:p-replay-b", false);
    e.herdr.live_pane("w1:p-replay-b", crate::testing::LivePane { rows: Some(40), ..Default::default() });

    // Both requests pass the permit check before either finishes its async pane registration.
    let (a, b) = tokio::join!(
        spawn_finish(e.app.clone(), &parent.id, &parent.hook_token, &permit, "w1:p-replay-a"),
        spawn_finish(e.app.clone(), &parent.id, &parent.hook_token, &permit, "w1:p-replay-b"),
    );
    let success_count = [a.as_str(), b.as_str()].iter().filter(|r| r.starts_with("HTTP/1.1 200")).count();
    assert_eq!(success_count, 1, "exactly one replay may consume the permit: a={a}; b={b}");
    let registered: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM panes WHERE host='local' AND owner_bot_id=? AND pane_id IN ('w1:p-replay-a','w1:p-replay-b')",
    )
    .bind(&parent.id)
    .fetch_one(&e.app.db)
    .await
    .unwrap();
    assert_eq!(registered, 1, "losing replay must not leave a second credential-bearing pane registered");
}

async fn spawn_claim(app: Arc<App>, bot_id: &str, token: &str, pane_id: &str) -> String {
    let body = format!("bot_id={bot_id}&pane_id={pane_id}");
    raw(
        app,
        format!(
            "POST /relay/spawn/claim HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {token}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await
}

/// issue #1790：shim 開完子 agent 登記「這顆 pane 是我開的」；bot 只能替自己登記（token 要屬於 body 的 `bot_id`）。
#[tokio::test]
async fn a_spawn_claim_registers_the_pane_for_the_authenticated_bot_only() {
    let e = crate::testing::env().await;
    let bot_a = distinct_bot(&e, "claim-a").await;
    let bot_b = distinct_bot(&e, "claim-b").await;
    let owner = |pane: &str| {
        let app = e.app.clone();
        let pane = pane.to_string();
        async move {
            sqlx::query_scalar::<_, String>("SELECT bot_id FROM spawn_claims WHERE pane_id = ?").bind(pane).fetch_optional(&app.db).await.unwrap()
        }
    };

    let forged = spawn_claim(e.app.clone(), &bot_b.id, &bot_a.hook_token, "w1:p-forged").await;
    assert!(forged.starts_with("HTTP/1.1 401"), "Bot A 不能替 Bot B 登記：{forged}");
    assert_eq!(owner("w1:p-forged").await, None, "被拒絕就不留任何登記");

    let missing = spawn_claim(e.app.clone(), &bot_a.id, &bot_a.hook_token, "").await;
    assert!(missing.starts_with("HTTP/1.1 400"), "{missing}");

    let ok = spawn_claim(e.app.clone(), &bot_a.id, &bot_a.hook_token, "w1:p-mine").await;
    assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("\"claimed\":true"), "{ok}");
    assert_eq!(owner("w1:p-mine").await.as_deref(), Some(bot_a.id.as_str()));
}

#[tokio::test]
async fn spawn_credentials_cannot_be_used_for_another_bots_identity() {
    let e = crate::testing::env().await;
    let bot_a = spawn_ready_bot(&e, "spawn-auth-a").await;
    let bot_b = distinct_bot(&e, "spawn-auth-b").await;

    let forged_begin = spawn_begin(e.app.clone(), &bot_b.id, &bot_a.hook_token).await;
    assert!(forged_begin.starts_with("HTTP/1.1 401"), "Bot A cannot reserve credentials as Bot B: {forged_begin}");

    let valid_begin = spawn_begin(e.app.clone(), &bot_a.id, &bot_a.hook_token).await;
    assert!(valid_begin.starts_with("HTTP/1.1 200"), "{valid_begin}");
    let permit = response_json(&valid_begin)["permit_id"].as_str().unwrap().to_string();
    let forged_finish = spawn_finish(e.app.clone(), &bot_b.id, &bot_a.hook_token, &permit, "w1:p-cross-bot").await;
    assert!(forged_finish.starts_with("HTTP/1.1 401"), "Bot A cannot register a credential-bearing pane for Bot B: {forged_finish}");

    let body = format!("bot_id={}&permit_id={permit}", bot_b.id);
    let forged_abort = raw(
        e.app.clone(),
        format!(
            "POST /relay/spawn/abort HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            bot_a.hook_token,
            body.len()
        ),
    )
    .await;
    assert!(forged_abort.starts_with("HTTP/1.1 401"), "Bot A cannot consume Bot B's permit: {forged_abort}");

    let release = raw(
        e.app.clone(),
        format!(
            "POST /relay/spawn/abort HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            bot_a.hook_token,
            format!("bot_id={}&permit_id={permit}", bot_a.id).len(),
            format!("bot_id={}&permit_id={permit}", bot_a.id)
        ),
    )
    .await;
    assert!(release.starts_with("HTTP/1.1 200") && release.contains("\"released\":true"), "Bot A can clean up its own permit: {release}");

    let registered: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes WHERE pane_id='w1:p-cross-bot'")
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(registered, 0, "denied cross-bot finish has no side effect");
}

#[tokio::test]
async fn a_deleted_parent_cannot_finish_or_start_with_its_inherited_proof() {
    let e = crate::testing::env().await;
    let parent = spawn_ready_bot(&e, "spawn-deleted-parent").await;
    let permit_response = spawn_begin(e.app.clone(), &parent.id, &parent.hook_token).await;
    assert!(permit_response.starts_with("HTTP/1.1 200"), "{permit_response}");
    let permit = response_json(&permit_response)["permit_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&parent.id).execute(&e.app.db).await.unwrap();

    let begin = spawn_begin(e.app.clone(), &parent.id, &parent.hook_token).await;
    assert!(begin.starts_with("HTTP/1.1 401"), "deleted parent's token is revoked for future children: {begin}");
    let finish = spawn_finish(e.app.clone(), &parent.id, &parent.hook_token, &permit, "w1:p-after-delete").await;
    assert!(finish.starts_with("HTTP/1.1 401"), "an in-flight child cannot complete with a deleted parent's token: {finish}");
    let registered: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes WHERE pane_id='w1:p-after-delete'")
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(registered, 0, "deleted parent's in-flight finish has no registration side effect");
}

/// #664：shim 回報 herdr 沒開出 pane 時，abort 放開 permit，輪替不再 409。
#[tokio::test]
async fn aborting_a_spawn_permit_lets_rotation_proceed() {
    let e = crate::testing::env().await;
    let parent = spawn_ready_bot(&e, "rotate-after-abort").await;
    let old = parent.hook_token.clone();
    let begin = spawn_begin(e.app.clone(), &parent.id, &old).await;
    assert!(begin.starts_with("HTTP/1.1 200"), "{begin}");
    let permit = response_json(&begin)["permit_id"].as_str().unwrap().to_string();
    let body = format!("bot_id={}&permit_id={permit}", parent.id);
    let abort = raw(
        e.app.clone(),
        format!(
            "POST /relay/spawn/abort HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Bot-Token: {old}\r\nContent-Type: application/x-www-form-urlencoded\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
    .await;
    assert!(abort.starts_with("HTTP/1.1 200") && abort.contains("\"released\":true"), "{abort}");
    let rotate = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            parent.id, e.app.ui_token
        ),
    )
    .await;
    assert!(rotate.starts_with("HTTP/1.1 200"), "abort 之後輪替不再被 child_spawn_in_progress 擋住：{rotate}");
}

/// #831：輪替一路讀完（bot、子孫、pane）、還沒寫新 token 的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時
/// 升級寫鎖直接 517，輪替被拒；寫鎖從讀之前就拿著，插進來的那一筆等，輪替照樣成功。
#[tokio::test]
async fn an_unrelated_writer_between_the_rotation_reads_and_the_token_write_does_not_refuse_it() {
    let e = crate::testing::env().await;
    let parent = distinct_bot(&e, "rotate-831").await;
    let old = parent.hook_token.clone();
    let other = crate::testing::arm_app_foreign_writer(&e.app, "credential_rotation_after_descendant_query", &parent.id);
    let rotate = raw(
        e.app.clone(),
        format!(
            "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
            parent.id, e.app.ui_token
        ),
    )
    .await;
    assert!(rotate.starts_with("HTTP/1.1 200"), "an unrelated writer must not refuse the rotation: {rotate}");
    assert_eq!(*other.lock().unwrap(), Some(false), "the rotation holds the write lock from its first read on; the other writer waits");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(current, old);
}

#[tokio::test]
async fn the_spawn_gate_refuses_a_child_created_after_the_descendant_snapshot() {
    let e = crate::testing::env().await;
    let parent = spawn_ready_bot(&e, "rotate-parent-spawn-race").await;
    let old = parent.hook_token.clone();
    let (send, receive) = tokio::sync::oneshot::channel();
    let app = e.app.clone();
    let bot_id = parent.id.clone();
    let token = old.clone();
    crate::lifecycle::race_point::arm("credential_rotation_after_descendant_query", &parent.id, move || async move {
        // This is the shim's pre-herdr request. A 409 means it will not invoke herdr and no
        // inherited-credential pane can appear between the descendant snapshot and UPDATE.
        let response = spawn_begin(app, &bot_id, &token).await;
        let _ = send.send(response);
    });
    let app = e.app.clone();
    let bot_id = parent.id.clone();
    let ui_token = app.ui_token.clone();
    let rotation = tokio::spawn(async move {
        raw(
            app,
            format!(
                "POST /api/bots/{}/credential/rotate HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nConnection: close\r\nContent-Length: 0\r\n\r\n",
                bot_id, ui_token
            ),
        )
        .await
    });
    let spawn_response = receive.await.unwrap();
    let rotation_response = rotation.await.unwrap();

    assert!(spawn_response.starts_with("HTTP/1.1 409") && spawn_response.contains("credential_rotation_pending"), "shim must refuse the spawn while the fence is visible: {spawn_response}");
    assert!(rotation_response.starts_with("HTTP/1.1 200"), "with no dependent pane, rotation can commit after the fence check: {rotation_response}");
    let current: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&parent.id).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(current, old, "only the fenced rotation may invalidate the old proof");
    let stale_spawn = spawn_begin(e.app.clone(), &parent.id, &old).await;
    assert!(stale_spawn.starts_with("HTTP/1.1 401"), "a delayed shim call cannot spawn with the revoked proof: {stale_spawn}");
}

#[tokio::test]
async fn a_running_child_can_spawn_after_its_parent_run_stops_and_session_changes_still_fence() {
    let e = crate::testing::env().await;
    let parent = spawn_ready_bot(&e, "stopped-parent-live-child").await;
    let run = db::active_run(&e.app.db, &parent.id).await.unwrap().unwrap();
    e.herdr.set_agent("surviving-child", "w1:p-surviving-child", false);
    sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?")
        .bind(db::now())
        .bind(&run.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    assert!(db::active_run(&e.app.db, &parent.id).await.unwrap().is_none(), "the parent run is stopped while the child pane remains live");

    // The real-shim pane split above exercises this inherited proof end to end. Here the
    // route-level calls also verify that a stopped parent run does not lose session fencing.
    let begin = spawn_begin(e.app.clone(), &parent.id, &parent.hook_token).await;
    assert!(begin.starts_with("HTTP/1.1 200"), "a surviving child can reserve a pane on the captured host/session: {begin}");
    let permit = response_json(&begin)["permit_id"].as_str().unwrap().to_string();
    let shell_pane = "w1:p-after-parent-stop";
    e.herdr.live_pane(shell_pane, crate::testing::LivePane { rows: Some(40), ..Default::default() });
    let finish = spawn_finish(e.app.clone(), &parent.id, &parent.hook_token, &permit, shell_pane).await;
    assert!(finish.starts_with("HTTP/1.1 200"), "the spawned shell pane is registered before releasing the permit: {finish}");
    let owner: String = sqlx::query_scalar("SELECT owner_bot_id FROM panes WHERE pane_id=?")
        .bind(shell_pane)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(owner, parent.id);

    let stale_begin = spawn_begin(e.app.clone(), &parent.id, &parent.hook_token).await;
    assert!(stale_begin.starts_with("HTTP/1.1 200"), "the unchanged host/session remains eligible: {stale_begin}");
    let stale_permit = response_json(&stale_begin)["permit_id"].as_str().unwrap().to_string();
    sqlx::query("UPDATE bots SET herdr_session='replacement-session' WHERE id=?")
        .bind(&parent.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    let stale_pane = "w1:p-old-session";
    e.herdr.live_pane(stale_pane, crate::testing::LivePane { rows: Some(40), ..Default::default() });
    let stale_finish = spawn_finish(e.app.clone(), &parent.id, &parent.hook_token, &stale_permit, stale_pane).await;
    assert!(stale_finish.starts_with("HTTP/1.1 409") && stale_finish.contains("spawn_session_superseded"), "a permit cannot finish after its session changes: {stale_finish}");
    let stale_registered: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM panes WHERE pane_id=?")
        .bind(stale_pane)
        .fetch_one(&e.app.db)
        .await
        .unwrap();
    assert_eq!(stale_registered, 0, "a pane from the retired session must not be registered");
}
