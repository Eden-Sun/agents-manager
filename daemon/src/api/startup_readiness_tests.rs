use super::*;

#[tokio::test]
async fn requests_during_startup_receive_retryable_503() {
    let env = crate::testing::env().await;
    env.app.set_startup_ready(false);
    let router = router(env.app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });

    let response = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
    assert_eq!(response.json::<Value>().await.unwrap()["error"], "starting");

    env.app.set_startup_ready(true);
    let response = reqwest::Client::new()
        .get(format!("http://{addr}/api/state"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "ready requests continue to normal auth");

    server.abort();
}
#[tokio::test]
async fn d3b2_quarantine_stop_returns_503_while_cli_api_is_ready() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('d3b2-api',?,'d3b2-api','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('d3b2-run','d3b2-api','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    sqlx::query("DROP TABLE api_bot_join_receipts").execute(&app.db).await.unwrap();
    crate::api_bot::startup_gate::barrier(&app).await;
    app.set_startup_ready(true);
    let router = router(app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let client = reqwest::Client::new();
    let state = client.get(format!("http://{addr}/api/state")).header("X-AM-Token", "test-token").send().await.unwrap();
    assert_eq!(state.status(), StatusCode::OK);
    let stop = client.post(format!("http://{addr}/api/bots/d3b2-api/stop")).header("X-AM-Token", "test-token").send().await.unwrap();
    assert_eq!(stop.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(stop.json::<Value>().await.unwrap()["error"], "api_startup_quarantined");
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id='d3b2-run'").fetch_one(&app.db).await.unwrap();
    assert_eq!(state, "running");
    server.abort();
}

