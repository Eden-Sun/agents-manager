use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// review 2026-09-16（deliv「沒把握」）：`ws_loop` 沒有 shutdown 通道，開著分頁時 SIGTERM 會不會卡在 graceful shutdown？
/// 實測不會：axum 0.8 的 `serve` 不追蹤已經 upgrade 的連線，`with_graceful_shutdown` 照樣立刻回來，之後 runtime 收掉
/// `ws_loop`。這條釘住它——哪天換成會等 upgrade 連線的 server，就得真的給 `ws_loop` 一條 shutdown 通道。
#[tokio::test]
async fn an_open_websocket_does_not_hold_up_a_graceful_shutdown() {
    let env = crate::testing::env().await;
    let router = super::router(env.app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = rx.await;
            })
            .await
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /ws?token=test-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
        .await
        .unwrap();
    let mut buf = vec![0u8; 1024];
    let n = client.read(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf[..n]);
    assert!(head.starts_with("HTTP/1.1 101"), "要真的是一條開著的 websocket：{head}");

    tx.send(()).unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(3), server).await;
    assert!(done.is_ok(), "開著的 websocket 讓 graceful shutdown 卡住了");
    drop(client);
}
