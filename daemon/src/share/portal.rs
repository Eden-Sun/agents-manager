//! 分享入口：獨立的 listener（`[share] listen`，預設建議 `127.0.0.1:7790`，Tailscale Funnel 只指這個 port）。
//!
//! router 上**只有**下面這幾條，沒有 fallback 到主 API、沒有主 UI、沒有 `/ws`、沒有 `/hook`（[`router`]）：
//!
//! | 路由 | 做什麼 |
//! |---|---|
//! | `GET /s/{token}` | 分享頁（嵌入的 `web/dist/share.html`；還沒打包時是一頁佔位） |
//! | `GET /s/{token}/api/info` | `{bot_name, status}` |
//! | `GET /s/{token}/api/messages?before=&limit=` | `{bot_name, status, messages, has_more}`：完整對話（只有 user／assistant；只給 id、role、誰送的、text、`created_at`、附件名） |
//! | `POST /s/{token}/api/messages` | `{text, client_request_id, attachments?}` → 照一般送訊息流程（停著就起、忙就排隊），來源記成分享使用者 |
//! | `GET /s/{token}/api/events` | SSE：`message`（新訊息）、`status`（思考中／閒置…）、`resync`（漏了，請重抓） |
//! | `POST /s/{token}/api/upload` | `multipart/form-data` 的 `file` 欄位（或原始位元組＋`?name=`）；存進工作目錄的 `inbox/`，回 `{id, name, size, mime}` |
//! | `GET /s/{token}/api/files`、`GET /s/{token}/api/files/{name}` | 這顆 bot 的 outbox（沿用 outbox 的擋法與下載標頭） |
//! | `GET /assets/{*path}` | 分享頁的 js／css（嵌入的 `web/dist/assets/`） |
//!
//! token 錯、分享關了、bot 刪了：一律同一個 404（不洩漏存在與否）。每個回應都帶 `Cache-Control: no-store`（assets 除外，
//! 檔名有雜湊）、`Referrer-Policy: no-referrer`、`nosniff`、只允許自己的 CSP；不設任何 CORS 標頭。

use std::collections::{HashMap, VecDeque};
use std::ffi::OsStr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{broadcast, Semaphore};

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
/// 每分鐘最多上傳幾個檔。
pub(crate) const UPLOADS_PER_MIN: usize = 20;
/// `inbox/` 的總量上限（位元組、檔數）：滿了要等 bot 的主人清掉。
pub(crate) const INBOX_MAX_BYTES: u64 = 200 * 1024 * 1024;
pub(crate) const INBOX_MAX_FILES: usize = 300;
/// 一則訊息最多帶幾個附件。
const MAX_ATTACHMENTS: usize = 10;
/// 同時開著的 SSE 連線（全部分享加起來）。
const MAX_STREAMS: usize = 32;
/// SSE 每隔多久重新確認一次 token 還有效（關分享／重產時另外會被 [`kick`] 叫醒）。
const STREAM_RECHECK: Duration = Duration::from_secs(30);

/// 每則分享使用者的訊息一律以這個開頭再打進 TUI。claude 的輸入框把**第一個字**當模式切換：`!` 是 bash 模式
/// （2026-10-03 實測：`--restricted --tools … --permission-mode dontAsk`、settings 也 deny Bash，`!echo … > 檔` 照樣真的跑了），
/// `/` 是 slash 指令（`/permissions`、`/add-dir`、`/login`…），`#` 是記憶。工具白名單管不到這一層，所以不讓 end user 的字出現在第一個字。
pub(crate) const SHARE_PREFIX: &str = "〔分享使用者〕 ";

/// 送給 bot 的訊息裡，附件清單前面的那一行。對話列表靠它把附件從文字裡拆出來（[`split_attachments`]）。
pub(crate) const ATTACH_MARK: &str = "〔分享使用者上傳的檔案，在工作目錄的 inbox/ 底下〕";

const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; connect-src 'self'; \
                   font-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

#[derive(Clone)]
pub(crate) struct Portal {
    app: Arc<App>,
    limits: Arc<Limits>,
    uploads: Arc<Semaphore>,
    streams: Arc<Semaphore>,
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
        uploads: Arc::new(Semaphore::new(2)),
        streams: Arc::new(Semaphore::new(MAX_STREAMS)),
    };
    Router::new()
        .route("/s/{token}", get(page))
        .route("/s/{token}/api/info", get(info))
        .route("/s/{token}/api/messages", get(list_messages).post(send_message).layer(DefaultBodyLimit::max(64 * 1024)))
        .route("/s/{token}/api/events", get(events))
        // multipart 的邊界與標頭另外留 64 KiB；檔案本身照樣 ≤ MAX_UPLOAD（解開之後再量）。
        .route("/s/{token}/api/upload", post(upload).layer(DefaultBodyLimit::max(MAX_UPLOAD + 64 * 1024)))
        .route("/s/{token}/api/files", get(files))
        .route("/s/{token}/api/files/{name}", get(file))
        .route("/assets/{*path}", get(asset))
        .fallback(fallback)
        .layer(middleware::from_fn(security_headers))
        .with_state(st)
}

async fn security_headers(uri: Uri, req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
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
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
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
    match store::resolve(&st.app.db, token).await {
        Ok(Some(id)) => {
            store::touch(&st.app.db, &id).await;
            Ok(id)
        }
        Ok(None) => Err(not_found()),
        Err(e) => {
            tracing::warn!(error = %e, "share token lookup failed");
            Err(unavailable())
        }
    }
}

const PLACEHOLDER_PAGE: &str = "<!doctype html><html lang=\"zh-Hant\"><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>分享</title></head>\
<body><p>分享頁還沒打包進這個版本（web/dist/share.html）。</p></body></html>";

async fn page(State(st): State<Portal>, Path(token): Path<String>) -> Response {
    if let Err(r) = bot_for(&st, &token).await {
        return r;
    }
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
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
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

/// 一則訊息對外的樣子：只有這幾個欄位。工具細節、turn、終端快照、系統訊息、轉寄來源的 bot id 都不給。
pub(crate) fn public_message(id: &str, role: &str, content: &str, attachments_json: Option<&str>, relay_from: Option<&str>, at: &str) -> Value {
    let (text, mut names) = if role == "user" { split_attachments(content) } else { (content.to_string(), Vec::new()) };
    let text = match (role, relay_from) {
        ("user", Some(SHARE_SENDER)) => text.strip_prefix(SHARE_PREFIX).map(str::to_string).unwrap_or(text),
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
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
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
    /// id、role、content、attachments_json、relay_from、created_at。
    type Row = (String, String, String, Option<String>, Option<String>, String);
    let rows: Result<Vec<Row>, _> = sqlx::query_as(
        "SELECT id, role, content, attachments_json, relay_from, created_at FROM messages
          WHERE conversation_id = ? AND role IN ('user','assistant') AND rowid < ?
          ORDER BY rowid DESC LIMIT ?",
    )
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
    for a in &b.attachments {
        if !stored_name_ok(a) || !inbox_has(&st.app, &bot_id, a).await {
            return bad("unknown_attachment", "附件不存在（請重新上傳）");
        }
    }
    if let Some(wait) = st.limits.take(&bot_id, "message", MESSAGES_PER_MIN, Duration::from_secs(60)) {
        return too_many(wait, "message");
    }
    let mut composed = format!("{SHARE_PREFIX}{text}");
    if !b.attachments.is_empty() {
        composed.push_str(&format!("\n\n{ATTACH_MARK}\n"));
        composed.push_str(&b.attachments.iter().map(|a| format!("- inbox/{a}")).collect::<Vec<_>>().join("\n"));
    }
    // 跟主 UI 的冪等鍵分開一個命名空間。
    let crid = format!("share:{crid}");
    let src = lifecycle::RelaySrc::trusted(Some(SHARE_SENDER));
    match lifecycle::prompt_starting_or_queue(&st.app, &bot_id, &composed, &crid, &[], src, true).await {
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

fn inbox_components(bot_id: &str) -> [&OsStr; 4] {
    [OsStr::new("shared-bots"), OsStr::new(bot_id), OsStr::new("workspace"), OsStr::new("inbox")]
}

async fn inbox_has(app: &Arc<App>, bot_id: &str, name: &str) -> bool {
    let (data, bot, name) = (app.data_dir.clone(), bot_id.to_string(), name.to_string());
    tokio::task::spawn_blocking(move || {
        let mut parts: Vec<&OsStr> = inbox_components(&bot).to_vec();
        parts.push(OsStr::new(&name));
        crate::trusted_open::open_bound_file(&data, &parts, None).is_ok()
    })
    .await
    .unwrap_or(false)
}

async fn events(State(st): State<Portal>, Path(token): Path<String>) -> Response {
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Ok(slot) = st.streams.clone().try_acquire_owned() else {
        return (StatusCode::SERVICE_UNAVAILABLE, [(header::RETRY_AFTER, "10")], Json(json!({"error": "too_many_streams"}))).into_response();
    };
    if let Some(wait) = st.limits.take(&bot_id, "stream", 30, Duration::from_secs(60)) {
        return too_many(wait, "stream");
    }
    let first = json!({"status": status_of(&st.app, &bot_id).await});
    let s = Stream { app: st.app.clone(), token, bot_id, rx: st.app.subscribe(), kicks: kicks().subscribe(), _slot: slot, pending: Some(first) };
    let stream = futures::stream::unfold(s, |mut s| async move { s.next().await.map(|ev| (Ok::<_, std::convert::Infallible>(ev), s)) });
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

struct Stream {
    app: Arc<App>,
    token: String,
    bot_id: String,
    rx: broadcast::Receiver<crate::state::WsEvent>,
    kicks: broadcast::Receiver<String>,
    _slot: tokio::sync::OwnedSemaphorePermit,
    pending: Option<Value>,
}

impl Stream {
    async fn still_valid(&self) -> bool {
        matches!(store::resolve(&self.app.db, &self.token).await, Ok(Some(id)) if id == self.bot_id)
    }

    /// 下一個要送的事件；`None`＝收掉這條連線（token 失效、bus 關了）。
    async fn next(&mut self) -> Option<Event> {
        if let Some(first) = self.pending.take() {
            return Some(Event::default().event("status").data(first.to_string()));
        }
        loop {
            let recheck = tokio::time::sleep(STREAM_RECHECK);
            tokio::select! {
                ev = self.rx.recv() => match ev {
                    Ok(ev) => {
                        if let Some(out) = self.map(&ev).await {
                            return Some(out);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => return Some(Event::default().event("resync").data("{}")),
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

    async fn map(&self, ev: &crate::state::WsEvent) -> Option<Event> {
        if ev.data.get("bot_id").and_then(Value::as_str) != Some(self.bot_id.as_str()) {
            return None;
        }
        match ev.kind.as_str() {
            "message_added" => {
                let m = ev.data.get("message")?;
                let role = m.get("role").and_then(Value::as_str)?;
                if role != "user" && role != "assistant" {
                    return None;
                }
                let s = |k: &str| m.get(k).and_then(Value::as_str);
                let out = public_message(s("id")?, role, s("content").unwrap_or(""), s("attachments_json"), s("relay_from"), s("created_at").unwrap_or(""));
                Some(Event::default().event("message").data(out.to_string()))
            }
            "bot_status" => Some(Event::default().event("status").data(json!({"status": status_of(&self.app, &self.bot_id).await}).to_string())),
            _ => None,
        }
    }
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
        "docx" | "xlsx" | "pptx" => starts(b"PK\x03\x04").then_some("application/zip"),
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
    Query(q): Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Ok(_permit) = st.uploads.clone().try_acquire_owned() else {
        return too_many(2, "upload");
    };
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
    if let Some(wait) = st.limits.take(&bot_id, "upload", UPLOADS_PER_MIN, Duration::from_secs(60)) {
        return too_many(wait, "upload");
    }
    let stored = format!("{}-{name}", db::ulid());
    let (data_dir, bot, file_name, len) = (st.app.data_dir.clone(), bot_id.clone(), stored.clone(), data.len());
    let saved = tokio::task::spawn_blocking(move || -> Result<(), &'static str> {
        use std::io::Write as _;
        let dir = crate::trusted_open::create_private_bound_dirs(&data_dir, &inbox_components(&bot)).map_err(|_| "inbox_unavailable")?;
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
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
        Err(r) => return r,
    };
    let Some(dir) = crate::outbox::dir_for(&st.app.data_dir, &bot_id) else { return not_found() };
    let data_dir = st.app.data_dir.clone();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let listed = tokio::task::spawn_blocking(move || match crate::outbox::open_trusted_dir(&data_dir, &dir) {
        Ok(Some(fd)) => Some(crate::outbox::scan(&fd, now)),
        Ok(None) => Some(Vec::new()),
        Err(()) => None,
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    // 只給名字、大小、還剩多久；目錄路徑不給。
    let out: Vec<Value> = listed
        .iter()
        .map(|f| {
            let modified_at = f["modified"].as_i64().and_then(|t| chrono::DateTime::from_timestamp(t, 0)).map(db::iso_at);
            json!({"name": f["name"], "size": f["size"], "modified_at": modified_at, "remaining_secs": f["remaining_secs"]})
        })
        .collect();
    Json(json!({"files": out})).into_response()
}

async fn file(State(st): State<Portal>, Path((token, name)): Path<(String, String)>) -> Response {
    let bot_id = match bot_for(&st, &token).await {
        Ok(id) => id,
        Err(r) => return r,
    };
    // 只認 outbox 第一層的檔名；子目錄、`..`、隱藏檔一律不給。
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.starts_with('.') || name.chars().any(char::is_control) {
        return not_found();
    }
    let q = HashMap::from([("path".to_string(), name)]);
    match crate::outbox::file(State(st.app.clone()), Path(bot_id), Query(q)).await {
        Ok(res) => res,
        Err(LcError::Conflict(v)) if v.get("reason").and_then(Value::as_str) == Some("file_too_large") => {
            (StatusCode::PAYLOAD_TOO_LARGE, Json(json!({"error": "too_large"}))).into_response()
        }
        Err(_) => not_found(),
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
