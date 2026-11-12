use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 起一顆真的 router，送一個帶標頭與 body 的 HTTP/1.1 請求，回整段回應文字（狀態行＋標頭＋body）。
async fn send(app: Arc<App>, head: &str, headers: &[(&str, &str)], body: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut req = format!("{head} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    server.abort();
    out
}

/// extractor 驗不過（型別錯、語法錯、沒帶 Content-Type、query 解析失敗）也要是 API.md §1 的 JSON 錯誤，不是 axum 的純文字。
#[tokio::test]
async fn extractor_rejections_are_json_like_every_other_error() {
    let e = crate::testing::env().await;
    let token = e.app.ui_token.clone();
    let auth = ("X-AM-Token", token.as_str());
    let json_ct = ("Content-Type", "application/json");

    let out = send(e.app.clone(), "POST /api/projects", &[auth, json_ct], r#"{"path": 1}"#).await;
    assert!(out.starts_with("HTTP/1.1 422"), "{out}");
    assert!(out.to_ascii_lowercase().contains("content-type: application/json"), "{out}");
    assert!(out.contains("\"error\":\"bad_request\"") && out.contains("\"message\":\""), "{out}");

    let out = send(e.app.clone(), "POST /api/projects", &[auth, json_ct], "not json").await;
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    assert!(out.contains("\"error\":\"bad_request\""), "{out}");

    let out = send(e.app.clone(), "POST /api/projects", &[auth], "{}").await;
    assert!(out.starts_with("HTTP/1.1 415"), "{out}");
    assert!(out.contains("\"error\":\"bad_request\""), "{out}");

    let out = send(e.app.clone(), "GET /api/search/messages?limit=abc", &[auth], "").await;
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    assert!(out.contains("\"error\":\"bad_request\""), "{out}");

    // 對照：handler 自己回的 JSON 錯誤原樣通過，不被重包成巢狀字串。
    let out = send(e.app.clone(), "POST /api/projects", &[auth, json_ct], r#"{"path":"relative/dir"}"#).await;
    assert!(out.starts_with("HTTP/1.1 400"), "{out}");
    assert!(out.contains("\"message\":\"project path must be absolute (or start with ~/)\""), "{out}");
}
