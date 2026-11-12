use super::*;
use crate::supervisor::bot_requests::flow_tests;

#[tokio::test]
async fn expert_ordinary_announce_checks_frozen_recipient_and_role() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "ordinary-advisor").await;
    let target = crate::testing::claude_bot(&env.app, &env.project_id, "ordinary-executor").await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&sender.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&target.id).execute(&env.app.db).await.unwrap();
    let sr = crate::testing::fake_run(&env.app, &sender.id).await;
    let tr = crate::testing::fake_run(&env.app, &target.id).await;
    sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:advisor", env.project_id)).bind(&sr).execute(&env.app.db).await.unwrap();
    let mut headers = HeaderMap::new(); headers.insert("X-AM-Bot-Token", sender.hook_token.parse().unwrap());
    let body = RelayAnnounce { bot_id:sender.id.clone(), to_agent:format!("bot:{}",target.id), to_bot_id:Some(target.id.clone()), to_run_id:Some(tr), delivery_id:Some(db::ulid()), text:"do work".into(), ack:None, reply_to:None };
    let (code, Json(value)) = relay_announce(State(env.app.clone()), headers, axum::extract::Form(body)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert!(value["reason"].as_str().unwrap().contains("executor"));
}

#[tokio::test]
async fn ordinary_announce_denies_parent_child_sender_to_expert_other_project() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "ordinary-source").await;
    let other = db::ulid();
    sqlx::query("INSERT INTO projects(id,path,label,host,mode,created_at) VALUES(?,?,'expert','local','expert',?)").bind(&other).bind(format!("/tmp/{other}")).bind(db::now()).execute(&env.app.db).await.unwrap();
    let target = crate::testing::claude_bot(&env.app, &other, "ordinary-target").await;
    let tr = crate::testing::fake_run(&env.app, &target.id).await;
    let mut headers = HeaderMap::new(); headers.insert("X-AM-Bot-Token", sender.hook_token.parse().unwrap());
    let body = RelayAnnounce { bot_id:sender.id, to_agent:format!("bot:{}",target.id), to_bot_id:Some(target.id), to_run_id:Some(tr), delivery_id:Some(db::ulid()), text:"work".into(), ack:None, reply_to:None };
    let (code, Json(value)) = relay_announce(State(env.app.clone()), headers, axum::extract::Form(body)).await;
    assert_eq!(code, StatusCode::FORBIDDEN); assert_eq!(value["reason"], "expert_cross_project");
}

async fn announce(app: &Arc<App>, to: &str, text: &str) -> (StatusCode, Value) {
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Token", "tok-w1".parse().unwrap());
    let run_id = format!("relay-run-{to}");
    // A concrete transport/run fixture, independent of the bot's display name.
    let _ = sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at,pane_id) SELECT ?,id,'running','idle',?,id FROM bots WHERE id=? AND NOT EXISTS (SELECT 1 FROM runs WHERE bot_id=? AND state='running')")
        .bind(&run_id).bind(db::now()).bind(to).bind(to).execute(&app.db).await;
    let target_run = db::active_run(&app.db, to).await.ok().flatten().map(|r| r.id).unwrap_or(run_id);
    let body = RelayAnnounce { bot_id: "w1".into(), to_agent: format!("bot:{to}"), to_bot_id: Some(to.into()),
        to_run_id: Some(target_run), delivery_id: Some(format!("delivery-{to}-{text}")), text: text.into(), ack: None, reply_to: None };
    let (code, Json(v)) = relay_announce(State(app.clone()), headers, axum::extract::Form(body)).await;
    (code, v)
}

#[tokio::test]
async fn nr_relay_role_routing_survives_rename_and_does_not_follow_the_old_name() {
    let app = flow_tests::app().await;
    flow_tests::configure_responder(&app).await;
    sqlx::query("UPDATE bots SET name='renamed-role' WHERE id='resp'").execute(&app.db).await.unwrap();
    sqlx::query("UPDATE bots SET name='AGM-responder' WHERE id='w2'").execute(&app.db).await.unwrap();
    let (code, body) = announce(&app, "resp", "交辦給穩定的角色身份").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["routed"], "responder");
    assert_eq!(bot_requests(&app).await, 1);
    let (code, body) = announce(&app, "w2", "交辦給重用舊顯示名稱的一般 bot").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body["to_pane_id"], "w2");
    assert_eq!(bot_requests(&app).await, 1, "一般 bot 不能因為同名被攔入角色佇列");
}

#[tokio::test]
async fn nr_relay_a_name_only_request_is_unknown_even_if_a_role_has_that_name() {
    let app = flow_tests::app().await;
    flow_tests::configure_responder(&app).await;
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Token", "tok-w1".parse().unwrap());
    let body = RelayAnnounce { bot_id:"w1".into(), to_agent:"AGM".into(), text:"不能用角色暱稱猜收件人".into(),
        to_bot_id:None, to_run_id:None, delivery_id:Some("nr-name-only".into()), ack:None, reply_to:None };
    let (code, Json(body)) = relay_announce(State(app.clone()), headers, axum::extract::Form(body)).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(body["error"], "relay_target_unverified");
    assert_eq!(bot_requests(&app).await, 0);
}

#[tokio::test]
async fn nr_claim_pane_targets_never_match_another_runs_agent_name() {
    let env = crate::testing::env().await;
    let sender = crate::testing::claude_bot(&env.app, &env.project_id, "sender").await;
    let target = crate::testing::claude_bot(&env.app, &env.project_id, "target").await;
    let stranger = crate::testing::claude_bot(&env.app, &env.project_id, "stranger").await;
    crate::testing::fake_run(&env.app, &sender.id).await;
    let run = crate::testing::fake_run(&env.app, &target.id).await;
    let other = crate::testing::fake_run(&env.app, &stranger.id).await;
    let pane = format!("pane-{}", target.id);
    sqlx::query("UPDATE runs SET agent_name=? WHERE id=?").bind(&pane).bind(other).execute(&env.app.db).await.unwrap();
    assert_eq!(relay_target_run(&env.app, &sender.id, &pane).await.unwrap().unwrap().id, run);
    sqlx::query("UPDATE runs SET agent_name='renamed' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    assert_eq!(relay_target_run(&env.app, &sender.id, &format!("bot:{}",target.id)).await.unwrap().unwrap().id, run);
    sqlx::query("UPDATE runs SET herdr_session='another-session' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    assert!(relay_target_run(&env.app, &sender.id, &pane).await.unwrap().is_none());
}

#[tokio::test]
async fn nr_claim_prompt_bot_selector_survives_rename() {
    let app = flow_tests::app().await;
    flow_tests::configure_responder(&app).await;
    sqlx::query("UPDATE bots SET name='renamed-role' WHERE id='patrol'").execute(&app.db).await.unwrap();
    sqlx::query("UPDATE bots SET name='AGM' WHERE id='w2'").execute(&app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,started_at,pane_id) VALUES('relay-run-patrol','patrol','running','idle',?,'patrol')").bind(db::now()).execute(&app.db).await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Token", "tok-w1".parse().unwrap());
    let body = RelayAnnounce { bot_id:"w1".into(), to_agent:"bot:patrol".into(), to_bot_id:None,
        to_run_id:None, delivery_id:Some("nr-claim-stable-selector".into()), text:"stable selector after rename".into(), ack:None, reply_to:None };
    let (code, Json(body)) = relay_announce(State(app.clone()), headers, axum::extract::Form(body)).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["routed"], "responder");
    assert_eq!(bot_requests(&app).await, 1);
}

async fn bot_requests(app: &Arc<App>) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='bot_request'").fetch_one(&app.db).await.unwrap()
}

/// 目標確定是 AGM、協調佇列卻寫不進去：以前 log 一句就 fallback，回 200 `{}`，shim 照原本流程把申請直接打進
/// AGM 的 pane——繞過 durable inbox、去重、wake／ack 語意，控制面也不知道走了旁路。要回 5xx，而且不 announce。
/// 查不出目標是不是 AGM（DB 讀失敗）也一樣。真的不是 AGM 的目標照舊直送。
#[tokio::test]
async fn a_request_for_agm_that_cannot_be_queued_is_not_sent_to_the_pane() {
    let app = flow_tests::app().await;
    flow_tests::configure_responder(&app).await;

    // 一般 bot：不是 AGM，照舊 announce、回 200 `{}`（shim 接著直送）。
    let (code, v) = announce(&app, "w2", "幫我看一下 relay-143-plain").await;
    assert_eq!((code, v), (StatusCode::OK, json!({"to_pane_id":"w2"})));
    assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "relay-run-w2", "幫我看一下 relay-143-plain").is_some(), "一般目標照舊記下直送");

    // 目標是 AGM，但協調佇列寫不進去。
    sqlx::query("CREATE TRIGGER test_inbox_down BEFORE INSERT ON supervisor_inbox BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
        .execute(&app.db)
        .await
        .unwrap();
    let (code, v) = announce(&app, "patrol", "請核准重建 relay-143-queue").await;
    assert!(code.is_server_error(), "寫不進佇列不能假裝成「不是 AGM」：{code} {v}");
    assert_eq!(v["routing_unavailable"], true, "{v}");
    assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "relay-run-patrol", "請核准重建 relay-143-queue").is_none(), "不能退回直接打進 pane");

    // DB 恢復後重試：只有一筆 durable 申請。
    sqlx::query("DROP TRIGGER test_inbox_down").execute(&app.db).await.unwrap();
    let (code, v) = announce(&app, "patrol", "請核准重建 relay-143-queue").await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(v["routed"], "responder", "{v}");
    assert_eq!(bot_requests(&app).await, 1);

    // 連「目標是不是 AGM」都查不出來（角色表讀不到）：一樣不能當成「不是」。
    sqlx::query("DROP TABLE supervisor_roles").execute(&app.db).await.unwrap();
    let (code, v) = announce(&app, "resp", "請核准重啟 relay-143-lookup").await;
    assert!(code.is_server_error(), "{code} {v}");
    assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, "relay-run-resp", "請核准重啟 relay-143-lookup").is_none());
}

/// #610：announce 的 managed-run 查詢讀不到時，不能先回 200 讓 shim 直送並永久略過 watchdog。
#[tokio::test]
async fn a_transient_relay_watch_admission_error_is_retryable_before_direct_announce() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    let to = "relay-610-unknown";
    let text = "請重試 relay-610-admission";
    sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES ('w1',?,'fixer','claude','cc0','tok-w1',?)")
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

    sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,?,?,'claude','target-token',?)")
        .bind(to).bind(&env.project_id).bind(to).bind(db::now()).execute(&app.db).await.unwrap();
    crate::testing::make_table_unreadable(&app, "runs").await;
    let (code, body) = announce(&app, to, text).await;
    let announced = crate::agent_relay::claim(crate::config::LOCAL_HOST, &format!("relay-run-{to}"), text);
    crate::testing::make_table_readable(&app, "runs").await;

    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "讀不到收件 run 時 shim 必須重試，不能直送：{body}");
    assert_eq!(body["watch_unavailable"], true, "回應要明確標記 watcher admission 失敗：{body}");
    assert_eq!(body["retryable"], true, "{body}");
    assert!(announced.is_none(), "watcher admission 失敗時不能先宣告、放 shim 直送");

    let (retry_code, retry_body) = announce(&app, to, text).await;
    assert_eq!((retry_code, retry_body), (StatusCode::OK, json!({"to_pane_id":to})), "資料庫恢復後可正常重試");
    assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, &format!("relay-run-{to}"), text).is_some(), "成功重試才記下直送 announce");
}

/// #1460：已移交的收件方無法驗證，不允許直送或建立回合。
#[tokio::test]
async fn an_announce_to_a_handed_off_agent_opens_no_turn_and_touches_no_pane() {
    let env = crate::testing::env().await;
    let app = env.app.clone();
    sqlx::query("INSERT INTO bots (id,project_id,name,kind,identity,hook_token,created_at) VALUES ('w1',?,'fixer','claude','cc0','tok-w1',?)")
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
    let recipient = crate::testing::claude_bot(&app, &env.project_id, "handoff-recipient").await;
    let run = crate::testing::fake_run(&app, &recipient.id).await;
    let text = "移交後的直送 relay-1460";

    // 移交前收件方認得到（同一組資料，所以下面的 None 只能是移交造成的）。
    assert!(crate::lifecycle::relay_watch::resolve(&app, "w1", &run).await.unwrap().is_some(), "移交前：收件方在這顆 daemon 底下");
    sqlx::query("UPDATE projects SET handed_off_to = 'agm-host' WHERE id = ?").bind(&env.project_id).execute(&app.db).await.unwrap();

    let calls_before = env.herdr.calls.lock().unwrap().len();
    let (code, v) = announce(&app, &recipient.id, text).await;
    assert_eq!(code, StatusCode::FORBIDDEN, "移交的收件方不能直送：{v}");
    assert_eq!(env.herdr.calls.lock().unwrap().len(), calls_before, "不替它呼叫 herdr：不盯 pane、不補 Enter");
    let external: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE origin='external'").fetch_one(&app.db).await.unwrap();
    assert_eq!(external, 0, "不替已移交的 bot 開 external 回合");
    assert!(crate::agent_relay::claim(crate::config::LOCAL_HOST, &run, text).is_none(), "移交後缺本機歸屬證據，不標記來源");
}
