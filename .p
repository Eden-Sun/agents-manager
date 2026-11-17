commit 129f31a3703f97c448ddc834da7138e102e46d4d
Author: Edan Sun <huntack478@gmail.com>
Date:   Sun Oct 4 00:47:06 2026 +0000

    fix(api): keep identity preference writes User-only

diff --git a/daemon/src/bot_scope_matrix.rs b/daemon/src/bot_scope_matrix.rs
index b6f7c9e0e..1b7224afe 100644
--- a/daemon/src/bot_scope_matrix.rs
+++ b/daemon/src/bot_scope_matrix.rs
@@ -944,3 +944,46 @@ async fn a_same_project_bot_cannot_spend_another_missions_round() {
         .unwrap();
     assert_eq!(used, 0, "a denied request must not advance the mission");
 }
+
+/// Global identity preferences belong to the User even when a registered AGM Bot carries a
+/// management role. This must stay distinct from ordinary cross-Bot management routes.
+#[tokio::test]
+async fn an_agm_role_bot_cannot_change_user_identity_preferences() {
+    let env = crate::testing::env().await;
+    env.app.set_startup_ready(true);
+    let bot = crate::testing::claude_bot(&env.app, &env.project_id, "identity-prefs-writer").await;
+    crate::supervisor::roles::set_env(
+        &env.app.db,
+        crate::supervisor::roles::Role::Responder,
+        &bot.id,
+        &env.project_id,
+        "/tmp",
+    )
+    .await
+    .unwrap();
+
+    let router = router(env.app.clone());
+    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
+    let addr = listener.local_addr().unwrap();
+    let server = tokio::spawn(async move {
+        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
+    });
+    let response = reqwest::Client::new()
+        .put(format!("http://{addr}/api/identities/cc1/disabled"))
+        .header("X-AM-Bot-Id", &bot.id)
+        .header("X-AM-Bot-Token", &bot.hook_token)
+        .json(&json!({"kind":"claude", "disabled":true, "host":"local"}))
+        .send()
+        .await
+        .unwrap();
+    let status = response.status();
+    let body = response.text().await.unwrap();
+    assert_eq!(status, StatusCode::FORBIDDEN, "AGM role Bots must not mutate User preferences: {body}");
+    assert!(body.contains("user_only"), "the denial must identify the User-only policy: {body}");
+    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM identity_prefs WHERE host='local' AND kind='claude' AND identity='cc1' AND disabled=1")
+        .fetch_one(&env.app.db)
+        .await
+        .unwrap();
+    assert_eq!(rows, 0, "a rejected Bot request must leave the preference unchanged");
+    server.abort();
+}
