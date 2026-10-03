use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::testing as tt;

async fn fixture() -> (tt::Env, String, Portal, SocketAddr, reqwest::Client) {
    let e = tt::env().await;
    let b = tt::claude_bot(&e.app, &e.project_id, "share-upload-tests").await;
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
    let folder = crate::share::folder::ShareFolderIn::New {
        name: "share-upload-tests".into(),
    };
    let (ws, made) = crate::share::admin::reserve_restricted(&e.app, &b.id, &folder, false)
        .await
        .unwrap();
    crate::share::admin::finish_restricted(&e.app, &b.id, &ws, made, true).await;
    let token = crate::share::store::enable(&e.app.db, &b.id)
        .await
        .unwrap()
        .unwrap();
    let state = Portal {
        app: e.app.clone(),
        limits: Arc::new(Limits::default()),
        uploads: Arc::new(Semaphore::new(2)),
        streams: Arc::new(Semaphore::new(MAX_STREAMS)),
        share_streams: Arc::new(ShareStreamLimits::default()),
        token_lookups: Arc::new(Semaphore::new(MAX_TOKEN_LOOKUPS)),
    };
    let addr = serve(router_with_state(state.clone())).await;
    (e, token, state, addr, client())
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
    tokio::time::timeout(Duration::from_secs(1), async {
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

#[tokio::test]
async fn issue_823_holds_both_upload_permits_until_body_read_and_rejects_a_third_unread() {
    let (_e, token, state, addr, _client) = fixture().await;
    let first = partial_upload(addr, &format!("/s/{token}/api/upload?name=x.txt")).await;
    let second = partial_upload(addr, &format!("/s/{token}/api/upload?name=x.txt")).await;
    tokio::time::timeout(Duration::from_secs(2), async {
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
    let (_e, token, state, addr, client) = fixture().await;
    let uri = format!("/s/{token}/api/upload?name=x.txt");
    let first = partial_upload(addr, &uri).await;
    let second = partial_upload(addr, &uri).await;
    tokio::time::timeout(Duration::from_secs(2), async {
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
    tokio::time::timeout(Duration::from_secs(2), async {
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
        "the 21st invalid attempt must be charged"
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
