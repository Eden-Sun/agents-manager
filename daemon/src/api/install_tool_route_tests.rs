use super::*;
use axum::response::IntoResponse;

async fn call(app: &Arc<App>, host: &str, body: &str, principal: RequestPrincipal) -> (StatusCode, Value) {
    let b: InstallTool = serde_json::from_str(body).unwrap();
    let resp = match install_tool(State(app.clone()), Path(host.to_string()), Extension(principal), Json(b)).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// issue #1931：`via_bot_id` 可省；省略時不碰 bot 表，直接走 shell 安裝的驗證（agy／亂寫的 kind 400、未知主機 404、
/// 同一 (host, kind) 已在裝 409）。帶了 `via_bot_id` 仍是借 bot 的舊行為（bot 不存在 404）。
#[tokio::test]
async fn via_bot_id_is_optional_and_the_shell_path_validates_before_opening_a_pane() {
    let e = crate::testing::env().await;
    let user = || RequestPrincipal::User;
    let (st, body) = call(&e.app, crate::config::LOCAL_HOST, r#"{"kind":"claude"}"#, RequestPrincipal::Bot("b1".into())).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    for kind in ["agy", "rm -rf /", ""] {
        let (st, body) = call(&e.app, crate::config::LOCAL_HOST, &json!({"kind": kind}).to_string(), user()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{kind}: {body}");
    }
    for install_id in ["", "bad id", "$(touch x)"] {
        let (st, _) = call(&e.app, crate::config::LOCAL_HOST, &json!({"kind": "codex", "install_id": install_id}).to_string(), user()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }
    let (st, _) = call(&e.app, "no-such-host", r#"{"kind":"codex"}"#, user()).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(&e.app, "no-such-host", r#"{"kind":"codex","via_bot_id":""}"#, user()).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "空字串等於省略");
    let (st, _) = call(&e.app, crate::config::LOCAL_HOST, r#"{"kind":"codex","via_bot_id":"no-such-bot"}"#, user()).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "帶了 via_bot_id 走借 bot 的舊路徑");

    let _busy = crate::tool_install_shell::Slot::take(crate::config::LOCAL_HOST, "grok").unwrap();
    let (st, body) = call(&e.app, crate::config::LOCAL_HOST, r#"{"kind":"grok"}"#, user()).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("tool_install_in_progress")), "{body}");
    assert_eq!((body["host"].as_str(), body["kind"].as_str()), (Some(crate::config::LOCAL_HOST), Some("grok")));
}
