use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn concurrent_uploads_are_capped_and_the_slot_is_freed_when_one_goes_away() {
    let env = crate::testing::env().await;
    let bot = crate::testing::claude_bot(&env.app, &env.project_id, "uploader").await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(env.app.clone());
    tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });
    let head = |len: usize| {
        format!(
            "POST /api/bots/{}/attachments?name=a.bin HTTP/1.1\r\nHost: 127.0.0.1\r\nX-AM-Token: test-token\r\nContent-Type: application/octet-stream\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n",
            bot.id
        )
    };
    // 占滿名額：每條連線送出標頭與一小段 body，之後不再送（慢速上傳）。
    let mut held = Vec::new();
    for _ in 0..super::UPLOAD_CONCURRENCY {
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(head(1000).as_bytes()).await.unwrap();
        c.write_all(&[7u8; 10]).await.unwrap();
        held.push(c);
    }
    let try_upload = || async {
        let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
        c.write_all(head(4).as_bytes()).await.unwrap();
        c.write_all(b"data").await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), c.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    };
    // 名額是在請求進來時才占的：等到伺服器都處理過那幾條。
    let mut refused = String::new();
    for _ in 0..100 {
        refused = try_upload().await;
        if refused.starts_with("HTTP/1.1 429") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(refused.starts_with("HTTP/1.1 429"), "名額滿了要 429：{refused}");
    assert!(refused.to_ascii_lowercase().contains("retry-after"), "{refused}");
    // 一條慢速連線斷掉，名額就回來。
    drop(held.pop());
    let mut ok = String::new();
    for _ in 0..100 {
        ok = try_upload().await;
        if ok.starts_with("HTTP/1.1 200") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(ok.starts_with("HTTP/1.1 200"), "名額放回來之後要收：{ok}");
}
