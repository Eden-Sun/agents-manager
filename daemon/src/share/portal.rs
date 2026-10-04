//! 分享入口：獨立的 listener（`[share] listen`，預設建議 `127.0.0.1:7790`，Tailscale Funnel 只指這個 port）。
//!
//! router 上**只有**下面這幾條，沒有 fallback 到主 API、沒有主 UI、沒有 `/ws`、沒有 `/hook`（[`router`]）：
//!
//! | 路由 | 做什麼 |
//! |---|---|
//! | `GET /s/{token}` | 分享頁（嵌入的 `web/dist/share.html`；還沒打包時是一頁佔位） |
//! | `GET /s/{token}/api/info` | `{bot_name, status}` |
//! | `GET /s/{token}/api/messages?before=&limit=` | `{bot_name, status, messages, has_more}`：end user 與 bot 的對話（end user 送的 user 與 assistant；倒回的、擁有者送的不給；只給 id、role、誰送的、text、`created_at`、附件名） |
//! | `POST /s/{token}/api/messages` | `{text, client_request_id, attachments?}` → 照一般送訊息流程（停著就起、忙就排隊），來源記成分享使用者 |
//! | `GET /s/{token}/api/events` | SSE：`message`（新訊息）、`status`（思考中／閒置…）、`resync`（漏了，請重抓） |
//! | `POST /s/{token}/api/upload` | `multipart/form-data` 的 `file` 欄位（或原始位元組＋`?name=`）；存進工作目錄的 `inbox/`，回 `{id, name, size, mime}` |
//! | `GET /s/{token}/api/files`、`GET /s/{token}/api/files/{name}` | 這顆 bot 的 outbox（沿用 outbox 的擋法與下載標頭）；`?inline=1` 只對圖片回 inline（[`inline_image`]） |
//! | `GET /assets/{*path}` | 分享頁的 js／css（嵌入的 `web/dist/assets/`） |
//!
//! token 錯、分享關了、bot 刪了：一律同一個 404（不洩漏存在與否）。每個回應都帶 `Cache-Control: no-store`（assets 除外，
//! 檔名有雜湊）、`Referrer-Policy: no-referrer`、`nosniff`、只允許自己的 CSP；不設任何 CORS 標頭。

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, OwnedMutexGuard, Semaphore};

use crate::db;
use crate::lifecycle::{self, LcError};
use crate::share::{multipart, store, SHARE_SENDER};
use crate::state::App;

/// 一則訊息的字數上限。
pub(crate) const MAX_TEXT_CHARS: usize = 8000;
/// 每個分享（每顆 bot）每分鐘最多送幾則。
pub(crate) const MESSAGES_PER_MIN: usize = 10;
/// 單檔上限 25 MiB。
pub(crate) const MAX_UPLOAD: usize = 25 * 1024 * 1024;
/// 每分鐘最多上傳幾個檔。分享頁一次選一整批手機照片是常態（客訴 2026-10-04：選 3 張以上就「傳得太快了」），
/// 一則最多 [`MAX_ATTACHMENTS`] 張，再留一倍給重傳；分享頁遇到 429 會照 `Retry-After` 自己等、自己重試。
pub(crate) const UPLOADS_PER_MIN: usize = 40;
/// `inbox/` 的總量上限（位元組、檔數）：滿了要等 bot 的主人清掉。
pub(crate) const INBOX_MAX_BYTES: u64 = 200 * 1024 * 1024;
pub(crate) const INBOX_MAX_FILES: usize = 300;
/// 一則訊息最多帶幾個附件：手機相簿一次選十幾張照片要能一則送出（原本 10，2026-10-04 調高）。
const MAX_ATTACHMENTS: usize = 20;
/// 全站同時讀取幾個上傳 body（每個最多 [`MAX_UPLOAD`]，佔記憶體）。
const UPLOAD_SLOTS: usize = 2;
/// 名額滿了的上傳排隊等多久：等的時候 body 還沒讀、不扣這個分享的每分鐘額度；等不到才 429。
#[cfg(not(test))]
const UPLOAD_QUEUE_WAIT: Duration = Duration::from_secs(60);
#[cfg(test)]
const UPLOAD_QUEUE_WAIT: Duration = Duration::from_millis(300);
/// 全站最多幾個上傳同時在排隊；再多的立刻 429（排隊的連線也是資源）。
const UPLOAD_QUEUE_MAX: usize = 64;
/// 同時開著的 SSE 連線（全部分享加起來）。
const MAX_STREAMS: usize = 32;
/// 單一分享同時開著的 SSE 連線；避免一個連結佔滿全部全域名額。
pub(crate) const MAX_STREAMS_PER_SHARE: usize = 4;
/// 進 token DB 查詢之前可同時佔用的全域名額。
const MAX_TOKEN_LOOKUPS: usize = 8;
/// SSE 每隔多久重新確認一次 token 還有效（關分享／重產時另外會被 [`kick`] 叫醒）。
const STREAM_RECHECK: Duration = Duration::from_secs(30);

/// 每則分享使用者的訊息一律以這個開頭再打進 TUI。claude 的輸入框把**第一個字**當模式切換：`!` 是 bash 模式
/// （2026-10-03 實測：`--restricted --tools … --permission-mode dontAsk`、settings 也 deny Bash，`!echo … > 檔` 照樣真的跑了），
/// `/` 是 slash 指令（`/permissions`、`/add-dir`、`/login`…），`#` 是記憶。工具白名單管不到這一層，所以不讓 end user 的字出現在第一個字。
pub(crate) const SHARE_PREFIX: &str = "〔分享使用者〕 ";

/// 送給 bot 的訊息裡，附件清單前面的那一行。對話列表靠它把附件從文字裡拆出來（[`split_attachments`]）。
pub(crate) const ATTACH_MARK: &str = "〔分享使用者上傳的檔案，在工作目錄的 inbox/ 底下〕";

/// `?inline=1` 的圖片回應用這份 CSP，取代頁面的 [`CSP`]：網址被直接開成文件時（SVG 會是一份 DOM），
/// `sandbox` 不帶 `allow-scripts` 讓裡面的 script 不跑、origin 變成 opaque，碰不到分享頁的 token。
const INLINE_IMAGE_CSP: &str = "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'";

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; \
                   font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

#[derive(Clone)]
pub(crate) struct Portal {
    app: Arc<App>,
    limits: Arc<Limits>,
    uploads: Arc<Semaphore>,
    /// 排隊等 [`Portal::uploads`] 的名額（[`UPLOAD_QUEUE_MAX`]）。
    upload_queue: Arc<Semaphore>,
    /// 排隊最多等多久（測試可改）。
    upload_wait: Duration,
    streams: Arc<Semaphore>,
    share_streams: Arc<ShareStreamLimits>,
    token_lookups: Arc<Semaphore>,
}

#[derive(Default)]
struct ShareStreamLimits {
    active: Mutex<HashMap<String, usize>>,
}

impl ShareStreamLimits {
    fn try_acquire(self: &Arc<Self>, bot_id: &str) -> Option<ShareStreamPermit> {
        let mut active = self.active.lock().unwrap_or_else(|e| e.into_inner());
        let count = active.entry(bot_id.to_string()).or_default();
        if *count >= MAX_STREAMS_PER_SHARE {
            return None;
        }
        *count += 1;
        Some(ShareStreamPermit { owner: self.clone(), bot_id: bot_id.to_string() })
    }
}

struct ShareStreamPermit {
    owner: Arc<ShareStreamLimits>,
    bot_id: String,
}

impl Drop for ShareStreamPermit {
    fn drop(&mut self) {
        let mut active = self.owner.active.lock().unwrap_or_else(|e| e.into_inner());
        let remove = if let Some(count) = active.get_mut(&self.bot_id) {
            *count -= 1;
            *count == 0
        } else {
            false
        };
        if remove {
            active.remove(&self.bot_id);
        }
    }
}

#[derive(Debug)]
enum TokenLookupError {
    Saturated,
    Database(sqlx::Error),
}

async fn resolve_token(pool: &sqlx::SqlitePool, slots: &Arc<Semaphore>, token: &str) -> Result<Option<String>, TokenLookupError> {
    // Reject malformed shapes without consuming a scarce slot or asking SQLite.
    if !store::token_shape_ok(token) {
        return Ok(None);
    }
    let permit = slots.clone().try_acquire_owned().map_err(|_| TokenLookupError::Saturated)?;
    let resolved = store::resolve(pool, token).await.map_err(TokenLookupError::Database);
    drop(permit);
    resolved
}

/// 固定視窗的計數（每顆 bot、每種動作一個佇列）。只在記憶體裡：daemon 重啟就歸零，這是防灌爆，不是帳本。
#[derive(Default)]
pub(crate) struct Limits {
    hits: Mutex<HashMap<(String, &'static str), VecDeque<Instant>>>,
}

impl Limits {
    /// 還沒到上限就記一筆、回 `None`；到了回還要等幾秒。
    pub(crate) fn take(&self, bot: &str, bucket: &'static str, max: usize, window: Duration) -> Option<u64> {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
        let q = hits.entry((bot.to_string(), bucket)).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if q.len() >= max {
            let wait = q.front().map(|t| window.saturating_sub(now.duration_since(*t))).unwrap_or(window);
            return Some(wait.as_secs().max(1));
        }
        q.push_back(now);
        None
    }
}

fn kicks() -> &'static broadcast::Sender<String> {
    static K: OnceLock<broadcast::Sender<String>> = OnceLock::new();
    K.get_or_init(|| broadcast::channel(64).0)
}

/// 關分享／重產連結之後叫醒這顆 bot 開著的 SSE，讓它們當下重新確認 token（舊 token 的就斷掉）。
pub(crate) fn kick(bot_id: &str) {
    let _ = kicks().send(bot_id.to_string());
}

pub fn router(app: Arc<App>) -> Router {
    let st = Portal {
        app,
        limits: Arc::new(Limits::default()),
        uploads: Arc::new(Semaphore::new(UPLOAD_SLOTS)),
        upload_queue: Arc::new(Semaphore::new(UPLOAD_QUEUE_MAX)),
        upload_wait: UPLOAD_QUEUE_WAIT,
        streams: Arc::new(Semaphore::new(MAX_STREAMS)),
        share_streams: Arc::new(ShareStreamLimits::default()),
        token_lookups: Arc::new(Semaphore::new(MAX_TOKEN_LOOKUPS)),
    };
    router_with_state(st)
}

fn router_with_state(st: Portal) -> Router {
    Router::new()
        .route("/s/{token}", get(page))
        .route("/s/{token}/api/info", get(info))
        .route("/s/{token}/api/messages", get(list_messages).post(send_message).layer(DefaultBodyLimit::max(64 * 1024)))
        .route("/s/{token}/api/events", get(events))
        // multipart 的邊界與標頭另外留 64 KiB；檔案本身照樣 ≤ MAX_UPLOAD（解開之後再量）。
        .route(
            "/s/{token}/api/upload",
            post(upload)
                .layer(DefaultBodyLimit::max(MAX_UPLOAD + 64 * 1024))
                .layer(middleware::from_fn_with_state(st.clone(), upload_admission)),
        )
        .route("/s/{token}/api/files", get(files))
        .route("/s/{token}/api/files/{name}", get(file))
        .route("/assets/{*path}", get(asset))
        .fallback(fallback)
        .layer(middleware::from_fn_with_state(st.clone(), security_headers))
        .with_state(st)
}

/// Funnel 轉進來的 `Host` 是 `[share] base_url` 的主機名。只放行那一個，再加上本機直接打的 loopback 名稱。
/// 不沿用 allow_lan 的後綴名單，所以別的 `.ts.net` 或任意網域過不來。
fn authority_host(authority: &str) -> String {
    let a = authority.trim();
    if let Some(rest) = a.strip_prefix('[') {
        if let Some((host, _)) = rest.split_once(']') {
            return host.to_ascii_lowercase();
        }
    }
    let host = match a.rsplit_once(':') {
        Some((h, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => h,
        _ => a,
    };
    host.trim().to_ascii_lowercase()
}

fn share_host_allowed(authority: &str, configured: Option<&str>) -> bool {
    let host = authority_host(authority);
    matches!(host.as_str(), "127.0.0.1" | "localhost" | "::1") || configured.is_some_and(|want| host == want)
}

fn configured_share_host(base: &str) -> Option<String> {
    let rest = base.strip_prefix("https://").or_else(|| base.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = if let Some(inner) = authority.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let host = host.trim().to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

async fn security_headers(State(st): State<Portal>, uri: Uri, req: axum::extract::Request, next: Next) -> Response {
    let configured = st.app.cfg.get().await.share.base().as_deref().and_then(configured_share_host);
    let headers = req.headers();
    let mut bad_host = false;
    if let Some(host) = headers.get(header::HOST) {
        bad_host = host.to_str().ok().is_none_or(|h| !share_host_allowed(h, configured.as_deref()));
    }
    if !bad_host {
        if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
            match origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://")) {
                Some(rest) if !rest.contains('/') && !rest.contains('@') && share_host_allowed(rest, configured.as_deref()) => {}
                _ => bad_host = true,
            }
        }
    }
    let mut res = if bad_host {
        (StatusCode::FORBIDDEN, Json(json!({"error": "bad origin"}))).into_response()
    } else {
        next.run(req).await
    };
    let asset_ok = uri.path().starts_with("/assets/") && res.status() == StatusCode::OK;
    let h = res.headers_mut();
    // 這個入口沒有任何 CORS：就算哪條路由不小心加了，也在這裡拿掉。
    h.remove(header::ACCESS_CONTROL_ALLOW_ORIGIN);
    h.remove(header::ACCESS_CONTROL_ALLOW_CREDENTIALS);
    let cache = if asset_ok { "public, max-age=31536000, immutable" } else { "no-store" };
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // 唯一的例外是 inline 圖片自己設的 sandbox CSP（比頁面的更嚴），其餘一律覆蓋成頁面的。
    if h.get(header::CONTENT_SECURITY_POLICY).is_none_or(|v| v != INLINE_IMAGE_CSP) {
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    }
    h.insert("Cross-Origin-Opener-Policy", HeaderValue::from_static("same-origin"));
    h.insert("Cross-Origin-Resource-Policy", HeaderValue::from_static("same-origin"));
    res
}

/// 找不到、token 錯、分享關了：全部長一樣。
fn not_found() -> Response {
    (StatusCode::NOT_FOUND, Json(json!({"error": "not_found"}))).into_response()
}

async fn fallback() -> Response {
    not_found()
}

fn unavailable() -> Response {
    (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "5")], Json(json!({"error": "unavailable"}))).into_response()
}

fn too_many(wait: u64, what: &str) -> Response {
    (StatusCode::TOO_MANY_REQUESTS, [(header::RETRY_AFTER, wait.to_string())], Json(json!({"error": "rate_limited", "what": what, "retry_after_s": wait})))
        .into_response()
}

fn bad(reason: &str, message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({"error": "bad_request", "reason": reason, "message": message}))).into_response()
}

/// token → bot id；不對就是 404。
#[allow(clippy::result_large_err)]
async fn bot_for(st: &Portal, token: &str) -> Result<String, Response> {
    match resolve_token(&st.app.db, &st.token_lookups, token).await {
        Ok(Some(id)) => {
            touch_share_if_stale(&st.app, &st.limits, &id).await;
            Ok(id)
        }
        Ok(None) => Err(not_found()),
        Err(TokenLookupError::Saturated) => Err(unavailable()),
        Err(TokenLookupError::Database(e)) => {
            tracing::warn!(error = %e, "share token lookup failed");
            Err(unavailable())
        }
    }
}

async fn touch_share_if_stale(app: &Arc<App>, limits: &Arc<Limits>, bot_id: &str) {
    let window = Duration::from_secs(60);
    if limits.take(bot_id, "touch", 1, window).is_some() {
        return;
    }
    let fresh = sqlx::query_scalar::<_, i64>("SELECT EXISTS(SELECT 1 FROM bot_shares WHERE bot_id = ? AND last_used_at >= ?)")
        .bind(bot_id)
        .bind(db::iso_in(-60))
        .fetch_one(&app.db)
        .await;
    match fresh {
        Ok(0) => store::touch(&app.db, bot_id).await,
        Ok(_) => {}
        Err(e) => tracing::warn!(bot = %bot_id, error = %e, "share last-used telemetry check failed"),
    }
}

/// Serialize capability validation with token rotation, disable, and bot deletion.
async fn authority_lock_app(
    app: &Arc<App>,
    token_lookups: &Arc<Semaphore>,
    limits: &Arc<Limits>,
    token: &str,
    bot_id: &str,
) -> Result<OwnedMutexGuard<()>, Response> {
    let lock = app.bot_lock(bot_id).await;
    let guard = lock.lock_owned().await;
    match resolve_token(&app.db, token_lookups, token).await {
        Ok(Some(id)) if id == bot_id => {
            touch_share_if_stale(app, limits, bot_id).await;
            Ok(guard)
        }
        Ok(_) => Err(not_found()),
        Err(TokenLookupError::Saturated) => Err(unavailable()),
        Err(TokenLookupError::Database(e)) => {
            tracing::warn!(error = %e, "share token recheck failed");
            Err(unavailable())
        }
    }
}

async fn authority_lock(st: &Portal, token: &str, bot_id: &str) -> Result<OwnedMutexGuard<()>, Response> {
    authority_lock_app(&st.app, &st.token_lookups, &st.limits, token, bot_id).await
}

async fn authorized_bot(st: &Portal, token: &str) -> Result<(String, OwnedMutexGuard<()>), Response> {
    let bot_id = bot_for(st, token).await?;
    let guard = authority_lock(st, token, &bot_id).await?;
    Ok((bot_id, guard))
}

/// 上傳入口先驗 token、占名額，占到了才扣嘗試額度，再把 request 交給會讀取 Bytes 的 handler。
/// `_permit` 活到 `next.run` 回應結束，慢速 body 也會占著同一個名額。
async fn upload_admission(State(st): State<Portal>, Path(token): Path<String>, mut req: axum::extract::Request, next: Next) -> Response {
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
        Err(response) => return response,
    };
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_upload_after_bot_for", &bot_id).await;
    // 全域上傳名額滿了就排隊等（分享頁一次選好幾張照片時會同時送來）。等的時候還沒讀 body，不算一次上傳嘗試，
    // 否則別的分享占滿名額時，重試會把這顆 bot 的每分鐘額度扣光。排隊的人太多、或等太久才 429。
    let _permit = match st.uploads.clone().try_acquire_owned() {
        Ok(p) => p,
        Err(_) => {
            let Ok(_queued) = st.upload_queue.clone().try_acquire_owned() else {
                return too_many(5, "upload");
            };
            match tokio::time::timeout(st.upload_wait, st.uploads.clone().acquire_owned()).await {
                Ok(Ok(p)) => p,
                _ => return too_many(5, "upload"),
            }
        }
    };
    if let Some(wait) = st.limits.take(&bot_id, "upload", UPLOADS_PER_MIN, Duration::from_secs(60)) {
        return too_many(wait, "upload");
    };
    req.extensions_mut().insert(bot_id);
    next.run(req).await
}

const PLACEHOLDER_PAGE: &str = "<!doctype html><html lang=\"zh-Hant\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>分享</title></head>\
<body><p>分享頁還沒打包進這個版本（web/dist/share.html）。</p></body></html>";

async fn page(State(st): State<Portal>, Path(token): Path<String>) -> Response {
    let (_bot_id, _authority) = match authorized_bot(&st, &token).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let html = crate::assets::embedded("share.html").unwrap_or_else(|| PLACEHOLDER_PAGE.as_bytes().to_vec());
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// 分享頁自己的靜態檔。只給 `assets/` 底下的 js／css／字型／圖，`..`、隱藏檔一律 404；主 UI 的 `index.html` 不在這條路上。
async fn asset(Path(path): Path<String>) -> Response {
    let ok_ext = [".js", ".css", ".woff2", ".woff", ".svg", ".png", ".webp", ".ico", ".map"];
    if path.split('/').any(|seg| seg.is_empty() || seg.starts_with('.')) || !ok_ext.iter().any(|e| path.ends_with(e)) {
        return not_found();
    }
    match crate::assets::embedded(&format!("assets/{path}")) {
        Some(data) => {
            let mime = mime_guess::from_path(&path).first_or_octet_stream();
            ([(header::CONTENT_TYPE, mime.as_ref().to_string())], data).into_response()
        }
        None => not_found(),
    }
}

/// 狀態燈（跟主 UI 的 `lamp` 同一套字：working／idle／starting／offline／blocked／unknown）。
async fn status_of(app: &Arc<App>, bot_id: &str) -> &'static str {
    match db::active_run(&app.db, bot_id).await {
        Ok(run) => crate::api::lamp(app.bot_connected(bot_id).await, run.as_ref()),
        Err(_) => "unknown",
    }
}

async fn info(State(st): State<Portal>, Path(token): Path<String>) -> Response {
    let (bot_id, _authority) = match authorized_bot(&st, &token).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let name = match db::bot(&st.app.db, &bot_id).await {
        Ok(Some(b)) => b.name,
        Ok(None) => return not_found(),
        Err(_) => return unavailable(),
    };
    Json(json!({"bot_name": name, "status": status_of(&st.app, &bot_id).await})).into_response()
}

/// 我們自己在送出時加在文字後面的附件清單拆回來：`(文字, [檔名])`。
pub(crate) fn split_attachments(content: &str) -> (String, Vec<String>) {
    match content.split_once(&format!("\n\n{ATTACH_MARK}\n")) {
        Some((text, list)) => {
            let names = list.lines().filter_map(|l| l.strip_prefix("- inbox/")).map(|n| display_name(n).to_string()).collect();
            (text.to_string(), names)
        }
        None => (content.to_string(), Vec::new()),
    }
}

/// `inbox/` 裡的檔名是 `<ulid>-<原檔名>`；給人看的是原檔名。
fn display_name(stored: &str) -> &str {
    match stored.split_once('-') {
        Some((id, rest)) if id.len() == 26 && id.chars().all(|c| c.is_ascii_alphanumeric()) && !rest.is_empty() => rest,
        _ => stored,
    }
}

/// 這則 user 訊息給不給分享頁看：只有 end user 自己送的；倒回的、擁有者（或別顆 bot）送的都不給。
pub(crate) fn shown_to_share(role: &str, relay_from: Option<&str>, rewound_at: Option<&str>) -> bool {
    rewound_at.is_none() && role == "user" && relay_from == Some(SHARE_SENDER)
}

/// 分享頁看得到的訊息（`m` 是 messages 的別名），列表與 SSE 共用：
/// - end user 自己送的 user 訊息（`relay_from = 'share'`）；
/// - bot 的回覆，除非觸發那一回合的 user 訊息是擁有者（或別顆 bot）送的、而且沒有帶 `share_reply_visible`（使用者 2026-10-04：
///   後台交代 bot 的「ok」不該出現在 end user 的對話裡）。觸發訊息＝同一個 turn 的第一則 user；沒有 turn 的看它前面最近一則 user。
///   前面沒有任何 user（開場白）照樣給。
/// 倒回的一律不給。`'share'` 是 [`SHARE_SENDER`]（測試釘住兩者一致）。
pub(crate) const VISIBLE_SQL: &str = "m.rewound_at IS NULL AND (
       (m.role = 'user' AND m.relay_from = 'share')
       OR (m.role = 'assistant' AND COALESCE((
             SELECT IFNULL(t.relay_from, '') = 'share' OR EXISTS (SELECT 1 FROM share_reply_visible v WHERE v.message_id = t.id)
               FROM messages t
              WHERE t.id = COALESCE(
                      (SELECT u.id FROM messages u WHERE m.turn_id IS NOT NULL AND u.turn_id = m.turn_id AND u.role = 'user' ORDER BY u.rowid LIMIT 1),
                      (SELECT p.id FROM messages p WHERE p.conversation_id = m.conversation_id AND p.role = 'user' AND p.rowid < m.rowid
                        ORDER BY p.rowid DESC LIMIT 1))
           ), 1))
     )";

/// bot 回覆若照抄了我們加的前綴或附件標記行，顯示前拿掉（只過濾顯示，不改 bot 也不改 DB）。
fn strip_internal_marks(text: &str) -> String {
    let tag = SHARE_PREFIX.trim_end();
    if !text.contains(tag) && !text.contains(ATTACH_MARK) {
        return text.to_string();
    }
    text.replace(ATTACH_MARK, "").replace(SHARE_PREFIX, "").replace(tag, "")
}

/// 一則訊息對外的樣子：只有這幾個欄位。工具細節、turn、終端快照、系統訊息、轉寄來源的 bot id 都不給。
pub(crate) fn public_message(id: &str, role: &str, content: &str, attachments_json: Option<&str>, relay_from: Option<&str>, at: &str) -> Value {
    let (text, mut names) = if role == "user" { split_attachments(content) } else { (content.to_string(), Vec::new()) };
    let text = match (role, relay_from) {
        ("user", Some(SHARE_SENDER)) => text.strip_prefix(SHARE_PREFIX).map(str::to_string).unwrap_or(text),
        ("assistant", _) => strip_internal_marks(&text),
        _ => text,
    };
    if let Some(list) = attachments_json.and_then(|s| serde_json::from_str::<Value>(s).ok()) {
        for a in list.as_array().into_iter().flatten() {
            if let Some(n) = a.get("name").and_then(Value::as_str) {
                names.push(n.to_string());
            }
        }
    }
    let by = match (role, relay_from) {
        ("assistant", _) => "bot",
        (_, Some(SHARE_SENDER)) => "share",
        _ => "owner",
    };
    let attachments: Vec<Value> = names.into_iter().map(|n| json!({"name": n})).collect();
    json!({"id": id, "role": role, "by": by, "text": text, "created_at": at, "attachments": attachments})
}

#[derive(Deserialize)]
struct PageQuery {
    before: Option<String>,
    limit: Option<i64>,
}

async fn list_messages(State(st): State<Portal>, Path(token): Path<String>, Query(q): Query<PageQuery>) -> Response {
    let (bot_id, _authority) = match authorized_bot(&st, &token).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let db = &st.app.db;
    let Ok(conv) = db::conversation_id(db, &bot_id).await else { return unavailable() };
    let limit = q.limit.unwrap_or(100).clamp(1, 200);
    let before = match q.before.as_deref().filter(|s| !s.is_empty()) {
        Some(b) => match sqlx::query_scalar::<_, i64>("SELECT rowid FROM messages WHERE id = ? AND conversation_id = ? AND role IN ('user','assistant')")
            .bind(b)
            .bind(&conv)
            .fetch_optional(db)
            .await
        {
            Ok(Some(r)) => Some(r),
            Ok(None) => return bad("bad_cursor", "before 不是這段對話裡的訊息"),
            Err(_) => return unavailable(),
        },
        None => None,
    };
    // 分享頁只有 end user 與 bot 的對話（使用者 2026-10-04）：規則見 [`VISIBLE_SQL`]，SSE 用同一段。
    /// id、role、content、attachments_json、relay_from、created_at。
    type Row = (String, String, String, Option<String>, Option<String>, String);
    let sql = format!(
            "SELECT m.id, m.role, m.content, m.attachments_json, m.relay_from, m.created_at FROM messages m
              WHERE m.conversation_id = ? AND m.rowid < ? AND {VISIBLE_SQL}
              ORDER BY m.rowid DESC LIMIT ?"
    );
    let rows: Result<Vec<Row>, _> = sqlx::query_as(&sql)
    .bind(&conv)
    .bind(before.unwrap_or(i64::MAX))
    .bind(limit + 1)
    .fetch_all(db)
    .await;
    let Ok(mut rows) = rows else { return unavailable() };
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    rows.reverse();
    let messages: Vec<Value> =
        rows.iter().map(|(id, role, content, att, relay, at)| public_message(id, role, content, att.as_deref(), relay.as_deref(), at)).collect();
    let bot_name = match db::bot(db, &bot_id).await {
        Ok(Some(b)) => b.name,
        _ => return unavailable(),
    };
    Json(json!({"bot_name": bot_name, "status": status_of(&st.app, &bot_id).await, "messages": messages, "has_more": has_more})).into_response()
}

#[derive(Deserialize)]
struct SendIn {
    text: String,
    client_request_id: String,
    #[serde(default)]
    attachments: Vec<String>,
}

/// 上傳後存進 `inbox/` 的名字（`<ulid>-<清過的檔名>`）。只認這個形狀，送訊息時帶來的附件 id 也用它驗。
pub(crate) fn stored_name_ok(name: &str) -> bool {
    name.len() <= 200
        && !name.starts_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && !name.chars().any(char::is_control)
        && display_name(name) != name
}

async fn send_message(State(st): State<Portal>, Path(token): Path<String>, Json(b): Json<SendIn>) -> Response {
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
        Err(r) => return r,
    };
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_send_after_bot_for", &bot_id).await;
    let authority = match authority_lock(&st, &token, &bot_id).await {
        Ok(g) => g,
        Err(r) => return r,
    };
    let crid = b.client_request_id.trim();
    if crid.is_empty() || crid.len() > 64 || !crid.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:".contains(c)) {
        return bad("bad_client_request_id", "client_request_id 要 1～64 個 [A-Za-z0-9-_.:]");
    }
    // 這段字會被貼進 TUI：換行以外的控制字元（含 ESC）一律拿掉，tab 換成空白（輸入框裡 tab 是補完）。
    let text: String = b.text.chars().map(|c| if c == '\t' { ' ' } else { c }).filter(|c| !c.is_control() || *c == '\n').collect();
    let text = text.trim();
    if text.chars().count() > MAX_TEXT_CHARS {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "text_too_long", "max_chars": MAX_TEXT_CHARS}))).into_response();
    }
    if b.attachments.len() > MAX_ATTACHMENTS {
        return bad("too_many_attachments", &format!("一則最多 {MAX_ATTACHMENTS} 個附件"));
    }
    if text.is_empty() && b.attachments.is_empty() {
        return bad("empty", "沒有內容");
    }
    // 附件檢查會排進 blocking pool，先扣額度讓無效附件不能免費放大檔案系統工作量。
    if let Some(wait) = st.limits.take(&bot_id, "message", MESSAGES_PER_MIN, Duration::from_secs(60)) {
        return too_many(wait, "message");
    }
    for a in &b.attachments {
        if !stored_name_ok(a) || !inbox_has(&st.app, &bot_id, a).await {
            return bad("unknown_attachment", "附件不存在（請重新上傳）");
        }
    }
    let mut composed = format!("{SHARE_PREFIX}{text}");
    if !b.attachments.is_empty() {
        composed.push_str(&format!("\n\n{ATTACH_MARK}\n"));
        composed.push_str(&b.attachments.iter().map(|a| format!("- inbox/{a}")).collect::<Vec<_>>().join("\n"));
    }
    // 跟主 UI 的冪等鍵分開一個命名空間。
    let crid = format!("share:{crid}");
    let src = lifecycle::RelaySrc::trusted(Some(SHARE_SENDER));
    drop(authority);
    match lifecycle::prompt_starting_or_queue_with_share_token(&st.app, &bot_id, &composed, &crid, &[], src, true, &token).await {
        Ok(out) => Json(json!({"accepted": true, "message_id": out.message_id, "delivery": out.delivery})).into_response(),
        Err(LcError::NotFound(_)) => not_found(),
        // 細節（pane、run、herdr）不給外面看：只說現在收不下，等一下再試。
        Err(LcError::Conflict(v)) => {
            tracing::info!(bot = %bot_id, detail = %v, "share message not accepted");
            (StatusCode::CONFLICT, Json(json!({"error": "not_accepted", "message": "上一則還在等回覆，等它回完再送"}))).into_response()
        }
        Err(e) => {
            tracing::warn!(bot = %bot_id, error = ?e, "share message failed");
            unavailable()
        }
    }
}

/// 這顆受限 bot 的資料夾（`shared_bots.workspace`）：上傳放它底下的 `inbox/`。讀不到就當沒有（fail closed）。
async fn folder_of(app: &Arc<App>, bot_id: &str) -> Option<std::path::PathBuf> {
    crate::share::store::workspace(&app.db, bot_id).await.ok().flatten().map(std::path::PathBuf::from)
}

async fn inbox_has(app: &Arc<App>, bot_id: &str, name: &str) -> bool {
    let Some(folder) = folder_of(app, bot_id).await else { return false };
    let name = name.to_string();
    tokio::task::spawn_blocking(move || crate::trusted_open::open_bound_file(&folder, &[OsStr::new("inbox"), OsStr::new(&name)], None).is_ok())
        .await
        .unwrap_or(false)
}

async fn events(State(st): State<Portal>, Path(token): Path<String>) -> Response {
    let (bot_id, authority) = match authorized_bot(&st, &token).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(share_slot) = st.share_streams.try_acquire(&bot_id) else {
        return too_many(10, "streams_per_share");
    };
    let Ok(slot) = st.streams.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10")], Json(json!({"error": "too_many_streams"}))).into_response();
    };
    if let Some(wait) = st.limits.take(&bot_id, "stream", 30, Duration::from_secs(60)) {
        return too_many(wait, "stream");
    }
    let s = Stream {
        app: st.app.clone(),
        token,
        bot_id,
        token_lookups: st.token_lookups.clone(),
        limits: st.limits.clone(),
        rx: st.app.subscribe(),
        kicks: kicks().subscribe(),
        _slot: slot,
        _share_slot: share_slot,
        pending: Some(Value::Null),
        emit_guard: Some(authority),
        revoked: false,
    };
    let stream = futures::stream::unfold(s, |mut s| async move { s.next().await.map(|ev| (Ok::<_, std::convert::Infallible>(ev), s)) });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

pub(crate) struct Stream {
    app: Arc<App>,
    token: String,
    bot_id: String,
    token_lookups: Arc<Semaphore>,
    limits: Arc<Limits>,
    rx: broadcast::Receiver<crate::state::WsEvent>,
    kicks: broadcast::Receiver<String>,
    _slot: tokio::sync::OwnedSemaphorePermit,
    _share_slot: ShareStreamPermit,
    pending: Option<Value>,
    emit_guard: Option<OwnedMutexGuard<()>>,
    revoked: bool,
}

impl Stream {
    async fn still_valid(&self) -> bool {
        authority_lock_app(&self.app, &self.token_lookups, &self.limits, &self.token, &self.bot_id).await.is_ok()
    }

    /// 下一個要送的事件；`None`＝收掉這條連線（token 失效、bus 關了）。
    async fn next(&mut self) -> Option<Event> {
        if self.pending.take().is_some() {
            let guard = match self.emit_guard.take() {
                Some(g) => g,
                None => match authority_lock_app(&self.app, &self.token_lookups, &self.limits, &self.token, &self.bot_id).await {
                    Ok(g) => g,
                    Err(_) => return None,
                },
            };
            let first = json!({"status": status_of(&self.app, &self.bot_id).await});
            self.emit_guard = Some(guard);
            return Some(Event::default().event("status").data(first.to_string()));
        }
        // Keep the previous event fenced until the consumer polls for another item.
        self.emit_guard.take();
        loop {
            let recheck = tokio::time::sleep(STREAM_RECHECK);
            tokio::select! {
                ev = self.rx.recv() => match ev {
                    Ok(ev) => {
                        if let Some(out) = self.map(&ev).await {
                            return Some(out);
                        }
                        if self.revoked {
                            return None;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let guard = match authority_lock_app(&self.app, &self.token_lookups, &self.limits, &self.token, &self.bot_id).await {
                            Ok(g) => g,
                            Err(_) => return None,
                        };
                        self.emit_guard = Some(guard);
                        return Some(Event::default().event("resync").data("{}"));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                },
                k = self.kicks.recv() => {
                    let mine = match k { Ok(id) => id == self.bot_id, Err(_) => true };
                    if mine && !self.still_valid().await {
                        return None;
                    }
                }
                _ = recheck => {
                    if !self.still_valid().await {
                        return None;
                    }
                }
            }
        }
    }

    async fn map(&mut self, ev: &crate::state::WsEvent) -> Option<Event> {
        if ev.data.get("bot_id").and_then(Value::as_str) != Some(self.bot_id.as_str()) {
            return None;
        }
        let guard = match authority_lock_app(&self.app, &self.token_lookups, &self.limits, &self.token, &self.bot_id).await {
            Ok(g) => g,
            Err(_) => {
                self.revoked = true;
                return None;
            }
        };
        let out = match ev.kind.as_str() {
            "message_added" => {
                let m = ev.data.get("message")?;
                let role = m.get("role").and_then(Value::as_str)?;
                let s = |k: &str| m.get(k).and_then(Value::as_str);
                let shown = match role {
                    "user" => shown_to_share(role, s("relay_from"), s("rewound_at")),
                    // bot 的回覆要看觸發它的那一則（[`VISIBLE_SQL`]）。讀不到就不推（fail closed），重抓時照列表的規則補回來。
                    "assistant" => match s("id") {
                        Some(id) => sqlx::query_scalar::<_, bool>(&format!("SELECT EXISTS(SELECT 1 FROM messages m WHERE m.id = ? AND {VISIBLE_SQL})"))
                            .bind(id)
                            .fetch_one(&self.app.db)
                            .await
                            .unwrap_or(false),
                        None => false,
                    },
                    _ => false,
                };
                if !shown {
                    return None;
                }
                let out = public_message(s("id")?, role, s("content").unwrap_or(""), s("attachments_json"), s("relay_from"), s("created_at").unwrap_or(""));
                Some(Event::default().event("message").data(out.to_string()))
            }
            // 擁有者在 AG Man 倒回對話：分享頁手上那幾則要消失，請它整頁重抓（重抓的清單已排除倒回的）。
            "messages_rewound" => Some(Event::default().event("resync").data("{}")),
            "bot_status" => Some(Event::default().event("status").data(json!({"status": status_of(&self.app, &self.bot_id).await}).to_string())),
            _ => None,
        };
        if out.is_some() {
            self.emit_guard = Some(guard);
        }
        out
    }
}

#[cfg(test)]
pub(crate) fn stream_for_test(app: &Arc<App>, token: &str, bot_id: &str, pending: Option<Value>) -> Stream {
    Stream {
        app: app.clone(),
        token: token.to_string(),
        bot_id: bot_id.to_string(),
        token_lookups: Arc::new(Semaphore::new(MAX_TOKEN_LOOKUPS)),
        limits: Arc::new(Limits::default()),
        rx: app.subscribe(),
        kicks: kicks().subscribe(),
        _slot: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        _share_slot: Arc::new(ShareStreamLimits::default()).try_acquire(bot_id).unwrap(),
        pending,
        emit_guard: None,
        revoked: false,
    }
}

#[cfg(test)]
pub(crate) async fn next_for_test(stream: &mut Stream) -> Option<Event> {
    stream.next().await
}

#[cfg(test)]
pub(crate) async fn map_for_test(stream: &mut Stream, ev: &crate::state::WsEvent) -> Option<Event> {
    stream.map(ev).await
}

/// 收得下的檔案種類：副檔名決定，內容要對得上（圖片／PDF／Office 看檔頭，文字檔要是 UTF-8、不能有 NUL）。
/// 呼叫端給的 Content-Type 不採信。
pub(crate) fn classify_upload(name: &str, data: &[u8]) -> Result<&'static str, &'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    let starts = |m: &[u8]| data.starts_with(m);
    let kind = match ext.as_str() {
        "png" => starts(b"\x89PNG\r\n\x1a\n").then_some("image/png"),
        "jpg" | "jpeg" => starts(b"\xff\xd8\xff").then_some("image/jpeg"),
        "gif" => (starts(b"GIF87a") || starts(b"GIF89a")).then_some("image/gif"),
        "webp" => (starts(b"RIFF") && data.get(8..12) == Some(b"WEBP")).then_some("image/webp"),
        "pdf" => starts(b"%PDF-").then_some("application/pdf"),
        "docx" => ooxml::valid_for(data, b"word/document.xml").then_some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "xlsx" => ooxml::valid_for(data, b"xl/workbook.xml").then_some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "pptx" => ooxml::valid_for(data, b"ppt/presentation.xml").then_some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        "txt" | "md" | "csv" | "tsv" | "json" | "xml" | "yaml" | "yml" | "log" | "html" | "htm" | "py" | "js" | "ts" | "rs" | "go"
        | "java" | "c" | "h" | "cpp" | "sql" => (std::str::from_utf8(data).is_ok() && !data.contains(&0)).then_some("text/plain"),
        _ => return Err("unsupported_type"),
    };
    kind.ok_or("content_mismatch")
}

/// 使用者給的檔名清成安全的：只取最後一段，去掉控制字元與路徑符號，不能是隱藏檔，長度有上限。
pub(crate) fn clean_upload_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if c.is_alphanumeric() || "._- ()".contains(c) { c } else { '_' })
        .collect();
    let cleaned = cleaned.trim().to_string();
    if cleaned.is_empty() || cleaned.starts_with('.') || !cleaned.chars().any(char::is_alphanumeric) || cleaned.chars().count() > 120 {
        return None;
    }
    if crate::outbox::withheld_name(&cleaned.to_ascii_lowercase()) {
        return None;
    }
    Some(cleaned)
}

async fn upload(
    State(st): State<Portal>,
    Path(token): Path<String>,
    Extension(bot_id): Extension<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    // 分享頁送 `FormData` 的 `file` 欄位；腳本也可以直接送原始位元組＋`?name=`。
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("");
    let (raw_name, data): (Option<String>, Bytes) = if content_type.to_ascii_lowercase().starts_with("multipart/") {
        match multipart::file_part(content_type, &body) {
            Ok((name, part)) => (Some(name), body.slice_ref(part)),
            Err(reason) => return bad(reason, "上傳的格式不對：要 multipart/form-data 的 file 欄位"),
        }
    } else {
        (q.get("name").cloned(), body)
    };
    if data.is_empty() {
        return bad("empty", "檔案是空的");
    }
    if data.len() > MAX_UPLOAD {
        return (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "too_large", "max": MAX_UPLOAD}))).into_response();
    }
    let Some(name) = raw_name.as_deref().and_then(clean_upload_name) else {
        return bad("bad_name", "檔名不能用（隱藏檔、金鑰／資料庫類的檔名、或太長）");
    };
    let mime = match classify_upload(&name, &data) {
        Ok(m) => m,
        Err(reason) => {
            return (StatusCode::UNSUPPORTED_MEDIA_TYPE, Json(json!({"error": "unsupported", "reason": reason, "message": "只收文字、圖片、PDF、Office 檔，而且內容要跟副檔名對得上"})))
                .into_response()
        }
    };
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_upload_before_authority_lock", &format!("{bot_id}:{name}")).await;
    let _authority = match authority_lock(&st, &token, &bot_id).await {
        Ok(g) => g,
        Err(r) => return r,
    };
    #[cfg(test)]
    crate::lifecycle::race_point::hit("share_upload_after_authority_lock", &bot_id).await;
    let stored = format!("{}-{name}", db::ulid());
    let Some(folder) = folder_of(&st.app, &bot_id).await else { return unavailable() };
    let (file_name, len) = (stored.clone(), data.len());
    let saved = tokio::task::spawn_blocking(move || -> Result<(), &'static str> {
        use std::io::Write as _;
        let dir = crate::share::folder::ensure_inbox(&folder).map_err(|_| "inbox_unavailable")?;
        let entries = crate::trusted_open::read_dir_bound(&dir).map_err(|_| "inbox_unavailable")?;
        let used: u64 = entries.iter().filter(|e| e.is_file).map(|e| e.size).sum();
        if entries.len() >= INBOX_MAX_FILES || used + len as u64 > INBOX_MAX_BYTES {
            return Err("inbox_full");
        }
        let mut f = crate::trusted_open::create_new_file_in(&dir, OsStr::new(&file_name), 0o600).map_err(|_| "inbox_unavailable")?;
        f.write_all(&data).and_then(|_| f.sync_all()).map_err(|_| {
            let _ = crate::trusted_open::unlink_in(&dir, OsStr::new(&file_name));
            "inbox_unavailable"
        })
    })
    .await
    .unwrap_or(Err("inbox_unavailable"));
    match saved {
        Ok(()) => Json(json!({"id": stored, "name": name, "size": len, "mime": mime})).into_response(),
        Err("inbox_full") => (StatusCode::INSUFFICIENT_STORAGE, Json(json!({"error": "inbox_full", "message": "上傳空間滿了"}))).into_response(),
        Err(_) => unavailable(),
    }
}

async fn files(State(st): State<Portal>, Path(token): Path<String>) -> Response {
    let (bot_id, _authority) = match authorized_bot(&st, &token).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let Some(dir) = crate::outbox::dir_for(&st.app.data_dir, &bot_id) else { return not_found() };
    let data_dir = st.app.data_dir.clone();
    #[cfg(test)]
    let fail_scan = take_fail_files_scan_task_for_test(&bot_id);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let listed = tokio::task::spawn_blocking(move || {
        #[cfg(test)]
        if fail_scan {
            panic!("injected share outbox scan task failure");
        }
        match crate::outbox::open_trusted_dir(&data_dir, &dir) {
            Ok(Some(fd)) => crate::outbox::scan_checked(&fd, now),
            Ok(None) => Ok(Vec::new()),
            Err(()) => Err(()),
        }
    })
    .await;
    let listed = match listed {
        Ok(Ok(files)) => files,
        Ok(Err(())) | Err(_) => {
            tracing::warn!(bot = %bot_id, "share outbox listing unavailable");
            return unavailable();
        }
    };
    // 只給名字、大小、時間；目錄路徑不給。分享用 bot 的 outbox 不清（`outbox-gc.sh` 看 `.am-share-keep`），所以不給倒數。
    let out: Vec<Value> = listed
        .iter()
        .map(|f| {
            let modified_at = f["modified"].as_i64().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(db::iso_at);
            json!({"name": f["name"], "size": f["size"], "modified_at": modified_at})
        })
        .collect();
    // 清單上的 .svg 有新版就在背景查一次是不是合法 XML，壞了自動提醒 bot（`svg_check`，SPEC §20）。
    crate::share::svg_check::spawn_check(&st.app, &bot_id, &out);
    Json(json!({"files": out})).into_response()
}

#[cfg(test)]
static FAIL_NEXT_FILES_SCAN: OnceLock<Mutex<std::collections::HashSet<String>>> = OnceLock::new();

#[cfg(test)]
pub(super) fn fail_next_files_scan_task_for_test(bot_id: &str) {
    FAIL_NEXT_FILES_SCAN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string());
}

#[cfg(test)]
fn take_fail_files_scan_task_for_test(bot_id: &str) -> bool {
    FAIL_NEXT_FILES_SCAN.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id)
}

#[derive(Deserialize)]
struct FileQuery {
    inline: Option<String>,
}

/// `?inline=1` 能 inline 的圖片：副檔名 → 正確的 MIME。白名單以外（HTML、PDF、文字…）照樣是附件，
/// 分享頁用 `<img>` 預覽 bot 做的圖卡要靠這個（SVG 平常的下載是 `application/octet-stream`，`<img>` 畫不出來）。
pub(crate) fn inline_image(name: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(name).extension().and_then(OsStr::to_str)?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

async fn file(State(st): State<Portal>, Path((token, name)): Path<(String, String)>, Query(q): Query<FileQuery>) -> Response {
    let (bot_id, _authority) = match authorized_bot(&st, &token).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    // 只認 outbox 第一層的檔名；子目錄、`..`、隱藏檔一律不給。
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') || name.chars().any(char::is_control) {
        return not_found();
    }
    match crate::outbox::share_file(&st.app, &bot_id, &name).await {
        Ok(res) => {
            let mut res = if inline_image(&name) == Some("image/svg+xml") { embed_photos(&st.app, &bot_id, res).await } else { res };
            if let (Some("1"), Some(mime)) = (q.inline.as_deref(), inline_image(&name)) {
                let h = res.headers_mut();
                h.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                let disposition = crate::outbox::content_disposition(&name).replacen("attachment", "inline", 1);
                if let Ok(v) = HeaderValue::from_str(&disposition) {
                    h.insert(header::CONTENT_DISPOSITION, v);
                }
                h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(INLINE_IMAGE_CSP));
            }
            res
        }
        Err(crate::outbox::ShareFileError::TooLarge) => {
            (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "too_large"}))).into_response()
        }
        Err(crate::outbox::ShareFileError::NotFound) => not_found(),
        Err(crate::outbox::ShareFileError::Unavailable) => {
            tracing::warn!(bot = %bot_id, "share outbox download unavailable");
            unavailable()
        }
    }
}

/// bot 做的 SVG 用相對路徑引用資料夾裡的照片（`<image href="inbox/…">`）：送出前嵌成 data URI（[`crate::share::compose`]）。
/// 原檔不改；資料夾讀不到或沒有要嵌的就原樣送。
async fn embed_photos(app: &Arc<App>, bot_id: &str, res: Response) -> Response {
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = axum::body::to_bytes(body, crate::outbox::MAX_BYTES as usize).await else { return unavailable() };
    if !crate::share::compose::wants_embed(&bytes) {
        return Response::from_parts(parts, axum::body::Body::from(bytes));
    }
    let Some(folder) = folder_of(app, bot_id).await else { return Response::from_parts(parts, axum::body::Body::from(bytes)) };
    let src = bytes.clone();
    let out = tokio::task::spawn_blocking(move || crate::share::compose::embed(&src, &folder)).await.ok().flatten();
    match out {
        Some(svg) => {
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, axum::body::Body::from(svg))
        }
        None => Response::from_parts(parts, axum::body::Body::from(bytes)),
    }
}

/// `[share] listen` 有設就開分享入口。只准 loopback（Tailscale Funnel 從本機轉進來），不能跟管理 API 同一個 port；
/// 開不起來只記 error，不擋 daemon 開機（管理介面照常）。設定改了要重啟 daemon 才生效。
pub(crate) async fn spawn_listener(app: &Arc<App>, main_port: u16) {
    let Some(listen) = app.cfg.get().await.share.listen.clone().filter(|s| !s.trim().is_empty()) else { return };
    let addr = match check_listen(&listen, main_port) {
        Ok(a) => a,
        Err(why) => {
            tracing::error!(listen = %listen, "share portal not started: {why}");
            return;
        }
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            tracing::error!(%addr, error = %e, "share portal not started: bind failed");
            return;
        }
    };
    tracing::info!(%addr, "share portal listening");
    let router = router(app.clone());
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router).await {
            tracing::error!(error = %e, "share portal stopped");
        }
    });
}

pub(crate) fn check_listen(listen: &str, main_port: u16) -> Result<std::net::SocketAddr, String> {
    let addr: std::net::SocketAddr = listen.trim().parse().map_err(|e| format!("`{listen}` is not ip:port ({e})"))?;
    if !addr.ip().is_loopback() {
        return Err("must be a loopback address (127.0.0.1 / ::1); expose it with tailscale funnel".into());
    }
    if addr.port() == main_port || addr.port() == 0 {
        return Err("must be its own port, not the management API's".into());
    }
    Ok(addr)
}

#[path = "portal_ooxml.rs"]
mod ooxml;

#[cfg(test)]
#[path = "portal_upload_tests.rs"]
mod upload_tests;
