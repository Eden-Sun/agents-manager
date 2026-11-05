//! 主機 shell 的推送串流：`GET /api/hosts/:name/shells/:pane_id/stream?source=visible&format=ansi`。
//!
//! 兩種來源，同一條 WebSocket（靠訊息的 `type` 分辨）：
//! - **observe（預設）**：每條 `(host, pane, cols, rows)` 起一個 `herdr terminal session observe` 子行程
//!   （[`am_base::herdr_observe`]）。herdr 吐的 ANSI 差異幀原封不動轉成 `{"type":"raw"}`；游標位置與同步更新
//!   都在 ANSI 裡，前端不用猜。client 送 `{"type":"size"}` 就換成那個尺寸的串流（尺寸不同各自一條，同尺寸共用）。
//!   新加入的觀看者先收到「完整幀＋之後的差異」（[`Entry::replay`]），所以中途連上也畫得對。
//! - **快照（退路）**：observe 起不來、或起來了卻一幀都沒給（舊版 herdr、遠端 ssh 不通…）時，這顆 host 冷卻
//!   [`OBSERVE_COOLDOWN`] 內改用 `pane.read` 輪詢，推 `{"type":"frame"}` 整張畫面（舊行為，不變）。
//!   冷卻期間新開的串流直接走快照；冷卻過了才再試 observe。
//!
//! 登記表是行程內的全域（key 含 `App` 的位址，測試之間不互相干擾）。最後一個觀看者離開，讀取迴圈就自己停
//! （observe 的子行程隨之被殺）。

use crate::api::shell;
use crate::api::{ct_eq, origin_is_local, WsSlot};
use am_base::herdr::HerdrClient;
use am_base::herdr_observe::{ObserveMsg, ObserveStream};
use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, Notify};

/// 快照模式：有變化時的輪詢間隔，也是送字／送鍵後第一次讀的間隔。
pub const FAST: Duration = Duration::from_millis(50);
/// 快照模式：連續沒變化時的上限。
pub const SLOW: Duration = Duration::from_secs(1);
/// 快照模式：每條串流讀的行數（`visible` 下是畫面高度，給個夠大的上限）。
const STREAM_LINES: u32 = 200;
/// observe：回放緩衝的上限（位元組／幀數）。超過就重開子行程，讓 herdr 送新的完整幀。
const REPLAY_MAX_BYTES: usize = 256 * 1024;
const REPLAY_MAX_FRAMES: usize = 1024;
/// observe 起不來或一幀都沒給時，這顆 host 冷卻這麼久才再試 observe。
const OBSERVE_COOLDOWN: Duration = Duration::from_secs(60);
/// observe 子行程意外結束後重開的退避：活得夠久就從最短重來，否則加倍。
const RESTART_MIN: Duration = Duration::from_millis(100);
const RESTART_MAX: Duration = Duration::from_secs(5);
const LONG_LIVED: Duration = Duration::from_secs(5);
/// 身分與幾何的確認間隔（快照模式每拍看情況問；observe 模式固定每秒確認一次 pane 還是登記的那一顆）。
const GEOMETRY_EVERY: Duration = Duration::from_secs(1);
/// 連上後等 client 送第一個 `size` 的時間；等不到就用預設尺寸（舊 client 不送 size 也能用）。
const SIZE_WAIT: Duration = Duration::from_millis(150);
/// 預設尺寸與 size 訊息的合法範圍（與前端 xterm 的常見大小涵蓋）。
pub const DEFAULT_COLS: u32 = 80;
pub const DEFAULT_ROWS: u32 = 24;
const SIZE_MAX: u32 = 500;

/// 快照模式的自適應輪詢節奏。純狀態、不碰時鐘，好測。
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

/// 推給 client 的內容。`Snapshot` 是快照模式的整張畫面；`Raw` 是 observe 的一幀（ANSI 差異或完整幀）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Snapshot { text: String, cols: Option<u32>, rows: Option<u32> },
    Raw { full: bool, cols: u32, rows: u32, bytes: Vec<u8> },
}

/// 一幀。`seq` 是這顆 `(host, pane, 尺寸)` 串流內的序號，只增不減（重開子行程也接著算）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub seq: u64,
    pub body: Body,
}

impl Frame {
    /// wire 形狀：快照 `{"type":"frame",…}`；observe `{"type":"raw","seq","full","cols","rows","data":"<base64 ANSI>"}`。
    pub fn message(&self) -> Value {
        match &self.body {
            Body::Snapshot { text, cols, rows } => json!({"type": "frame", "seq": self.seq, "text": text, "cols": cols, "rows": rows}),
            Body::Raw { full, cols, rows, bytes } => {
                json!({"type": "raw", "seq": self.seq, "full": full, "cols": cols, "rows": rows, "data": STANDARD.encode(bytes)})
            }
        }
    }

    fn size(&self) -> usize {
        match &self.body {
            Body::Snapshot { text, .. } => text.len(),
            Body::Raw { bytes, .. } => bytes.len(),
        }
    }

    fn is_full(&self) -> bool {
        match &self.body {
            Body::Snapshot { .. } => true,
            Body::Raw { full, .. } => *full,
        }
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

/// client 的尺寸訊息：`{"type":"size","cols":C,"rows":R}`。不合法（缺欄位、超出範圍）回 `None`，當沒送。
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    let v: Value = serde_json::from_str(text).ok()?;
    if v.get("type")?.as_str()? != "size" {
        return None;
    }
    let cols = u32::try_from(v.get("cols")?.as_u64()?).ok()?;
    let rows = u32::try_from(v.get("rows")?.as_u64()?).ok()?;
    ((1..=SIZE_MAX).contains(&cols) && (1..=SIZE_MAX).contains(&rows)).then_some((cols, rows))
}

/// 登記表的 key：`(App 的位址, host, pane_id, cols, rows)`。快照模式下尺寸只是分組用，不影響內容。
type Key = (usize, String, String, u32, u32);

#[derive(Clone, Debug)]
enum Event {
    Frame(Arc<Frame>),
    Error(&'static str),
}

struct Entry {
    tx: broadcast::Sender<Event>,
    /// 回放緩衝：最近一張完整幀開始的所有幀（快照模式就只有最後一張）。新觀看者與掉幀的觀看者靠它補畫面。
    replay: Vec<Arc<Frame>>,
    replay_bytes: usize,
    seq: u64,
    wake: Arc<Notify>,
}

impl Entry {
    fn new() -> Self {
        Entry { tx: broadcast::channel(16).0, replay: Vec::new(), replay_bytes: 0, seq: 0, wake: Arc::new(Notify::new()) }
    }
}

fn hub() -> &'static Mutex<HashMap<Key, Entry>> {
    static HUB: OnceLock<Mutex<HashMap<Key, Entry>>> = OnceLock::new();
    HUB.get_or_init(|| Mutex::new(HashMap::new()))
}

fn key(app: &Arc<App>, host: &str, pane_id: &str, cols: u32, rows: u32) -> Key {
    (Arc::as_ptr(app) as usize, host.to_string(), pane_id.to_string(), cols, rows)
}

/// 送字／送鍵之後叫醒這顆 pane 所有尺寸的讀取迴圈（沒有觀看者就什麼都不做）。
pub fn wake(app: &Arc<App>, host: &str, pane_id: &str) {
    let addr = Arc::as_ptr(app) as usize;
    for (k, e) in hub().lock().unwrap().iter() {
        if k.0 == addr && k.1 == host && k.2 == pane_id {
            e.wake.notify_one();
        }
    }
}

fn wake_key(k: &Key) {
    if let Some(e) = hub().lock().unwrap().get(k) {
        e.wake.notify_one();
    }
}

struct Joined {
    replay: Vec<Arc<Frame>>,
    rx: broadcast::Receiver<Event>,
    wake: Arc<Notify>,
    /// 這次 join 是否要自己開讀取迴圈（登記表裡還沒有這顆）。
    spawn: bool,
}

/// 加入觀看。回放與 `rx` 在同一把鎖裡取得，所以不會漏幀也不會重複。
fn join(k: &Key) -> Joined {
    let mut h = hub().lock().unwrap();
    let spawn = !h.contains_key(k);
    let e = h.entry(k.clone()).or_insert_with(Entry::new);
    let rx = e.tx.subscribe();
    e.wake.notify_one();
    Joined { replay: e.replay.clone(), rx, wake: e.wake.clone(), spawn }
}

/// 加入並視需要開讀取迴圈。`test` 也走這裡，跟真的連線同一段。
fn attach(app: &Arc<App>, k: &Key, host: &str, pane_id: &str) -> Joined {
    let j = join(k);
    if j.spawn {
        tokio::spawn(Box::pin(run(app.clone(), k.clone(), j.wake.clone(), host.to_string(), pane_id.to_string())));
    }
    j
}

/// 推一幀進回放與廣播。完整幀清掉舊回放；沒有完整幀打頭的差異直接丟掉（client 還沒有基準）。
/// 回傳 true 表示回放已超過上限，呼叫端應重開子行程，讓 herdr 送新的完整幀。
fn publish(k: &Key, body: Body) -> bool {
    let mut h = hub().lock().unwrap();
    let Some(e) = h.get_mut(k) else { return false };
    e.seq += 1;
    let f = Arc::new(Frame { seq: e.seq, body });
    if f.is_full() {
        e.replay.clear();
        e.replay_bytes = 0;
    } else if e.replay.is_empty() {
        e.seq -= 1;
        return false;
    }
    e.replay_bytes += f.size();
    e.replay.push(f.clone());
    let over = e.replay.len() > REPLAY_MAX_FRAMES || e.replay_bytes > REPLAY_MAX_BYTES;
    let _ = e.tx.send(Event::Frame(f));
    over
}

/// 目前的回放（新觀看者或掉幀後補畫面用）。
fn replay(k: &Key) -> Vec<Arc<Frame>> {
    hub().lock().unwrap().get(k).map(|e| e.replay.clone()).unwrap_or_default()
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

/// observe 的冷卻表：`(App 位址, host) → 冷卻到何時`。
fn observe_cooldowns() -> &'static Mutex<HashMap<(usize, String), Instant>> {
    static COOLDOWNS: OnceLock<Mutex<HashMap<(usize, String), Instant>>> = OnceLock::new();
    COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn observe_is_cooling(app: &Arc<App>, host: &str) -> bool {
    let addr = Arc::as_ptr(app) as usize;
    observe_cooldowns().lock().unwrap().get(&(addr, host.to_string())).is_some_and(|until| Instant::now() < *until)
}

fn start_observe_cooldown(app: &Arc<App>, host: &str) {
    let addr = Arc::as_ptr(app) as usize;
    observe_cooldowns().lock().unwrap().insert((addr, host.to_string()), Instant::now() + OBSERVE_COOLDOWN);
}

/// observe 一趟的結果。
enum Outcome {
    /// 沒有觀看者了，或讀取失敗（已經 `fail`）：整個迴圈結束。
    Stop,
    /// 子行程結束或回放超過上限：重開（活得夠久就從最短退避重來）。
    Restart { lived: Duration },
    /// observe 起不來、或一幀都沒給：這顆 host 改走快照。
    Fallback,
}

/// 讀取迴圈：一個 `(host, pane, 尺寸)` 只有一條，所有觀看者共用。
/// observe 可用就用它；不可用（或一幀都沒給）就冷卻後退回快照輪詢。
async fn run(app: Arc<App>, k: Key, wake: Arc<Notify>, host: String, pane_id: String) {
    if !observe_is_cooling(&app, &host) {
        let mut backoff = RESTART_MIN;
        loop {
            match Box::pin(observe_once(&app, &k, &wake, &host, &pane_id)).await {
                Outcome::Stop => return,
                Outcome::Restart { lived } => {
                    backoff = if lived >= LONG_LIVED { RESTART_MIN } else { (backoff * 2).min(RESTART_MAX) };
                    tokio::time::sleep(backoff).await;
                    if !keep_alive(&k) {
                        return;
                    }
                }
                Outcome::Fallback => {
                    tracing::warn!(host = %host, pane = %pane_id, "herdr terminal session observe 不可用，這顆 host 改用快照輪詢一段時間");
                    start_observe_cooldown(&app, &host);
                    break;
                }
            }
        }
    }
    Box::pin(snapshot(&app, &k, &wake, &host, &pane_id)).await;
}

/// 一條 observe 子行程的生命：讀幀、推幀、每秒確認 pane 身分。
async fn observe_once(app: &Arc<App>, k: &Key, wake: &Arc<Notify>, host: &str, pane_id: &str) -> Outcome {
    let client: HerdrClient = match shell::observe_client(app, host, pane_id).await {
        Ok(c) => c,
        Err(e) => {
            fail(k, error_code(&e));
            return Outcome::Stop;
        }
    };
    let cmd = client.observe_cmd(pane_id, k.3, k.4);
    let mut stream = match ObserveStream::start(&cmd) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "起不來 herdr observe 子行程");
            return Outcome::Fallback;
        }
    };
    let started = Instant::now();
    let mut got = false;
    let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + GEOMETRY_EVERY, GEOMETRY_EVERY);
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(ObserveMsg::Frame { full, width, height, bytes, .. }) => {
                    got = true;
                    if publish(k, Body::Raw { full, cols: width, rows: height, bytes }) {
                        return Outcome::Restart { lived: started.elapsed() };
                    }
                    if !keep_alive(k) {
                        return Outcome::Stop;
                    }
                }
                Some(ObserveMsg::Closed { .. }) | None => break,
            },
            _ = wake.notified() => {
                if !keep_alive(k) {
                    return Outcome::Stop;
                }
            }
            _ = ticker.tick() => {
                if !keep_alive(k) {
                    return Outcome::Stop;
                }
                if let Err(e) = shell::geometry_checked(app, host, pane_id).await {
                    fail(k, error_code(&e));
                    return Outcome::Stop;
                }
            }
        }
    }
    if got {
        Outcome::Restart { lived: started.elapsed() }
    } else {
        Outcome::Fallback
    }
}

/// 快照模式（退路）：輪詢 `pane.read`，用內容 hash 判斷有沒有變；身分與幾何只在畫面變了、或距上次確認超過 [`GEOMETRY_EVERY`] 才問。
async fn snapshot(app: &Arc<App>, k: &Key, wake: &Arc<Notify>, host: &str, pane_id: &str) {
    let mut cadence = Cadence::new();
    let mut last_text: Option<u64> = None;
    let mut last_fp: Option<u64> = None;
    let mut geom: (Option<u32>, Option<u32>) = (None, None);
    let mut checked: Option<Instant> = None;
    loop {
        let (_cwd, read) = match shell::read_quick(app, host, pane_id, STREAM_LINES).await {
            Ok(v) => v,
            Err(e) => {
                fail(k, error_code(&e));
                return;
            }
        };
        let text_fp = fingerprint(&read.text, None, None);
        let text_changed = last_text != Some(text_fp);
        if text_changed || !checked.is_some_and(|t| t.elapsed() < GEOMETRY_EVERY) {
            match shell::geometry_checked(app, host, pane_id).await {
                Ok(g) => {
                    geom = g;
                    checked = Some(Instant::now());
                }
                Err(e) => {
                    fail(k, error_code(&e));
                    return;
                }
            }
        }
        let fp = fingerprint(&read.text, geom.0, geom.1);
        let changed = last_fp != Some(fp);
        if changed {
            last_fp = Some(fp);
            last_text = Some(text_fp);
            publish(k, Body::Snapshot { text: read.text, cols: geom.0, rows: geom.1 });
        }
        cadence.observe(changed);
        if !keep_alive(k) {
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

/// 依序送一批幀（`sent` 是已送到的最大 seq；只送比它新的）。送失敗回 false。
async fn send_frames(socket: &mut WebSocket, frames: &[Arc<Frame>], sent: &mut u64, timeout: Duration) -> bool {
    for f in frames {
        if f.seq <= *sent {
            continue;
        }
        if !send_json(socket, &f.message(), timeout).await {
            return false;
        }
        *sent = f.seq;
    }
    true
}

/// 連上後等第一個 `size`；等不到（舊 client）就回 `None`，用預設尺寸。
async fn first_size(socket: &mut WebSocket) -> Option<(u32, u32)> {
    tokio::time::timeout(SIZE_WAIT, async {
        loop {
            match socket.recv().await {
                Some(Ok(Message::Text(t))) => {
                    if let Some(sz) = parse_size(t.as_str()) {
                        return Some(sz);
                    }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return None,
                _ => {}
            }
        }
    })
    .await
    .ok()
    .flatten()
}

/// 一條連線：先送回放（完整幀起頭），之後照推送送；收到不同尺寸的 `size` 就換成那個尺寸的串流；錯誤幀後關閉。
async fn serve(app: Arc<App>, host: String, pane_id: String, mut socket: WebSocket) {
    let timeout = app.ws_send_timeout();
    let (mut cols, mut rows) = first_size(&mut socket).await.unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
    let mut k = key(&app, &host, &pane_id, cols, rows);
    let joined = attach(&app, &k, &host, &pane_id);
    let mut rx = joined.rx;
    let mut sent = 0u64;
    if send_frames(&mut socket, &joined.replay, &mut sent, timeout).await {
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
                    // 慢的 client 漏了幾幀：回放是完整狀態（以完整幀起頭），整段重送即可。
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let mut s = 0u64;
                        if !send_frames(&mut socket, &replay(&k), &mut s, timeout).await {
                            break;
                        }
                        sent = sent.max(s);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                msg = socket.recv() => match msg {
                    Some(Ok(Message::Text(t))) => {
                        let Some((c, r)) = parse_size(t.as_str()) else { continue };
                        if (c, r) == (cols, rows) {
                            continue;
                        }
                        let old = std::mem::replace(&mut k, key(&app, &host, &pane_id, c, r));
                        (cols, rows) = (c, r);
                        let joined = attach(&app, &k, &host, &pane_id);
                        // 先換掉 rx（舊的接收端在這裡丟掉），舊的串流才看得到沒人了。
                        rx = joined.rx;
                        wake_key(&old);
                        sent = 0;
                        if !send_frames(&mut socket, &joined.replay, &mut sent, timeout).await {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                    _ => {}
                },
            }
        }
    }
    drop(rx);
    // 讓迴圈看見觀看者少了（沒人了就自己停）。
    wake_key(&k);
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
                None => Box::pin(serve(app, name, pane_id, socket)).await,
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
    fn wire_shapes_match_the_documented_contract() {
        let snap = Frame { seq: 3, body: Body::Snapshot { text: "\u{1b}[0mhi".into(), cols: Some(80), rows: None } };
        assert_eq!(snap.message(), json!({"type": "frame", "seq": 3, "text": "\u{1b}[0mhi", "cols": 80, "rows": null}));
        let raw = Frame { seq: 4, body: Body::Raw { full: false, cols: 100, rows: 30, bytes: b"hi".to_vec() } };
        assert_eq!(raw.message(), json!({"type": "raw", "seq": 4, "full": false, "cols": 100, "rows": 30, "data": "aGk="}));
        assert_eq!(error_message("not_found"), json!({"type": "error", "code": "not_found"}));
    }

    #[test]
    fn size_messages_are_checked_and_out_of_range_ones_are_ignored() {
        assert_eq!(parse_size(r#"{"type":"size","cols":120,"rows":40}"#), Some((120, 40)));
        assert_eq!(parse_size(r#"{"type":"size","cols":0,"rows":40}"#), None, "0 不合法");
        assert_eq!(parse_size(r#"{"type":"size","cols":501,"rows":40}"#), None, "超過上限");
        assert_eq!(parse_size(r#"{"type":"size","cols":"80","rows":24}"#), None, "型別錯");
        assert_eq!(parse_size(r#"{"type":"raw"}"#), None);
        assert_eq!(parse_size("not json"), None);
    }

    #[test]
    fn errors_map_to_codes_without_leaking_messages() {
        assert_eq!(error_code(&LcError::NotFound("shell".into())), "not_found");
        assert_eq!(error_code(&LcError::Upstream("herdr socket /run/secret".into())), "upstream");
        assert_eq!(error_code(&LcError::Bad("bad source".into())), "bad_request");
    }

    /// 假的 observe 子行程：把尺寸印進幀裡，之後一直掛著（像真的 CLI 一樣等下一幀）。
    /// 參數照 `terminal session observe <pane> --cols C --rows R` 排：`$6`＝C、`$8`＝R。
    fn fake_observe(tag: &str, body: &str) -> String {
        let dir = crate::test_dirs::scratch_dir(&format!("shell-observe-{tag}"));
        let path = dir.join("observe.sh");
        // 並行的別條測試在別的執行緒 fork 時會短暫繼承寫入 fd，這時 exec 回 ETXTBSY → observe 起不來 →
        // 退回快照幀，第一幀就不是 raw（整批測試偶發紅的真正原因）。write_exec 寫完先 exec 探測到成功才回來。
        crate::testing::write_exec(&path, format!("#!/bin/sh\n{body}\n"));
        path.display().to_string()
    }

    /// 一幀完整畫面（內容 `hello`），寬高取自參數，之後掛著。
    const HELLO_THEN_HANG: &str = r#"printf '{"type":"terminal.frame","seq":1,"encoding":"ansi","width":%s,"height":%s,"full":true,"bytes":"aGVsbG8="}\n' "$6" "$8"; exec sleep 30"#;

    /// 測試之間 `App` 的位址可能被重用，而前一個測試的 runtime 關掉時它的登記不會被收（沒有人叫醒它）。
    /// 所以先清掉同一位址、已經沒有觀看者的殘留登記（還有人看的留著），免得接到前一個測試的畫面。
    fn purge_app(app: &Arc<App>) {
        let addr = Arc::as_ptr(app) as usize;
        hub().lock().unwrap().retain(|k, e| k.0 != addr || e.tx.receiver_count() > 0);
    }

    /// 測試用：起一條串流並回傳觀看端（跟 `serve` 的 attach 同一段）。
    fn watch(app: &Arc<App>, host: &str, pane_id: &str, cols: u32, rows: u32) -> (Key, broadcast::Receiver<Event>, Arc<Notify>) {
        purge_app(app);
        let k = key(app, host, pane_id, cols, rows);
        let j = attach(app, &k, host, pane_id);
        (k, j.rx, j.wake)
    }

    async fn next_frame(rx: &mut broadcast::Receiver<Event>, within: Duration) -> Option<Arc<Frame>> {
        match tokio::time::timeout(within, rx.recv()).await {
            Ok(Ok(Event::Frame(f))) => Some(f),
            _ => None,
        }
    }

    /// 快照模式（退路）：observe 起不來，所以走 `pane.read`。
    fn snapshot_only(app: &Arc<App>) {
        app.herdr.set_observe_program(Some("/bin/false".into()));
    }

    /// 沒變的畫面不推；變了才推，seq 遞增。
    #[tokio::test]
    async fn a_still_screen_is_not_pushed_and_a_changed_one_is_pushed_once() {
        let env = crate::testing::env().await;
        let app = &env.app;
        snapshot_only(app);
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "hello");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
        let first = next_frame(&mut rx, Duration::from_secs(2)).await.expect("連上先來一張完整畫面");
        assert_eq!(first.seq, 1);
        assert!(matches!(&first.body, Body::Snapshot { text, .. } if text == "hello"));
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(rx.try_recv().is_err(), "畫面沒變就不推");
        env.herdr.set_screen(&pane.pane_id, "hello\nworld");
        let second = next_frame(&mut rx, Duration::from_secs(2)).await.expect("變了要推");
        assert_eq!(second.seq, 2);
        assert!(matches!(&second.body, Body::Snapshot { text, .. } if text == "hello\nworld"));
    }

    /// 沒有觀看者了，讀取迴圈要自己停、登記要拿掉。
    #[tokio::test]
    async fn the_loop_stops_and_forgets_the_pane_when_the_last_watcher_leaves() {
        let env = crate::testing::env().await;
        let app = &env.app;
        app.herdr.set_observe_program(Some(fake_observe("stop", HELLO_THEN_HANG)));
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "x");
        let (k, rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
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

    /// 兩個觀看者同尺寸：共用同一個讀取迴圈；第二個 join 不會再開一條。
    #[tokio::test]
    async fn watchers_of_one_pane_and_size_share_one_loop() {
        let env = crate::testing::env().await;
        let app = &env.app;
        snapshot_only(app);
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "shared");
        let k = key(app, "local", &pane.pane_id, 80, 24);
        let a = attach(app, &k, "local", &pane.pane_id);
        let b = attach(app, &k, "local", &pane.pane_id);
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
        snapshot_only(app);
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "$ ");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
        assert!(next_frame(&mut rx, Duration::from_secs(2)).await.is_some());
        // 讓節奏慢下來（連續沒變，會退到 1s）。
        tokio::time::sleep(Duration::from_millis(1600)).await;
        env.herdr.set_screen(&pane.pane_id, "$ ls");
        wake(app, "local", &pane.pane_id);
        let f = next_frame(&mut rx, Duration::from_millis(400)).await;
        assert!(f.is_some_and(|f| matches!(&f.body, Body::Snapshot { text, .. } if text == "$ ls")), "喚醒後應在閒置間隔（最多 1s）之前推出來");
    }

    /// 讀取失敗（pane 不在了）：先給錯誤，登記拿掉。
    #[tokio::test]
    async fn a_pane_that_is_gone_ends_the_stream_with_an_error_code() {
        let env = crate::testing::env().await;
        let app = &env.app;
        snapshot_only(app);
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "__READ_ERROR__");
        let (k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
        let ev = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await.expect("要有結果");
        assert!(matches!(ev, Ok(Event::Error("upstream"))), "讀取失敗要回錯誤碼：{ev:?}");
        assert!(!hub().lock().unwrap().contains_key(&k));
    }

    /// 畫面不動的時候，每一拍只讀畫面；身分與幾何不是每拍都問（遠端每趟 100 多毫秒，這是推送跟不跟得上的差別）。
    #[tokio::test]
    async fn an_idle_pane_costs_one_read_per_tick_not_the_full_check() {
        let env = crate::testing::env().await;
        let app = &env.app;
        snapshot_only(app);
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "idle");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
        assert!(next_frame(&mut rx, Duration::from_secs(2)).await.is_some());
        let gets_before = env.herdr.calls_to("pane.get").len();
        let reads_before = env.herdr.calls_to("pane.read").len();
        tokio::time::sleep(Duration::from_millis(500)).await;
        let gets = env.herdr.calls_to("pane.get").len() - gets_before;
        let reads = env.herdr.calls_to("pane.read").len() - reads_before;
        assert!(reads >= 2, "閒置時照常每拍讀：reads={reads}");
        assert!(gets <= 1, "閒置時不該每拍都查身分：pane.get={gets}（reads={reads}）");
    }

    /// observe 的一幀原封不動轉成 raw：位元組、完整旗標、畫面尺寸都不動。
    #[tokio::test]
    async fn observe_frames_are_forwarded_as_they_are() {
        let env = crate::testing::env().await;
        let app = &env.app;
        app.herdr.set_observe_program(Some(fake_observe("forward", HELLO_THEN_HANG)));
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "unused");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 100, 30);
        let f = next_frame(&mut rx, Duration::from_secs(3)).await.expect("observe 的完整幀要推出來");
        assert_eq!(f.seq, 1);
        assert_eq!(f.body, Body::Raw { full: true, cols: 100, rows: 30, bytes: b"hello".to_vec() });
        assert_eq!(f.message()["type"], "raw");
        assert_eq!(f.message()["data"], "aGVsbG8=");
    }

    /// 尺寸不同各自一條串流：同一顆 pane，100×30 的觀看者拿到 100×30 的幀。
    #[tokio::test]
    async fn a_different_size_gets_its_own_observe_stream() {
        let env = crate::testing::env().await;
        let app = &env.app;
        app.herdr.set_observe_program(Some(fake_observe("sizes", HELLO_THEN_HANG)));
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "unused");
        let (_a, mut rx_a, _wa) = watch(app, "local", &pane.pane_id, 80, 24);
        let (_b, mut rx_b, _wb) = watch(app, "local", &pane.pane_id, 100, 30);
        let fa = next_frame(&mut rx_a, Duration::from_secs(3)).await.expect("80×24 的幀");
        let fb = next_frame(&mut rx_b, Duration::from_secs(3)).await.expect("100×30 的幀");
        assert!(matches!(fa.body, Body::Raw { cols: 80, rows: 24, .. }), "{:?}", fa.body);
        assert!(matches!(fb.body, Body::Raw { cols: 100, rows: 30, .. }), "{:?}", fb.body);
    }

    /// observe 子行程意外結束：重開，新的第一幀是完整幀（client 會 reset 再畫）。
    #[tokio::test]
    async fn an_observe_that_exits_is_restarted_and_its_new_first_frame_is_full() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let dir = crate::test_dirs::scratch_dir("shell-observe-restart");
        let count = dir.join("count");
        let body = format!(
            r#"F='{}'; n=$(cat "$F" 2>/dev/null || echo 0); n=$((n+1)); echo $n > "$F"
if [ $n -eq 1 ]; then B=aGVsbG8=; else B=d29ybGQ=; fi
printf '{{"type":"terminal.frame","seq":%s,"encoding":"ansi","width":80,"height":24,"full":true,"bytes":"%s"}}\n' $n $B
if [ $n -eq 1 ]; then exit 0; fi
exec sleep 30"#,
            count.display()
        );
        app.herdr.set_observe_program(Some(fake_observe("restart", &body)));
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "unused");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
        let first = next_frame(&mut rx, Duration::from_secs(3)).await.expect("第一趟");
        assert!(matches!(&first.body, Body::Raw { full: true, bytes, .. } if bytes == b"hello"));
        let second = next_frame(&mut rx, Duration::from_secs(3)).await.expect("斷線後重開");
        assert!(matches!(&second.body, Body::Raw { full: true, bytes, .. } if bytes == b"world"), "{:?}", second.body);
        assert!(second.seq > first.seq, "seq 接著算");
    }

    /// observe 起不來：這顆 host 退回快照，畫面照樣推；之後冷卻期間也直接走快照。
    #[tokio::test]
    async fn an_observe_that_cannot_start_falls_back_to_snapshot_frames() {
        let env = crate::testing::env().await;
        let app = &env.app;
        snapshot_only(app);
        let pane = crate::api::shell::tracked_test_pane(app).await;
        env.herdr.set_screen(&pane.pane_id, "fallback");
        let (_k, mut rx, _wake) = watch(app, "local", &pane.pane_id, 80, 24);
        let f = next_frame(&mut rx, Duration::from_secs(3)).await.expect("快照要照樣來");
        assert!(matches!(&f.body, Body::Snapshot { text, .. } if text == "fallback"), "{:?}", f.body);
        assert_eq!(f.message()["type"], "frame");
        assert!(observe_is_cooling(app, "local"), "起不來要冷卻，之後不要每次都重試");
    }

    /// 回放：後來加入的觀看者先拿到完整幀，再拿到之後的差異（中途連上也畫得對）。
    #[tokio::test]
    async fn a_late_watcher_gets_the_full_frame_before_the_later_deltas() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let k = key(app, "local", "w-replay", 80, 24);
        // 直接用內部的發布路徑塞兩幀：一張完整、一張差異（不啟動子行程）。
        hub().lock().unwrap().insert(k.clone(), Entry::new());
        publish(&k, Body::Raw { full: true, cols: 80, rows: 24, bytes: b"A".to_vec() });
        publish(&k, Body::Raw { full: false, cols: 80, rows: 24, bytes: b"B".to_vec() });
        let j = join(&k);
        assert!(!j.spawn);
        let bytes: Vec<Vec<u8>> = j
            .replay
            .iter()
            .map(|f| match &f.body {
                Body::Raw { bytes, .. } => bytes.clone(),
                _ => vec![],
            })
            .collect();
        assert_eq!(bytes, vec![b"A".to_vec(), b"B".to_vec()], "完整幀在前，差異接在後");
        hub().lock().unwrap().remove(&k);
    }

    /// 回放沒有完整幀打頭的差異：丟掉（client 還沒有基準，畫了也是錯的）。
    #[tokio::test]
    async fn a_delta_before_any_full_frame_is_dropped() {
        let env = crate::testing::env().await;
        let app = &env.app;
        let k = key(app, "local", "w-delta", 80, 24);
        hub().lock().unwrap().insert(k.clone(), Entry::new());
        assert!(!publish(&k, Body::Raw { full: false, cols: 80, rows: 24, bytes: b"orphan".to_vec() }));
        assert!(replay(&k).is_empty());
        hub().lock().unwrap().remove(&k);
    }

    /// 完整真連線（WebSocket）：連上後 client 送 size，之後推 raw；錯誤參數回錯誤幀；快照退路照樣能用。
    #[tokio::test]
    async fn the_socket_sends_raw_frames_at_the_client_size_and_refuses_bad_params() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let env = crate::testing::env().await;
        let app = env.app.clone();
        app.set_startup_ready(true);
        purge_app(&app);
        app.herdr.set_observe_program(Some(fake_observe("socket", HELLO_THEN_HANG)));
        let pane = crate::api::shell::tracked_test_pane(&app).await;
        env.herdr.set_screen(&pane.pane_id, "unused");
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
        // client→server 的文字幀必須遮罩（RFC 6455）；這裡只送短訊息。
        async fn send_text(c: &mut tokio::net::TcpStream, text: &str) {
            let payload = text.as_bytes();
            assert!(payload.len() < 126);
            let mask = [0x12u8, 0x34, 0x56, 0x78];
            let mut f = vec![0x81, 0x80 | payload.len() as u8];
            f.extend_from_slice(&mask);
            f.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
            c.write_all(&f).await.unwrap();
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
        send_text(&mut c, r#"{"type":"size","cols":100,"rows":30}"#).await;
        let first = read_frame(&mut c).await;
        assert_eq!(first["type"], "raw");
        assert_eq!((first["cols"].as_u64(), first["rows"].as_u64()), (Some(100), Some(30)), "照 client 送的尺寸");
        assert_eq!(first["full"], true);
        assert_eq!(first["data"], "aGVsbG8=");

        // 文字版不是這條串流的格式：連線照開，給一張錯誤幀。
        let mut bad = open(addr, &format!("/api/hosts/local/shells/{}/stream?format=text&token=test-token", pane.pane_id)).await;
        assert_eq!(read_frame(&mut bad).await, error_message("bad_format"));
    }
}
