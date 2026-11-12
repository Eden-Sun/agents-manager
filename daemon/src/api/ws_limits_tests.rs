use serde_json::json;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn serve(app: std::sync::Arc<crate::state::App>) -> std::net::SocketAddr {
    let router = super::router(app);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    addr
}

async fn connect(addr: std::net::SocketAddr) -> (tokio::net::TcpStream, String) {
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /ws?token=test-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap();
    let mut head = Vec::new();
    let mut b = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        c.read_exact(&mut b).await.unwrap();
        head.push(b[0]);
    }
    (c, String::from_utf8_lossy(&head).into_owned())
}

/// client→server 一個 `len` 位元組的 binary 幀（要 mask；全 0 的 mask key 讓 payload 原樣）。
fn masked_binary_frame(len: usize) -> Vec<u8> {
    let mut f = vec![0x82, 0xFF];
    f.extend_from_slice(&(len as u64).to_be_bytes());
    f.extend_from_slice(&[0, 0, 0, 0]);
    f.resize(f.len() + len, 0);
    f
}

async fn eventually_closed(app: &crate::state::App) -> bool {
    for _ in 0..200 {
        if app.ws_connections.load(Ordering::SeqCst) == 0 {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    false
}

/// 這個 server 從不看 client 送來的內容，所以一個幾十 MiB 的訊息只是白白被緩衝（tungstenite 預設 64 MiB／幀 16 MiB）：
/// 超過上限就斷線。
#[tokio::test]
async fn an_oversized_client_message_closes_the_connection() {
    let env = crate::testing::env().await;
    let addr = serve(env.app.clone()).await;
    let (mut c, head) = connect(addr).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert_eq!(env.app.ws_connections.load(Ordering::SeqCst), 1);
    let _ = c.write_all(&masked_binary_frame(2 * 1024 * 1024)).await;
    assert!(eventually_closed(&env.app).await, "2 MiB 的 client 訊息要被斷線，而不是整個吃進記憶體");
}

#[tokio::test]
async fn the_number_of_concurrent_connections_is_capped() {
    let env = crate::testing::env().await;
    env.app.ws_max_connections.store(3, Ordering::SeqCst);
    let addr = serve(env.app.clone()).await;
    let mut held = Vec::new();
    for _ in 0..3 {
        let (c, head) = connect(addr).await;
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        held.push(c);
    }
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(b"GET /ws?token=test-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n").await.unwrap();
    let mut out = vec![0u8; 256];
    let n = c.read(&mut out).await.unwrap();
    assert!(String::from_utf8_lossy(&out[..n]).starts_with("HTTP/1.1 503"), "第 4 條要被拒：{}", String::from_utf8_lossy(&out[..n]));
    // 一條斷線，名額就回來。
    drop(held.pop());
    let mut ok = String::new();
    for _ in 0..100 {
        let (_c, head) = connect(addr).await;
        ok = head;
        if ok.starts_with("HTTP/1.1 101") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ok.starts_with("HTTP/1.1 101"), "{ok}");
}

/// 連上就不讀的 client：送不出去的那條 `socket.send` 以前會永遠卡住（連線與記憶體都不放）。
#[tokio::test]
async fn a_client_that_never_reads_is_dropped_after_the_send_timeout() {
    let env = crate::testing::env().await;
    env.app.set_ws_send_timeout(std::time::Duration::from_millis(300));
    let addr = serve(env.app.clone()).await;
    let (_stuck, head) = connect(addr).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    let big = "x".repeat(1024 * 1024);
    // 把 kernel 的送出／接收緩衝塞滿：之後的 send 才會真的卡住。
    for _ in 0..96 {
        env.app.emit("message_added", json!({"text": big})).await;
        tokio::task::yield_now().await;
    }
    assert!(eventually_closed(&env.app).await, "不讀的連線要在逾時後被放掉");
}

/// 事件是廣播給所有 UI 連線的：憑證欄位不能進去（也不能進重播環），就算哪個呼叫端手滑把整個物件塞進 payload。
#[tokio::test]
async fn credential_fields_never_leave_in_an_event() {
    let env = crate::testing::env().await;
    let mut rx = env.app.subscribe();
    for (kind, data) in [
        (
            "bot_changed",
            json!({"bot": {"id": "b1", "hook_token": "SECRET-HOOK", "nested": [{"id": 1, "ok": 1, "input_tokens": 21, "token_value": "SECRET-RAW-TOKEN", "ui_token": "SECRET-UI", "X-AM-Bot-Token": "SECRET-BOT-HEADER", "X-AM-Service-Token": "SECRET-SERVICE-HEADER", "lease_token": "SECRET-LEASE", "access_token": "SECRET-ACCESS", "refresh_token": "SECRET-REFRESH", "client_secret": "SECRET-CLIENT", "secret": "SECRET-GENERIC"}]}, "AM_BOT_TOKEN": "SECRET-BOT"}),
        ),
        ("supervisor_changed", json!({"lease": {"resource": "rebuild", "lease_token": "SECRET-LEASE-EVENT"}})),
        ("upstream_update", json!({"host": {"refresh_token": "SECRET-REFRESH-EVENT"}})),
    ] {
        env.app.emit(kind, data).await;
        let ev = rx.recv().await.unwrap();
        let wire = serde_json::to_string(&ev).unwrap();
        assert!(!wire.contains("SECRET"), "{kind}: {wire}");
        if kind == "bot_changed" {
            assert!(wire.contains("\"id\":\"b1\"") && wire.contains("\"ok\":1") && wire.contains("\"input_tokens\":21"), "其他欄位與 token 計數照舊：{wire}");
        }
    }
    let backlog = env.app.backlog(0).await.unwrap_or_default();
    assert!(!serde_json::to_string(&backlog).unwrap().contains("SECRET"), "重播環裡也不能有");
}
