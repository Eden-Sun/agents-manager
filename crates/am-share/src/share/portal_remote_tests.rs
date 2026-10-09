//! #955 R-S3：分享入口對「專案在遠端主機」的 I/O（上傳、附件檢查、列表、串流下載、SVG 嵌照片、壞 SVG 提醒）。
//!
//! 遠端用 `remote_fs::test_support::local_sh_fake`：把 ssh 腳本交給本機 `/bin/sh` 跑，`HOME` 指到 scratch 目錄——
//! 腳本的每一條擋法都真的被執行，而且不碰真實 HOME。本機路徑的行為由 `tests.rs` 既有的案例釘住，這裡不重複。

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::*;
use crate::config::HostCfg;
use crate::db;
use crate::state::App;
use crate::testing as tt;

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().redirect(reqwest::redirect::Policy::none()).build().unwrap()
}

async fn serve(router: axum::Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await;
    });
    format!("http://{addr}")
}

struct Remote {
    bot: db::Bot,
    token: String,
    host: String,
    home: PathBuf,
    workspace: PathBuf,
    outbox: PathBuf,
    conn: Arc<crate::hosts::HostConn>,
    base: String,
}

impl Remote {
    fn url(&self, tail: &str) -> String {
        format!("{}/s/{}/api/{tail}", self.base, self.token)
    }

    fn inbox(&self) -> PathBuf {
        self.workspace.join("inbox")
    }

    async fn upload(&self, name: &str, data: Vec<u8>) -> reqwest::Response {
        client().post(self.url("upload")).query(&[("name", name)]).body(data).send().await.unwrap()
    }

    async fn site(&self, app: &Arc<App>) -> super::site::RemoteSite {
        match super::site::resolve(app, &self.bot.id).await.expect("site resolves") {
            super::site::ShareSite::Remote(s) => s,
            other => panic!("expected a remote site, got {other:?}"),
        }
    }
}

/// 一顆在「遠端主機」上的受限分享 bot：專案的 `host` 指到假主機，工作目錄、inbox、outbox 都在 scratch 家目錄底下。
async fn setup(e: &tt::Env, tag: &str) -> Remote {
    let home = super::test_dirs::scratch_dir(&format!("rs3-{tag}"));
    let host = format!("rs3-{tag}-{}", db::ulid().to_lowercase());
    super::remote_fs::test_support::local_sh_fake(&host, &home);
    let cfg = HostCfg {
        name: host.clone(),
        ssh: "fake-ssh".into(),
        ssh_port: 22,
        ssh_opts: vec![],
        herdr_session: "test-session".into(),
        shared_session: false,
        remote_path: String::new(),
    };
    let conn = e.app.hosts.insert_remote_for_test(cfg).await;
    conn.connected.store(true, Ordering::SeqCst);
    sqlx::query("UPDATE projects SET host = ? WHERE id = ?").bind(&host).bind(&e.project_id).execute(&e.app.db).await.unwrap();
    let bot = tt::claude_bot(&e.app, &e.project_id, tag).await;
    let workspace = home.join("shared-bots").join(tag);
    std::fs::create_dir_all(workspace.join("inbox")).unwrap();
    std::fs::set_permissions(workspace.join("inbox"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let outbox = PathBuf::from(crate::outbox_remote::remote_dir(&home.to_string_lossy(), e.app.instance().as_deref(), &bot.id).unwrap());
    std::fs::create_dir_all(&outbox).unwrap();
    store::insert_share_bot(&e.app.db, &bot.id, "restricted", &workspace.to_string_lossy()).await.unwrap();
    let token = store::enable(&e.app.db, &bot.id).await.unwrap().expect("a fresh share");
    let base = serve(portal::router(e.app.clone())).await;
    Remote { bot, token, host, home, workspace, outbox, conn, base }
}

fn jpeg(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 128]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgb8(img).write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg).unwrap();
    out
}

// ───────────────────────── 上傳 ─────────────────────────

#[tokio::test]
async fn an_upload_lands_in_the_remote_inbox() {
    let e = tt::env().await;
    let r = setup(&e, "up-ok").await;
    let resp = r.upload("note.txt", b"hello remote".to_vec()).await;
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let id = body["id"].as_str().unwrap();
    assert!(portal::stored_name_ok(id), "{body}");
    assert_eq!((body["name"].as_str(), body["size"].as_u64()), (Some("note.txt"), Some(12)));
    assert_eq!(std::fs::read(r.inbox().join(id)).unwrap(), b"hello remote", "檔案真的在遠端 inbox");
    let mode = std::fs::metadata(r.inbox().join(id)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "遠端檔案 0600");
}

/// 配額數字同本機：inbox 已經 300 個檔就 507 `inbox_full`，而且沒有多寫一個檔。
#[tokio::test]
async fn a_full_remote_inbox_is_507_and_writes_nothing() {
    let e = tt::env().await;
    let r = setup(&e, "up-full").await;
    for i in 0..portal::INBOX_MAX_FILES {
        std::fs::write(r.inbox().join(format!("seed-{i}")), b"x").unwrap();
    }
    let resp = r.upload("late.txt", b"hello".to_vec()).await;
    assert_eq!(resp.status(), 507);
    assert_eq!(resp.json::<Value>().await.unwrap()["error"], "inbox_full");
    assert_eq!(std::fs::read_dir(r.inbox()).unwrap().count(), portal::INBOX_MAX_FILES, "沒有多出檔案");
}

/// inbox 只剩 1 個空位：第 300 個進得去，第 301 個 507（跟本機的「>= 300 就滿」同一條線）。
#[tokio::test]
async fn the_remote_inbox_quota_line_matches_the_local_one() {
    let e = tt::env().await;
    let r = setup(&e, "up-line").await;
    for i in 0..portal::INBOX_MAX_FILES - 1 {
        std::fs::write(r.inbox().join(format!("seed-{i}")), b"x").unwrap();
    }
    assert_eq!(r.upload("last.txt", b"hello".to_vec()).await.status(), 200);
    assert_eq!(r.upload("over.txt", b"hello".to_vec()).await.status(), 507);
}

/// 目標名是懸空符號連結：`set -C`（O_EXCL）不寫穿，連結外面不會多出檔案。
#[tokio::test]
async fn a_dangling_symlink_name_is_not_written_through() {
    let e = tt::env().await;
    let r = setup(&e, "up-link").await;
    let site = r.site(&e.app).await;
    let outside = r.home.join("outside-target.txt");
    std::os::unix::fs::symlink(&outside, r.inbox().join("01ARZ3NDEKTSV4RRFFQ69G5FAV-evil.txt")).unwrap();
    let res = site.inbox_write("01ARZ3NDEKTSV4RRFFQ69G5FAV-evil.txt", b"payload", portal::INBOX_MAX_BYTES, portal::INBOX_MAX_FILES).await;
    assert!(res.is_err(), "{res:?}");
    assert!(!outside.exists(), "沒有寫穿符號連結");
}

#[tokio::test]
async fn the_per_minute_upload_limit_is_the_same_as_local() {
    let e = tt::env().await;
    let r = setup(&e, "up-rate").await;
    for i in 0..portal::UPLOADS_PER_MIN {
        assert_eq!(r.upload(&format!("f{i}.txt"), b"x".to_vec()).await.status(), 200, "第 {i} 個");
    }
    assert_eq!(r.upload("one-too-many.txt", b"x".to_vec()).await.status(), 429);
}

// ───────────────────────── 主機斷線 ─────────────────────────

/// 主機斷線：上傳、列表、下載、附件檢查都是一般 503（不洩漏主機名），不是 404 也不是空清單。
#[tokio::test]
async fn a_disconnected_host_is_a_plain_503_everywhere() {
    let e = tt::env().await;
    let r = setup(&e, "down").await;
    std::fs::write(r.outbox.join("a.txt"), b"hello").unwrap();
    r.conn.connected.store(false, Ordering::SeqCst);

    let up = r.upload("note.txt", b"x".to_vec()).await;
    assert_eq!(up.status(), 503);
    assert!(!up.text().await.unwrap().contains(&r.host), "不洩漏主機名");
    assert_eq!(client().get(r.url("files")).send().await.unwrap().status(), 503, "列表不是空清單");
    assert_eq!(client().get(r.url("files/a.txt")).send().await.unwrap().status(), 503);
    let attachment = format!("{}-a.txt", db::ulid());
    let send = client().post(r.url("messages")).json(&json!({"text": "hi", "client_request_id": "c1", "attachments": [attachment]})).send().await.unwrap();
    assert_eq!(send.status(), 503, "附件檢查連不上＝503，不送出");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(n, 0, "沒有一則進到對話");
}

#[tokio::test]
async fn an_attachment_that_is_not_in_the_remote_inbox_is_a_400() {
    let e = tt::env().await;
    let r = setup(&e, "att").await;
    let attachment = format!("{}-missing.txt", db::ulid());
    let send = client().post(r.url("messages")).json(&json!({"text": "hi", "client_request_id": "c1", "attachments": [attachment]})).send().await.unwrap();
    assert_eq!(send.status(), 400);
    assert_eq!(send.json::<Value>().await.unwrap()["reason"], "unknown_attachment");
}

// ───────────────────────── 列表 ─────────────────────────

#[tokio::test]
async fn listing_an_outbox_that_does_not_exist_yet_is_empty_not_an_error() {
    let e = tt::env().await;
    let r = setup(&e, "ls-none").await;
    std::fs::remove_dir_all(&r.outbox).unwrap();
    let resp = client().get(r.url("files")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Value>().await.unwrap()["files"], json!([]));
}

#[tokio::test]
async fn an_untrusted_remote_outbox_is_503_not_an_empty_list() {
    let e = tt::env().await;
    let r = setup(&e, "ls-link").await;
    let elsewhere = r.home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("secret.txt"), b"nope").unwrap();
    std::fs::remove_dir_all(&r.outbox).unwrap();
    std::os::unix::fs::symlink(&elsewhere, &r.outbox).unwrap();
    assert_eq!(client().get(r.url("files")).send().await.unwrap().status(), 503);
    assert_eq!(client().get(r.url("files/secret.txt")).send().await.unwrap().status(), 503, "下載也不跟連結");
}

/// 分享用 bot 的遠端 outbox 不照 1 小時清：兩小時前的檔列得出來、列表之後還在。
#[tokio::test]
async fn files_older_than_an_hour_are_listed_and_never_deleted() {
    let e = tt::env().await;
    let r = setup(&e, "ls-old").await;
    let old = r.outbox.join("old-report.txt");
    std::fs::write(&old, b"still here").unwrap();
    let status = std::process::Command::new("touch").arg("-d").arg("2 hours ago").arg(&old).status().unwrap();
    assert!(status.success());
    for _ in 0..2 {
        let resp = client().get(r.url("files")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["files"][0]["name"], "old-report.txt", "{v}");
        assert_eq!(v["files"][0]["size"], 10);
        assert!(v["files"][0]["modified_at"].is_string());
    }
    assert!(old.exists(), "列表沒有刪任何檔");
}

#[tokio::test]
async fn the_remote_listing_hides_what_the_local_one_hides() {
    let e = tt::env().await;
    let r = setup(&e, "ls-hide").await;
    std::fs::write(r.outbox.join("ok.txt"), b"hello").unwrap();
    std::fs::write(r.outbox.join("server.pem"), b"-----BEGIN PRIVATE KEY-----").unwrap();
    std::fs::write(r.outbox.join(".hidden"), b"x").unwrap();
    std::fs::write(r.outbox.join("renamed.txt"), b"SQLite format 3\0 and more").unwrap();
    std::fs::create_dir_all(r.outbox.join("subdir")).unwrap();
    let v: Value = client().get(r.url("files")).send().await.unwrap().json().await.unwrap();
    let names: Vec<&str> = v["files"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["ok.txt"], "{v}");
}

// ───────────────────────── 下載 ─────────────────────────

#[tokio::test]
async fn a_remote_download_streams_the_whole_file_with_a_content_length() {
    let e = tt::env().await;
    let r = setup(&e, "dl-big").await;
    // 64 MiB 稀疏檔：大小剛好在上限，內容全是 0（不是黑名單開頭）。
    let big = r.outbox.join("big.bin");
    std::fs::File::create(&big).unwrap().set_len(crate::outbox::MAX_BYTES).unwrap();
    let resp = client().get(r.url("files/big.bin")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-length"], crate::outbox::MAX_BYTES.to_string().as_str());
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert!(resp.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"));
    assert_eq!(resp.bytes().await.unwrap().len() as u64, crate::outbox::MAX_BYTES);
}

#[tokio::test]
async fn a_file_over_the_cap_is_413_and_a_blacklisted_head_is_404() {
    let e = tt::env().await;
    let r = setup(&e, "dl-rules").await;
    std::fs::File::create(r.outbox.join("huge.bin")).unwrap().set_len(crate::outbox::MAX_BYTES + 1).unwrap();
    assert_eq!(client().get(r.url("files/huge.bin")).send().await.unwrap().status(), 413);
    std::fs::write(r.outbox.join("looks-fine.txt"), b"SQLite format 3\0 pretend db").unwrap();
    assert_eq!(client().get(r.url("files/looks-fine.txt")).send().await.unwrap().status(), 404, "檔頭是 SQLite 的擋下");
    std::fs::write(r.outbox.join("server.pem"), b"x").unwrap();
    assert_eq!(client().get(r.url("files/server.pem")).send().await.unwrap().status(), 404);
    assert_eq!(client().get(r.url("files/missing.txt")).send().await.unwrap().status(), 404);
    for bad in ["..%2Fsecret", ".hidden"] {
        assert_eq!(client().get(r.url(&format!("files/{bad}"))).send().await.unwrap().status(), 404, "{bad}");
    }
}

#[tokio::test]
async fn inline_images_follow_the_local_rules() {
    let e = tt::env().await;
    let r = setup(&e, "dl-inline").await;
    std::fs::write(r.outbox.join("pic.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
    std::fs::write(r.outbox.join("note.txt"), b"hello").unwrap();
    let inline = client().get(format!("{}?inline=1", r.url("files/pic.png"))).send().await.unwrap();
    assert_eq!(inline.status(), 200);
    assert_eq!(inline.headers()["content-type"], "image/png");
    assert!(inline.headers()["content-disposition"].to_str().unwrap().starts_with("inline"));
    assert!(inline.headers()["content-security-policy"].to_str().unwrap().contains("sandbox"));
    let text = client().get(format!("{}?inline=1", r.url("files/note.txt"))).send().await.unwrap();
    assert!(text.headers()["content-disposition"].to_str().unwrap().starts_with("attachment"), "不是白名單圖片就照樣是附件");
}

/// 檔案在傳輸中被截短（宣告的長度對不上）：中斷連線，不補零。
#[tokio::test]
async fn a_truncated_remote_file_aborts_the_connection() {
    let e = tt::env().await;
    let r = setup(&e, "dl-cut").await;
    std::fs::write(r.outbox.join("a.bin"), vec![7u8; 4096]).unwrap();
    let home = r.home.clone();
    am_base::hosts::set_ssh_fake_io(&r.host, move |script: &str, stdin: &[u8]| {
        use std::io::Write as _;
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env("HOME", &home)
            .env("TMPDIR", &home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        child.stdin.take().unwrap().write_all(stdin)?;
        let mut out = child.wait_with_output()?.stdout;
        if out.starts_with(b"AM_RFS1\nFILE ") {
            out.truncate(out.len() - 100);
        }
        Ok(out)
    });
    // 連線在 body 中途被中斷：client 要嘛在收到標頭之前就看到錯誤（小檔常見），要嘛 body 讀到一半出錯——不會收到補零的完整檔案。
    match client().get(r.url("files/a.bin")).send().await {
        Err(_) => {}
        Ok(resp) => {
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.headers()["content-length"], "4096");
            assert!(resp.bytes().await.is_err(), "少了的位元組不補零：連線被中斷");
        }
    }
}

async fn wait_until(mut ok: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ok()
}

/// client 讀了一點就斷線：主機名額（4）與下載名額都回收，之後又能下載。
#[tokio::test]
async fn a_client_that_hangs_up_gives_the_download_slots_back() {
    let e = tt::env().await;
    let r = setup(&e, "dl-hangup").await;
    std::fs::write(r.outbox.join("slow.bin"), vec![1u8; 1024 * 1024]).unwrap();
    am_base::hosts::set_ssh_stream_chunk_delay(&r.host, 16 * 1024, Duration::from_millis(40));
    let slot = super::remote_fs::host_slot(&r.host);
    let mut resp = client().get(r.url("files/slow.bin")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.chunk().await.unwrap();
    assert!(slot.available_permits() < 4, "下載進行中占著主機名額");
    drop(resp);
    assert!(wait_until(|| slot.available_permits() == 4).await, "斷線後主機名額回收");
    // 下載名額（每分享 2）也回收：連開兩個再開第三個，不是 429。
    am_base::hosts::set_ssh_stream_chunk_delay(&r.host, 1024 * 1024, Duration::from_millis(0));
    for _ in 0..4 {
        assert_eq!(client().get(r.url("files/slow.bin")).send().await.unwrap().bytes().await.unwrap().len(), 1024 * 1024);
    }
}

/// 主機的分享 ssh 名額用完：下載立刻 429 `what=download`，不排隊。
#[tokio::test]
async fn an_exhausted_host_slot_pool_is_a_429_for_downloads() {
    let e = tt::env().await;
    let r = setup(&e, "dl-slots").await;
    std::fs::write(r.outbox.join("a.txt"), b"hello").unwrap();
    let slot = super::remote_fs::host_slot(&r.host);
    let held: Vec<_> = (0..slot.available_permits()).map(|_| slot.clone().try_acquire_owned().unwrap()).collect();
    let resp = client().get(r.url("files/a.txt")).send().await.unwrap();
    assert_eq!(resp.status(), 429);
    assert!(resp.headers().contains_key("retry-after"));
    drop(held);
    assert_eq!(client().get(r.url("files/a.txt")).send().await.unwrap().status(), 200);
}

// ───────────────────────── SVG 嵌照片 ─────────────────────────

#[tokio::test]
async fn a_remote_svg_gets_its_inbox_photos_embedded() {
    let e = tt::env().await;
    let r = setup(&e, "svg-embed").await;
    std::fs::write(r.inbox().join("p.jpg"), jpeg(400, 300)).unwrap();
    std::fs::write(
        r.outbox.join("poster.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink"><image href="inbox/p.jpg"/><image href="inbox/nope.jpg"/></svg>"#,
    )
    .unwrap();
    let resp = client().get(format!("{}?inline=1", r.url("files/poster.svg"))).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "image/svg+xml");
    let body = resp.text().await.unwrap();
    assert!(body.contains("data:image/jpeg;base64,"), "照片嵌進去了：{body}");
    assert!(body.contains(r#"data-am-embed="not_found""#), "抓不到的那張標 not_found（同本機）：{body}");
    // 原檔不改。
    assert!(std::fs::read_to_string(r.outbox.join("poster.svg")).unwrap().contains(r#"href="inbox/p.jpg""#));
}

/// 照片路徑上有符號連結（指到工作目錄外）：不嵌，標 not_found。
#[tokio::test]
async fn a_symlinked_photo_is_not_embedded() {
    let e = tt::env().await;
    let r = setup(&e, "svg-link").await;
    let outside = r.home.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.jpg"), jpeg(50, 50)).unwrap();
    std::os::unix::fs::symlink(&outside, r.workspace.join("linked")).unwrap();
    std::fs::write(r.outbox.join("p.svg"), r#"<svg xmlns="http://www.w3.org/2000/svg"><image href="linked/secret.jpg"/></svg>"#).unwrap();
    let body = client().get(format!("{}?inline=1", r.url("files/p.svg"))).send().await.unwrap().text().await.unwrap();
    assert!(!body.contains("base64"), "{body}");
    assert!(body.contains(r#"data-am-embed="not_found""#), "{body}");
}

/// 一次嵌圖從遠端抓回來的照片合計 ≤ 64 MiB：超過的那幾張 `source_too_large`。
#[tokio::test]
async fn the_remote_photo_fetch_has_a_total_cap() {
    let e = tt::env().await;
    let r = setup(&e, "svg-total").await;
    let site = r.site(&e.app).await;
    for name in ["a.bin", "b.bin", "c.bin"] {
        // 25 MiB 稀疏檔（不是圖，但大小要算進合計）。
        std::fs::File::create(r.inbox().join(name)).unwrap().set_len(25 * 1024 * 1024).unwrap();
    }
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="inbox/a.bin"/><image href="inbox/b.bin"/><image href="inbox/c.bin"/></svg>"#.to_vec();
    let out = String::from_utf8(super::remote_io::embed_remote(&site, svg).await).unwrap();
    let marks: Vec<&str> = out.match_indices("data-am-embed=\"").map(|(i, _)| out[i + 15..].split('"').next().unwrap()).collect();
    assert_eq!(marks.len(), 3, "{out}");
    assert_eq!(marks[2], "source_too_large", "前兩張 50 MiB 內，第三張把合計推過 64 MiB：{marks:?}");
    assert_ne!(marks[0], "source_too_large");
    assert_ne!(marks[1], "source_too_large");
}

/// 快取鍵含主機：兩台主機同一條路徑、同樣的 inode／大小／mtime，不會串到對方的縮圖。
#[test]
fn the_photo_cache_does_not_mix_two_hosts() {
    use super::compose::{embed_with, PhotoMeta, PrefetchedSource};
    let rel = vec!["inbox".to_string(), "same.jpg".to_string()];
    let meta = PhotoMeta { ino: 7, len: 1234, mtime_ns: 99 };
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="inbox/same.jpg"/></svg>"#;
    let on = |host: &str, bytes: Vec<u8>| {
        let src = PrefetchedSource::new(host, "/home/u/shared-bots/x", std::slice::from_ref(&rel), &[Some(meta)], vec![Some(Ok(bytes))]);
        String::from_utf8(embed_with(svg, &src).unwrap()).unwrap()
    };
    let a = on("host-a", jpeg(200, 100));
    let b = on("host-b", jpeg(100, 200));
    assert_ne!(a, b, "同一條路徑、同樣的身分，兩台主機各自的照片");
    assert!(a.contains("data:image/jpeg;base64,") && b.contains("data:image/jpeg;base64,"));
    // 同一台再來一次走快取（身分一樣就不重解，即使這次給的位元組是壞的）。
    let again = on("host-a", b"not an image at all".to_vec());
    assert_eq!(again, a);
}

/// `embed_with` 的本機來源與預先抓好的來源，同樣的照片給同樣的結果。
#[test]
fn a_prefetched_source_embeds_the_same_as_the_local_folder() {
    use super::compose::{embed, embed_with, PhotoMeta, PrefetchedSource};
    use std::os::unix::fs::MetadataExt as _;
    let dir = super::test_dirs::scratch_dir("rs3-same");
    std::fs::create_dir_all(dir.join("inbox")).unwrap();
    let bytes = jpeg(640, 480);
    std::fs::write(dir.join("inbox/p.jpg"), &bytes).unwrap();
    let svg = br#"<svg xmlns="http://www.w3.org/2000/svg"><image href="inbox/p.jpg"/><image href="inbox/missing.jpg"/></svg>"#;
    let local = embed(svg, &dir).unwrap();
    let meta = std::fs::metadata(dir.join("inbox/p.jpg")).unwrap();
    let rels = [vec!["inbox".to_string(), "p.jpg".to_string()], vec!["inbox".to_string(), "missing.jpg".to_string()]];
    let metas = [Some(PhotoMeta { ino: meta.ino(), len: meta.len(), mtime_ns: meta.mtime() as i128 * 1_000_000_000 + meta.mtime_nsec() as i128 }), None];
    let src = PrefetchedSource::new("host-x", "/w", &rels, &metas, vec![Some(Ok(bytes)), None]);
    let remote = embed_with(svg, &src).unwrap();
    assert_eq!(String::from_utf8(local).unwrap(), String::from_utf8(remote).unwrap());
    assert_eq!(super::compose::referenced_rels(svg), rels.to_vec(), "先找出引用的路徑，遠端才知道要抓哪些");
}

// ───────────────────────── 壞 SVG 提醒 ─────────────────────────

/// 遠端 outbox 的壞 SVG：分享頁讀清單時查到，提醒 bot 一次；同一版本不重送。
#[tokio::test]
async fn a_broken_remote_svg_gets_one_reminder() {
    let e = tt::env().await;
    let r = setup(&e, "svg-bad").await;
    std::fs::write(r.outbox.join("card.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\">\n<text x=\"540\" y=\"380\"font-size=\"100\">嗨</text>\n</svg>").unwrap();
    let listed = client().get(r.url("files")).send().await.unwrap();
    assert_eq!(listed.status(), 200);
    for _ in 0..2 {
        let err = super::svg_check::check_file(&e.app, &r.bot.id, "card.svg").await.expect("壞掉的檔");
        assert_eq!((err.line, err.col), (2, 22));
    }
    let reminders: Vec<(String,)> = sqlx::query_as("SELECT content FROM messages WHERE role = 'user' AND content LIKE '%card.svg%'").fetch_all(&e.app.db).await.unwrap();
    assert_eq!(reminders.len(), 1, "同一個檔同一個錯誤只提醒一次：{reminders:?}");
    std::fs::write(r.outbox.join("card.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"><text x=\"540\" y=\"380\" font-size=\"100\">嗨</text></svg>").unwrap();
    assert_eq!(super::svg_check::check_file(&e.app, &r.bot.id, "card.svg").await, None, "修好了就不再提醒");
}

/// 讀不到（主機斷線）不提醒、也不記成「查過了」：下次還會再查。
#[tokio::test]
async fn an_unreadable_remote_svg_is_not_checked_and_not_remembered() {
    let e = tt::env().await;
    let r = setup(&e, "svg-down").await;
    std::fs::write(r.outbox.join("card.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"><text x=\"1\"y=\"2\"/></svg>").unwrap();
    r.conn.connected.store(false, Ordering::SeqCst);
    assert_eq!(super::svg_check::check_file(&e.app, &r.bot.id, "card.svg").await, None);
    r.conn.connected.store(true, Ordering::SeqCst);
    assert!(super::svg_check::check_file(&e.app, &r.bot.id, "card.svg").await.is_some(), "連回來之後照樣查得到");
}
