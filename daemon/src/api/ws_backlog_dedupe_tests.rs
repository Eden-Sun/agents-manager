use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 讀到 `\r\n\r\n` 為止，一次一個 byte：整塊讀會把後面那些 WS 幀的位元組一起吃掉。
async fn read_handshake(s: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// 修正前：那一則落在 backlog 與即時串流兩邊，客戶端連收兩次同一個 `seq`——而
/// `bots_restart_progress` 這類 handler 是純累加，重複就是多算一次。
#[tokio::test]
async fn an_event_emitted_between_subscribe_and_backlog_is_sent_once() {
    let env = crate::testing::env().await;
    // 環裡先有一則，`since` 才有東西可以往後接。
    env.app.emit("bot_changed", json!({"mark": "before"})).await;
    let since = env.app.current_seq();

    // 訂閱之後、讀環之前送出的那一則。key 用 data-dir，平行測試不會互相觸發。
    let app2 = env.app.clone();
    crate::lifecycle::race_point::arm("ws_subscribed", &env.app.data_dir.display().to_string(), move || async move {
        app2.emit("bot_changed", json!({"mark": "in_window"})).await;
    });

    let router = super::router(env.app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /ws?token=test-token&since={since} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    client.write_all(req.as_bytes()).await.unwrap();
    let head = read_handshake(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 101"), "要真的升級成 websocket：{head}");

    // 第一幀＝backlog 裡的那一則（沒有它就是漏送，不是重複）。
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), crate::testing::read_text_frame(&mut client)).await.unwrap();
    assert_eq!(first["data"]["mark"], "in_window", "窗口裡那一則要從 backlog 送出來：{first}");
    let first_seq = first["seq"].as_u64().unwrap();

    // 哨兵在第一幀之後才送，所以它一定排在「重複的那一則」後面：下一幀是誰就見分曉。
    env.app.emit("bot_changed", json!({"mark": "sentinel"})).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), crate::testing::read_text_frame(&mut client)).await.unwrap();
    assert_eq!(second["data"]["mark"], "sentinel", "同一個 seq 被送第二次（backlog 一次、即時串流一次）：{second}");
    assert!(second["seq"].as_u64().unwrap() > first_seq, "哨兵的 seq 要比前一則大：{second}");
    drop(client);
}
