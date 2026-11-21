use super::*;

async fn expert_fixture() -> (crate::testing::Env, String, String) {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "mgr").await;
    let advisor = crate::testing::claude_bot(&env.app, &env.project_id, "api-adv").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET kind='claude-api', role='advisor', model='claude-opus-5-5' WHERE id=?").bind(&advisor.id).execute(&env.app.db).await.unwrap();
    let mgr_run = crate::testing::fake_run(&env.app, &manager.id).await;
    sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:manager", env.project_id)).bind(mgr_run).execute(&env.app.db).await.unwrap();
    (env, manager.id, advisor.id)
}

#[tokio::test]
async fn claude_api_non_advisor_dispatch_is_forbidden() {
    let (env, manager, non_adv) = expert_fixture().await;
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&non_adv).execute(&env.app.db).await.unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", manager.parse().unwrap());
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body: PromptIn = serde_json::from_value(json!({"text":"consult","client_request_id":"c1","expect_result":true})).unwrap();
    let err = prompt_bot(State(env.app.clone()), Path(non_adv), headers, Extension(RequestPrincipal::Bot(manager)), Json(body)).await.unwrap_err();
    assert!(matches!(err, LcError::Forbidden(ref v) if v["error"] == "member_not_dispatchable"));
}

#[tokio::test]
async fn claude_api_advisor_refuses_attachments_and_non_expect_result() {
    let (env, manager, advisor) = expert_fixture().await;
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", manager.parse().unwrap());
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());

    let body: PromptIn = serde_json::from_value(json!({"text":"consult","client_request_id":"c1"})).unwrap();
    let err = prompt_bot(State(env.app.clone()), Path(advisor.clone()), headers.clone(), Extension(RequestPrincipal::Bot(manager.clone())), Json(body)).await.unwrap_err();
    assert!(matches!(err, LcError::Bad(ref s) if s.contains("api_native_prompt_unsupported")));

    let body: PromptIn = serde_json::from_value(json!({"text":"consult","client_request_id":"c2","expect_result":true,"attachments":["att-1"]})).unwrap();
    let err = prompt_bot(State(env.app.clone()), Path(advisor), headers, Extension(RequestPrincipal::Bot(manager)), Json(body)).await.unwrap_err();
    assert!(matches!(err, LcError::Bad(ref s) if s.contains("api_native_prompt_unsupported")));
}

#[tokio::test]
async fn claude_api_advisor_total_cap_error_fails_relay_request() {
    let (env, manager, advisor) = expert_fixture().await;
    let adv_run = crate::testing::fake_run(&env.app, &advisor).await;
    sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(adv_run).execute(&env.app.db).await.unwrap();

    env.app.cfg.update(move |cfg| {
        cfg.claude_api.api_bot.total_cap_usd = Some(1.0);
        cfg.claude_api.api_bot.total_cap_since = Some("2026-11-01T00:00:00Z".into());
        Ok(())
    }).await.unwrap();

    sqlx::query("INSERT INTO api_bot_attempts (id, bot_id, generation, ordinal, request_hash, requested_speed, state, estimated_microusd, reserved_microusd, pricing_version, created_at) VALUES (?, ?, 1, 0, 'h', 'standard', 'recorded', 1500000, 1500000, 'v1', ?)")
        .bind(db::ulid()).bind(&advisor).bind("2026-11-02T00:00:00Z")
        .execute(&env.app.db).await.unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", manager.parse().unwrap());
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body: PromptIn = serde_json::from_value(json!({"text":"consult","client_request_id":"cap-c1","expect_result":true})).unwrap();
    let res = prompt_bot(State(env.app.clone()), Path(advisor.clone()), headers, Extension(RequestPrincipal::Bot(manager)), Json(body)).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
    let val: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(val["delivery"], "failed");

    let (state, err_str): (String, Option<String>) = sqlx::query_as("SELECT state, error FROM relay_requests WHERE id=?")
        .bind(val["request_id"].as_str().unwrap()).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(state, "failed");
    assert!(err_str.unwrap().contains("api_total_cap_reached"));
}

#[tokio::test]
async fn claude_api_advisor_busy_queues_and_reconciles_when_idle() {
    let (env, manager, advisor) = expert_fixture().await;
    let adv_run = crate::testing::fake_run(&env.app, &advisor).await;
    sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&adv_run).execute(&env.app.db).await.unwrap();

    let conv = db::conversation_id(&env.app.db, &advisor).await.unwrap();
    let in_flight_turn = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)")
        .bind(&in_flight_turn).bind(&conv).bind(&adv_run).bind(db::now())
        .execute(&env.app.db).await.unwrap();

    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", manager.parse().unwrap());
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body: PromptIn = serde_json::from_value(json!({"text":"busy question","client_request_id":"busy-c1","expect_result":true})).unwrap();
    let res = prompt_bot(State(env.app.clone()), Path(advisor.clone()), headers, Extension(RequestPrincipal::Bot(manager.clone())), Json(body)).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
    let val: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(val["delivery"], "pending");

    let req_id = val["request_id"].as_str().unwrap();
    let state: String = sqlx::query_scalar("SELECT state FROM relay_requests WHERE id=?")
        .bind(req_id).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(state, "pending");

    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?")
        .bind(db::now()).bind(&in_flight_turn).execute(&env.app.db).await.unwrap();
}

#[tokio::test]
async fn claude_api_advisor_stopped_auto_starts_or_fails_request_on_start_error() {
    let (env, manager, advisor) = expert_fixture().await;
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", manager.parse().unwrap());
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body: PromptIn = serde_json::from_value(json!({"text":"consult stopped advisor","client_request_id":"c-stopped","expect_result":true})).unwrap();

    let res = prompt_bot(State(env.app.clone()), Path(advisor.clone()), headers, Extension(RequestPrincipal::Bot(manager)), Json(body)).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 65536).await.unwrap();
    let val: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(val["delivery"], "failed");
    assert_eq!(val["error"], "start_failed:api_bot_disabled");

    let (state, err_str): (String, Option<String>) = sqlx::query_as("SELECT state, error FROM relay_requests WHERE id=?")
        .bind(val["request_id"].as_str().unwrap()).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(state, "failed");
    assert_eq!(err_str.as_deref(), Some("start_failed:api_bot_disabled"));
}

/// 母子模式（parent_child）：parent 對 parent 要能溝通。同專案頂層 bot 請教沒有角色的頂層 API bot 會通過派工閘
/// （之後照 API bot 規則要 `expect_result`）；child 不能直接請教，照舊 403。
#[tokio::test]
async fn parent_child_top_level_bots_can_consult_an_api_bot() {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET mode='parent_child' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let parent = crate::testing::claude_bot(&env.app, &env.project_id, "pc-parent").await;
    let api = crate::testing::claude_bot(&env.app, &env.project_id, "pc-api").await;
    sqlx::query("UPDATE bots SET kind='openai-api', model='gpt-6-luna' WHERE id=?").bind(&api.id).execute(&env.app.db).await.unwrap();
    let child = crate::testing::claude_bot(&env.app, &env.project_id, "pc-child").await;
    sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&child.id).execute(&env.app.db).await.unwrap();
    let send = |from: String, crid: &'static str| {
        let app = env.app.clone();
        let to = api.id.clone();
        async move {
            let mut headers = HeaderMap::new();
            headers.insert("X-AM-Bot-Id", from.parse().unwrap());
            headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
            let body: PromptIn = serde_json::from_value(json!({"text":"consult","client_request_id":crid})).unwrap();
            prompt_bot(State(app), Path(to), headers, Extension(RequestPrincipal::Bot(from)), Json(body)).await.unwrap_err()
        }
    };
    let err = send(parent.id.clone(), "p1").await;
    assert!(matches!(&err, LcError::Bad(s) if s.contains("requires expect_result")), "parent passes the dispatch gate: {err:?}");
    let err = send(child.id.clone(), "c1").await;
    assert!(matches!(&err, LcError::Forbidden(v) if v["error"] == "member_not_dispatchable"), "child still refused: {err:?}");

    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let err = send(parent.id.clone(), "p2").await;
    assert!(matches!(&err, LcError::Forbidden(v) if v["error"] == "member_not_dispatchable"), "expert keeps advisor-only: {err:?}");
}

/// 使用者從網頁對 claude-api bot 夾圖片／PDF：不在派工閘被 `api_native_prompt_unsupported` 擋掉（native 路徑自己驗附件）。
#[tokio::test]
async fn a_user_prompt_to_a_claude_api_bot_may_carry_attachments() {
    let (env, _manager, advisor) = expert_fixture().await;
    let body: PromptIn = serde_json::from_value(json!({"text":"看圖","client_request_id":"u1","attachments":["att-1"]})).unwrap();
    let r = prompt_bot(State(env.app.clone()), Path(advisor), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await;
    assert!(!matches!(&r, Err(LcError::Bad(s)) if s.contains("api_native_prompt_unsupported")), "{:?}", r.err());
}
