//! 重置券提示「不再提醒」的端點：User-only、只收 1/24/72、取消。

use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn raw(app: Arc<App>, request: String) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    let _ = c.write_all(request.as_bytes()).await;
    let mut bytes = Vec::new();
    let _ = c.read_to_end(&mut bytes).await;
    server.abort();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn req(app: &App, method: &str, path: &str, bot: Option<&db::Bot>, body: &str) -> String {
    let headers = match bot {
        Some(b) => format!("X-AM-Bot-Id: {}\r\nX-AM-Bot-Token: {}\r\n", b.id, b.hook_token),
        None => format!("X-AM-Token: {}\r\n", app.ui_token),
    };
    format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{headers}Content-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn reset_credit_snooze_is_user_only_and_accepts_only_1_24_72_hours() {
    let e = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&e.app, &e.project_id, "snooze-bot").await;
    let bot = db::bot(&e.app.db, &bot.id).await.unwrap().unwrap();
    let key = "mac_builder/codex:co0";
    let body = json!({"key": key, "hours": 24}).to_string();

    let ok = raw(e.app.clone(), req(&e.app, "PUT", "/api/quota/reset-credit/snooze", None, &body)).await;
    assert!(ok.starts_with("HTTP/1.1 200") && ok.contains("snoozed_until"), "user can snooze: {ok}");
    assert!(crate::reset_offer_snooze::snoozed_until(&e.app.data_dir, key, chrono::Utc::now()).is_some());

    for hours in [0, 2, 25, 168] {
        let bad = json!({"key": key, "hours": hours}).to_string();
        let r = raw(e.app.clone(), req(&e.app, "PUT", "/api/quota/reset-credit/snooze", None, &bad)).await;
        assert!(r.starts_with("HTTP/1.1 400"), "hours={hours} rejected: {r}");
    }

    let denied = raw(e.app.clone(), req(&e.app, "PUT", "/api/quota/reset-credit/snooze", Some(&bot), &body)).await;
    assert!(denied.starts_with("HTTP/1.1 403") && denied.contains("user_only"), "bot cannot snooze: {denied}");
    let denied = raw(e.app.clone(), req(&e.app, "DELETE", &format!("/api/quota/reset-credit/snooze?key={key}"), Some(&bot), "")).await;
    assert!(denied.starts_with("HTTP/1.1 403") && denied.contains("user_only"), "bot cannot cancel: {denied}");

    let cancelled = raw(e.app.clone(), req(&e.app, "DELETE", &format!("/api/quota/reset-credit/snooze?key={key}"), None, "")).await;
    assert!(cancelled.starts_with("HTTP/1.1 200") && cancelled.contains("\"cleared\":true"), "user can cancel: {cancelled}");
    assert!(crate::reset_offer_snooze::snoozed_until(&e.app.data_dir, key, chrono::Utc::now()).is_none());
}
