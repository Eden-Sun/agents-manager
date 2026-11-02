//! 主機 shell 的推送串流：`GET /api/hosts/:name/shells/:pane_id/stream?source=visible&format=ansi`。
//!
//! 事件來源：herdr 0.9.5 的 `events.subscribe` 沒有「pane 畫面變了」這種事件（變動事件只有 `pane.updated`
//! 之類的狀態欄位，且 `pane.get` 的 `revision` 不跟著畫面動），所以這裡**輪詢** `pane.read`（ANSI），
//! 用內容 hash 判斷有沒有變。輪詢間隔自適應：有變化立刻回到 [`FAST`]，連續沒變化每拍加倍、封頂 [`SLOW`]；
//! 送字／送鍵（[`wake`]）與有新觀看者、觀看者斷線都會把睡眠喚醒。
//!
//! 同一顆 pane 的所有觀看者共用一個讀取迴圈（[`Entry`]）；最後一個觀看者離開，迴圈就自己停。
//! 登記表是行程內的全域（key 含 `App` 的位址，測試之間不互相干擾）。

use crate::api::{origin_is_local, ct_eq, WsSlot};
use crate::lifecycle::LcError;
use crate::api::shell;
use crate::state::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::sync::{broadcast, Notify};

/// 有變化時的輪詢間隔，也是送字／送鍵後第一次讀的間隔。
pub const FAST: Duration = Duration::from_millis(50);
/// 連續沒變化時的上限。
pub const SLOW: Duration = Duration::from_secs(1);
/// 每條串流讀的行數（`visible` 下是畫面高度，給個夠大的上限）。
const STREAM_LINES: u32 = 200;

/// 自適應輪詢的節奏。純狀態、不碰時鐘，好測。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    idle_steps: u32,
}

impl Cadence {
    pub fn new() -> Self {
        Cadence { idle_steps: 0 }
    }

    /// 下一拍要睡多久。`idle_steps` 每加一，間隔加倍，超過 [`SLOW`] 就封頂。
    pub fn delay(&self) -> Duration {
        FAST.checked_mul(1u32 << self.idle_steps.min(16)).unwrap_or(SLOW).min(SLOW)
    }

    /// 這一拍讀完的結果：有變化回到最快；沒變化就往慢的方向退一格。
    pub fn observe(&mut self, changed: bool) {
        self.idle_steps = if changed { 0 } else { (self.idle_steps + 1).min(8) };
    }

    /// 被喚醒（送字／送鍵、新觀看者、有人離開）：立刻回到最快。
    pub fn reset(&mut self) {
        self.idle_steps = 0;
    }
}

/// 一張完整畫面的指紋（文字＋幾何）。只用來判斷「變了沒」，不跨行程持久化。
pub fn fingerprint(text: &str, cols: Option<u32>, rows: Option<u32>) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    cols.hash(&mut h);
    rows.hash(&mut h);
    h.finish()
}

/// 推給 client 的一幀。`seq` 是這顆 pane 這一輪串流內的序號（串流結束後歸零）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq: u64,
    pub text: String,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
}

impl Frame {
    /// wire 形狀：`{"type":"frame","seq":N,"text":"<ANSI>","cols":C,"rows":R}`。
    pub fn message(&self) -> Value {
        json!({"type": "frame", "seq": self.seq, "text": self.text, "cols": self.cols, "rows": self.rows})
    }
}

/// 錯誤幀：`{"type":"error","code":"..."}`，之後關閉連線。
pub fn error_message(code: &str) -> Value {
    json!({"type": "error", "code": code})
}

/// 錯誤碼的對應：不把內部訊息送到瀏覽器。
pub fn error_code(e: &LcError) -> &'static str {
    match e {
        LcError::NotFound(_) | LcError::NotFoundValue(_) => "not_found",
        LcError::Upstream(_) => "upstream",
        LcError::Bad(_) | LcError::BadValue(_) => "bad_request",
        _ => "error",
    }
}

/// 登記表的 key：`(App 的位址, host, pane_id)`。
type Key = (usize, String, String);

#[derive(Clone, Debug)]
enum Event {
    Frame(Arc<Frame>),
    Error(&'static str),
}

struct Entry {
    tx: broadcast::Sender<Event>,
    latest: Option<Arc<Frame>>,
    seq: u64,
    wake: Arc<Notify>,
}

impl Entry {
    fn new() -> Self {
        Entry { tx: broadcast::channel(16).0, latest: None, seq: 0, wake: Arc::new(Notify::new()) }
    }
}

fn hub() -> &'static Mutex<HashMap<Key, Entry>> {
    static HUB: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(app: &Arc<App>, host: &str, pane_id: &str) -> Key {
    (Arc::as_ptr(app) as usize, host.to_string(), pane_id.to_string())
}

/// 送字／送鍵之後叫醒這顆 pane 的讀取迴圈（沒有觀看者就什麼都不做）。
pub fn wake(app: &Arc<App>, host: &str, pane_id: &str) {
    if let Some(e) = hub().lock().unwrap().get(&key(app, host, pane_id)) {
        e.wake.notify_one();
    }
}

struct Joined {
    latest: Option<Arc<Frame>>,
    rx: broadcast::Receiver<Event>,
    wake: Arc<Notify>,
    /// 這次 join 是否要自己開讀取迴圈（登記表裡還沒有這顆）。
    spawn: bool,
}

/// 加入觀看。`latest` 與 `rx` 在同一把鎖裡取得，所以不會漏幀也不會重複。
fn join(k: &Key) -> Joined {
    let mut h = hub().lock().unwrap();
    let spawn = !h.contains_key(k);
    let e = h.entry(k.clone()).or_insert_with(Entry::new);
    let rx = e.tx.subscribe();
    e.wake.notify_one();
    Joined { latest: e.latest.clone(), rx, wake: e.wake.clone(), spawn }
}

fn publish_frame(k: &Key, text: String, cols: Option<u32>, rows: Option<u32>) {
    let mut h = hub().lock().unwrap();
    if let Some(e) = h.get_mut(k) {
        e.seq += 1;
        let f = Arc::new(Frame { seq: e.seq, text, cols, rows });
        e.latest = Some(f.clone());
        let _ = e.tx.send(Event::Frame(f));
    }
}

/// 還有人看就回 true；沒人了就把登記拿掉並回 false。判斷與 [`join`] 在同一把鎖裡，所以不會跟新觀看者交錯。
fn keep_alive(k: &Key) -> bool {
    let mut h = hub().lock().unwrap();
    match h.get(k) {
        Some(e) if e.tx.receiver_count() > 0 => true,
        _ => {
            h.remove(k);
            false
        }
    }
}

/// 讀取失敗：先把錯誤幀送給所有還在的觀看者，再拿掉登記（之後的觀看者會重開迴圈）。
fn fail(k: &Key, code: &'static str) {
    if let Some(e) = hub().lock().unwrap().remove(k) {
        let _ = e.tx.send(Event::Error(code));
    }
}

fn latest(k: &Key) -> Option<Arc<Frame>> {
    hub().lock().unwrap().get(k).and_then(|e| e.latest.clone())
}

/// 幾何與身分的確認間隔：畫面沒變的時候，最多這麼久確認一次（pane 被重用、或 client 被縮放）。
const GEOMETRY_EVERY: Duration = Duration::from_secs(1);

/// 讀取迴圈：一顆 pane 只有一條，所有觀看者共用。
/// 每一拍只花一趟 herdr 呼叫（`pane.read`）；身分與幾何（兩趟）只在畫面變了、或距上次確認超過 [`GEOMETRY_EVERY`] 才問。
/// 遠端主機每趟約 100 多毫秒，這個差別決定推送是不是跟得上打字。
async fn run(app: Arc<App>, k: Key, wake: Arc<Notify>, host: String, pane_id: String) {
    let mut cadence = Cadence::new();
    let mut last_text: Option<u64> = None;
    let mut last_fp: Option<u64> = None;
    let mut geom: (Option<u32>, Option<u32>) = (None, None);
    let mut checked: Option<std::time::Instant> = None;
    loop {
        let (_cwd, read) = match shell::read_quick(&app, &host, &pane_id, STREAM_LINES).await {
            Ok(v) => v,
            Err(e) => {
                fail(&k, error_code(&e));
                return;
            }
        };
        let text_fp = fingerprint(&read.text, None, None);
        let text_changed = last_text != Some(text_fp);
        if text_changed || !checked.is_some_and(|t| t.elapsed() < GEOMETRY_EVERY) {
            match shell::geometry_checked(&app, &host, &pane_id).await {
                Ok(g) => {
                    geom = g;
                    checked = Some(std::time::Instant::now());
                }
                Err(e) => {
                    fail(&k, error_code(&e));
                    return;
                }
            }
        }
        let fp = fingerprint(&read.text, geom.0, geom.1);
        let changed = last_fp != Some(fp);
        if changed {
            last_fp = Some(fp);
            last_text = Some(text_fp);
            publish_frame(&k, read.text, geom.0, geom.1);
        }
        cadence.observe(changed);
        if !keep_alive(&k) {
            return;
        }
        let woke = tokio::select! {
            _ = tokio::time::sleep(cadence.delay()) => false,
            _ = wake.notified() => true,
        };
        if woke {
            // 給 herdr 一拍把字畫進去；之後的節奏從最快重新算。
            cadence.reset();
            tokio::time::sleep(FAST).await;
        }
    }
}

async fn send_json(socket: &mut WebSocket, v: &Value, timeout: Duration) -> bool {
    matches!(tokio::time::timeout(timeout, socket.send(Message::Text(v.to_string().into()))).await, Ok(Ok(())))
}

/// 一條連線：先送目前這一張，之後照推送送；錯誤幀後關閉。
async fn serve(app: Arc<App>, k: Key, host: String, pane_id: String, mut socket: WebSocket) {
    let timeout = app.ws_send_timeout();
    let j = join(&k);
    if j.spawn {
        tokio::spawn(Box::pin(run(app.clone(), k.clone(), j.wake.clone(), host.clone(), pane_id.clone())));
    }
    let mut rx = j.rx;
    let mut sent = 0u64;
    if let Some(f) = j.latest {
        sent = f.seq;
        if !send_json(&mut socket, &f.message(), timeout).await {
            return;
        }
    }
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(Event::Frame(f)) => {
                    if f.seq <= sent {
                        continue;
                    }
                    sent = f.seq;
                    if !send_json(&mut socket, &f.message(), timeout).await {
                        break;
                    }
                }
                Ok(Event::Error(code)) => {
                    let _ = send_json(&mut socket, &error_message(code), timeout).await;
                    break;
                }
                // 慢的 client 漏了幾幀：整張畫面是完整狀態，直接補最新一張。
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if let Some(f) = latest(&k) {
                        if f.seq > sent {
                            sent = f.seq;
                            if !send_json(&mut socket, &f.message(), timeout).await {
                                break;
                            }
                        }
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    drop(rx);
    // 讓迴圈看見觀看者少了（沒人了就自己停）。
    wake(&app, &host, &pane_id);
}

/// `GET /api/hosts/:name/shells/:pane_id/stream`。認證與 `/ws` 相同：Origin 必須是本機、
/// 只接受 UI token（query `token`），帶 bot／service／child 憑證一律拒絕；連線數共用 `/ws` 的上限。
pub async fn stream(
    State(app): State<Arc<App>>,
    Path((name, pane_id)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !origin_is_local(&headers, app.port, app.allow_lan()) {
        return (StatusCode::FORBIDDEN, "bad origin").into_response();
    }
    if ["X-AM-Bot-Id", "X-AM-Bot-Token", "X-AM-Child-Spawn-Id", "X-AM-Child-Token", "X-AM-Service-Id", "X-AM-Service-Token"]
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return (StatusCode::UNAUTHORIZED, "mixed credentials").into_response();
    }
    if !ct_eq(q.get("token").map(|s| s.as_str()).unwrap_or(""), &app.ui_token) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    // 參數錯誤走錯誤幀（連線照開），跟其他推送的錯誤路徑一致。
    let param_error = match (q.get("source").map(String::as_str).unwrap_or("visible"), q.get("format").map(String::as_str).unwrap_or("ansi")) {
        ("visible", "ansi") => None,
        (_, "ansi") => Some("bad_source"),
        _ => Some("bad_format"),
    };
    let Some(slot) = WsSlot::take(&app) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many websocket connections").into_response();
    };
    ws.max_message_size(crate::state::WS_MAX_INBOUND_BYTES)
        .max_frame_size(crate::state::WS_MAX_INBOUND_BYTES)
        .on_upgrade(move |mut socket| async move {
            match param_error {
                Some(code) => {
                    let _ = socket.send(Message::Text(error_message(code).to_string().into())).await;
                }
                None => {
                    let k = key(&app, &name, &pane_id);
                    serve(app, k, name, pane_id, socket).await;
                }
            }
            drop(slot);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cadence_starts_fast_backs_off_to_one_second_and_snaps_back_on_change() {
        let mut c = Cadence::new();
        assert_eq!(c.delay(), Duration::from_millis(50));
        let mut seen = vec![];
        for _ in 0..6 {
            c.observe(false);
            seen.push(c.delay().as_millis());
        }
        assert_eq!(seen, vec![100, 200, 400, 800, 1000, 1000], "沒變化就逐步加倍，封頂 1s");
        c.observe(true);
        assert_eq!(c.delay(), Duration::from_millis(50), "有變化立刻回到 50ms");
        c.observe(false);
        c.reset();
        assert_eq!(c.delay(), Duration::from_millis(50), "被喚醒也回到最快");
    }

    #[test]
    fn fingerprint_moves_with_text_and_geometry_only() {
        let a = fingerprint("$ ls", Some(80), Some(24));
        assert_eq!(a, fingerprint("$ ls", Some(80), Some(24)), "同內容同指紋");
        assert_ne!(a, fingerprint("$ ls\n", Some(80), Some(24)), "文字變了要變");
        assert_ne!(a, fingerprint("$ ls", Some(120), Some(24)), "寬度變了也要變");
        assert_ne!(a, fingerprint("$ ls", None, None), "幾何未知與已知要分得開");
    }

    #[test]
    fn frame_message_has_the_documented_shape() {
        let f = Frame { seq: 3, text: "\u{1b}[0mhi".into(), cols: Some(80), rows: None };
        assert_eq!(f.message(), json!({"type": "frame", "seq": 3, "text": "\u{1b}[0mhi", "cols": 80, "rows": null}));
        assert_eq!(error_message("not_found"), json!({"type": "error", "code": "not_found"}));
    }

    #[test]
    fn errors_map_to_codes_without_leaking_messages() {
        assert_eq!(error_code(&LcError::NotFound("shell".into())), "not_found");
        assert_eq!(error_code(&LcError::Upstream("herdr socket /run/secret".into())), "upstream");
        assert_eq!(error_code(&LcError::Bad("bad source".into())), "bad_request");
    }

    /// 測試用：起一條讀取迴圈並回傳觀看端（跟 `serve` 的 join＋spawn 同一段）。
    fn watch(app: &Arc<App>, host: &str, pane_id: &str) -> (Key, broadcast::Receiver<Event>, Arc<Notify>) {
        let k = key(app, host, pane_id);
        let j = join(&k);
        if j.spawn {
            tokio::spawn(Box::pin(run(app.clone(), k.clone(), j.wake.clone(), host.to_string(), pane_id.to_string())));
        }
        (k, j.rx, j.wake)
    }

    async fn next_frame(rx: &mut broadcast::Receiver<Event>, within: Duration) -> Option<Arc<Frame>> {
        match tokio::time::timeout(within, rx.recv()).await {
            Ok(Ok(Event::Frame(f))) => Some(f),
            _ => None,
        }
    }

    /// 沒變的畫面不推；變了才推，seq 遞增。
    #[tokio::test]
    async fn a_still_screen_is_not_pushed_and_a_changed_one_is_pushed_once() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "hello");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id);
        let first = next_frame(&mut rx, Duration::from_secs(2)).await.expect("連上先來一張完整畫面");
        assert_eq!((first.seq, first.text.as_str()), (1, "hello"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(rx.try_recv().is_err(), "畫面沒變就不推");
        env.herdr.set_screen(&pane.pane_id, "hello\nworld");
        let second = next_frame(&mut rx, Duration::from_secs(2)).await.expect("變了要推");
        assert_eq!((second.seq, second.text.as_str()), (2, "hello\nworld"));
    }

    /// 沒有觀看者了，讀取迴圈要自己停、登記要拿掉。
    #[tokio::test]
    async fn the_loop_stops_and_forgets_the_pane_when_the_last_watcher_leaves() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "x");
        let (k, rx, _wake) = watch(app, "local", &pane.pane_id);
        assert!(hub().lock().unwrap().contains_key(&k));
        drop(rx);
        wake(app, "local", &pane.pane_id);
        let gone = tokio::time::timeout(Duration::from_secs(2), async {
            while hub().lock().unwrap().contains_key(&k) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(gone.is_ok(), "最後一個觀看者離開後，迴圈與登記都要收掉");
    }

    /// 兩個觀看者共用同一個讀取迴圈：第二個 join 不會再開一條。
    #[tokio::test]
    async fn watchers_of_one_pane_share_one_loop() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "shared");
        let k = key(app, "local", &pane.pane_id);
        let a = join(&k);
        let b = join(&k);
        assert!(a.spawn, "第一個觀看者開迴圈");
        assert!(!b.spawn, "第二個只加入");
        assert_eq!(hub().lock().unwrap().get(&k).unwrap().tx.receiver_count(), 2);
        drop(a);
        drop(b);
        wake(app, "local", &pane.pane_id);
    }

    /// 送字／送鍵之後的喚醒：畫面在輪詢閒置期間變了，喚醒後要很快（遠早於閒置間隔）就推出來。
    #[tokio::test]
    async fn a_wake_after_a_send_pushes_the_change_without_waiting_for_the_idle_interval() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "$ ");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id);
        assert!(next_frame(&mut rx, Duration::from_secs(2)).await.is_some());
        // 讓節奏慢下來（連續沒變，會退到 1s）。
        tokio::time::sleep(Duration::from_millis(1600)).await;
        env.herdr.set_screen(&pane.pane_id, "$ ls");
        wake(app, "local", &pane.pane_id);
        let f = next_frame(&mut rx, Duration::from_millis(400)).await;
        assert!(f.is_some_and(|f| f.text == "$ ls"), "喚醒後應在閒置間隔（最多 1s）之前推出來");
    }

    /// 讀取失敗（pane 不在了）：先給錯誤，登記拿掉。
    #[tokio::test]
    async fn a_pane_that_is_gone_ends_the_stream_with_an_error_code() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "__READ_ERROR__");
        let (k, mut rx, _wake) = watch(app, "local", &pane.pane_id);
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.expect("要有結果");
        assert!(matches!(ev, Ok(Event::Error("upstream"))), "讀取失敗要回錯誤碼：{ev:?}");
        assert!(!hub().lock().unwrap().contains_key(&k));
    }

    /// 整條路徑：真的 WebSocket 握手（UI token）、連上先收到一張完整畫面、錯誤參數回錯誤幀。
    #[tokio::test]
    async fn the_socket_sends_the_current_frame_then_pushes_changes_and_refuses_bad_params() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let env = crate::testing::env().await;
        let app = env.app.clone();
        app.set_startup_ready(true);
        let pane = crate::api::shell::tracked_test_pane(&app).await;
        env.herdr.set_screen(&pane.pane_id, "over the wire");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = crate::api::router(app.clone());
        tokio::spawn(async move { axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>()).await });

        async fn open(addr: std::net::SocketAddr, path: &str) -> tokio::net::TcpStream {
            let mut c = tokio::net::TcpStream::connect(addr).await.unwrap();
            let req = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n");
            c.write_all(req.as_bytes()).await.unwrap();
            // 握手回應只有一次：讀到空行為止，確認是 101。
            let mut head = Vec::new();
            loop {
                let mut b = [0u8; 1];
                c.read_exact(&mut b).await.unwrap();
                head.push(b[0]);
                if head.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"), "要升級成 WebSocket：{}", String::from_utf8_lossy(&head));
            c
        }
        // 讀一個伺服器→client 的文字幀（不遮罩）。每一幀都設上限：卡住要失敗，不能掛著。
        async fn read_frame(c: &mut tokio::net::TcpStream) -> Value {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut h = [0u8; 2];
                c.read_exact(&mut h).await.unwrap();
                assert_eq!(h[0], 0x81, "文字幀");
                let len = match h[1] & 0x7f {
                    126 => {
                        let mut e = [0u8; 2];
                        c.read_exact(&mut e).await.unwrap();
                        u16::from_be_bytes(e) as usize
                    }
                    n => n as usize,
                };
                let mut body = vec![0u8; len];
                c.read_exact(&mut body).await.unwrap();
                serde_json::from_slice::<Value>(&body).unwrap()
            })
            .await
            .expect("5 秒內要有一幀")
        }

        let path = format!("/api/hosts/local/shells/{}/stream?source=visible&format=ansi&token=test-token", pane.pane_id);
        let mut c = open(addr, &path).await;
        let first = read_frame(&mut c).await;
        assert_eq!(first["type"], "frame");
        assert_eq!(first["seq"], 1);
        assert_eq!(first["text"], "over the wire");

        env.herdr.set_screen(&pane.pane_id, "over the wire\nmore");
        let second = read_frame(&mut c).await;
        assert_eq!(second["seq"], 2, "變了就推下一幀");
        assert_eq!(second["text"], "over the wire\nmore");

        // 文字版不是這條串流的格式：連線照開，給一張錯誤幀。
        let mut bad = open(addr, &format!("/api/hosts/local/shells/{}/stream?format=text&token=test-token", pane.pane_id)).await;
        assert_eq!(read_frame(&mut bad).await, error_message("bad_format"));
    }

    /// 畫面不動的時候，每一拍只讀畫面；身分與幾何不是每拍都問（遠端每趟 100 多毫秒，這是推送跟不跟得上的差別）。
    #[tokio::test]
    async fn an_idle_pane_costs_one_read_per_tick_not_the_full_check() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "idle");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id);
        assert!(next_frame(&mut rx, Duration::from_secs(2)).await.is_some());
        let gets_before = env.herdr.calls_to("pane.get").len();
        let reads_before = env.herdr.calls_to("pane.read").len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let gets = env.herdr.calls_to("pane.get").len() - gets_before;
        let reads = env.herdr.calls_to("pane.read").len() - reads_before;
        assert!(reads >= 2, "閒置時照常每拍讀：reads={reads}");
        assert!(gets <= 1, "閒置時不該每拍都查身分：pane.get={gets}（reads={reads}）");
    }
}
