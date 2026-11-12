use super::*;

#[tokio::test]
async fn session_bootstrap_keeps_ui_access_but_never_returns_the_ui_token_to_a_bot() {
    let env = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&env.app, &env.project_id, "session-reader").await;
    let router = router(env.app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let client = reqwest::Client::new();
    let response = client
        .get(format!("http://{addr}/api/session"))
        .header("X-AM-Bot-Id", &bot.id)
        .header("X-AM-Bot-Token", &bot.hook_token)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "a Bot must not reach the User-token bootstrap: {body}");
    assert!(!body.contains(&env.app.ui_token), "the User token must stay secret: {body}");
    for header in ["X-AM-Child-Spawn-Id", "X-AM-Child-Token"] {
        let response = client.get(format!("http://{addr}/api/session")).header(header, "child-proof").send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(!response.text().await.unwrap().contains(&env.app.ui_token));
    }

    let response = client.get(format!("http://{addr}/api/session")).send().await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "the browser bootstrap remains unauthenticated on loopback");
    assert_eq!(response.json::<Value>().await.unwrap()["token"], env.app.ui_token);
    server.abort();
}
