use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::testing as tt;

/// 等名額被占滿／放回的上限。只是失敗時的保險（成功時幾毫秒就到）；被等的是真的 TCP＋DB，
/// 2 秒在整樹平行時會被排擠到逾時（#1155，同 #952）。
const PERMIT_WAIT: Duration = Duration::from_secs(30);

async fn fixture() -> (tt::Env, String, Portal<crate::state::App>, SocketAddr, reqwest::Client) {
    fixture_with(UPLOAD_QUEUE_WAIT).await
}

/// `upload_wait`：名額滿了排隊等多久。
async fn fixture_with(upload_wait: Duration) -> (tt::Env, String, Portal<crate::state::App>, SocketAddr, reqwest::Client) {
    let e = tt::env().await;
    let root = tt::scratch_dir("am-share-upload-root");
    let root = root.to_string_lossy().into_owned();
    e.app
        .cfg
        .update(|cfg| {
            cfg.share.folders_root = Some(root.clone());
            Ok(())
        })
        .await
        .unwrap();
    let (_, token) = make_share(&e, "share-upload-tests").await;
    let state = Portal {
        app: e.app.clone(),
        limits: Arc::new(Limits::default()),
        uploads: Arc::new(Semaphore::new(UPLOAD_SLOTS)),
        upload_queue: Arc::new(Semaphore::new(UPLOAD_QUEUE_MAX)),
        share_uploads: Arc::new(PerShareSlots::new(UPLOAD_INFLIGHT_PER_SHARE)),
        downloads: Arc::new(Semaphore::new(DOWNLOAD_SLOTS)),
        share_downloads: Arc::new(PerShareSlots::new(DOWNLOADS_PER_SHARE)),
        upload_wait,
        streams: Arc::new(Semaphore::new(MAX_STREAMS)),
        share_streams: Arc::new(PerShareSlots::new(MAX_STREAMS_PER_SHARE)),
        token_lookups: Arc::new(Semaphore::new(MAX_TOKEN_LOOKUPS)),
    };
    let addr = serve(router_with_state(state.clone())).await;
    (e, token, state, addr, client())
}

/// 再開一顆分享用 bot（`cfg.share.folders_root` 要先設好）：回（bot id, token）。
async fn make_share(e: &tt::Env, name: &str) -> (String, String) {
    let b = tt::claude_bot(&e.app, &e.project_id, name).await;
    let folder = crate::share::folder::ShareFolderIn::New { name: name.into() };
    let (ws, made) = crate::share::admin::reserve_restricted(&e.app, &b.id, &folder, false).await.unwrap();
    crate::share::admin::finish_restricted(&e.app, &b.id, &ws, made, true).await;
    let token = crate::share::store::enable(&e.app.db, &b.id).await.unwrap().unwrap();
    (b.id, token)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    addr
}

async fn call(
    client: &reqwest::Client,
    addr: SocketAddr,
    uri: &str,
    content_type: &str,
    body: Vec<u8>,
) -> StatusCode {
    client
        .post(format!("http://{addr}{uri}"))
        .header("content-type", content_type)
        .body(body)
        .send()
        .await
        .unwrap()
        .status()
}

async fn partial_upload(addr: SocketAddr, uri: &str) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let headers = format!(
        "POST {uri} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/octet-stream\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream
}

async fn response_status(stream: &mut tokio::net::TcpStream) -> u16 {
    let mut line = Vec::new();
    // 第三個上傳要先排隊 300ms，高負載下更久：上限只是放棄的期限，不是成功的條件（#950）。
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut byte = [0_u8; 1];
            stream.read_exact(&mut byte).await.unwrap();
            line.push(byte[0]);
            if byte[0] == b'\n' {
                break;
            }
        }
    })
    .await
    .expect("response should arrive without the declared body");
    String::from_utf8_lossy(&line)
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn issue_823_rejects_an_invalid_token_before_polling_the_upload_body() {
    let e = tt::env().await;
    let addr = serve(router(e.app.clone())).await;
    let mut request = partial_upload(addr, "/s/not-a-token/api/upload?name=x.txt").await;
    assert_eq!(
        response_status(&mut request).await,
        StatusCode::NOT_FOUND.as_u16()
    );
}

/// 名額滿了的第三個排隊等；等不到（測試 300ms）才 429，而且一樣沒讀 body。
#[tokio::test]
async fn issue_823_holds_both_upload_permits_until_body_read_and_rejects_a_third_unread() {
    let (_e, token, state, addr, _client) = fixture().await;
    let first = partial_upload(addr, &format!("/s/{token}/api/upload?name=x.txt")).await;
    let second = partial_upload(addr, &format!("/s/{token}/api/upload?name=x.txt")).await;
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("first two slow bodies should hold both upload permits");

    let mut third = partial_upload(addr, &format!("/s/{token}/api/upload?name=x.txt")).await;
    assert_eq!(
        response_status(&mut third).await,
        StatusCode::TOO_MANY_REQUESTS.as_u16()
    );
    drop((first, second, third));
}

#[tokio::test]
async fn a_capacity_rejection_does_not_spend_the_upload_rate_limit() {
    let (_e, token, state, addr, client) = fixture_with(Duration::from_millis(20)).await;
    let uri = format!("/s/{token}/api/upload?name=x.txt");
    let first = partial_upload(addr, &uri).await;
    let second = partial_upload(addr, &uri).await;
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two slow bodies should hold both upload permits");

    for _ in 0..UPLOADS_PER_MIN {
        let mut rejected = partial_upload(addr, &uri).await;
        assert_eq!(response_status(&mut rejected).await, StatusCode::TOO_MANY_REQUESTS.as_u16());
    }
    drop((first, second));
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closing the slow bodies should return an upload permit");

    assert_eq!(
        call(&client, addr, &format!("/s/{token}/api/upload?name=note.txt"), "application/octet-stream", b"hello".to_vec()).await,
        StatusCode::OK,
        "concurrency rejections must not fill the per-share upload window"
    );
}

#[tokio::test]
async fn issue_824_invalid_uploads_spend_the_upload_rate_limit() {
    let (_e, token, _state, addr, client) = fixture().await;
    for attempt in 0..UPLOADS_PER_MIN {
        assert_eq!(
            call(
                &client,
                addr,
                &format!("/s/{token}/api/upload?name=not-a-png.png"),
                "application/octet-stream",
                b"not a png".to_vec()
            )
            .await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "attempt {attempt}"
        );
    }

    assert_eq!(
        call(
            &client,
            addr,
            &format!("/s/{token}/api/upload?name=not-a-png.png"),
            "application/octet-stream",
            b"not a png".to_vec()
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS,
        "the attempt past UPLOADS_PER_MIN must be charged"
    );
}

#[tokio::test]
async fn issue_825_invalid_attachment_checks_spend_the_message_rate_limit() {
    let (_e, token, _state, addr, client) = fixture().await;
    let cheap_invalid =
        serde_json::to_vec(&json!({"text": "see this", "client_request_id": "has space"})).unwrap();
    assert_eq!(
        call(
            &client,
            addr,
            &format!("/s/{token}/api/messages"),
            "application/json",
            cheap_invalid
        )
        .await,
        StatusCode::BAD_REQUEST,
        "cheap shape errors are checked before rate charging"
    );
    let missing = "01ARZ3NDEKTSV4RRFFQ69G5FAV-missing.txt";
    for attempt in 0..MESSAGES_PER_MIN {
        let body = serde_json::to_vec(&json!({"text": "see this", "client_request_id": format!("c{attempt}"), "attachments": [missing]})).unwrap();
        assert_eq!(
            call(
                &client,
                addr,
                &format!("/s/{token}/api/messages"),
                "application/json",
                body
            )
            .await,
            StatusCode::BAD_REQUEST,
            "attempt {attempt}"
        );
    }

    let body = serde_json::to_vec(
        &json!({"text": "see this", "client_request_id": "c-last", "attachments": [missing]}),
    )
    .unwrap();
    assert_eq!(
        call(
            &client,
            addr,
            &format!("/s/{token}/api/messages"),
            "application/json",
            body
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS,
        "the 11th invalid attachment must be charged before filesystem lookup"
    );
}

fn zip_stored(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn u16(out: &mut Vec<u8>, value: u16) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn u32(out: &mut Vec<u8>, value: u32) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let name = name.as_bytes();
        let offset = out.len() as u32;
        let crc = crc32(data);
        u32(&mut out, 0x0403_4b50);
        u16(&mut out, 20);
        u16(&mut out, 0);
        u16(&mut out, 0);
        u16(&mut out, 0);
        u16(&mut out, 0);
        u32(&mut out, crc);
        u32(&mut out, data.len() as u32);
        u32(&mut out, data.len() as u32);
        u16(&mut out, name.len() as u16);
        u16(&mut out, 0);
        out.extend_from_slice(name);
        out.extend_from_slice(data);

        u32(&mut central, 0x0201_4b50);
        u16(&mut central, 20);
        u16(&mut central, 20);
        u16(&mut central, 0);
        u16(&mut central, 0);
        u16(&mut central, 0);
        u16(&mut central, 0);
        u32(&mut central, crc);
        u32(&mut central, data.len() as u32);
        u32(&mut central, data.len() as u32);
        u16(&mut central, name.len() as u16);
        u16(&mut central, 0);
        u16(&mut central, 0);
        u16(&mut central, 0);
        u16(&mut central, 0);
        u32(&mut central, 0);
        u32(&mut central, offset);
        central.extend_from_slice(name);
    }
    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);
    u32(&mut out, 0x0605_4b50);
    u16(&mut out, 0);
    u16(&mut out, 0);
    u16(&mut out, entries.len() as u16);
    u16(&mut out, entries.len() as u16);
    u32(&mut out, central_size);
    u32(&mut out, central_offset);
    u16(&mut out, 0);
    out
}

#[test]
fn issue_826_office_uploads_require_bounded_ooxml_zip_structure_and_return_office_mimes() {
    let common = [
        ("[Content_Types].xml", b"<Types/>".as_slice()),
        ("_rels/.rels", b"<Relationships/>".as_slice()),
    ];
    let cases = [
        (
            "report.docx",
            "word/document.xml",
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        ),
        (
            "sheet.xlsx",
            "xl/workbook.xml",
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        ),
        (
            "slides.pptx",
            "ppt/presentation.xml",
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        ),
    ];
    for (name, root, mime) in cases {
        let mut entries = common.to_vec();
        entries.push((root, b"<root/>".as_slice()));
        assert_eq!(
            classify_upload(name, &zip_stored(&entries)),
            Ok(mime),
            "{name}"
        );
        assert_eq!(
            classify_upload(name, &zip_stored(&[("arbitrary.bin", b"payload")])),
            Err("content_mismatch"),
            "generic archive: {name}"
        );
        assert_eq!(
            classify_upload(name, &zip_stored(&common)),
            Err("content_mismatch"),
            "missing document root: {name}"
        );
        assert_eq!(
            classify_upload(
                name,
                &zip_stored(&[("[Content_Types].xml", b"<Types/>".as_slice())])
            ),
            Err("content_mismatch"),
            "content types alone is not an Office package: {name}"
        );
        assert_eq!(
            classify_upload(name, b"PK\x03\x04"),
            Err("content_mismatch"),
            "truncated archive: {name}"
        );
    }

    // 第一個 stored member 的宣告大小延伸到其餘 local record，central entries 因而重疊。
    let mut overlapping = zip_stored(&[
        ("[Content_Types].xml", b"<Types/>"),
        ("_rels/.rels", b"<Relationships/>"),
        ("word/document.xml", b"<root/>"),
    ]);
    let eocd = overlapping.len() - 22;
    let central_offset =
        u32::from_le_bytes(overlapping[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
    let payload_offset = 30 + "[Content_Types].xml".len();
    let extended_size = (central_offset - payload_offset) as u32;
    overlapping[18..22].copy_from_slice(&extended_size.to_le_bytes());
    overlapping[22..26].copy_from_slice(&extended_size.to_le_bytes());
    overlapping[central_offset + 20..central_offset + 24]
        .copy_from_slice(&extended_size.to_le_bytes());
    overlapping[central_offset + 24..central_offset + 28]
        .copy_from_slice(&extended_size.to_le_bytes());
    assert_eq!(
        classify_upload("report.docx", &overlapping),
        Err("content_mismatch"),
        "overlapping local entries are rejected"
    );

    let mut too_many_entries = vec![
        ("[Content_Types].xml", b"<Types/>".as_slice()),
        ("_rels/.rels", b"<Relationships/>".as_slice()),
        ("word/document.xml", b"<root/>".as_slice()),
    ];
    let many_names: Vec<String> = (0..4094).map(|i| format!("filler/{i}.bin")).collect();
    too_many_entries.extend(
        many_names
            .iter()
            .map(|name| (name.as_str(), b"".as_slice())),
    );
    assert_eq!(
        classify_upload("report.docx", &zip_stored(&too_many_entries)),
        Err("content_mismatch"),
        "entry-count limit is enforced"
    );

    let mut large_directory = vec![
        ("[Content_Types].xml", b"<Types/>".as_slice()),
        ("_rels/.rels", b"<Relationships/>".as_slice()),
        ("word/document.xml", b"<root/>".as_slice()),
    ];
    let long_names: Vec<String> = (0..1024)
        .map(|i| format!("{}-{i}", "x".repeat(1020)))
        .collect();
    large_directory.extend(
        long_names
            .iter()
            .map(|name| (name.as_str(), b"".as_slice())),
    );
    assert_eq!(
        classify_upload("report.docx", &zip_stored(&large_directory)),
        Err("content_mismatch"),
        "central-directory size limit is enforced"
    );
}

/// 客訴 2026-10-04：分享頁一次選好幾張照片、同時送出，第三張起就 429「傳得太快了」。名額（2 個）都被慢的 body 占著時，
/// 同時再來的上傳要排隊等、不 429；名額一空出來就輪到，全部存進 `inbox/`。
/// 單一分享進行中＋排隊合計上限 [`UPLOAD_INFLIGHT_PER_SHARE`]（4）：2 個慢的＋2 個排隊剛好用滿（官方頁本來就一張一張送）。
#[tokio::test]
async fn queued_uploads_wait_for_the_two_slots_instead_of_failing() {
    let (_e, token, state, addr, client) = fixture_with(Duration::from_secs(30)).await;
    let slow = format!("/s/{token}/api/upload?name=slow.txt");
    let mut first = partial_upload(addr, &slow).await;
    let mut second = partial_upload(addr, &slow).await;
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two slow bodies should hold both upload permits");

    let queued: Vec<_> = (0..UPLOAD_INFLIGHT_PER_SHARE - 2)
        .map(|i| {
            let (client, uri) = (client.clone(), format!("/s/{token}/api/upload?name=photo-{i}.txt"));
            tokio::spawn(async move { call(&client, addr, &uri, "application/octet-stream", format!("photo {i}").into_bytes()).await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(queued.iter().all(|h| !h.is_finished()), "名額滿了要排隊，不能先回 429");

    // 慢的兩個把 body 送完：名額空出來，排隊的依序進去。
    for s in [&mut first, &mut second] {
        s.write_all(&vec![b'a'; 1_048_576]).await.unwrap();
    }
    assert_eq!(response_status(&mut first).await, 200);
    assert_eq!(response_status(&mut second).await, 200);
    for h in queued {
        assert_eq!(h.await.unwrap(), StatusCode::OK);
    }
    let bot_id: String = sqlx::query_scalar("SELECT id FROM bots WHERE name = 'share-upload-tests'").fetch_one(&state.app.db).await.unwrap();
    let inbox = folder_of(&state.app, &bot_id).await.unwrap().join("inbox");
    assert_eq!(std::fs::read_dir(inbox).unwrap().count(), UPLOAD_INFLIGHT_PER_SHARE);
}

/// 一則訊息帶一整批照片（上限 [`MAX_ATTACHMENTS`]）、每分鐘上傳額度（[`UPLOADS_PER_MIN`]）都要容得下「一次選 10 張」再加重傳。
#[test]
fn a_batch_of_ten_phone_photos_fits_the_limits() {
    assert!(MAX_ATTACHMENTS >= 10);
    assert!(UPLOADS_PER_MIN >= 2 * 10, "10 張再重傳一輪也不撞每分鐘額度");
    assert!(INBOX_MAX_FILES >= 10 && INBOX_MAX_BYTES >= 10 * 8 * 1024 * 1024, "10 張原尺寸 8 MB 的照片放得下");
}

/// #844：一個分享連結不能把全站的上傳排隊位子佔滿；別的分享仍進得了排隊。
#[tokio::test]
async fn one_share_cannot_take_every_upload_queue_slot() {
    let (e, token_a, state, addr, client) = fixture_with(Duration::from_secs(30)).await;
    let (_, token_b) = make_share(&e, "share-upload-tests-b").await;
    let slow = format!("/s/{token_a}/api/upload?name=slow.txt");
    let mut first = partial_upload(addr, &slow).await;
    let mut second = partial_upload(addr, &slow).await;
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two slow bodies should hold both upload permits");

    // A 再排滿它自己的名額（進行中 2 ＋ 排隊 UPLOAD_INFLIGHT_PER_SHARE - 2）。
    let queued: Vec<_> = (0..UPLOAD_INFLIGHT_PER_SHARE - 2)
        .map(|i| {
            let (client, uri) = (client.clone(), format!("/s/{token_a}/api/upload?name=a-{i}.txt"));
            tokio::spawn(async move { call(&client, addr, &uri, "application/octet-stream", format!("a {i}").into_bytes()).await })
        })
        .collect();
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.upload_queue.available_permits() != UPLOAD_QUEUE_MAX - (UPLOAD_INFLIGHT_PER_SHARE - 2) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("A's extra uploads should be waiting in the queue");

    // 第 5 個起 A 立刻 429 upload_per_share，而且沒有再佔排隊位子。
    let r = client
        .post(format!("http://{addr}/s/{token_a}/api/upload?name=over.txt"))
        .header("content-type", "application/octet-stream")
        .body(b"over".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["what"], "upload_per_share");
    assert!(state.upload_queue.available_permits() >= UPLOAD_QUEUE_MAX - (UPLOAD_INFLIGHT_PER_SHARE - 2));

    // B 不受 A 影響：能進排隊，A 放開名額後 B 成功。
    let b_upload = {
        let (client, uri) = (client.clone(), format!("/s/{token_b}/api/upload?name=b.txt"));
        tokio::spawn(async move { call(&client, addr, &uri, "application/octet-stream", b"b".to_vec()).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!b_upload.is_finished(), "B 應該在排隊等名額，不是被 A 擋掉");
    for s in [&mut first, &mut second] {
        s.write_all(&vec![b'a'; 1_048_576]).await.unwrap();
    }
    assert_eq!(response_status(&mut first).await, 200);
    assert_eq!(response_status(&mut second).await, 200);
    assert_eq!(b_upload.await.unwrap(), StatusCode::OK);
    for h in queued {
        assert_eq!(h.await.unwrap(), StatusCode::OK);
    }
}

/// #844：排隊逾時回 429 之後，這個分享的計數要歸零（RAII 放掉）。
#[tokio::test]
async fn per_share_upload_permits_are_released_on_timeout() {
    let (_e, token, state, addr, client) = fixture_with(Duration::from_millis(50)).await;
    let slow = format!("/s/{token}/api/upload?name=slow.txt");
    let first = partial_upload(addr, &slow).await;
    let second = partial_upload(addr, &slow).await;
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("two slow bodies should hold both upload permits");

    // 排隊逾時（50ms）→ 429。
    assert_eq!(
        call(&client, addr, &format!("/s/{token}/api/upload?name=late.txt"), "application/octet-stream", b"late".to_vec()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    // 慢的兩個斷線之後，計數要回到 0：連續 UPLOAD_INFLIGHT_PER_SHARE 個上傳都不會吃到 upload_per_share。
    drop((first, second));
    tokio::time::timeout(PERMIT_WAIT, async {
        while state.uploads.available_permits() != 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("closing the slow bodies should return both upload permits");
    for i in 0..UPLOAD_INFLIGHT_PER_SHARE {
        assert_eq!(
            call(&client, addr, &format!("/s/{token}/api/upload?name=ok-{i}.txt"), "application/octet-stream", b"ok".to_vec()).await,
            StatusCode::OK,
            "逾時與斷線的 permit 都要放掉（第 {i} 個）"
        );
    }
}

#[test]
fn the_permit_wait_is_a_failure_bound_not_a_timing_assertion() {
    assert!(PERMIT_WAIT >= Duration::from_secs(30), "被等的是真的 TCP＋DB：上限太緊在高負載下會假紅（#952）");
}
