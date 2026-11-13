//! Through the real `prompt_bot` handler and `IntoResponse`: the status codes the API documents.
use super::*;

async fn typed_bot(e: &crate::testing::Env, kind: &str) -> String {
    typed_bot_named(e, kind, "route-bot", "pane-route").await
}

/// 同一個專案要好幾顆（名字與 pane 都不能撞）的測試用。
async fn typed_bot_named(e: &crate::testing::Env, kind: &str, name: &str, pane: &str) -> String {
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET kind = ? WHERE id = ?").bind(kind).bind(&bot.id).execute(&e.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1',?,?,'test',1,?)",
    )
    .bind(db::ulid())
    .bind(&bot.id)
    .bind(pane)
    .bind(name)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    bot.id
}

async fn call(e: &crate::testing::Env, bot: &str, text: String, crid: &str) -> (StatusCode, Value) {
    let body = PromptIn { text, expect_result: false, client_request_id: Some(crid.into()), attachments: vec![], relay_from: None, ack: false, reply_to: None, send_now: false, start_if_stopped: false, queue_if_busy: false, clear_draft: false, submit_draft: false, expect_draft_token: None, share_reply_visible: false, track: None, cluster: None };
    let resp = match prompt_bot(State(e.app.clone()), Path(bot.to_string()), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn call_queue_if_busy(e: &crate::testing::Env, bot: &str, text: &str, crid: &str, attachments: &[String]) -> (StatusCode, Value) {
    let body: PromptIn = serde_json::from_value(json!({
        "text": text,
        "client_request_id": crid,
        "queue_if_busy": true,
        "attachments": attachments,
    })).unwrap();
    let resp = match prompt_bot(State(e.app.clone()), Path(bot.to_string()), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 不可能照原樣送出的 prompt 是真的 HTTP 422，body 說清楚原因（sol 第八輪 #1）。
#[tokio::test]
async fn an_unsendable_prompt_is_http_422_with_a_machine_readable_body() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    let (status, body) = call(&e, &bot, "x".repeat(crate::lifecycle::MAX_PROVABLE_CHARS + 1), "route-422").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"], "delivery_unprovable");
    assert_eq!(body["reason"], "prompt_too_long_to_prove");
    assert_eq!(body["sent"], false);
}

/// 派工者只能撤回自己派出、已驗證、還在佇列的那一則（使用者 2026-11-08 第 8 項）。
/// 每個情境一顆 bot：同一顆 bot 的 relay 佇列一次只排一則。
#[tokio::test]
async fn a_dispatcher_withdraws_only_its_own_verified_queued_turn() {
    async fn busy_bot(app: &Arc<App>, project_id: &str, name: &str) -> (String, String) {
        let bot = crate::testing::claude_bot(app, project_id, name).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let run = crate::testing::fake_run(app, &bot.id).await;
        let sent = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(&sent).bind(&conv).bind(&run).bind(db::now()).execute(&app.db).await.unwrap();
        (bot.id, sent)
    }
    let e = crate::testing::env().await;
    let (mine_bot, sent) = busy_bot(&e.app, &e.project_id, "wd-mine").await;
    let (theirs_bot, _) = busy_bot(&e.app, &e.project_id, "wd-theirs").await;
    let (unverified_bot, _) = busy_bot(&e.app, &e.project_id, "wd-unverified").await;
    let mine = lifecycle::prompt_relayed_queueable(&e.app, &mine_bot, "mine", "dispatch-a:internal", Some("sender-a"), lifecycle::QueueClass::Relay).await.unwrap().turn_id;
    let theirs = lifecycle::prompt_relayed_queueable(&e.app, &theirs_bot, "theirs", "dispatch-b:internal", Some("sender-b"), lifecycle::QueueClass::Relay).await.unwrap().turn_id;
    let unverified = lifecycle::prompt_relayed_queueable(&e.app, &unverified_bot, "unverified", "dispatch-c:internal", Some("sender-a"), lifecycle::QueueClass::Relay).await.unwrap().turn_id;
    for (turn, flag) in [(&mine, 0), (&theirs, 0), (&unverified, 1)] {
        sqlx::query("UPDATE messages SET relay_unverified=? WHERE turn_id=? AND role='user'").bind(flag).bind(turn).execute(&e.app.db).await.unwrap();
    }
    assert!(lifecycle::sender_may_withdraw(&e.app, &mine, "sender-a").await.unwrap(), "own verified queued turn");
    assert!(!lifecycle::sender_may_withdraw(&e.app, &theirs, "sender-a").await.unwrap(), "someone else's turn");
    assert!(!lifecycle::sender_may_withdraw(&e.app, &unverified, "sender-a").await.unwrap(), "self-reported relay_from is not proof");
    assert!(!lifecycle::sender_may_withdraw(&e.app, &sent, "sender-a").await.unwrap(), "already sent (in flight)");
    assert!(lifecycle::withdraw_turn_by_sender(&e.app, &theirs, "sender-a").await.is_err(), "someone else's turn");
    assert!(lifecycle::withdraw_turn_by_sender(&e.app, &sent, "sender-a").await.is_err(), "sent turns stay put");
    lifecycle::withdraw_turn_by_sender(&e.app, &mine, "sender-a").await.unwrap();
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&mine).fetch_one(&e.app.db).await.unwrap();
    assert_ne!(status, "queued", "withdrawn turn leaves the queue");
    assert!(lifecycle::withdraw_turn_by_sender(&e.app, &mine, "sender-a").await.is_err(), "cannot withdraw twice");
    assert!(lifecycle::sender_may_withdraw(&e.app, &theirs, "sender-b").await.unwrap(), "the other sender still owns its own");
}

#[tokio::test]
async fn external_notice_prefixes_and_state_projection_keep_writer_lanes() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    let conv = db::conversation_id(&e.app.db, &bot).await.unwrap();
    let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=?").bind(&bot).fetch_one(&e.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
        .bind(db::ulid()).bind(&conv).bind(run).bind(db::now()).execute(&e.app.db).await.unwrap();
    let notice = lifecycle::prompt_relayed_queueable(&e.app, &bot, "notice", "child-done:internal", Some("daemon"), lifecycle::QueueClass::DaemonNotice).await.unwrap();
    let relay = lifecycle::prompt_relayed_queueable(&e.app, &bot, "relay", "dispatch:internal", Some("other-bot"), lifecycle::QueueClass::Relay).await.unwrap();
    let (status, user) = call_queue_if_busy(&e, &bot, "user", "child-done:external", &[]).await;
    assert_eq!(status, StatusCode::OK, "{user}");
    let user_id = user["turn_id"].as_str().unwrap();
    let lane: String = sqlx::query_scalar("SELECT queue_class FROM turns WHERE id=?").bind(user_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(lane, "user");
    let (status, _) = call_queue_if_busy(&e, &bot, "second user", "resume-nudge:external", &[]).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let projected = |state: Value| state["projects"][0]["bots"].as_array().unwrap().iter().find(|b| b["id"] == bot).unwrap()["queued_turn"]["id"].clone();
    assert_eq!(projected(state_json(&e.app).await.unwrap()), user_id);
    withdraw_turn(State(e.app.clone()), Path(user_id.to_string())).await.unwrap();
    assert_eq!(projected(state_json(&e.app).await.unwrap()), relay.turn_id);
    assert!(withdraw_turn(State(e.app.clone()), Path(relay.turn_id.clone())).await.is_err(), "AGM relay keeps its own withdrawal policy");
    withdraw_turn(State(e.app.clone()), Path(notice.turn_id)).await.unwrap();
    assert_eq!(projected(state_json(&e.app).await.unwrap()), relay.turn_id);
}

#[tokio::test]
async fn a_user_prompt_can_claim_the_single_busy_queue_slot_idempotently() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'")
        .bind(&bot).fetch_one(&e.app.db).await.unwrap();
    let conversation_id = db::conversation_id(&e.app.db, &bot).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
        .bind(db::ulid()).bind(&conversation_id).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();

    let attachment_id = db::ulid();
    sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at) VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)")
        .bind(&attachment_id).bind(&bot).bind(db::now()).execute(&e.app.db).await.unwrap();
    let attachments = vec![attachment_id.clone()];
    let (status, first) = call_queue_if_busy(&e, &bot, "稍後送出", "busy-queue-1", &attachments).await;
    assert_eq!(status, StatusCode::OK, "queue_if_busy 收下忙碌中的 prompt：{first}");
    assert_eq!(first["delivery"], "queued", "{first}");
    let turn_id = first["turn_id"].as_str().unwrap();
    let message_id = first["message_id"].as_str().unwrap();
    let stored: (String, String, String, i64, Option<String>) = sqlx::query_as("SELECT status, origin, delivery, awaits_idle, prompt_text FROM turns WHERE id=?")
        .bind(turn_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((stored.0.as_str(), stored.1.as_str(), stored.2.as_str(), stored.3), ("queued", "web", "pending", 1));
    assert!(stored.4.as_deref().unwrap().starts_with("稍後送出\n\n"), "附件路徑要存在 flush 可送的 prompt_text：{stored:?}");
    let replay = call_queue_if_busy(&e, &bot, "稍後送出", "busy-queue-1", &attachments).await;
    assert_eq!(replay.0, StatusCode::OK, "{}", replay.1);
    assert_eq!(replay.1["turn_id"], turn_id, "same client_request_id returns the queued turn");
    assert_eq!(replay.1["message_id"], message_id);

    let (status, second) = call_queue_if_busy(&e, &bot, "再一則", "busy-queue-2", &[]).await;
    assert_eq!(status, StatusCode::CONFLICT, "one busy queue slot per bot: {second}");
    assert_eq!(second["reason"], "queue_slot_taken", "{second}");
    assert_eq!(second["turn_id"], turn_id, "the conflict identifies the existing slot");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE conversation_id=? AND status='queued'")
        .bind(&conversation_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(count, 1, "the refused request writes no turn");
    let message_turn: String = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id=?")
        .bind(message_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(message_turn, turn_id);
    let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?")
        .bind(&attachment_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(bound.as_deref(), Some(message_id), "message and attachment binding commit together");
    let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(turn_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(serde_json::to_value(turn).unwrap()["awaits_idle"], 1, "turn JSON exposes the wait flag");
}

/// 使用者 queue_if_busy 只與 user lane 衝突；relay 與 daemon lane 同時排著時仍接受使用者訊息。
#[tokio::test]
async fn a_queue_slot_conflicts_only_with_the_same_trusted_lane() {
    let e = crate::testing::env().await;
    let agm = typed_bot_named(&e, "claude", "agm-bot", "pane-agm").await;
    sqlx::query("INSERT INTO supervisors (id, bot_id, created_at, updated_at) VALUES ('sup-1', ?, ?, ?)").bind(&agm).bind(db::now()).bind(db::now()).execute(&e.app.db).await.unwrap();
    let other = typed_bot_named(&e, "claude", "other-bot", "pane-other").await;

    // (佔著的人, lane, awaits_idle, awaits_start, crid, relay_from, 預期結果, holder.kind)
    let cases: Vec<(&str, &str, i64, i64, Option<String>, Option<String>, bool, &str)> = vec![
        ("agm", "relay", 0, 0, None, Some(agm.clone()), false, "agm"),
        ("bot", "relay", 0, 0, None, Some(other.clone()), false, "bot"),
        ("daemon", "daemon", 0, 0, None, Some("daemon".into()), false, "daemon"),
        ("start", "user", 0, 1, None, None, true, "start"),
        ("user", "user", 1, 0, None, None, true, "user"),
    ];
    for (name, queue_class, awaits_idle, awaits_start, crid, relay_from, should_conflict, kind) in cases {
        let bot = typed_bot_named(&e, "grok", &format!("holder-{name}"), &format!("pane-{name}")).await;
        let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&bot).fetch_one(&e.app.db).await.unwrap();
        let conv = db::conversation_id(&e.app.db, &bot).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(db::ulid()).bind(&conv).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
        let held = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle, awaits_start, client_request_id, queue_class) VALUES (?, ?, 'web', 'queued', 'pending', 'held', ?, ?, ?, ?, ?)")
            .bind(&held).bind(&conv).bind(db::now()).bind(awaits_idle).bind(awaits_start).bind(crid).bind(queue_class).execute(&e.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, created_at) VALUES (?, ?, ?, 'user', 'held', 'web', ?, 0, ?)")
            .bind(db::ulid()).bind(&conv).bind(&held).bind(relay_from).bind(db::now()).execute(&e.app.db).await.unwrap();

        let (status, body) = call_queue_if_busy(&e, &bot, "我也要排隊", &format!("slot-{name}"), &[]).await;
        if should_conflict {
            assert_eq!(status, StatusCode::CONFLICT, "{name}: {body}");
            assert_eq!(body["reason"], "queue_slot_taken", "{name}: {body}");
            assert_eq!(body["turn_id"], held, "{name}: 指出佔著的那一筆：{body}");
            assert_eq!(body["holder"]["kind"], kind, "{name}: {body}");
        } else {
            assert_eq!(status, StatusCode::OK, "不同 lane 不應擋使用者：{name}: {body}");
            assert_eq!(body["delivery"], "queued", "{name}: 使用者訊息仍被保存");
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM turns WHERE conversation_id=? AND status='queued'").bind(&conv).fetch_one(&e.app.db).await.unwrap();
        assert_eq!(count, if should_conflict { 1 } else { 2 }, "{name}: 同 lane 衝突不寫入，不同 lane 保留兩筆");
    }
}

/// `relay_from` 沒帶 bot token 時只是**自稱**（`relay_unverified`）：同一 relay lane 再送時不能把它講成 AGM／真實 bot。
#[tokio::test]
async fn a_self_declared_relay_source_is_never_named_or_called_agm() {
    let e = crate::testing::env().await;
    let agm = typed_bot_named(&e, "claude", "agm-bot", "pane-agm2").await;
    sqlx::query("INSERT INTO supervisors (id, bot_id, created_at, updated_at) VALUES ('sup-2', ?, ?, ?)").bind(&agm).bind(db::now()).bind(db::now()).execute(&e.app.db).await.unwrap();
    let victim = typed_bot_named(&e, "grok", "holder-spoof", "pane-spoof").await;
    let sender = typed_bot_named(&e, "claude", "sender-spoof", "pane-sender").await;
    let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'").bind(&victim).fetch_one(&e.app.db).await.unwrap();
    let conv = db::conversation_id(&e.app.db, &victim).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
        .bind(db::ulid()).bind(&conv).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
    let held = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_idle, queue_class) VALUES (?, ?, 'web', 'queued', 'pending', 'held', ?, 0, 'relay')")
        .bind(&held).bind(&conv).bind(db::now()).execute(&e.app.db).await.unwrap();
    // 自稱是 AGM 的 bot（沒帶 token）。
    sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, relay_from, relay_unverified, created_at) VALUES (?, ?, ?, 'user', 'held', 'web', ?, 1, ?)")
        .bind(db::ulid()).bind(&conv).bind(&held).bind(&agm).bind(db::now()).execute(&e.app.db).await.unwrap();

    let body: PromptIn = serde_json::from_value(json!({
        "text": "我也要排隊",
        "client_request_id": "slot-spoof",
        "queue_if_busy": true,
    })).unwrap();
    let sender_token: String = sqlx::query_scalar("SELECT hook_token FROM bots WHERE id=?").bind(&sender).fetch_one(&e.app.db).await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", sender.parse().unwrap());
    headers.insert("X-AM-Bot-Token", sender_token.parse().unwrap());
    let response = match prompt_bot(State(e.app.clone()), Path(victim.clone()), headers, Extension(RequestPrincipal::Bot(sender)), Json(body)).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    };
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["reason"], "queue_slot_taken", "{body}");
    assert_eq!(body["holder"]["kind"], "bot", "自稱的來源不能被講成 AGM：{body}");
    assert!(body["holder"].get("bot_name").is_none() && body["holder"].get("bot_id").is_none(), "自稱的來源不帶名字與 id：{body}");
}

/// web 的 Enter 永遠帶 `queue_if_busy`，bot 沒在跑時再加 `start_if_stopped`。畫面上的 run 比 daemon 慢一拍
/// （bot 剛起來、frame 還沒到）時兩個旗標會一起打到「已經在跑而且忙」的 bot：`start_if_stopped`
/// 「bot 在跑就跟沒帶一樣」，所以結果該跟只帶 `queue_if_busy` 相同——落地排隊，而不是丟掉旗標回 409（Refs #733）。
#[tokio::test]
async fn start_if_stopped_does_not_drop_queue_if_busy_for_a_running_busy_bot() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'")
        .bind(&bot).fetch_one(&e.app.db).await.unwrap();
    let conversation_id = db::conversation_id(&e.app.db, &bot).await.unwrap();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
        .bind(db::ulid()).bind(&conversation_id).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
    let body: PromptIn = serde_json::from_value(json!({
        "text": "兩個旗標一起來",
        "client_request_id": "start-and-queue",
        "start_if_stopped": true,
        "queue_if_busy": true,
    })).unwrap();
    let resp = match prompt_bot(State(e.app.clone()), Path(bot.clone()), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await {
        Ok(r) => r,
        Err(err) => err.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let out: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    assert_eq!(status, StatusCode::OK, "在跑的忙碌 bot 照 queue_if_busy 排隊：{out}");
    assert_eq!(out["delivery"], "queued", "{out}");
    let awaits_idle: i64 = sqlx::query_scalar("SELECT awaits_idle FROM turns WHERE id=?")
        .bind(out["turn_id"].as_str().unwrap()).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(awaits_idle, 1);
}

#[tokio::test]
async fn queue_if_busy_uses_the_agent_status_when_no_turn_is_in_flight() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=? AND state='running'")
        .bind(&bot).execute(&e.app.db).await.unwrap();
    let (status, body) = call_queue_if_busy(&e, &bot, "等 agent 閒下來", "busy-agent-status", &[]).await;
    assert_eq!(status, StatusCode::OK, "非 idle 狀態也要持久排隊：{body}");
    assert_eq!(body["delivery"], "queued", "{body}");
    assert!(e.herdr.calls_to("agent.prompt").is_empty());
    assert!(e.herdr.calls_to("pane.send_text").is_empty());
}

#[tokio::test]
async fn busy_prompt_admission_dispatch_and_withdrawal_share_the_durable_queue() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "claude").await;
    let run_id: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=? AND state='running'")
        .bind(&bot).fetch_one(&e.app.db).await.unwrap();
    let conversation_id = db::conversation_id(&e.app.db, &bot).await.unwrap();
    let previous_id = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
        .bind(&previous_id).bind(&conversation_id).bind(&run_id).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();

    // B can restore a still-queued prompt submitted by A before C dispatches it.
    let (status, withdrawn) = call_queue_if_busy(&e, &bot, "先放回輸入框", "queue-withdraw-first", &[]).await;
    assert_eq!(status, StatusCode::OK, "{withdrawn}");
    let withdrawn_id = withdrawn["turn_id"].as_str().unwrap().to_string();
    let response = withdraw_turn(State(e.app.clone()), Path(withdrawn_id.clone())).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20).await.unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), json!({"text": "先放回輸入框", "attachments": []}));
    let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(&withdrawn_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(status, "failed");

    // A second prompt remains queued while its predecessor is active, then C dispatches it at idle.
    let (status, admitted) = call_queue_if_busy(&e, &bot, "等閒下來再送", "queue-dispatch-next", &[]).await;
    assert_eq!(status, StatusCode::OK, "{admitted}");
    let queued_id = admitted["turn_id"].as_str().unwrap();
    crate::lifecycle::flush_queued_locked(&e.app, &bot).await.unwrap();
    let still_queued: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(queued_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(still_queued, "queued", "busy run keeps the accepted prompt durable");
    assert!(e.herdr.calls_to("pane.send_text").is_empty());

    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), ..Default::default() });
    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&previous_id).execute(&e.app.db).await.unwrap();
    sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run_id).execute(&e.app.db).await.unwrap();
    crate::lifecycle::flush_queued_locked(&e.app, &bot).await.unwrap();
    let sent: (String, String) = sqlx::query_as("SELECT status, delivery FROM turns WHERE id=?").bind(queued_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((sent.0.as_str(), sent.1.as_str()), ("in_flight", "ok"));
    assert_eq!(e.herdr.calls_to("pane.send_text").len(), 1, "C sends the admitted prompt once");

    let response = withdraw_turn(State(e.app.clone()), Path(queued_id.to_string())).await.unwrap_err().into_response();
    assert_eq!(response.status(), StatusCode::CONFLICT, "B cannot restore a turn C has already claimed");
}

#[tokio::test]
async fn queue_if_busy_on_an_idle_run_keeps_direct_delivery() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    let (status, body) = call_queue_if_busy(&e, &bot, "現在就送", "idle-queue-flag", &[]).await;
    assert_eq!(status, StatusCode::OK, "idle runs keep the existing direct prompt route: {body}");
    assert_ne!(body["delivery"], "queued", "an idle run sends directly");
    let awaits_idle: i64 = sqlx::query_scalar("SELECT awaits_idle FROM turns WHERE id=?")
        .bind(body["turn_id"].as_str().unwrap()).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(awaits_idle, 0);
}

#[tokio::test]
async fn a_failed_attachment_binding_rolls_back_the_busy_queue_admission() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    sqlx::query("UPDATE runs SET agent_status='working' WHERE bot_id=? AND state='running'")
        .bind(&bot).execute(&e.app.db).await.unwrap();
    let attachment_id = db::ulid();
    sqlx::query("INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at) VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)")
        .bind(&attachment_id).bind(&bot).bind(db::now()).execute(&e.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER refuse_busy_queue_attachment BEFORE UPDATE OF message_id ON attachments BEGIN SELECT RAISE(ABORT, 'injected bind failure'); END")
        .execute(&e.app.db).await.unwrap();

    let (status, body) = call_queue_if_busy(&e, &bot, "有附件", "busy-queue-rollback", &[attachment_id.clone()]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "failed attachment binding must fail the admission: {body}");
    let (turns, messages): (i64, i64) = sqlx::query_as("SELECT (SELECT count(*) FROM turns WHERE client_request_id='busy-queue-rollback'), (SELECT count(*) FROM messages WHERE content='有附件')")
        .fetch_one(&e.app.db).await.unwrap();
    let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?")
        .bind(&attachment_id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!((turns, messages, bound), (0, 0, None), "turn, message and attachment binding all roll back");
}

/// #337：同一個 client_request_id 換了內容不能回第一則的結果；完全一樣的重送照舊回同一個 turn。
#[tokio::test]
async fn a_reused_request_id_with_different_text_is_a_409_not_the_first_result() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    let (s1, b1) = call(&e, &bot, "first".into(), "crid-x").await;
    assert_eq!(s1, StatusCode::OK, "{b1}");
    let (s2, b2) = call(&e, &bot, "first".into(), "crid-x").await;
    assert_eq!((s2, &b2["turn_id"]), (StatusCode::OK, &b1["turn_id"]), "一樣的重送回同一個 turn");
    let (s3, b3) = call(&e, &bot, "second".into(), "crid-x").await;
    assert_eq!(s3, StatusCode::CONFLICT, "{b3}");
    assert_eq!(b3["reason"], "text_mismatch");
}

/// #923：`/prompt` 的 client_request_id 進 turns 的唯一索引、再隨回應回出去：過長、含空白或控制字元一律 400，不建回合。
#[tokio::test]
async fn a_prompt_with_an_unsafe_client_request_id_is_refused_without_a_turn() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    for bad in ["a b".to_string(), "a\nb".to_string(), "x".repeat(201)] {
        let (status, body) = call(&e, &bot, "hello".into(), &bad).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {body}");
    }
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(turns, 0, "被拒的請求不建回合");
}

/// 已刪除的 bot 不能收 prompt（送進使用者已經刪掉的 pane 還回 200）。
#[tokio::test]
async fn a_soft_deleted_bot_does_not_take_a_prompt() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    sqlx::query("UPDATE bots SET deleted_at=? WHERE id=?").bind(db::now()).bind(&bot).execute(&e.app.db).await.unwrap();
    let (status, body) = call(&e, &bot, "hello".into(), "del-1").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

/// 輸入框有字是 HTTP 409、可重試，沒有建立 turn。
#[tokio::test]
async fn a_busy_composer_is_http_409_and_retryable() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane(
        "pane-route",
        crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
    );
    let (status, body) = call(&e, &bot, "Reply with PONG".into(), "route-409").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["reason"], "composer_busy");
    assert_eq!(body["retryable"], true);
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(turns, 0);
}

async fn get_draft(e: &crate::testing::Env, bot: &str) -> (StatusCode, Value) {
    let resp = match get_composer_draft(State(e.app.clone()), Path(bot.to_string())).await {
        Ok(Json(v)) => Json(v).into_response(),
        Err(err) => err.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// `GET /bots/:id/composer-draft`：長草稿**完整**回給網頁（不像 409 截到 500 字），token 跟同一刻 409 給的一樣，後續動作通用。
#[tokio::test]
async fn composer_draft_read_returns_the_full_draft_with_the_same_token_as_the_409() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    let long = format!("{}尾", "長".repeat(700));
    e.herdr.live_pane(
        "pane-route",
        crate::testing::LivePane { width: Some(120), boxed: true, composer: vec![long.clone()], ..Default::default() },
    );
    let (status, body) = get_draft(&e, &bot).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "draft");
    assert_eq!(body["draft_truncated"], false);
    let full = body["draft"].as_str().unwrap();
    assert!(full.chars().count() > crate::lifecycle::composer_draft::DRAFT_SHOWN_CHARS, "完整、沒有截到 500 字");
    assert!(full.ends_with('尾'));
    assert_eq!(body["draft_actions"], json!(["submit", "clear"]));
    let (status409, conflict) = call(&e, &bot, "hi".into(), "full-draft-409").await;
    assert_eq!(status409, StatusCode::CONFLICT);
    assert_eq!(conflict["draft_truncated"], true);
    assert_eq!(conflict["draft_token"], body["draft_token"], "token 綁完整草稿，兩個入口給的相同");
}

/// 空輸入框：`state:"empty"`、沒有草稿、沒有動作；唯讀、不建 turn。
#[tokio::test]
async fn composer_draft_read_reports_an_empty_box_and_never_creates_a_turn() {
    let e = crate::testing::env().await;
    let bot = typed_bot(&e, "grok").await;
    e.herdr.live_pane("pane-route", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
    let (status, body) = get_draft(&e, &bot).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "empty");
    assert!(body["draft"].is_null());
    assert_eq!(body["draft_actions"], json!([]));
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(turns, 0);
}

/// 沒有 active run 就是 404（跟 `GET /terminal` 一樣）。
#[tokio::test]
async fn composer_draft_read_without_a_run_is_404() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "no-run-bot").await;
    let (status, _) = get_draft(&e, &bot.id).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
