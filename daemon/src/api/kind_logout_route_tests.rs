use super::*;
use axum::response::IntoResponse;

async fn call(app: &Arc<App>, host: &str, kind: &str, principal: RequestPrincipal) -> (StatusCode, Value) {
    let resp = match logout_kind_default(State(app.clone()), Path((host.to_string(), kind.to_string())), Extension(principal)).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 預設帳號登出（SPEC §16.9）：只有使用者能按；kind 不認得 400；沒這台主機 404；CLI 不在 PATH 409（不開 pane）。
/// agy 走既有的刪憑證檔路徑（沒這台主機一樣 404）。
#[tokio::test]
async fn the_default_account_logout_is_user_only_and_validates_kind_host_and_cli() {
    let e = crate::testing::env().await;
    let local = crate::config::LOCAL_HOST;
    let (st, body) = call(&e.app, local, "grok", RequestPrincipal::Bot("b1".into())).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    let (st, _) = call(&e.app, local, "nope", RequestPrincipal::User).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = call(&e.app, "no-such-host", "grok", RequestPrincipal::User).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = call(&e.app, "no-such-host", "agy", RequestPrincipal::User).await;
    assert_eq!(st, StatusCode::NOT_FOUND, "agy 走刪憑證檔那條，一樣先認主機");
    let (st, body) = call(&e.app, local, "grok", RequestPrincipal::User).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::CONFLICT, Some("identity_login_unavailable")), "測試環境沒有 grok 在 PATH：{body}");
}

/// 登出指令就是該 CLI 自己的（2026-11-14 用 `--help` 確認三家都有），預設帳號不帶任何身份 env。
#[test]
fn the_default_logout_commands_are_each_clis_own_and_carry_no_identity_env() {
    let none = BTreeMap::new();
    assert_eq!(crate::tools::identity_logout_command("claude", &none).as_deref(), Some("claude auth logout"));
    assert_eq!(crate::tools::identity_logout_command("codex", &none).as_deref(), Some("codex logout"));
    assert_eq!(crate::tools::identity_logout_command("grok", &none).as_deref(), Some("grok logout"));
}
