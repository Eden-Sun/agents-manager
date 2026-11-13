use super::*;
use crate::testing::{env, Env};

async fn create(e: &Env, body: Value) -> Result<Value, LcError> {
    let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(body).unwrap())).await?;
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    Ok(serde_json::from_slice(&bytes).unwrap())
}

async fn bots_in_config(e: &Env) -> usize {
    e.app.cfg.get().await.projects[0].bots.len()
}

/// #1944：claude-api 可建；模型固定、effort 預設 high、CLI 專用欄位與遠端專案一律 400。
#[tokio::test]
async fn creating_a_claude_api_bot_pins_model_defaults_effort_and_refuses_cli_fields() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let ok = create(&e, json!({"name": "apibot", "kind": "claude-api", "fast": true})).await.unwrap();
    let bot = db::bot(&e.app.db, ok["bot_id"].as_str().unwrap()).await.unwrap().unwrap();
    assert_eq!(bot.kind, "claude-api");
    assert_eq!(bot.model.as_deref(), Some("claude-opus-5-5"));
    assert_eq!(bot.effort.as_deref(), Some("high"));
    assert_eq!(bot.fast, 1);
    let ok = create(&e, json!({"name": "apibot2", "kind": "claude-api", "model": "claude-opus-5-5", "effort": "low"})).await.unwrap();
    let bot = db::bot(&e.app.db, ok["bot_id"].as_str().unwrap()).await.unwrap().unwrap();
    assert_eq!((bot.effort.as_deref(), bot.fast), (Some("low"), 0));
    for body in [
        json!({"name": "x1", "kind": "claude-api", "model": "claude-haiku-5-5"}),
        json!({"name": "x2", "kind": "claude-api", "effort": "ultra"}),
        json!({"name": "x3", "kind": "claude-api", "args": ["--foo"]}),
        json!({"name": "x4", "kind": "claude-api", "identity": "cc1"}),
        json!({"name": "x5", "kind": "claude-api", "autostart": true}),
        json!({"name": "x6", "kind": "claude-api", "share_profile": "restricted"}),
    ] {
        assert!(matches!(create(&e, body.clone()).await, Err(LcError::Bad(_) | LcError::BadValue(_))), "{body}");
    }
    assert_eq!(e.app.cfg.get().await.projects[0].bots.len(), 2, "拒絕的請求沒寫進 config");
    assert!(matches!(create(&e, json!({"name": "x7", "kind": "gemini"})).await, Err(LcError::Bad(_))));
}

#[test]
fn the_claude_api_model_list_is_static() {
    let v = api_bot_models();
    assert_eq!(v["models"][0]["id"], "claude-opus-5-5");
    assert_eq!(v["models"][0]["efforts"], json!(["low", "medium", "high", "xhigh", "max"]));
}

#[tokio::test]
async fn a_fable_api_bot_keeps_its_model_in_config_and_database() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let out = create(&e, json!({"name":"fable-api","kind":"claude-api","model":"claude-fable-5-2"})).await.unwrap();
    let id = out["bot_id"].as_str().unwrap();
    assert_eq!(db::bot(&e.app.db,id).await.unwrap().unwrap().model.as_deref(), Some("claude-fable-5-2"));
    assert_eq!(e.app.cfg.get().await.projects[0].bots.iter().find(|b| b.name=="fable-api").unwrap().model.as_deref(), Some("claude-fable-5-2"));
    assert!(api_bot_models()["models"].as_array().unwrap().iter().any(|m| m["id"]=="claude-fable-5-2" && m["is_default"]==false));
    let invalid = patch_bot(State(e.app.clone()), Path(id.into()), Extension(RequestPrincipal::User), Json(serde_json::from_value(json!({"model":"unapproved"})).unwrap())).await;
    assert!(matches!(invalid, Err(LcError::BadValue(_))));
    assert_eq!(db::bot(&e.app.db,id).await.unwrap().unwrap().model.as_deref(), Some("claude-fable-5-2"));
    for model in ["claude-opus-5-5", "claude-fable-5-2"] {
        assert!(patch_bot(State(e.app.clone()), Path(id.into()), Extension(RequestPrincipal::User), Json(serde_json::from_value(json!({"model":model})).unwrap())).await.is_ok());
        assert_eq!(db::bot(&e.app.db,id).await.unwrap().unwrap().model.as_deref(), Some(model));
    }
}

/// #654：child 占著 `review` 時，再建同名要 409，config 不能先寫進去。
#[tokio::test]
async fn creating_a_bot_with_a_childs_name_conflicts_and_the_next_create_still_works() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    create(&e, json!({"name": "alfa", "kind": "claude"})).await.unwrap();
    sqlx::query(
        "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, managed_by, created_at)
             VALUES ('child-review',?,'review','claude','[]',0,1,'tok','child',?)",
    )
    .bind(&e.project_id)
    .bind(db::now())
    .execute(&e.app.db)
    .await
    .unwrap();
    let err = create(&e, json!({"name": "review", "kind": "claude"})).await.unwrap_err();
    match err {
        LcError::Conflict(v) => assert_eq!(v["reason"], "bot name already in use"),
        other => panic!("要 409，不是投影 502：{other:?}"),
    }
    let names: Vec<String> = e.app.cfg.get().await.projects[0].bots.iter().map(|b| b.name.clone()).collect();
    assert_eq!(names, vec!["alfa".to_string()]);
    let bravo = create(&e, json!({"name": "bravo", "kind": "claude"})).await.unwrap();
    assert!(db::bot(&e.app.db, bravo["bot_id"].as_str().unwrap()).await.unwrap().is_some());
}

#[tokio::test]
async fn a_resent_create_with_the_same_request_id_returns_the_original_bot() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let body = |name: &str| json!({"name": name, "name_auto": true, "kind": "claude", "client_request_id": "req-1"});
    let first = create(&e, body("cc1-1")).await.unwrap();
    // 回應遺失後重送：瀏覽器清單已同步，`nextName` 算出來的名字變成 cc1-2——名字只是 name_auto 的提示，不算請求內容。
    let again = create(&e, body("cc1-2")).await.unwrap();
    assert_eq!((again["bot_id"].clone(), again["name"].clone()), (first["bot_id"].clone(), first["name"].clone()), "重送拿回原本那顆：{again}");
    assert_eq!(bots_in_config(&e).await, 1, "只有一顆 bot");
    // 持久在 config.toml（瀏覽器重整、daemon 重啟都在）。
    let text = std::fs::read_to_string(&e.app.cfg.path).unwrap();
    assert!(text.contains("req-1"), "request id 要寫進 config.toml：{text}");
}

#[tokio::test]
async fn the_same_request_id_with_a_different_request_is_a_409() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    create(&e, json!({"name": "a", "kind": "claude", "client_request_id": "req-2"})).await.unwrap();
    let err = create(&e, json!({"name": "b", "kind": "codex", "client_request_id": "req-2"})).await.unwrap_err();
    assert!(matches!(err, LcError::Conflict(_)), "同 id 換 kind：{err:?}");
    assert_eq!(bots_in_config(&e).await, 1);
}

#[tokio::test]
async fn a_new_request_id_is_a_deliberate_second_create() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let a = create(&e, json!({"name": "cc1-1", "name_auto": true, "kind": "claude", "client_request_id": "req-3"})).await.unwrap();
    let b = create(&e, json!({"name": "cc1-1", "name_auto": true, "kind": "claude", "client_request_id": "req-4"})).await.unwrap();
    assert_ne!(a["bot_id"], b["bot_id"]);
    assert_eq!(b["name"], "cc1-2", "新的請求照常用 name_auto 往後找");
    assert_eq!(bots_in_config(&e).await, 2);
}

#[tokio::test]
async fn a_create_without_a_request_id_behaves_as_before() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    create(&e, json!({"name": "x", "name_auto": true, "kind": "claude"})).await.unwrap();
    create(&e, json!({"name": "x", "name_auto": true, "kind": "claude"})).await.unwrap();
    assert_eq!(bots_in_config(&e).await, 2);
}
