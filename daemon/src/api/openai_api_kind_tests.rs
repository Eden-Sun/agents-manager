//! openai-api kind 在 HTTP 入口的行為（D2b-2）：v1 沒有的功能在送出前明確拒絕、模型清單是本機核准的那一份。
use super::*;

async fn openai_bot(env: &crate::testing::Env, name: &str) -> String {
    let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
    sqlx::query("UPDATE bots SET kind='openai-api', model='gpt-6-luna', effort='medium' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
    bot.id
}

async fn user_prompt(env: &crate::testing::Env, bot: &str, body: Value) -> Result<Response, LcError> {
    let body: PromptIn = serde_json::from_value(body).unwrap();
    prompt_bot(State(env.app.clone()), Path(bot.to_string()), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await
}

/// 帶附件的 prompt：400 `attachments_unsupported_for_kind`，沒有建立訊息或回合（不靜默丟掉附件）。
#[tokio::test]
async fn a_prompt_with_attachments_to_an_openai_api_bot_is_refused_by_name_before_anything_is_stored() {
    let env = crate::testing::env().await;
    let bot = openai_bot(&env, "oa-attach").await;
    let err = user_prompt(&env, &bot, json!({"text": "look at this", "client_request_id": "c-att", "attachments": ["att-1"]})).await.unwrap_err();
    assert!(matches!(&err, LcError::BadValue(v) if v["reason"] == "attachments_unsupported_for_kind" && v["kind"] == "openai-api" && v["sent"] == false), "{err:?}");
    let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages m JOIN conversations c ON c.id=m.conversation_id WHERE c.bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
    assert_eq!((stored, turns), (0, 0));
}

/// `/compact`：openai-api 沒有壓縮。當成 prompt 送、或打 compact 端點，都回 `compact_unsupported`，不會被當成一般文字送給模型。
#[tokio::test]
async fn compact_is_unsupported_for_an_openai_api_bot_both_as_a_prompt_and_as_the_endpoint() {
    let env = crate::testing::env().await;
    let bot = openai_bot(&env, "oa-compact").await;
    for text in ["/compact", "  /compact  ", "/compact keep the plan"] {
        let err = user_prompt(&env, &bot, json!({"text": text, "client_request_id": crate::db::ulid()})).await.unwrap_err();
        assert!(matches!(&err, LcError::BadValue(v) if v["error"] == "compact_unsupported" && v["kind"] == "openai-api"), "{text:?}: {err:?}");
    }
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns t JOIN conversations c ON c.id=t.conversation_id WHERE c.bot_id=?").bind(&bot).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(turns, 0, "沒有任何一則被排進去");
    crate::testing::fake_run(&env.app, &bot).await;
    let err = compact_bot(State(env.app.clone()), Path(bot.clone())).await.unwrap_err();
    assert!(matches!(&err, LcError::BadValue(v) if v["error"] == "compact_unsupported"), "{err:?}");
    assert!(!crate::lifecycle::slash::compact_slash_command("openai-api").is_some());
}

/// 模型清單不問 CLI、不打 `/v1/models`：本機核准的三個（astra 預設、sol、luna）；effort 依模型（astra／sol 不收 none）、預設 medium、沒有 fast。
#[tokio::test]
async fn the_openai_api_model_list_is_the_locally_approved_one() {
    let env = crate::testing::env().await;
    let q = ModelsQuery { kind: "openai-api".into(), host: None, refresh: None, identity: None };
    let Json(models) = get_models(State(env.app.clone()), Extension(RequestPrincipal::User), Query(q)).await.unwrap();
    assert_eq!((models["kind"].clone(), models["source"].clone()), (json!("openai-api"), json!("static")));
    let list = models["models"].as_array().unwrap();
    let ids: Vec<&str> = list.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["gpt-6.2-astra", "gpt-6.2-sol", "gpt-6-luna"]);
    assert_eq!((list[0]["is_default"].clone(), list[0]["default_effort"].clone()), (json!(true), json!("medium")));
    assert_eq!(list[0]["efforts"], json!(["low", "medium", "high", "xhigh", "max"]), "astra 不收 none");
    assert_eq!(list[1]["efforts"], json!(["low", "medium", "high", "xhigh", "max"]), "sol 不收 none");
    assert_eq!(list[2]["efforts"], json!(["none", "low", "medium", "high", "xhigh", "max"]), "luna 仍收 none");
    assert!(list.iter().all(|m| m["service_tiers"] == json!([])));
    let q = ModelsQuery { kind: "claude-api".into(), host: None, refresh: None, identity: None };
    let Json(claude) = get_models(State(env.app.clone()), Extension(RequestPrincipal::User), Query(q)).await.unwrap();
    assert_eq!(claude["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect::<Vec<_>>(), ["claude-opus-5-5", "claude-fable-5-2"]);
}

/// 專家模式專案：一個管理者（假 run）、一個 openai-api 顧問、一個 claude-api 顧問。
async fn advisors() -> (crate::testing::Env, String, String, String) {
    let env = crate::testing::env().await;
    sqlx::query("UPDATE projects SET mode='expert' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
    let manager = crate::testing::claude_bot(&env.app, &env.project_id, "mgr").await;
    sqlx::query("UPDATE bots SET role='manager' WHERE id=?").bind(&manager.id).execute(&env.app.db).await.unwrap();
    let run = crate::testing::fake_run(&env.app, &manager.id).await;
    sqlx::query("UPDATE runs SET runtime_role_policy=? WHERE id=?").bind(format!("{}:local:expert:manager", env.project_id)).bind(run).execute(&env.app.db).await.unwrap();
    let openai = openai_bot(&env, "oa-adv").await;
    let claude = crate::testing::claude_bot(&env.app, &env.project_id, "ca-adv").await.id;
    sqlx::query("UPDATE bots SET role='advisor' WHERE id=?").bind(&openai).execute(&env.app.db).await.unwrap();
    sqlx::query("UPDATE bots SET kind='claude-api', role='advisor', model='claude-opus-5-5' WHERE id=?").bind(&claude).execute(&env.app.db).await.unwrap();
    (env, manager.id, openai, claude)
}

async fn consult(env: &crate::testing::Env, manager: &str, advisor: &str, body: Value) -> Result<Value, LcError> {
    let mut headers = HeaderMap::new();
    headers.insert("X-AM-Bot-Id", manager.parse().unwrap());
    headers.insert("X-AM-Bot-Token", "tok".parse().unwrap());
    let body: PromptIn = serde_json::from_value(body).unwrap();
    let res = prompt_bot(State(env.app.clone()), Path(advisor.to_string()), headers, Extension(RequestPrincipal::Bot(manager.to_string())), Json(body)).await?;
    Ok(serde_json::from_slice(&axum::body::to_bytes(res.into_body(), 65536).await.unwrap()).unwrap())
}

async fn spend(env: &crate::testing::Env, bot: &str, provider: &str, microusd: i64) {
    sqlx::query("INSERT INTO api_bot_attempts (id, bot_id, generation, ordinal, request_hash, requested_speed, state, estimated_microusd, reserved_microusd, pricing_version, provider, created_at) VALUES (?, ?, 1, 0, 'h', 'standard', 'recorded', ?, ?, 'v1', ?, '2026-11-02T00:00:00Z')")
        .bind(db::ulid()).bind(bot).bind(microusd).bind(microusd).bind(provider).execute(&env.app.db).await.unwrap();
}

/// openai-api 的顧問跟 claude-api 的顧問同一套派工規則：非顧問不可派、要 `expect_result`、不收附件。
#[tokio::test]
async fn an_openai_api_advisor_follows_the_same_dispatch_rules_as_a_claude_api_one() {
    let (env, manager, openai, _claude) = advisors().await;
    let err = consult(&env, &manager, &openai, json!({"text": "consult", "client_request_id": "n1"})).await.unwrap_err();
    assert!(matches!(&err, LcError::Bad(s) if s.contains("api_native_prompt_unsupported")), "{err:?}");
    let err = consult(&env, &manager, &openai, json!({"text": "consult", "client_request_id": "n2", "expect_result": true, "attachments": ["att-1"]})).await.unwrap_err();
    assert!(matches!(&err, LcError::BadValue(v) if v["reason"] == "attachments_unsupported_for_kind"), "{err:?}");
    sqlx::query("UPDATE bots SET role='executor' WHERE id=?").bind(&openai).execute(&env.app.db).await.unwrap();
    let err = consult(&env, &manager, &openai, json!({"text": "consult", "client_request_id": "n3", "expect_result": true})).await.unwrap_err();
    assert!(matches!(&err, LcError::Forbidden(v) if v["error"] == "member_not_dispatchable" && v["message"].as_str().unwrap().contains("openai-api")), "{err:?}");
}

/// 派工前的總上限各家算各的：OpenAI 的額度用完，請教 openai-api 顧問當場失敗（`api_total_cap_reached`），
/// 請教 claude-api 顧問不受影響；反過來 Claude 用完也不擋 OpenAI。（沒被上限擋的會走到下一關：這裡是啟動檢查。）
#[tokio::test]
async fn the_pre_dispatch_total_cap_is_per_provider_and_one_does_not_block_the_other() {
    let (env, manager, openai, claude) = advisors().await;
    env.app.cfg.update(|cfg| {
        cfg.openai_api.api_bot.total_cap_usd = Some(1.0);
        cfg.openai_api.api_bot.total_cap_since = Some("2026-11-01T00:00:00Z".into());
        cfg.claude_api.api_bot.total_cap_usd = Some(1.0);
        cfg.claude_api.api_bot.total_cap_since = Some("2026-11-01T00:00:00Z".into());
        Ok(())
    }).await.unwrap();
    spend(&env, &openai, "openai", 1_500_000).await;
    let blocked = consult(&env, &manager, &openai, json!({"text": "q", "client_request_id": "cap-1", "expect_result": true})).await.unwrap();
    assert_eq!(blocked["delivery"], "failed");
    assert_eq!(blocked["error"], "api_total_cap_reached:cap=1:spent=1.5", "{blocked}");
    let error: Option<String> = sqlx::query_scalar("SELECT error FROM relay_requests WHERE id=?").bind(blocked["request_id"].as_str().unwrap()).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(error.as_deref(), Some("api_total_cap_reached:cap=1:spent=1.5"));
    let through = consult(&env, &manager, &claude, json!({"text": "q", "client_request_id": "cap-2", "expect_result": true})).await.unwrap();
    assert_eq!(through["error"], "start_failed:api_bot_disabled", "Claude 沒被 OpenAI 的上限擋，走到了啟動檢查：{through}");

    sqlx::query("DELETE FROM api_bot_attempts").execute(&env.app.db).await.unwrap();
    spend(&env, &claude, "anthropic", 1_500_000).await;
    let blocked = consult(&env, &manager, &claude, json!({"text": "q", "client_request_id": "cap-3", "expect_result": true})).await.unwrap();
    assert!(blocked["error"].as_str().unwrap().starts_with("api_total_cap_reached"), "{blocked}");
    let through = consult(&env, &manager, &openai, json!({"text": "q", "client_request_id": "cap-4", "expect_result": true})).await.unwrap();
    assert_eq!(through["error"], "start_failed:openai_not_configured", "OpenAI 沒被 Claude 的上限擋：{through}");
}

/// 排隊中的請求（顧問當時忙）在背景補送時也用同一條規則：OpenAI 的額度用完就把請求標失敗，不送。
#[tokio::test]
async fn a_queued_request_to_an_openai_api_advisor_is_failed_by_the_sweep_when_the_openai_cap_is_exhausted() {
    let (env, manager, openai, _claude) = advisors().await;
    let run = crate::testing::fake_run(&env.app, &openai).await;
    sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
    let conv = db::conversation_id(&env.app.db, &openai).await.unwrap();
    let busy = db::ulid();
    sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'in_flight', 'ok', ?)").bind(&busy).bind(&conv).bind(&run).bind(db::now()).execute(&env.app.db).await.unwrap();
    let queued = consult(&env, &manager, &openai, json!({"text": "later", "client_request_id": "q-1", "expect_result": true})).await.unwrap();
    assert_eq!(queued["delivery"], "pending", "{queued}");
    let request = queued["request_id"].as_str().unwrap().to_string();

    sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?").bind(db::now()).bind(&busy).execute(&env.app.db).await.unwrap();
    env.app.cfg.update(|cfg| {
        cfg.openai_api.api_bot.total_cap_usd = Some(1.0);
        cfg.openai_api.api_bot.total_cap_since = Some("2026-11-01T00:00:00Z".into());
        Ok(())
    }).await.unwrap();
    spend(&env, &openai, "openai", 2_000_000).await;
    crate::runners::relay_requests::sweep(&env.app).await;
    let (state, error): (String, Option<String>) = sqlx::query_as("SELECT state, error FROM relay_requests WHERE id=?").bind(&request).fetch_one(&env.app.db).await.unwrap();
    assert_eq!((state.as_str(), error.as_deref()), ("failed", Some("api_total_cap_reached:cap=1:spent=2")));
    let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND id<>?").bind(&conv).bind(&busy).fetch_one(&env.app.db).await.unwrap();
    assert_eq!(turns, 0, "沒有送出任何回合");
}

/// 用量端點：OpenAI 的 usage 欄位（`cached_tokens`／`cache_write_tokens`，而且 `input_tokens` 含它們）換成跟 Claude 同一種語意，
/// 命中率才對；沒有壓縮門檻、沒有快取倒數。Claude 的帳照原本的欄位算，不受影響。
#[tokio::test]
async fn api_usage_reads_openai_cache_fields_in_the_same_terms_as_claude() {
    let env = crate::testing::env().await;
    let bot = openai_bot(&env, "oa-usage").await;
    async fn turn(env: &crate::testing::Env, bot: &str) -> String {
        let run = crate::testing::fake_run(&env.app, bot).await;
        let conv = db::conversation_id(&env.app.db, bot).await.unwrap();
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?, ?, ?, 'web', 'completed', 'ok', ?)").bind(&id).bind(&conv).bind(&run).bind(db::now()).execute(&env.app.db).await.unwrap();
        id
    }
    let t1 = turn(&env, &bot).await;
    let usage = json!({"input_tokens": 1000, "cached_tokens": 600, "cache_write_tokens": 100, "cache_write_12h_tokens": 0, "output_tokens": 50, "reasoning_tokens": 20, "service_tier": "default"});
    sqlx::query("INSERT INTO api_bot_attempts (id, bot_id, turn_id, generation, ordinal, request_hash, requested_speed, state, estimated_microusd, reserved_microusd, pricing_version, provider, usage_json, created_at, finished_at) VALUES (?, ?, ?, 1, 0, 'h', 'standard', 'recorded', 74, 200, 'v1', 'openai', ?, ?, ?)")
        .bind(db::ulid()).bind(&bot).bind(&t1).bind(usage.to_string()).bind(db::now()).bind(db::now()).execute(&env.app.db).await.unwrap();
    let Json(out) = api_bot_usage(State(env.app.clone()), Path(bot.clone()), Extension(RequestPrincipal::User)).await.unwrap();
    assert_eq!(out["tokens"], json!({"input": 300, "output": out["tokens"]["output"].clone(), "cache_read": 600, "cache_creation": 100}), "{out}");
    assert_eq!(out["cache_hit_rate"], json!(0.6));
    assert_eq!((out["last"]["input_tokens"].clone(), out["last"]["cache_read_input_tokens"].clone(), out["last"]["cache_creation_input_tokens"].clone()), (json!(300), json!(600), json!(100)));
    assert_eq!(out["context"]["tokens"], 1000, "context 大小＝整個輸入");
    assert!(out["context"]["compact_at"].is_null() && out["cache"]["ttl_secs"].is_null(), "{out}");
    let turns = crate::api_bot_turn_cost::recent_turns(&env.app.db, &bot).await.unwrap();
    assert_eq!((turns[0]["input_tokens"].clone(), turns[0]["cache_read_tokens"].clone(), turns[0]["cache_write_tokens"].clone()), (json!(300), json!(600), json!(100)), "{}", turns[0]);

    assert_eq!(turns[0]["reasoning_tokens"], 20, "{}", turns[0]);
    let more = json!({"input_tokens": 10, "cached_tokens": 0, "cache_write_tokens": 0, "output_tokens": 40, "reasoning_tokens": 15});
    for (ordinal, state) in [(1, "recorded"), (2, "unknown")] {
        sqlx::query("INSERT INTO api_bot_attempts (id, bot_id, turn_id, generation, ordinal, request_hash, requested_speed, state, estimated_microusd, reserved_microusd, pricing_version, provider, usage_json, created_at, finished_at) VALUES (?, ?, ?, 1, ?, 'h', 'standard', ?, 21, 200, 'v1', 'openai', ?, ?, ?)")
            .bind(db::ulid()).bind(&bot).bind(&t1).bind(ordinal).bind(state).bind(more.to_string()).bind(db::now()).bind(db::now()).execute(&env.app.db).await.unwrap();
    }
    let turns = crate::api_bot_turn_cost::recent_turns(&env.app.db, &bot).await.unwrap();
    assert_eq!((turns[0]["reasoning_tokens"].clone(), turns[0]["output_tokens"].clone(), turns[0]["cost_usd"].clone()), (json!(35), json!(90), json!(0.000095)), "{}", turns[0]);
    let Json(via_api) = api_bot_usage(State(env.app.clone()), Path(bot.clone()), Extension(RequestPrincipal::User)).await.unwrap();
    assert_eq!(via_api["turns"][0]["reasoning_tokens"], 35);

    let claude = crate::testing::claude_bot(&env.app, &env.project_id, "ca-usage").await.id;
    sqlx::query("UPDATE bots SET kind='claude-api' WHERE id=?").bind(&claude).execute(&env.app.db).await.unwrap();
    let t2 = turn(&env, &claude).await;
    let usage = json!({"input_tokens": 300, "cache_read_input_tokens": 600, "cache_creation_input_tokens": 100, "output_tokens": 50});
    sqlx::query("INSERT INTO api_bot_attempts (id, bot_id, turn_id, generation, ordinal, request_hash, requested_speed, state, estimated_microusd, reserved_microusd, pricing_version, usage_json, created_at, finished_at) VALUES (?, ?, ?, 1, 0, 'h', 'standard', 'recorded', 74, 200, 'v1', ?, ?, ?)")
        .bind(db::ulid()).bind(&claude).bind(&t2).bind(usage.to_string()).bind(db::now()).bind(db::now()).execute(&env.app.db).await.unwrap();
    let Json(same) = api_bot_usage(State(env.app.clone()), Path(claude), Extension(RequestPrincipal::User)).await.unwrap();
    assert!(same["turns"][0].get("reasoning_tokens").is_none(), "Claude 的回合不帶 reasoning_tokens：{}", same["turns"][0]);
    assert_eq!((same["tokens"]["input"].clone(), same["tokens"]["cache_read"].clone(), same["tokens"]["cache_creation"].clone(), same["cache_hit_rate"].clone()), (json!(300), json!(600), json!(100), json!(0.6)));
}
