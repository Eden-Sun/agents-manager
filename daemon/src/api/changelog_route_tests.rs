use super::*;
use axum::response::IntoResponse;

async fn call(kind: Option<&str>, host: Option<&str>) -> (StatusCode, Value) {
    let e = crate::testing::env().await;
    let q = ChangelogQuery {
        kind: kind.map(str::to_string),
        host: host.map(str::to_string),
        from: None,
        to: None,
    };
    match get_changelog(State(e.app.clone()), Query(q)).await {
        Ok(Json(v)) => (StatusCode::OK, v),
        Err(err) => {
            let resp = err.into_response();
            let status = resp.status();
            let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
            (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
        }
    }
}

/// API.md：changelog **永遠 200**；沒有來源的 kind 是 `found:false`，不是 400。
#[tokio::test]
async fn an_unknown_kind_is_200_found_false_not_400() {
    let (status, body) = call(Some("nope"), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["found"], false);
    assert!(body["error"].as_str().is_some_and(|s| !s.is_empty()), "{body}");
}

/// API.md：host 不存在也是 200 `found:false`，不是 404——UI 必須能寫「找不到 changelog」。
/// 而且講的是**這台 host 不認得**，不能退回去問本機：只看 `found:false`＋有 error 的話，改成退回本機也照綠
/// （測試環境的本機 claude 一樣讀不到版本，#220 的測試原本就是這樣沒釘住）。
#[tokio::test]
async fn an_unknown_host_is_200_found_false_not_404() {
    let (status, body) = call(Some("claude"), Some("no-such-host")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["found"], false);
    assert_eq!(body["host"], "no-such-host", "回的是問的那台，不是 local：{body}");
    assert!(body["installed_version"].is_null(), "沒去問任何一台的版本：{body}");
    let err = body["error"].as_str().unwrap_or_default();
    assert!(err.contains("unknown host") && err.contains("no-such-host"), "錯誤要講是這台 host 不認得：{body}");
}
