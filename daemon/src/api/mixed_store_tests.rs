use super::*;
use crate::testing::{env, Env};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn add(e: &Env, name: &str) -> String {
    let res = create_bot(State(e.app.clone()), Path(e.project_id.clone()), Json(serde_json::from_value(json!({"name": name, "kind": "claude"})).unwrap())).await.unwrap();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["bot_id"].as_str().unwrap().to_string()
}

async fn patch(e: &Env, id: &str, body: Value) -> Result<Response, LcError> {
    patch_bot(State(e.app.clone()), Path(id.to_string()), Extension(RequestPrincipal::User), Json(serde_json::from_value(body).unwrap())).await
}

async fn pin_state(e: &Env, id: &str) -> (i64, i64) {
    sqlx::query_as("SELECT is_primary, primary_position FROM bots WHERE id = ?").bind(id).fetch_one(&e.app.db).await.unwrap()
}

async fn model_of(e: &Env, id: &str) -> Option<String> {
    e.app.cfg.get().await.projects[0].bots.iter().find(|b| b.id.as_deref() == Some(id)).unwrap().model.clone()
}

async fn child(e: &Env, name: &str) -> String {
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET managed_by = 'child' WHERE id = ?")
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
    bot.id
}

async fn patch_http(e: &Env, id: &str, body: Value) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = super::router(e.app.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let payload = serde_json::to_string(&body).unwrap();
    let request = format!(
        "PATCH /api/bots/{id} HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        e.app.ui_token,
        payload.len(),
    );
    let mut connection = tokio::net::TcpStream::connect(addr).await.unwrap();
    connection.write_all(request.as_bytes()).await.unwrap();
    let mut response = String::new();
    connection.read_to_string(&mut response).await.unwrap();
    server.abort();
    response
}

async fn child_fields(e: &Env, id: &str) -> (i64, i64, Option<String>) {
    sqlx::query_as("SELECT is_primary, primary_position, model FROM bots WHERE id = ?")
        .bind(id)
        .fetch_one(&e.app.db)
        .await
        .unwrap()
}

#[tokio::test]
async fn child_mixed_patch_failure_leaves_pin_and_model_unchanged_and_emits_no_change_event() {
    let e = env().await;
    let id = child(&e, "atomic-failure").await;
    let before = child_fields(&e, &id).await;
    assert_eq!(before.0, 0);
    assert_eq!(before.1, 0);
    sqlx::query(
        "CREATE TRIGGER fail_child_model_patch BEFORE UPDATE OF model ON bots
             BEGIN SELECT RAISE(ABORT, 'injected model write failure'); END",
    )
    .execute(&e.app.db)
    .await
    .unwrap();
    let mut events = e.app.subscribe();

    let response = patch_http(&e, &id, json!({"primary": true, "model": "opus"})).await;
    assert!(response.starts_with("HTTP/1.1 502"), "失敗的 PATCH 要回 upstream 錯誤：{response}");
    assert_eq!(child_fields(&e, &id).await, before, "同一條 UPDATE 失敗時所有欄位都不變");
    while let Ok(event) = events.try_recv() {
        assert!(
            event.kind != "bot_changed" || event.data["bot_id"] != id,
            "失敗的 PATCH 不能送出 bot_changed：{event:?}"
        );
    }
}

#[tokio::test]
async fn child_mixed_patch_applies_pin_and_model_and_single_field_patches_still_work() {
    let e = env().await;
    let id = child(&e, "atomic-success").await;

    let response = patch_http(&e, &id, json!({"primary": true, "model": "opus"})).await;
    assert!(response.starts_with("HTTP/1.1 200"), "有效的 child 混送要成功：{response}");
    let (pinned, position, model) = child_fields(&e, &id).await;
    assert_eq!((pinned, position), (1, 1), "primary 與位置要一併生效");
    assert_eq!(model.as_deref(), Some("claude-opus-5-5"), "model 要一併生效");

    let response = patch_http(&e, &id, json!({"primary": false})).await;
    assert!(response.starts_with("HTTP/1.1 200"), "child 單獨取消釘選要成功：{response}");
    assert_eq!(child_fields(&e, &id).await.0, 0);
    assert_eq!(child_fields(&e, &id).await.1, 1, "取消釘選保留原位置");

    let response = patch_http(&e, &id, json!({"model": "sonnet"})).await;
    assert!(response.starts_with("HTTP/1.1 200"), "child 單獨改 model 要成功：{response}");
    assert_eq!(child_fields(&e, &id).await.2.as_deref(), Some("sonnet"));
}

#[tokio::test]
async fn a_patch_mixing_primary_with_a_config_field_is_rejected_and_changes_nothing() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let id = add(&e, "mix").await;
    let before = model_of(&e, &id).await;
    let err = patch(&e, &id, json!({"primary": true, "model": "opus"})).await.unwrap_err();
    assert!(matches!(err, LcError::Bad(_)), "混送要 400：{err:?}");
    assert_eq!(pin_state(&e, &id).await, (0, 0), "primary 不能單獨生效");
    assert_eq!(model_of(&e, &id).await, before, "config 欄位也沒動");
    // 分開送各自照常成功。
    patch(&e, &id, json!({"primary": true})).await.unwrap();
    assert_eq!(pin_state(&e, &id).await.0, 1);
    patch(&e, &id, json!({"model": "opus"})).await.unwrap();
    assert_eq!(model_of(&e, &id).await.as_deref(), Some("claude-opus-5-5"));
}

#[tokio::test]
async fn an_order_request_mixing_primary_with_config_orders_is_rejected_and_changes_nothing() {
    let e = env().await;
    crate::testing::seed_project_config(&e).await;
    let a = add(&e, "a").await;
    let b = add(&e, "b").await;
    let order_of = |e: &Env| {
        let app = e.app.clone();
        async move { app.cfg.get().await.projects[0].bots.iter().map(|x| x.id.clone().unwrap()).collect::<Vec<_>>() }
    };
    let before = order_of(&e).await;
    let mut bots = BTreeMap::new();
    bots.insert(e.project_id.clone(), vec![b.clone(), a.clone()]);
    let mixed = SetOrder { projects: None, bots: Some(bots), primary: Some(vec![b.clone(), a.clone()]) };
    let err = set_order(State(e.app.clone()), Json(mixed)).await.unwrap_err();
    assert!(matches!(err, LcError::Bad(_)), "混送要 400：{err:?}");
    assert_eq!(order_of(&e).await, before, "config 順序沒動");
    assert_eq!(pin_state(&e, &a).await.1, 0, "primary_position 也沒寫");
}
