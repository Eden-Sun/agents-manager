//! `GET /api/healthz`（#2036）：DB 正常回 200；池被佔滿回 503，不掛住。

use super::*;

async fn body_json(res: Response) -> (StatusCode, Value) {
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 64 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn healthz_answers_200_when_the_db_pool_answers() {
    let env = crate::testing::env().await;
    let (status, body) = body_json(healthz(State(env.app.clone())).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["ok"], json!(true));
    assert!(body["db_ms"].is_u64(), "db_ms 要是數字：{body}");
}

#[tokio::test]
async fn healthz_answers_503_with_db_unavailable_when_the_pool_is_exhausted() {
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect("sqlite::memory:").await.unwrap();
    let _held = pool.acquire().await.unwrap();

    let probe = probe_db(&pool, std::time::Duration::from_millis(100)).await;
    assert!(probe.is_err(), "池被佔滿時探測要失敗，不能一直等");
    let (status, body) = body_json(healthz_response(probe)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("db_unavailable"));
    assert!(body["detail"].as_str().is_some_and(|d| !d.is_empty()), "detail 要有內容：{body}");
}
