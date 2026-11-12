use super::*;
use axum::response::IntoResponse;

async fn call(app: &Arc<App>, host: &str, principal: RequestPrincipal) -> (StatusCode, Value) {
    let resp = match logout_agy(State(app.clone()), Path(host.to_string()), Extension(principal)).await {
        Ok(j) => j.into_response(),
        Err(e) => e.into_response(),
    };
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 契約：User 才能呼叫（Bot 403 `user_only`）；沒有這台主機 404；檔案不在 `{"removed":false}`、在 `{"removed":true}`。
#[tokio::test]
async fn only_the_user_can_log_agy_out_and_the_answer_says_whether_a_file_was_removed() {
    let _lock = crate::quota_agy::token_test_lock().await;
    let e = crate::testing::env().await;
    let (st, body) = call(&e.app, crate::config::LOCAL_HOST, RequestPrincipal::Bot("b1".into())).await;
    assert_eq!((st, body["reason"].as_str()), (StatusCode::FORBIDDEN, Some("user_only")), "{body}");
    let (st, _) = call(&e.app, "no-such-host", RequestPrincipal::User).await;
    assert_eq!(st, StatusCode::NOT_FOUND);

    let tok = crate::home::dir().unwrap().join(crate::quota_agy::TOKEN_FILE);
    std::fs::create_dir_all(tok.parent().unwrap()).unwrap();
    std::fs::write(&tok, "secret").unwrap();
    let (st, body) = call(&e.app, crate::config::LOCAL_HOST, RequestPrincipal::User).await;
    assert_eq!((st, body["removed"].as_bool()), (StatusCode::OK, Some(true)), "{body}");
    let (st, body) = call(&e.app, crate::config::LOCAL_HOST, RequestPrincipal::User).await;
    assert_eq!((st, body["removed"].as_bool()), (StatusCode::OK, Some(false)), "{body}");
}

/// #865：`LogoutError::Superseded` ＝ 409 `host_superseded`（可重試），跟 `lifecycle::start` 同一個 reason。
#[tokio::test]
async fn a_superseded_logout_is_a_retryable_409() {
    let resp = LcError::conflict("host_superseded", json!({"host": "h", "retryable": true})).into_response();
    assert_eq!(resp.status(), StatusCode::CONFLICT);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!((body["reason"].as_str(), body["retryable"].as_bool()), (Some("host_superseded"), Some(true)), "{body}");
}
