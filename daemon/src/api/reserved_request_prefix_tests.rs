use super::*;

/// `withdrawn:` 跟 `relay-request:`／`relay-result:` 一樣是 daemon 保留的 client_request_id 前綴（專家派工撤回紀錄用）。
#[tokio::test]
async fn prompt_refuses_daemon_reserved_request_ids() {
    let env = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&env.app, &env.project_id, "reserved-prefix").await;
    for crid in ["relay-request:x", "relay-result:x", "withdrawn:x"] {
        let body: PromptIn = serde_json::from_value(json!({"text": "hi", "client_request_id": crid})).unwrap();
        let r = prompt_bot(State(env.app.clone()), Path(bot.id.clone()), HeaderMap::new(), Extension(RequestPrincipal::User), Json(body)).await;
        assert!(matches!(&r, Err(LcError::Bad(m)) if m.contains("reserved daemon prefix")), "{crid}: {:?}", r.err());
    }
}
