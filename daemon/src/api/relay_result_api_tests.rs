use super::*;

#[tokio::test]
async fn explicit_bot_id_resolves_across_hosts_without_running_target() {
    let env = crate::testing::env().await;
    let a = crate::testing::claude_bot(&env.app, &env.project_id, "resolve-a").await;
    let project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at) VALUES(?,?,'remote','mac_builder',?)")
        .bind(&project).bind(format!("/remote/{project}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let b = crate::testing::claude_bot(&env.app, &project, "resolve-b").await;
    let Json(result) = resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(a.id)), Query(RelayResolveQuery { target: format!("bot:{}", b.id) })).await.unwrap();
    assert_eq!(result["to_bot_id"], b.id);
    assert!(result["to_run_id"].is_null());
}

#[tokio::test]
async fn stopped_project_member_names_resolve_and_duplicates_are_ambiguous() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "name-sender").await;
    let first = crate::testing::claude_bot(&env.app, &env.project_id, "api").await;
    let resolve = || resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(sender.id.clone())), Query(RelayResolveQuery { target: "api".into() }));
    let Json(result) = resolve().await.unwrap();
    assert_eq!(result["to_bot_id"], first.id);
    assert!(result["to_run_id"].is_null());
    let second = crate::testing::claude_bot(&env.app, &env.project_id, "api-two").await;
    // Exercise ambiguity even if a legacy database contains duplicate display names.
    sqlx::query("DROP INDEX bots_name_project_live").execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET name='api' WHERE id=?").bind(&second.id).execute(&env.app.db).await.unwrap();
    let LcError::Conflict(body) = resolve().await.unwrap_err() else { panic!("expected ambiguity") };
    let candidates = body["candidates"].as_array().unwrap();
    for id in [&first.id, &second.id] {
        assert!(candidates.iter().any(|c| c["name"] == "api" && c["bot_id"] == *id));
    }
}

async fn insert_remote_run(app: &Arc<App>, name: &str, agent_name: &str, pane_id: &str) -> (String, String) {
    let project = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,created_at) VALUES(?,?,'remote','mac_builder',?)")
        .bind(&project).bind(format!("/remote/{project}")).bind(db::now()).execute(&app.db).await.unwrap();
    let bot = crate::testing::claude_bot(app, &project, name).await;
    let run = db::ulid();
    sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,pane_id,agent_name,started_at) VALUES(?,?,'running','idle',?,?,?)")
        .bind(&run).bind(&bot.id).bind(pane_id).bind(agent_name).bind(db::now()).execute(&app.db).await.unwrap();
    (bot.id, run)
}

#[tokio::test]
async fn agent_name_resolves_across_hosts() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "xhost-sender").await;
    let (remote_bot, remote_run) = insert_remote_run(&env.app, "xhost-remote", "agents-manager-xhost-rly", "remote-pane").await;
    let Json(result) = resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(sender.id.clone())), Query(RelayResolveQuery { target: "agents-manager-xhost-rly".into() })).await.unwrap();
    assert_eq!(result["to_bot_id"], remote_bot);
    assert_eq!(result["to_run_id"], remote_run);
    assert_eq!(result["to_agent"], "agents-manager-xhost-rly");
}

#[tokio::test]
async fn bot_name_and_pane_id_do_not_resolve_across_hosts() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "xhost-clash-sender").await;
    let (_remote_bot, _run) = insert_remote_run(&env.app, "xhost-clash-name", "xhost-clash-agent", "pane-clash").await;
    for target in ["xhost-clash-name", "pane-clash"] {
        let error = resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(sender.id.clone())), Query(RelayResolveQuery { target: target.into() })).await.unwrap_err();
        assert_eq!(error.into_response().status(), StatusCode::NOT_FOUND, "{target} must not match a remote-host bot");
    }
}

#[tokio::test]
async fn policy_blocked_candidate_resolves_to_403_with_reason() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "blocked-sender").await;
    let (remote_bot, _) = insert_remote_run(&env.app, "blocked-remote", "agents-manager-blocked-rly", "pane-blocked").await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=(SELECT project_id FROM bots WHERE id=?)").bind(&remote_bot).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&remote_bot).execute(&env.app.db).await.unwrap();
    let error = resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(sender.id.clone())), Query(RelayResolveQuery { target: "agents-manager-blocked-rly".into() })).await.unwrap_err();
    let LcError::Forbidden(body) = error else { panic!("expected 403") };
    assert_eq!(body["reason"], "expert_cross_project");
    let missing = resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(sender.id)), Query(RelayResolveQuery { target: "no-such-agent".into() })).await.unwrap_err();
    assert_eq!(missing.into_response().status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_name_on_two_hosts_is_ambiguous() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "xhost-amb-sender").await;
    let (first, _) = insert_remote_run(&env.app, "xhost-amb-one", "agents-manager-xhost-amb", "pane-amb-1").await;
    let (second, _) = insert_remote_run(&env.app, "xhost-amb-two", "agents-manager-xhost-amb", "pane-amb-2").await;
    let error = resolve_relay_target(State(env.app.clone()), Extension(RequestPrincipal::Bot(sender.id.clone())), Query(RelayResolveQuery { target: "agents-manager-xhost-amb".into() })).await.unwrap_err();
    let LcError::Conflict(body) = error else { panic!("expected ambiguous conflict") };
    assert_eq!(body["error"], "ambiguous_relay_target");
    let candidates = body["candidates"].as_array().unwrap();
    assert!(candidates.iter().any(|c| c["bot_id"] == first) && candidates.iter().any(|c| c["bot_id"] == second));
}

#[tokio::test]
async fn result_request_requires_bot_principal() {
    let env = crate::testing::env().await;
    let b = crate::testing::claude_bot(&env.app, &env.project_id, "requests-user-target").await;
    let body: PromptIn = serde_json::from_value(json!({"text":"hello","expect_result":true})).unwrap();
    let error = prompt_bot(State(env.app.clone()), Path(b.id), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await.unwrap_err();
    assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_requests").fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn relay_request_query_only_shows_callers_participation() {
    let env = crate::testing::env().await;
    let a = crate::testing::claude_bot(&env.app, &env.project_id, "requests-a").await;
    let b = crate::testing::claude_bot(&env.app, &env.project_id, "requests-b").await;
    let c = crate::testing::claude_bot(&env.app, &env.project_id, "requests-c").await;
    for (from,to) in [(&a.id,&b.id),(&b.id,&c.id)] {
        sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,state,created_at) VALUES(?,?,?,'key','text','[]','pending',?)")
            .bind(db::ulid()).bind(from).bind(to).bind(db::now()).execute(&env.app.db).await.unwrap();
    }
    let Json(result) = list_relay_requests(State(env.app.clone()), Extension(RequestPrincipal::Bot(a.id.clone())), Query(RelayRequestsQuery { from: None, to: None, open: Some("1".into()) })).await.unwrap();
    let rows = result["requests"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["from_bot_id"], a.id);
    assert_eq!(rows[0]["to_bot_id"], b.id);
}

#[tokio::test]
async fn replay_notified_request_after_new_conversation_does_not_deliver_again() {
    let env = crate::testing::env().await;
    let a = crate::testing::claude_bot(&env.app, &env.project_id, "replay-source").await;
    let b = crate::testing::claude_bot(&env.app, &env.project_id, "replay-target").await;
    let conv = db::conversation_id(&env.app.db, &b.id).await.unwrap();
    let turn = db::ulid();
    sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,created_at,completed_at) VALUES(?,?,'web','completed','ok',?,?)").bind(&turn).bind(&conv).bind(db::now()).bind(db::now()).execute(&env.app.db).await.unwrap();
    let request = db::ulid();
    sqlx::query("INSERT INTO relay_requests(id,from_bot_id,to_bot_id,client_request_id,prompt_text,attachments_json,to_turn_id,state,created_at) VALUES(?,?,?,'stable','please answer','[]',?,'notified',?)").bind(&request).bind(&a.id).bind(&b.id).bind(&turn).bind(db::now()).execute(&env.app.db).await.unwrap();
    // Replace the target conversation, reproducing the boundary where turn-level keys differ.
    let archived = db::ulid();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude','archived',?)").bind(&archived).bind(&env.project_id).bind("archived-owner").bind(db::now()).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE conversations SET bot_id=? WHERE id=?").bind(&archived).bind(&conv).execute(&env.app.db).await.unwrap();
    let new_conv = db::conversation_id(&env.app.db, &b.id).await.unwrap();
    assert_ne!(conv, new_conv);
    let mut headers = HeaderMap::new(); headers.insert("X-AM-Bot-Id", a.id.parse().unwrap()); headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body: PromptIn = serde_json::from_value(json!({"text":"please answer","client_request_id":"stable","expect_result":true})).unwrap();
    let response = prompt_bot(State(env.app.clone()), Path(b.id.clone()), headers, Extension(RequestPrincipal::Bot(a.id)), Json(body)).await.unwrap();
    let value: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(value["request_id"], request); assert_eq!(value["state"], "notified"); assert_eq!(value["turn_id"], turn);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE conversation_id=?").bind(new_conv).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn first_request_holds_lock_during_sweep_and_respects_terminal_state() {
    for terminal in ["failed", "delivered"] {
        let env = crate::testing::env().await;
        let a = crate::testing::claude_bot(&env.app, &env.project_id, "race-source").await;
        let b = crate::testing::claude_bot(&env.app, &env.project_id, "race-target").await;
        let run = crate::testing::fake_run(&env.app, &b.id).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(run).execute(&env.app.db).await.unwrap();
        let key = db::ulid();
        let turn = db::ulid();
        let conv = db::conversation_id(&env.app.db, &b.id).await.unwrap();
        let app = env.app.clone(); let key2 = key.clone(); let turn2 = turn.clone(); let conv2 = conv.clone();
        crate::lifecycle::race_point::arm("relay_request_after_insert", &key, move || async move {
            crate::runners::relay_requests::sweep(&app).await;
            let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE client_request_id=?").bind(&key2).fetch_one(&app.db).await.unwrap();
            assert_eq!(state, "pending", "sweep must skip the locked request");
            if terminal == "delivered" {
                sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,created_at) VALUES(?,?,'web','in_flight','ok',?)").bind(&turn2).bind(&conv2).bind(db::now()).execute(&app.db).await.unwrap();
                sqlx::query("UPDATE relay_requests SET state='delivered',to_turn_id=? WHERE client_request_id=?").bind(&turn2).bind(&key2).execute(&app.db).await.unwrap();
            } else {
                sqlx::query("UPDATE relay_requests SET state='failed' WHERE client_request_id=?").bind(&key2).execute(&app.db).await.unwrap();
            }
        });
        let mut headers = HeaderMap::new(); headers.insert("X-AM-Bot-Id", a.id.parse().unwrap()); headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
        let body = serde_json::from_value(json!({"text":"work","client_request_id":key,"expect_result":true})).unwrap();
        let response = prompt_bot(State(env.app.clone()), Path(b.id.clone()), headers, Extension(RequestPrincipal::Bot(a.id)), Json(body)).await.unwrap();
        let value: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(value["state"], terminal);
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE conversation_id=?").bind(&conv).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(count, if terminal == "delivered" { 1 } else { 0 });
        if terminal == "delivered" { assert_eq!(value["turn_id"], turn); } else { assert!(value["turn_id"].is_null()); }
    }
}

#[tokio::test]
async fn same_authenticated_request_id_queues_only_once() {
    let env = crate::testing::env().await;
    let a = crate::testing::claude_bot(&env.app, &env.project_id, "relay-idempotent-a").await;
    let b = crate::testing::claude_bot(&env.app, &env.project_id, "relay-idempotent-b").await;
    let run = crate::testing::fake_run(&env.app, &b.id).await;
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    let mut h = HeaderMap::new();
    h.insert("X-AM-Bot-Id", a.id.parse().unwrap());
    h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body = json!({"text":"please answer","client_request_id":"stable-key","expect_result":true,"queue_if_busy":true});
    let mut ids = Vec::new();
    for _ in 0..2 {
        let response = prompt_bot(State(env.app.clone()), Path(b.id.clone()), h.clone(), Extension(RequestPrincipal::Bot(a.id.clone())), Json(serde_json::from_value(body.clone()).unwrap())).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        ids.push(value["request_id"].as_str().unwrap().to_string());
    }
    assert_eq!(ids[0], ids[1]);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_requests WHERE from_bot_id=? AND to_bot_id=?")
        .bind(&a.id).bind(&b.id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(n, 1);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=?")
        .bind(&b.id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(n, 1);
}

/// #2069：群集請求被改派（`claim_target` 改 `to_bot_id`）之後，呼叫端照原樣重送給原目標，
/// 只能拿回原本那筆 request，不能再建一筆新的派工；不論它還在 pending 或已送達、已回覆。內容不符仍 409。
#[tokio::test]
async fn retrying_a_cluster_request_after_it_moved_returns_the_original() {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let mut members = Vec::new();
    for (name, role) in [("mgr", "manager"), ("w1", "executor"), ("w2", "executor")] {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET role=?, model='claude-haiku-5-5' WHERE id=?").bind(role).bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='running', agent_status=?, runtime_role_policy=? WHERE id=?")
            .bind(if role == "manager" { "idle" } else { "working" }).bind(format!("{}:local:expert:{role}", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
        members.push(bot);
    }
    let (manager, a, b) = (&members[0], &members[1], &members[2]);
    let mut h = HeaderMap::new();
    h.insert("X-AM-Bot-Id", manager.id.parse().unwrap());
    h.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let send = |text: &'static str| {
        let (app, h, to, from) = (env.app.clone(), h.clone(), a.id.clone(), manager.id.clone());
        async move {
            let body = json!({"text":text,"client_request_id":"stable-key","expect_result":true,"cluster":"haiku"});
            prompt_bot(State(app), Path(to), h, Extension(RequestPrincipal::Bot(from)), Json(serde_json::from_value(body).unwrap())).await
        }
    };
    let first = send("work").await.unwrap();
    let first: Value = serde_json::from_slice(&axum::body::to_bytes(first.into_body(), 65536).await.unwrap()).unwrap();
    assert_eq!(first["delivery"], "pending", "{first}");
    let request = first["request_id"].as_str().unwrap().to_string();
    // b 先閒下來，群集把請求改派給它。
    sqlx::query("UPDATE runs SET agent_status='idle' WHERE bot_id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
    assert_eq!(crate::expert_cluster::claim_target(&env.app, &request, &a.id, "haiku").await.unwrap(), Some(b.id.clone()));
    sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=?").bind(&b.id).execute(&env.app.db).await.unwrap();
    for state in ["pending", "delivered", "answered"] {
        sqlx::query("UPDATE relay_requests SET state=? WHERE id=?").bind(state).bind(&request).execute(&env.app.db).await.unwrap();
        let again = send("work").await.unwrap();
        let again: Value = serde_json::from_slice(&axum::body::to_bytes(again.into_body(), 65536).await.unwrap()).unwrap();
        assert_eq!(again["request_id"], request, "{state}: {again}");
        assert_eq!(again["state"], state, "{again}");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM relay_requests WHERE from_bot_id=?").bind(&manager.id).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 1, "{state}: an identical retry must not create a second request");
    }
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id IN (?,?)")
        .bind(&a.id).bind(&b.id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(turns, 0);
    assert!(matches!(send("different").await, Err(LcError::Conflict(_))), "same key with different text stays a conflict");
}
