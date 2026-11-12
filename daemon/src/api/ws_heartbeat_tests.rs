use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn read_handshake(s: &mut tokio::net::TcpStream) {
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        s.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
}

async fn connect(app: &std::sync::Arc<crate::state::App>) -> tokio::net::TcpStream {
    let router = super::router(app.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /ws?token=test-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n")
        .await
        .unwrap();
    read_handshake(&mut client).await;
    client
}

/// 沒有任何事件也要定期有幀：靜止的連線若不送東西，客戶端分不出「沒事」與「線已經斷了」。
#[tokio::test]
async fn an_idle_socket_gets_a_ping_frame_without_a_seq() {
    let env = crate::testing::env().await;
    env.app.set_ws_ping_every(std::time::Duration::from_millis(60));
    let mut client = connect(&env.app).await;
    for _ in 0..2 {
        let ping = tokio::time::timeout(std::time::Duration::from_secs(5), crate::testing::read_text_frame(&mut client)).await.unwrap();
        assert_eq!(ping["type"], "ping", "{ping}");
        assert!(ping.get("seq").is_none(), "ping 不佔 seq（不能推動客戶端的 lastSeq／重播環）：{ping}");
    }
    assert_eq!(env.app.current_seq(), 0, "ping 不是事件，不能讓 seq 前進");
    drop(client);
}

/// 有事件在跑時 ping 照送，事件不被 ping 吞掉或換序。
#[tokio::test]
async fn events_still_arrive_between_pings() {
    let env = crate::testing::env().await;
    env.app.set_ws_ping_every(std::time::Duration::from_millis(60));
    let mut client = connect(&env.app).await;
    env.app.emit("bot_changed", serde_json::json!({"mark": "x"})).await;
    let mut got_event = false;
    for _ in 0..5 {
        let f = tokio::time::timeout(std::time::Duration::from_secs(5), crate::testing::read_text_frame(&mut client)).await.unwrap();
        if f["type"] == "bot_changed" {
            got_event = true;
            break;
        }
        assert_eq!(f["type"], "ping");
    }
    assert!(got_event);
    drop(client);
}
