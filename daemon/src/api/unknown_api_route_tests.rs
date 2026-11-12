use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn raw(app: std::sync::Arc<crate::state::App>, req: &str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(format!("{req} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = String::new();
    c.read_to_string(&mut out).await.unwrap();
    server.abort();
    out
}

/// 打錯路徑（或新前端打舊 daemon 還沒有的端點）以前拿到 200 的 index.html：呼叫端看不出「沒這個端點」，
/// 而且不帶 token 也回 200。API.md §1：找不到是 404 JSON。
#[tokio::test]
async fn an_unknown_api_path_is_a_json_404_not_the_spa_shell() {
    let env = crate::testing::env().await;
    for req in ["GET /api/no-such-endpoint", "POST /api/no-such-endpoint", "GET /api/bots/x/no-such-sub", "GET /api/projects/x/messages", "POST /api/projects/x/chat"] {
        let out = raw(env.app.clone(), req).await;
        assert!(out.starts_with("HTTP/1.1 404"), "{req}: {out}");
        assert!(out.contains("\"error\":\"not_found\""), "{req}: {out}");
    }
}
