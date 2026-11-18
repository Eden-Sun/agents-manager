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
    // kind 層面就沒有這個指令（不映射成別的）。
    assert!(!crate::lifecycle::slash::compact_slash_command("openai-api").is_some());
}

/// 模型清單不問 CLI、不打 `/v1/models`：固定是本機核准的 gpt-6-luna，effort 照 luna 的清單、預設 medium、沒有 fast。
#[tokio::test]
async fn the_openai_api_model_list_is_the_locally_approved_one() {
    let env = crate::testing::env().await;
    let q = ModelsQuery { kind: "openai-api".into(), host: None, refresh: None, identity: None };
    let Json(models) = get_models(State(env.app.clone()), Extension(RequestPrincipal::User), Query(q)).await.unwrap();
    assert_eq!((models["kind"].clone(), models["source"].clone()), (json!("openai-api"), json!("static")));
    let list = models["models"].as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0]["id"].clone(), list[0]["is_default"].clone(), list[0]["default_effort"].clone()), (json!("gpt-6-luna"), json!(true), json!("medium")));
    assert_eq!(list[0]["efforts"], json!(["none", "low", "medium", "high", "xhigh", "max"]));
    assert_eq!(list[0]["service_tiers"], json!([]));
    // claude-api 的清單不變。
    let q = ModelsQuery { kind: "claude-api".into(), host: None, refresh: None, identity: None };
    let Json(claude) = get_models(State(env.app.clone()), Extension(RequestPrincipal::User), Query(q)).await.unwrap();
    assert_eq!(claude["models"].as_array().unwrap().iter().map(|m| m["id"].as_str().unwrap()).collect::<Vec<_>>(), ["claude-opus-5-5", "claude-fable-5-2"]);
}
