use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn request(app: Arc<App>, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = super::router(app);
    let server = tokio::spawn(async move {
        axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await
    });
    let extra: String = headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1\r\n{extra}Connection: close\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
    c.write_all(head.as_bytes()).await.unwrap();
    c.write_all(body).await.unwrap();
    let mut out = Vec::new();
    c.read_to_end(&mut out).await.unwrap();
    server.abort();
    String::from_utf8_lossy(&out).into_owned()
}

fn status(response: &str) -> &str {
    response.split_whitespace().nth(1).unwrap_or("")
}

/// File APIs are browser surfaces. A valid bot credential must not upload into another bot's
/// project or read attachments, local images, or outbox files through the shared daemon.
#[tokio::test]
async fn bot_credentials_cannot_read_or_upload_user_facing_files() {
    let e = crate::testing::env().await;
    let caller = crate::testing::claude_bot(&e.app, &e.project_id, "caller").await;
    let owner = crate::testing::claude_bot(&e.app, &e.project_id, "owner").await;
    let caller_token = "caller-only-token";
    sqlx::query("UPDATE bots SET hook_token=? WHERE id=?").bind(caller_token).bind(&caller.id).execute(&e.app.db).await.unwrap();

    let attachment = crate::attach::save(&e.app, &owner.id, "private.txt", "text/plain", b"private attachment").await.unwrap();
    let project_path: String = sqlx::query_scalar("SELECT path FROM projects WHERE id=?").bind(&e.project_id).fetch_one(&e.app.db).await.unwrap();
    std::fs::write(std::path::Path::new(&project_path).join("private.png"), b"private image").unwrap();
    let outbox = crate::outbox::dir_for(&e.app.data_dir, &owner.id).unwrap();
    std::fs::create_dir_all(&outbox).unwrap();
    std::fs::write(outbox.join("private.txt"), b"private outbox").unwrap();

    let bot_headers = [("X-AM-Bot-Id", caller.id.as_str()), ("X-AM-Bot-Token", caller_token)];
    let bot_responses = [
        request(e.app.clone(), "POST", &format!("/api/bots/{}/attachments?name=foreign.txt", owner.id), &bot_headers, b"injected",).await,
        request(e.app.clone(), "GET", &format!("/api/attachments/{}", attachment.id), &bot_headers, b"").await,
        request(e.app.clone(), "GET", &format!("/api/bots/{}/local-image?path=private.png", owner.id), &bot_headers, b"").await,
        request(e.app.clone(), "GET", &format!("/api/bots/{}/outbox", owner.id), &bot_headers, b"").await,
        request(e.app.clone(), "GET", &format!("/api/bots/{}/outbox/file?path=private.txt", owner.id), &bot_headers, b"").await,
    ];
    let bot_statuses: Vec<&str> = bot_responses.iter().map(|r| status(r)).collect();
    assert_eq!(bot_statuses, ["403"; 5], "bot principal crossed the user-facing file API boundary: {bot_statuses:?}");

    // These same operations remain available to the UI token.
    let user_headers = [("X-AM-Token", e.app.ui_token.as_str()), ("Content-Type", "text/plain")];
    let user_responses = [
        request(e.app.clone(), "POST", &format!("/api/bots/{}/attachments?name=allowed.txt", owner.id), &user_headers, b"allowed",).await,
        request(e.app.clone(), "GET", &format!("/api/attachments/{}", attachment.id), &user_headers, b"").await,
        request(e.app.clone(), "GET", &format!("/api/bots/{}/local-image?path=private.png", owner.id), &user_headers, b"").await,
        request(e.app.clone(), "GET", &format!("/api/bots/{}/outbox", owner.id), &user_headers, b"").await,
        request(e.app.clone(), "GET", &format!("/api/bots/{}/outbox/file?path=private.txt", owner.id), &user_headers, b"").await,
    ];
    let user_statuses: Vec<&str> = user_responses.iter().map(|r| status(r)).collect();
    assert_eq!(user_statuses, ["200"; 5], "UI access regressed: {user_statuses:?}");
}
