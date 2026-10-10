//! 分享入口的遠端 I/O 接合層（#955 R-S3，remote-share-design §5）。
//!
//! `portal`／`svg_check` 先問 [`remote_site`]：`None`＝本機（走原本的程式碼，行為一字不改），`Some(site)`＝專案在遠端主機，
//! 檔案動作改走 [`crate::share::remote_fs`]。解析不出來（DB 讀不到、主機不認得或斷線）一律 `Err(())`，對外是一般 503（不洩漏主機名）。
//!
//! 這個檔也放遠端專用的回應組裝：串流下載（長度由首行宣告，實際位元組數對不上就中斷連線、不補零）與 SVG 嵌照片（先抓好再同步嵌）。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use tokio::io::AsyncReadExt as _;
use tokio::sync::OwnedSemaphorePermit;

use crate::outbox::{content_disposition, mime_of, ShareFileError, MAX_BYTES};
use crate::share::compose::{self, PhotoMeta, PrefetchedSource};
use crate::share::remote_fs::{RemoteFile, TIMEOUT_STREAM_IDLE};
use am_base::hosts::{HostFence, SshStream};
use crate::share::site::{self, RemoteSite, ShareSite, SiteEnv, SiteError};

/// 「這顆分享 bot 的檔案在哪」的窄入口。`SiteEnv` 與 `ShareStorage` 有同名方法（`db_pool`、`data_dir`），兩個都當 supertrait 會讓泛型程式碼
/// 的 `app.db_pool()` 變成歧義；所以 `PortalEnv`／`SvgCheckEnv` 只依賴這個（方法名獨一無二），任何 `SiteEnv`（例如 `App`）自動實作。
pub trait ShareSiteResolver: Send + Sync {
    fn resolve_site(&self, bot_id: &str) -> impl Future<Output = Result<ShareSite, SiteError>> + Send;
}

impl<T: SiteEnv> ShareSiteResolver for T {
    fn resolve_site(&self, bot_id: &str) -> impl Future<Output = Result<ShareSite, SiteError>> + Send {
        site::resolve(self, bot_id)
    }
}

/// `Ok(None)`＝本機（或不是分享 bot，交給原本的路徑處理）；`Ok(Some)`＝遠端；`Err(())`＝解析不出來（fail closed，呼叫端回 503）。
pub async fn remote_site<H: ShareSiteResolver + ?Sized>(app: &H, bot_id: &str) -> Result<Option<RemoteSite>, ()> {
    match app.resolve_site(bot_id).await {
        Ok(ShareSite::Remote(site)) => Ok(Some(site)),
        Ok(ShareSite::Local { .. }) | Err(SiteError::NotShareBot) => Ok(None),
        Err(SiteError::Unavailable) => Err(()),
    }
}

/// 串流 body 一次最多讀這麼多。
const CHUNK: usize = 64 * 1024;

struct StreamState {
    head: Option<Bytes>,
    body: SshStream,
    // 每一塊讀取都要在這個權威仍是當前時才做（#1026）：repoint 之後舊主機的位元組不再送給 client。
    fence: HostFence,
    remaining: u64,
    idle: Duration,
    failed: bool,
    // 名額跟著串流活到 body 送完或 client 斷線（RAII）；ssh 行程也一樣（`SshStream` drop 時殺掉）。
    _permit: OwnedSemaphorePermit,
    _download_permit: Option<OwnedSemaphorePermit>,
}

/// 把遠端檔案串成 body：先送已讀的檔頭，再送剩下的 stdout；每塊重新計 [`TIMEOUT_STREAM_IDLE`] 閒置逾時。
/// 提早 EOF（檔被截短）、讀取錯誤、逾時都回 `Err`，讓 hyper 直接中斷連線——絕不補零，client 才知道檔案不完整。
fn body_stream(file: RemoteFile, idle: Duration) -> impl futures::Stream<Item = Result<Bytes, std::io::Error>> + Send {
    let RemoteFile { len, head, body, fence, permit, download_permit } = file;
    let head = Bytes::from(head);
    let remaining = len.saturating_sub(head.len() as u64);
    futures::stream::unfold(
        StreamState { head: Some(head).filter(|h| !h.is_empty()), body, fence, remaining, idle, failed: false, _permit: permit, _download_permit: download_permit },
        |mut st| async move {
            if st.failed {
                return None;
            }
            if let Some(h) = st.head.take() {
                return Some((Ok(h), st));
            }
            if st.remaining == 0 {
                return None;
            }
            let want = st.remaining.min(CHUNK as u64) as usize;
            let mut buf = vec![0u8; want];
            let fence = st.fence.clone();
            match tokio::time::timeout(st.idle, fence.run_current(st.body.read(&mut buf))).await {
                Ok(None) => {
                    st.failed = true;
                    Some((Err(superseded()), st))
                }
                Ok(Some(Ok(0))) => {
                    st.failed = true;
                    Some((Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "remote file was truncated")), st))
                }
                Ok(Some(Ok(n))) => {
                    buf.truncate(n);
                    st.remaining -= n as u64;
                    Some((Ok(Bytes::from(buf)), st))
                }
                Ok(Some(Err(e))) => {
                    st.failed = true;
                    Some((Err(e), st))
                }
                Err(_) => {
                    st.failed = true;
                    Some((Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "remote download went idle")), st))
                }
            }
        },
    )
}

/// 圍籬已經換掉（repoint／重連）：串流中途斷線，不把舊主機的剩餘位元組補給 client。
fn superseded() -> std::io::Error {
    std::io::Error::other("remote host authority was replaced during the download")
}

fn headers_for(name: &str, len: u64) -> [(header::HeaderName, String); 5] {
    [
        (header::CONTENT_TYPE, mime_of(std::path::Path::new(name)).to_string()),
        (header::CONTENT_DISPOSITION, content_disposition(name)),
        (header::CONTENT_LENGTH, len.to_string()),
        (header::CACHE_CONTROL, "private, no-store".to_string()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
    ]
}

/// 一般下載：驗證（首行宣告的大小、檔頭黑名單）已由 `outbox_stream` 做完，這裡只組回應，body 邊收邊送。
pub fn stream_response(name: &str, file: RemoteFile) -> Response {
    let headers = headers_for(name, file.len);
    (StatusCode::OK, headers, axum::body::Body::from_stream(body_stream(file, TIMEOUT_STREAM_IDLE))).into_response()
}

/// 整份讀進記憶體（SVG 嵌照片要先拿到全文）：長度不等於宣告的就當失敗。
async fn collect(file: RemoteFile, idle: Duration) -> Result<Vec<u8>, ()> {
    let RemoteFile { len, head: mut out, mut body, fence, permit, download_permit } = file;
    let want = len as usize;
    out.reserve(want.saturating_sub(out.len()));
    let mut chunk = vec![0u8; CHUNK];
    while out.len() < want {
        let take = (want - out.len()).min(CHUNK);
        match tokio::time::timeout(idle, fence.run_current(body.read(&mut chunk[..take]))).await {
            Ok(Some(Ok(n))) if n > 0 => out.extend_from_slice(&chunk[..n]),
            _ => return Err(()),
        }
    }
    // 讀完才發現主機已經換掉：這份全文是舊主機的，不嵌、不回（#1026）。
    if !fence.is_current_now() {
        return Err(());
    }
    // 讀完就放掉主機名額：接下來的 stat／抓照片各自再拿（整段仍受每分享 2、全站 8 的下載名額限制），
    // 不然 4 個同時嵌圖的下載各占一格、再等第五格，全部互等。
    drop(permit);
    drop(download_permit);
    Ok(out)
}

/// 遠端 `.svg`（`?inline=1` 用）：全文讀回來，把引用的 inbox 照片嵌成 data URI（[`embed_remote`]），回整份 body。
pub async fn svg_response(site: &RemoteSite, name: &str, file: RemoteFile) -> Result<Response, ShareFileError> {
    let svg = collect(file, TIMEOUT_STREAM_IDLE).await.map_err(|_| ShareFileError::Unavailable)?;
    let svg = embed_remote(site, svg).await;
    let headers = headers_for(name, svg.len() as u64);
    Ok((StatusCode::OK, headers, axum::body::Body::from(svg)).into_response())
}

/// 遠端 SVG 嵌照片（§5.4）：純函式找出引用路徑 → 一趟 `photo_stats` → 快取命中的不抓 → 沒命中的一趟 `photo_fetch`（每張 ≤ 40 MiB、這一次合計 ≤ 64 MiB）
/// → 預先抓好的 [`PrefetchedSource`] 在 `spawn_blocking` 跑 [`compose::embed_with`]。抓不到的照片該張 `data-am-embed="not_found"`（同本機）。
pub async fn embed_remote(site: &RemoteSite, svg: Vec<u8>) -> Vec<u8> {
    if !compose::wants_embed(&svg) {
        return svg;
    }
    let svg = Arc::new(svg);
    let rels = {
        let svg = svg.clone();
        tokio::task::spawn_blocking(move || compose::referenced_rels(&svg)).await.unwrap_or_default()
    };
    if rels.is_empty() {
        return Arc::try_unwrap(svg).unwrap_or_else(|a| (*a).clone());
    }
    let metas: Vec<Option<PhotoMeta>> = match site.photo_stats(&rels).await {
        Ok(stats) => stats.into_iter().map(|s| s.map(|p| PhotoMeta { ino: p.ino, len: p.size, mtime_ns: p.mtime_ns })).collect(),
        Err(e) => {
            tracing::warn!(host = %site.host, error = %e, "share svg: could not stat the photos on the remote host");
            vec![None; rels.len()]
        }
    };
    let scope = PrefetchedSource::scope_of(&site.host, &site.workspace);
    let mut need: Vec<usize> = Vec::new();
    for (i, meta) in metas.iter().enumerate() {
        if let Some(meta) = meta {
            if meta.len <= compose::PHOTO_SOURCE_MAX && !compose::is_cached(&scope, &rels[i], meta) {
                need.push(i);
            }
        }
    }
    let mut data: Vec<Option<Result<Vec<u8>, &'static str>>> = vec![None; rels.len()];
    if !need.is_empty() {
        let wanted: Vec<Vec<String>> = need.iter().map(|&i| rels[i].clone()).collect();
        match site.photo_fetch(&wanted, compose::PHOTO_SOURCE_MAX, REMOTE_PHOTOS_TOTAL_MAX).await {
            Ok(fetched) => {
                for (&i, one) in need.iter().zip(fetched) {
                    data[i] = Some(one);
                }
            }
            Err(e) => {
                tracing::warn!(host = %site.host, error = %e, "share svg: could not fetch the photos from the remote host");
                for &i in &need {
                    data[i] = Some(Err("read_failed"));
                }
            }
        }
    }
    let src = PrefetchedSource::new(&site.host, &site.workspace, &rels, &metas, data);
    let source = svg.clone();
    match tokio::task::spawn_blocking(move || compose::embed_with(&source, &src)).await {
        Ok(Some(embedded)) => embedded,
        _ => Arc::try_unwrap(svg).unwrap_or_else(|a| (*a).clone()),
    }
}

/// 一次嵌圖從遠端抓回來的照片合計上限（§5.4）；超過的那幾張 `source_too_large`。
pub const REMOTE_PHOTOS_TOTAL_MAX: u64 = 64 * 1024 * 1024;

/// 遠端 outbox 單檔大小上限（與本機分享下載相同）。
pub const DOWNLOAD_MAX: u64 = MAX_BYTES;
