//! 網頁推播（Web Push，RFC 8030／8291／8292）：先用在排程終態通知。
//!
//! 自行實作而不用 `web-push` crate：該 crate 的加密後端（`ece`）預設走 OpenSSL，會把系統 libssl 拉進建置；
//! 這裡用純 Rust 的 RustCrypto（`p256`、`hkdf`、`aes-gcm`）加既有的 `reqwest`，約百來行。
//! * VAPID 私鑰第一次需要時產生，存 `{data_dir}/vapid.json`（0600）；不進 config.toml、不進 log，API 只給公鑰。
//! * 訂閱存 `{data_dir}/push-subscriptions.json`（0600，原子寫入）；同 endpoint 去重；推送端回 404／410 就刪。
//! * 送出一律背景、有逾時，失敗只記 warn，不影響排程。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use axum::extract::{Query, State};
use axum::{Extension, Json};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine as _;
use hkdf::Hkdf;
use p256::ecdh::EphemeralSecret;
use p256::ecdsa::{signature::Signer, Signature, SigningKey};
use p256::{EncodedPoint, PublicKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::Sha256;

use crate::api::RequestPrincipal;
use crate::lifecycle::LcError;
use crate::state::App;

const SEND_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_SUBSCRIPTIONS: usize = 50;
/// VAPID `sub` 的最後退路：Apple 會檢查 `sub` 是否為合法的 https／mailto，`.invalid` 網域會被拒（403 BadJwtToken）。
const FALLBACK_SUBJECT: &str = "mailto:noreply@example.com";
/// VAPID JWT 有效期：Apple 上限 24 小時，保守取 1 小時。
const VAPID_TTL_SECS: u64 = 3600;
/// 記進 log／回給網頁的推送服務拒絕原因長度上限。
const REASON_MAX: usize = 120;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    #[serde(default)]
    pub created_at: String,
    /// 訂閱當下網頁的 https origin（VAPID `sub` 的預設來源）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// 選 VAPID `sub`：config > 訂閱的 https origin > 內建 mailto。不合法的值（非 https／mailto、含 .invalid）一律略過。
pub(crate) fn pick_subject(configured: Option<&str>, origin: Option<&str>) -> String {
    let ok = |s: &str| {
        let s = s.trim();
        let valid = if let Some(rest) = s.strip_prefix("mailto:") {
            rest.contains('@') && !rest.contains(char::is_whitespace)
        } else {
            reqwest::Url::parse(s).map(|u| u.scheme() == "https" && u.host_str().is_some()).unwrap_or(false)
        };
        valid && !s.to_ascii_lowercase().contains(".invalid")
    };
    [configured, origin].into_iter().flatten().map(str::trim).find(|s| ok(s)).map(String::from).unwrap_or_else(|| FALLBACK_SUBJECT.to_string())
}

/// 把推送服務回應 body 濃縮成一段短原因（優先 JSON 的 `reason`）；不含 endpoint 或金鑰。
pub(crate) fn extract_reason(body: &str) -> String {
    let from_json = serde_json::from_str::<Value>(body).ok().and_then(|v| v.get("reason").and_then(|r| r.as_str()).map(String::from));
    let raw = from_json.unwrap_or_else(|| body.trim().to_string());
    raw.chars().filter(|c| !c.is_control()).take(REASON_MAX).collect()
}

fn user_only(principal: &RequestPrincipal) -> Result<(), LcError> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    Ok(())
}

// ---------- 檔案（0600、原子寫入） ----------

fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let res = (|| {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

fn vapid_path(dir: &Path) -> PathBuf {
    dir.join("vapid.json")
}
fn subs_path(dir: &Path) -> PathBuf {
    dir.join("push-subscriptions.json")
}

/// 同一時間只有一個人讀改寫這兩個檔。
static FILE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn load_or_create_key(dir: &Path) -> anyhow::Result<SigningKey> {
    let path = vapid_path(dir);
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Some(sk) = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v["private_key"].as_str().and_then(|s| B64.decode(s).ok()))
            .and_then(|b| SigningKey::from_slice(&b).ok())
        {
            return Ok(sk);
        }
    }
    let sk = SigningKey::random(&mut rand::rngs::OsRng);
    write_private(&path, json!({"private_key": B64.encode(sk.to_bytes())}).to_string().as_bytes())?;
    Ok(sk)
}

fn public_key_b64(sk: &SigningKey) -> String {
    B64.encode(sk.verifying_key().to_encoded_point(false).as_bytes())
}

fn load_subs(dir: &Path) -> Vec<Subscription> {
    std::fs::read_to_string(subs_path(dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_subs(dir: &Path, subs: &[Subscription]) -> std::io::Result<()> {
    write_private(&subs_path(dir), serde_json::to_string(subs)?.as_bytes())
}

// ---------- RFC 8291 加密＋RFC 8292 VAPID ----------

/// aes128gcm 單一 record 加密。`ua_public`＝訂閱的 p256dh（65 bytes）、`auth`＝16 bytes。
pub(crate) fn encrypt(ua_public: &[u8], auth: &[u8], plaintext: &[u8]) -> anyhow::Result<Vec<u8>> {
    let ua_pk = PublicKey::from_sec1_bytes(ua_public).map_err(|e| anyhow::anyhow!("bad p256dh: {e}"))?;
    let as_secret = EphemeralSecret::random(&mut rand::rngs::OsRng);
    let as_public = EncodedPoint::from(as_secret.public_key());
    let shared = as_secret.diffie_hellman(&ua_pk);
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes()).expand(&key_info, &mut ikm).map_err(|e| anyhow::anyhow!("hkdf: {e}"))?;
    let mut salt = [0u8; 16];
    rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut salt);
    let hk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
    hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).map_err(|e| anyhow::anyhow!("hkdf: {e}"))?;
    hk.expand(b"Content-Encoding: nonce\0", &mut nonce).map_err(|e| anyhow::anyhow!("hkdf: {e}"))?;
    let mut data = plaintext.to_vec();
    data.push(0x02); // 最後一個 record 的分隔符
    let ct = Aes128Gcm::new_from_slice(&cek)
        .map_err(|e| anyhow::anyhow!("aes key: {e}"))?
        .encrypt(Nonce::from_slice(&nonce), data.as_slice())
        .map_err(|e| anyhow::anyhow!("aes-gcm: {e}"))?;
    let mut body = Vec::with_capacity(86 + ct.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&4096u32.to_be_bytes());
    body.push(as_public.len() as u8);
    body.extend_from_slice(as_public.as_bytes());
    body.extend_from_slice(&ct);
    Ok(body)
}

fn vapid_header(sk: &SigningKey, endpoint: &str, subject: &str, now_secs: u64) -> anyhow::Result<String> {
    let url = reqwest::Url::parse(endpoint)?;
    let aud = format!("{}://{}", url.scheme(), url.host_str().ok_or_else(|| anyhow::anyhow!("endpoint has no host"))?);
    let aud = match url.port() {
        Some(p) => format!("{aud}:{p}"),
        None => aud,
    };
    let head = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
    let claims = B64.encode(json!({"aud": aud, "exp": now_secs + VAPID_TTL_SECS, "sub": subject}).to_string());
    let signing_input = format!("{head}.{claims}");
    let sig: Signature = sk.sign(signing_input.as_bytes());
    Ok(format!("vapid t={signing_input}.{}, k={}", B64.encode(sig.to_bytes()), public_key_b64(sk)))
}

// ---------- 送出 ----------

/// 結果：`Gone`＝訂閱失效（404／410），要刪；`Failed`＝暫時性失敗，保留。
#[derive(Debug, PartialEq)]
pub enum SendOutcome {
    Sent,
    Gone,
    Failed(String),
}

type SendFut = std::pin::Pin<Box<dyn std::future::Future<Output = SendOutcome> + Send>>;
/// 測試用的假 sender（正式環境永遠是 `None`，走真的 HTTP 推送）。
pub(crate) type SendFn = Arc<dyn Fn(Subscription, Vec<u8>) -> SendFut + Send + Sync>;
static SENDER_OVERRIDE: std::sync::Mutex<Option<SendFn>> = std::sync::Mutex::new(None);

#[cfg(test)]
pub(crate) fn set_test_sender(f: Option<SendFn>) {
    *SENDER_OVERRIDE.lock().unwrap() = f;
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

async fn send_http(client: &reqwest::Client, sk: &SigningKey, configured_subject: Option<&str>, sub: &Subscription, payload: &[u8]) -> SendOutcome {
    let build = || -> anyhow::Result<(String, Vec<u8>)> {
        let ua = B64.decode(&sub.p256dh)?;
        let auth = B64.decode(&sub.auth)?;
        Ok((vapid_header(sk, &sub.endpoint, &pick_subject(configured_subject, sub.origin.as_deref()), now_secs())?, encrypt(&ua, &auth, payload)?))
    };
    let (authz, body) = match build() {
        Ok(x) => x,
        Err(e) => return SendOutcome::Failed(format!("{e:#}")),
    };
    let res = client
        .post(&sub.endpoint)
        .header("Authorization", authz)
        .header("Content-Encoding", "aes128gcm")
        .header("Content-Type", "application/octet-stream")
        .header("TTL", "86400")
        .header("Urgency", "high")
        .body(body)
        .timeout(SEND_TIMEOUT)
        .send()
        .await;
    match res {
        Ok(r) if r.status().is_success() => SendOutcome::Sent,
        Ok(r) if matches!(r.status().as_u16(), 404 | 410) => SendOutcome::Gone,
        Ok(r) => {
            let status = r.status();
            let body = r.text().await.unwrap_or_default();
            let reason = extract_reason(&body);
            SendOutcome::Failed(if reason.is_empty() { format!("push service answered {status}") } else { format!("push service answered {status}: {reason}") })
        }
        Err(e) => SendOutcome::Failed(format!("{e}")),
    }
}

/// 對所有訂閱發同一則通知；404／410 的訂閱順手刪掉。回 `(sent, removed, failed)`。
pub async fn broadcast(dir: &Path, subject: Option<&str>, payload: &Value) -> (usize, usize, usize) {
    let r = broadcast_detail(dir, subject, payload).await;
    (r.0, r.1, r.2)
}

/// 同 `broadcast`，另回失敗原因（去重、已截短）。
pub async fn broadcast_detail(dir: &Path, subject: Option<&str>, payload: &Value) -> (usize, usize, usize, Vec<String>) {
    let subs = { let _g = FILE_LOCK.lock().await; load_subs(dir) };
    if subs.is_empty() {
        return (0, 0, 0, Vec::new());
    }
    let sk = match { let _g = FILE_LOCK.lock().await; load_or_create_key(dir) } {
        Ok(sk) => sk,
        Err(e) => {
            tracing::warn!(error = %e, "web push: vapid key unavailable");
            return (0, 0, subs.len(), vec!["vapid key unavailable".into()]);
        }
    };
    let body = payload.to_string().into_bytes();
    let over = SENDER_OVERRIDE.lock().unwrap().clone();
    let client = reqwest::Client::new();
    let subject = subject.map(String::from);
    let outcomes = futures::future::join_all(subs.iter().map(|sub| {
        let (over, client, sk, body, sub, subject) = (over.clone(), client.clone(), sk.clone(), body.clone(), sub.clone(), subject.clone());
        async move {
            match over {
                Some(f) => f(sub, body).await,
                None => send_http(&client, &sk, subject.as_deref(), &sub, &body).await,
            }
        }
    }))
    .await;
    let (mut sent, mut failed) = (0, 0);
    let mut gone = Vec::new();
    let mut reasons: Vec<String> = Vec::new();
    for (sub, out) in subs.iter().zip(outcomes) {
        match out {
            SendOutcome::Sent => sent += 1,
            SendOutcome::Gone => gone.push(sub.endpoint.clone()),
            SendOutcome::Failed(why) => {
                failed += 1;
                tracing::warn!(reason = %why, "web push send failed");
                if !reasons.contains(&why) {
                    reasons.push(why);
                }
            }
        }
    }
    if !gone.is_empty() {
        let _g = FILE_LOCK.lock().await;
        let mut cur = load_subs(dir);
        cur.retain(|s| !gone.contains(&s.endpoint));
        if let Err(e) = save_subs(dir, &cur) {
            tracing::warn!(error = %e, "web push: could not prune subscriptions");
        }
    }
    (sent, gone.len(), failed, reasons)
}

// ---------- 排程終態通知 ----------

/// 排程終態通知的內容：`S3 DON上班 ✅ 08:58 完成（47 秒）`。時間用排程的時區。
pub(crate) fn schedule_payload(code: &str, name: &str, outcome: &str, status: &str, finished: chrono::DateTime<chrono::Utc>, tz: &str, duration_ms: Option<i64>, project_id: &str, schedule_id: &str, occurrence_id: &str) -> Value {
    let label = match (status, outcome) {
        ("unknown", _) | (_, "unknown") => "⚠ 結果不明",
        (_, "refused") => "⚠ 拒絕執行",
        ("failed", _) | (_, "failed") => "❌ 失敗",
        _ => "✅",
    };
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::UTC);
    let hhmm = finished.with_timezone(&tz).format("%H:%M");
    let verb = if label == "✅" { "完成" } else { "結束" };
    let dur = duration_ms.filter(|d| *d >= 0).map(|d| format!("（{}）", fmt_duration(d))).unwrap_or_default();
    let title = [code, name].iter().filter(|p| !p.is_empty()).copied().collect::<Vec<_>>().join(" ");
    json!({
        "title": title,
        "body": format!("{label} {hhmm} {verb}{dur}"),
        "tag": format!("schedule:{occurrence_id}"),
        "url": format!("/?schedule={schedule_id}&project={project_id}&occurrence={occurrence_id}"),
    })
}

fn fmt_duration(ms: i64) -> String {
    let s = (ms + 500) / 1000;
    if s < 60 {
        format!("{s} 秒")
    } else if s < 3600 {
        format!("{} 分 {} 秒", s / 60, s % 60)
    } else {
        format!("{} 小時 {} 分", s / 3600, (s % 3600) / 60)
    }
}

/// 在送 `schedule_notice` 的同一處呼叫：背景送出，失敗不影響排程。
pub(crate) fn spawn_schedule_push(app: &Arc<App>, t: &crate::schedules::store::Transition) {
    let app = app.clone();
    let (code, name, outcome, status, pid, sid, oid) = (
        t.schedule_code.clone(),
        t.schedule_name.clone(),
        t.outcome.clone(),
        t.status.clone(),
        t.project_id.clone(),
        t.schedule_id.clone(),
        t.occurrence_id.clone(),
    );
    tokio::spawn(async move {
        if load_subs(&app.data_dir).is_empty() {
            return;
        }
        let subject = app.cfg.get().await.server.push_subject.clone();
        let tz: String = sqlx::query_scalar("SELECT tz FROM schedules WHERE id = ?").bind(&sid).fetch_optional(&app.db).await.ok().flatten().unwrap_or_default();
        let started: Option<String> = sqlx::query_scalar("SELECT first_started_at FROM schedule_occurrences WHERE id = ?").bind(&oid).fetch_optional(&app.db).await.ok().flatten().flatten();
        let now = chrono::Utc::now();
        let dur = started.and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok()).map(|s| (now - s.with_timezone(&chrono::Utc)).num_milliseconds());
        let payload = schedule_payload(&code, &name, &outcome, &status, now, &tz, dur, &pid, &sid, &oid);
        let _ = tokio::time::timeout(Duration::from_secs(60), broadcast(&app.data_dir, subject.as_deref(), &payload)).await;
    });
}

// ---------- 主力 bot 推播（回合做完／需要回應） ----------

/// 同 bot 同類通知的最短間隔：30 秒內多次合併（只送第一則）。
const MAIN_THROTTLE: Duration = Duration::from_secs(30);
/// blocked 要等畫面判斷（假 blocked 會被改回）才送。
const BLOCKED_SETTLE: Duration = Duration::from_secs(3);
/// presence 超過這麼久沒更新就視為失效（頁面關了或被凍結），照推。
const PRESENCE_STALE: Duration = Duration::from_secs(90);
/// 「正在看」要求最近這麼久內有互動。
const PRESENCE_ACTIVE_SECS: u64 = 60;
const BODY_MAX: usize = 80;

fn settings_path(dir: &Path) -> PathBuf {
    dir.join("push-settings.json")
}

/// 推播開關（`main_bots`／`updates`／`reset_credits`）；沒有檔案、沒有這個鍵或讀不懂＝開（預設開）。
pub(crate) fn setting_enabled(dir: &Path, key: &str) -> bool {
    std::fs::read_to_string(settings_path(dir)).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v[key].as_bool()).unwrap_or(true)
}

pub(crate) fn main_bots_enabled(dir: &Path) -> bool {
    setting_enabled(dir, "main_bots")
}

fn settings_json(dir: &Path) -> Value {
    json!({"main_bots": setting_enabled(dir, "main_bots"), "updates": setting_enabled(dir, "updates"), "reset_credits": setting_enabled(dir, "reset_credits")})
}

#[derive(Clone, Debug)]
struct Presence {
    bot_id: Option<String>,
    visible: bool,
    /// 回報當下距離上次互動幾秒。
    idle_secs: u64,
    received: std::time::Instant,
}

static PRESENCE: std::sync::Mutex<Option<std::collections::HashMap<String, Presence>>> = std::sync::Mutex::new(None);

fn record_presence(client: &str, p: Presence) {
    let mut g = PRESENCE.lock().unwrap_or_else(|e| e.into_inner());
    let map = g.get_or_insert_with(Default::default);
    map.retain(|_, v| v.received.elapsed() < PRESENCE_STALE * 4);
    if map.len() < 64 || map.contains_key(client) {
        map.insert(client.to_string(), p);
    }
}

/// 有任何一個網頁正看著這顆 bot（可見、選中、最近 60 秒有互動、回報沒過期）。
fn user_is_watching(bot_id: &str) -> bool {
    let g = PRESENCE.lock().unwrap_or_else(|e| e.into_inner());
    g.as_ref().is_some_and(|m| {
        m.values().any(|p| {
            p.visible && p.bot_id.as_deref() == Some(bot_id) && p.received.elapsed() < PRESENCE_STALE && p.idle_secs + p.received.elapsed().as_secs() <= PRESENCE_ACTIVE_SECS
        })
    })
}

#[cfg(test)]
fn clear_presence() {
    *PRESENCE.lock().unwrap() = None;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum MainKind {
    Done,
    Blocked,
}

/// 去重與節流的帳：同一個 key（回合 id／blocked 段落）只推一次，同 bot 同類 30 秒內只推一次。
#[derive(Default)]
pub(crate) struct MainNotifier {
    last_key: std::sync::Mutex<std::collections::HashMap<(String, MainKind), String>>,
    last_sent: std::sync::Mutex<std::collections::HashMap<(String, MainKind), std::time::Instant>>,
    blocked_open: std::sync::Mutex<std::collections::HashSet<String>>,
    episode: std::sync::atomic::AtomicU64,
    /// 已推過的更新通知 key（同版本只推一次）。
    updates_seen: std::sync::Mutex<std::collections::HashSet<String>>,
    /// 測試把它設 0；正式環境用 `BLOCKED_SETTLE`。
    settle: std::sync::Mutex<Option<Duration>>,
}

impl MainNotifier {
    /// 該不該送；順便把 key 記成「已處理」（被節流或被 presence 擋掉的也不補送）。
    fn admit(&self, bot_id: &str, kind: MainKind, key: &str, watching: bool) -> bool {
        let id = (bot_id.to_string(), kind);
        {
            let mut keys = self.last_key.lock().unwrap_or_else(|e| e.into_inner());
            if keys.get(&id).is_some_and(|k| k == key) {
                return false;
            }
            keys.insert(id.clone(), key.to_string());
        }
        if watching {
            return false;
        }
        let mut sent = self.last_sent.lock().unwrap_or_else(|e| e.into_inner());
        if sent.get(&id).is_some_and(|t| t.elapsed() < MAIN_THROTTLE) {
            return false;
        }
        sent.insert(id, std::time::Instant::now());
        true
    }
}

/// 第一個非空行，截 80 字（以字元計）。
pub(crate) fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let mut out: String = line.chars().filter(|c| !c.is_control()).take(max).collect();
    if line.chars().count() > max {
        out.push('…');
    }
    out
}

fn main_payload(bot_id: &str, bot_name: &str, kind: MainKind, line: &str) -> Value {
    let body = match kind {
        MainKind::Done => if line.is_empty() { "回覆完成".to_string() } else { line.to_string() },
        MainKind::Blocked => if line.is_empty() { "需要你回應".to_string() } else { format!("需要你回應：{line}") },
    };
    let k = if kind == MainKind::Done { "done" } else { "blocked" };
    json!({"title": bot_name, "body": body, "tag": format!("bot:{bot_id}:{k}"), "url": format!("/bots/{bot_id}")})
}

async fn send_main(app: &Arc<App>, n: &MainNotifier, bot: &crate::db::Bot, kind: MainKind, key: &str, line: impl std::future::Future<Output = String>) {
    // 先去重／節流／presence，再等譯文或摘要：等的時候不會重複推，也不會因為等而漏推。
    if !n.admit(&bot.id, kind, key, user_is_watching(&bot.id)) {
        return;
    }
    let payload = main_payload(&bot.id, &bot.name, kind, &line.await);
    let subject = app.cfg.get().await.server.push_subject.clone();
    let _ = tokio::time::timeout(Duration::from_secs(60), broadcast(&app.data_dir, subject.as_deref(), &payload)).await;
}

/// 等譯文、等摘要各自的上限（毫秒）；測試會縮短。
static LINE_WAIT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(20_000);

fn line_wait() -> Duration {
    Duration::from_millis(LINE_WAIT_MS.load(Ordering::Relaxed))
}

/// 推播內文（使用者 2026-11-10）：會被系統自動翻譯的（英文、自動翻譯開著）等譯文（上限 `line_wait`，逾時用原文）；
/// 之後一律是一句短的話：單行且 ≤ 40 字直接用，否則用同一個 claude_api 通道摘要成一句，失敗或功能關閉退回第一行截斷。
/// `id`：譯文快取的鍵（回覆用訊息 id，與網頁共用快取）。
async fn push_line(app: &Arc<App>, id: &str, text: &str) -> String {
    use crate::message_translate as mt;
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    let (master, state) = {
        let cfg = app.cfg.get().await;
        (cfg.claude_api.enabled, cfg.claude_use_state("message_translate"))
    };
    let on = master && state.as_ref().is_some_and(|s| s.enabled);
    let auto = on && state.as_ref().is_some_and(|s| s.auto == Some(true));
    let mut work = text.to_string();
    if auto && mt::is_mostly_english(text) && text.chars().count() <= mt::MAX_CHARS {
        if let Ok(Ok(v)) = tokio::time::timeout(line_wait(), mt::translate(app, id, text)).await {
            if let Some(t) = v["translation"].as_str().map(str::trim).filter(|t| !t.is_empty()) {
                work = t.to_string();
            }
        }
    }
    let nonblank = work.lines().filter(|l| !l.trim().is_empty()).count();
    if nonblank <= 1 && work.chars().count() <= mt::SUMMARY_MAX_CHARS {
        return first_line(&work, BODY_MAX);
    }
    if on {
        if let Ok(Ok(s)) = tokio::time::timeout(line_wait(), mt::summarize(app, &work)).await {
            return first_line(&s, BODY_MAX);
        }
    }
    first_line(&work, BODY_MAX)
}

async fn primary_bot(app: &Arc<App>, bot_id: &str) -> Option<crate::db::Bot> {
    crate::db::bot(&app.db, bot_id).await.ok().flatten().filter(|b| b.is_primary == 1)
}

async fn on_turn_done(app: &Arc<App>, n: &MainNotifier, bot_id: &str, turn_id: &str) {
    if !main_bots_enabled(&app.data_dir) || load_subs(&app.data_dir).is_empty() {
        return;
    }
    let Some(bot) = primary_bot(app, bot_id).await else { return };
    let reply: Option<(String, String)> = sqlx::query_as("SELECT id, content FROM messages WHERE turn_id = ? AND role = 'assistant' ORDER BY rowid DESC LIMIT 1")
        .bind(turn_id)
        .fetch_optional(&app.db)
        .await
        .ok()
        .flatten();
    let (mid, text) = reply.unwrap_or_default();
    send_main(app, n, &bot, MainKind::Done, turn_id, async { push_line(app, &mid, &text).await }).await;
}

async fn blocked_line(app: &Arc<App>, bot_id: &str, run_id: &str) -> String {
    use axum::extract::Path;
    let mut text = String::new();
    if let Ok(Json(v)) = crate::runners::pending_question::get_pending_question(State(app.clone()), Path(bot_id.to_string())).await {
        if let Some(q) = v["questions"][0]["question"].as_str() {
            text = q.to_string();
        }
    }
    if text.trim().is_empty() {
        text = crate::blocked_reason::of(run_id).map(|r| r.text).unwrap_or_default();
    }
    push_line(app, "push-blocked", &text).await
}

async fn on_blocked(app: Arc<App>, n: Arc<MainNotifier>, bot_id: String, episode: u64) {
    let settle = n.settle.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or(BLOCKED_SETTLE);
    tokio::time::sleep(settle).await;
    if !main_bots_enabled(&app.data_dir) || load_subs(&app.data_dir).is_empty() {
        return;
    }
    let Some(bot) = primary_bot(&app, &bot_id).await else { return };
    let Some(run) = crate::db::active_run(&app.db, &bot_id).await.ok().flatten() else { return };
    if run.agent_status != "blocked" {
        return;
    }
    send_main(&app, &n, &bot, MainKind::Blocked, &format!("{}:{episode}", run.id), blocked_line(&app, &bot_id, &run.id)).await;
}

pub(crate) async fn handle_event(app: &Arc<App>, n: &Arc<MainNotifier>, ev: &crate::state::WsEvent) {
    match ev.kind.as_str() {
        "turn_updated" => {
            let status = ev.data["turn"]["status"].as_str().unwrap_or("");
            if !matches!(status, "completed" | "completed_fallback") {
                return;
            }
            if let (Some(bot_id), Some(turn_id)) = (ev.data["bot_id"].as_str(), ev.data["turn"]["id"].as_str()) {
                on_turn_done(app, n, bot_id, turn_id).await;
            }
        }
        "bot_status" => {
            let Some(bot_id) = ev.data["bot_id"].as_str() else { return };
            if ev.data["run"]["agent_status"].as_str() == Some("blocked") {
                let first = n.blocked_open.lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string());
                if first {
                    let episode = n.episode.fetch_add(1, Ordering::SeqCst) + 1;
                    tokio::spawn(on_blocked(app.clone(), n.clone(), bot_id.to_string(), episode));
                }
            } else {
                n.blocked_open.lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id);
            }
        }
        "upstream_update" | "cli_update_done" => {
            if !setting_enabled(&app.data_dir, "updates") {
                return;
            }
            if let Some((key, payload)) = update_notice(&ev.kind, &ev.data) {
                if n.updates_seen.lock().unwrap_or_else(|e| e.into_inner()).insert(key) {
                    send_plain(app, payload).await;
                }
            }
        }
        _ => {}
    }
}

/// 啟動時呼叫一次：訂閱事件流，主力 bot 回合做完／需要回應就推播。
pub fn spawn_main_notifier(app: Arc<App>) {
    let mut rx = app.subscribe();
    let n = Arc::new(MainNotifier::default());
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                // 回合完成要等譯文／摘要（最久數十秒）：另開工作做，不卡住事件流。去重在 send_main 一開始就記帳。
                Ok(ev) if ev.kind == "turn_updated" => {
                    let (app, n) = (app.clone(), n.clone());
                    tokio::spawn(async move { handle_event(&app, &n, &ev).await });
                }
                Ok(ev) => handle_event(&app, &n, &ev).await,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
}


// ---------- 更新與重置券推播 ----------

/// 不是 bot 的通用通知：標題固定 AG Man 以外的短標題＋內文。
fn plain_payload(title: &str, body: &str, tag: &str, url: &str) -> Value {
    json!({"title": title, "body": body, "tag": tag, "url": url})
}

async fn send_plain(app: &Arc<App>, payload: Value) {
    if load_subs(&app.data_dir).is_empty() {
        return;
    }
    let subject = app.cfg.get().await.server.push_subject.clone();
    let _ = tokio::time::timeout(Duration::from_secs(60), broadcast(&app.data_dir, subject.as_deref(), &payload)).await;
}

fn kind_label(kind: &str) -> String {
    match kind {
        "herdr" => "herdr".to_string(),
        k => format!("{k} CLI"),
    }
}

/// 更新類事件 → 通知內容（沒有要通知就 `None`）。key 用來同版本只推一次。
pub(crate) fn update_notice(kind: &str, data: &Value) -> Option<(String, Value)> {
    match kind {
        // 上游偵測到新版：`notify == "update"` 本來就是「每個新版本一次」。
        "upstream_update" if data["notify"] == "update" => {
            let k = data["kind"].as_str()?;
            let v = data["latest_version"].as_str()?;
            Some((format!("upstream:{k}:{v}"), plain_payload("有新版", &format!("{} 有新版 {v}", kind_label(k)), &format!("update:{k}"), "/")))
        }
        // 網頁上「已安裝」的結果：成功才通知。
        "cli_update_done" if data["ok"] == true => {
            let k = data["kind"].as_str()?;
            let v = data["target_version"].as_str()?;
            let h = data["host"].as_str().unwrap_or("local");
            Some((format!("installed:{h}:{k}:{v}"), plain_payload("已更新", &format!("{} 已更新到 {v}", kind_label(k)), &format!("updated:{k}"), "/")))
        }
        _ => None,
    }
}

/// daemon 換版：記下上次啟動的 build，不同就是剛換版（第一次沒有紀錄不推）。
pub(crate) fn build_changed(dir: &Path, sha: &str) -> bool {
    let path = dir.join("push-last-build");
    let prev = std::fs::read_to_string(&path).ok().map(|t| t.trim().to_string());
    if prev.as_deref() != Some(sha) {
        let _ = write_private(&path, sha.as_bytes());
    }
    prev.is_some_and(|p| !p.is_empty() && p != sha)
}

/// 額度格是不是用完了（同前端 `exhaustionMarker`）：回事件標記，同一次耗盡不變。
pub(crate) fn exhaustion_marker(cell: &Value, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    let later = |t: &Value| t.as_str().and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).is_some_and(|d| d > now);
    let hit = &cell["limit_hit"];
    if hit.is_object() && (hit["until"].is_null() || later(&hit["until"])) {
        return Some(format!("hit:{}", hit["at"].as_str().unwrap_or("")));
    }
    for name in ["five_hour", "seven_day"] {
        let w = &cell[name];
        let used_up = w["used_pct"].as_f64().is_some_and(|p| p > 99.9);
        if used_up && later(&w["resets_at"]) {
            return Some(format!("win:{name}:{}", w["resets_at"].as_str().unwrap_or("")));
        }
    }
    None
}

#[derive(Debug, PartialEq)]
pub(crate) struct ResetOffer {
    pub id: String,
    pub key: String,
    pub available: i64,
    pub held: Vec<String>,
}

/// 額度快照（`/api/quota` 的 `kinds`）→ 目前用完且有券的 codex 帳號。
pub(crate) fn reset_offers(snap: &Value, now: chrono::DateTime<chrono::Utc>) -> Vec<ResetOffer> {
    let mut out = Vec::new();
    let Some(kinds) = snap["kinds"].as_object() else { return out };
    for (key, cell) in kinds {
        let kind = key.rsplit('/').next().unwrap_or("").split(':').next().unwrap_or("");
        let available = cell["reset_credits"]["available"].as_i64().unwrap_or(0);
        if kind != "codex" || cell["stale"] == true || available <= 0 {
            continue;
        }
        let Some(marker) = exhaustion_marker(cell, now) else { continue };
        let held = cell["held_bots"].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
        out.push(ResetOffer { id: format!("{key}|{marker}"), key: key.clone(), available, held });
    }
    out
}

fn reset_payload(o: &ResetOffer) -> Value {
    let held = if o.held.is_empty() { String::new() } else { format!("（影響：{}）", o.held.join("、")) };
    let key_enc: String = o.key.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect();
    plain_payload("額度用完", &format!("codex 額度用完，有 {} 張重置券可用{held}", o.available), &format!("reset:{}", o.key), &format!("/?reset_offer={key_enc}"))
}

/// 已通知過的耗盡事件（持久化，7 天後清掉）。只通知，絕不兌換。
fn load_reset_seen(dir: &Path) -> std::collections::HashMap<String, i64> {
    std::fs::read_to_string(dir.join("push-reset-seen.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub(crate) async fn scan_reset_credits(app: &Arc<App>, snap: &Value, now: chrono::DateTime<chrono::Utc>) -> usize {
    if !setting_enabled(&app.data_dir, "reset_credits") {
        return 0;
    }
    let mut seen = load_reset_seen(&app.data_dir);
    let ts = now.timestamp();
    seen.retain(|_, at| ts - *at < 7 * 86400);
    let fresh: Vec<ResetOffer> = reset_offers(snap, now).into_iter().filter(|o| !seen.contains_key(&o.id)).collect();
    if fresh.is_empty() {
        return 0;
    }
    for o in &fresh {
        seen.insert(o.id.clone(), ts);
    }
    let _ = write_private(&app.data_dir.join("push-reset-seen.json"), serde_json::to_string(&seen).unwrap_or_default().as_bytes());
    for o in &fresh {
        send_plain(app, reset_payload(o)).await;
    }
    fresh.len()
}

/// 啟動時呼叫：每分鐘看一次額度，用完且有重置券就推一次；另在 daemon 換版後推一次。
pub fn spawn_extra_notifiers(app: Arc<App>) {
    if setting_enabled(&app.data_dir, "updates") && build_changed(&app.data_dir, crate::build_info::BUILD_SHA) {
        let app2 = app.clone();
        tokio::spawn(async move {
            // 等訂閱與事件流起來再送。
            tokio::time::sleep(Duration::from_secs(10)).await;
            send_plain(&app2, plain_payload("已更新", &format!("AG Man 已更新到 {}", crate::build_info::BUILD_SHA), "updated:agm", "/")).await;
        });
    } else {
        let _ = build_changed(&app.data_dir, crate::build_info::BUILD_SHA);
    }
    crate::background_loop::spawn_periodic(&app, "reset credit push", Duration::from_secs(60), Duration::from_secs(30), |app| async move {
        let mut snap = crate::quota::snapshot(&app).await;
        crate::runners::reset_credit::annotate_held_bots(&app, &mut snap).await;
        scan_reset_credits(&app, &snap, chrono::Utc::now()).await;
    });
}

// ---------- API ----------

pub(crate) async fn get_vapid_key(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    let _g = FILE_LOCK.lock().await;
    let sk = load_or_create_key(&app.data_dir).map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    Ok(Json(json!({"public_key": public_key_b64(&sk)})))
}

pub(crate) async fn list(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    let _g = FILE_LOCK.lock().await;
    // 不回 keys：只給 endpoint（網頁用來判斷「這個瀏覽器是否已訂閱」）與建立時間。
    let subs: Vec<Value> = load_subs(&app.data_dir).into_iter().map(|s| json!({"endpoint": s.endpoint, "created_at": s.created_at})).collect();
    Ok(Json(json!({"subscriptions": subs})))
}

#[derive(Deserialize)]
pub(crate) struct SubKeys {
    p256dh: String,
    auth: String,
}
#[derive(Deserialize)]
pub(crate) struct SubInput {
    endpoint: String,
    keys: SubKeys,
    /// 訂閱當下網頁的 origin（`location.origin`），選填。
    #[serde(default)]
    origin: Option<String>,
}

fn bad(reason: &str) -> LcError {
    LcError::BadValue(json!({"error": "bad_request", "reason": reason}))
}

pub(crate) async fn subscribe(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>, Json(input): Json<SubInput>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    let url = reqwest::Url::parse(&input.endpoint).map_err(|_| bad("invalid_endpoint"))?;
    if url.scheme() != "https" || url.host_str().is_none() || input.endpoint.len() > 2048 {
        return Err(bad("invalid_endpoint"));
    }
    let ua = B64.decode(input.keys.p256dh.trim_end_matches('=')).map_err(|_| bad("invalid_p256dh"))?;
    if PublicKey::from_sec1_bytes(&ua).is_err() {
        return Err(bad("invalid_p256dh"));
    }
    if B64.decode(input.keys.auth.trim_end_matches('=')).map(|a| a.len() != 16).unwrap_or(true) {
        return Err(bad("invalid_auth"));
    }
    let origin = input.origin.as_deref().and_then(|o| reqwest::Url::parse(o).ok()).filter(|u| u.scheme() == "https" && u.host_str().is_some()).map(|u| u.origin().ascii_serialization());
    let _g = FILE_LOCK.lock().await;
    let mut subs = load_subs(&app.data_dir);
    let sub = Subscription { endpoint: input.endpoint.clone(), p256dh: input.keys.p256dh.trim_end_matches('=').into(), auth: input.keys.auth.trim_end_matches('=').into(), created_at: crate::db::now(), origin };
    let existed = subs.iter().any(|s| s.endpoint == sub.endpoint);
    if let Some(slot) = subs.iter_mut().find(|s| s.endpoint == sub.endpoint) {
        *slot = sub; // 同 endpoint 去重：更新 keys
    } else {
        if subs.len() >= MAX_SUBSCRIPTIONS {
            return Err(bad("too_many_subscriptions"));
        }
        subs.push(sub);
    }
    save_subs(&app.data_dir, &subs).map_err(|e| LcError::Upstream(format!("{e}")))?;
    Ok(Json(json!({"ok": true, "created": !existed, "count": subs.len()})))
}

#[derive(Deserialize)]
pub(crate) struct EndpointQuery {
    endpoint: String,
}

pub(crate) async fn unsubscribe(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>, Query(q): Query<EndpointQuery>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    let _g = FILE_LOCK.lock().await;
    let mut subs = load_subs(&app.data_dir);
    let before = subs.len();
    subs.retain(|s| s.endpoint != q.endpoint);
    if subs.len() != before {
        save_subs(&app.data_dir, &subs).map_err(|e| LcError::Upstream(format!("{e}")))?;
    }
    Ok(Json(json!({"ok": true, "removed": before - subs.len()})))
}

pub(crate) async fn test_push(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    let payload = json!({"title": "AG Man", "body": "測試通知：推播正常 ✅", "tag": "test", "url": "/"});
    let subject = app.cfg.get().await.server.push_subject.clone();
    let (sent, removed, failed, reasons) = broadcast_detail(&app.data_dir, subject.as_deref(), &payload).await;
    Ok(Json(json!({"sent": sent, "removed": removed, "failed": failed, "reasons": reasons})))
}

pub(crate) async fn get_settings(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    Ok(Json(settings_json(&app.data_dir)))
}

#[derive(Deserialize)]
pub(crate) struct SettingsInput {
    #[serde(default)]
    main_bots: Option<bool>,
    #[serde(default)]
    updates: Option<bool>,
    #[serde(default)]
    reset_credits: Option<bool>,
}

pub(crate) async fn put_settings(State(app): State<Arc<App>>, Extension(p): Extension<RequestPrincipal>, Json(input): Json<SettingsInput>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    let _g = FILE_LOCK.lock().await;
    let mut cur = settings_json(&app.data_dir);
    for (k, v) in [("main_bots", input.main_bots), ("updates", input.updates), ("reset_credits", input.reset_credits)] {
        if let Some(v) = v {
            cur[k] = json!(v);
        }
    }
    write_private(&settings_path(&app.data_dir), cur.to_string().as_bytes()).map_err(|e| LcError::Upstream(format!("{e}")))?;
    Ok(Json(cur))
}

#[derive(Deserialize)]
pub(crate) struct PresenceInput {
    client_id: String,
    #[serde(default)]
    bot_id: Option<String>,
    #[serde(default)]
    visible: bool,
    #[serde(default)]
    idle_secs: u64,
}

/// 網頁回報「我正看著哪顆 bot」：只存記憶體，供推播抑制用。
pub(crate) async fn post_presence(Extension(p): Extension<RequestPrincipal>, Json(input): Json<PresenceInput>) -> Result<Json<Value>, LcError> {
    user_only(&p)?;
    if input.client_id.is_empty() || input.client_id.len() > 64 {
        return Err(bad("invalid_client_id"));
    }
    record_presence(&input.client_id, Presence { bot_id: input.bot_id, visible: input.visible, idle_secs: input.idle_secs, received: std::time::Instant::now() });
    Ok(Json(json!({"ok": true})))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 瀏覽器端的解密（測試用）：驗證 `encrypt` 真的符合 RFC 8291。
    fn decrypt(ua_secret: &p256::SecretKey, auth: &[u8], body: &[u8]) -> Vec<u8> {
        let salt = &body[..16];
        let idlen = body[20] as usize;
        let as_public = &body[21..21 + idlen];
        let ct = &body[21 + idlen..];
        let ua_public = EncodedPoint::from(ua_secret.public_key());
        let shared = p256::ecdh::diffie_hellman(ua_secret.to_nonzero_scalar(), PublicKey::from_sec1_bytes(as_public).unwrap().as_affine());
        let mut key_info = b"WebPush: info\0".to_vec();
        key_info.extend_from_slice(ua_public.as_bytes());
        key_info.extend_from_slice(as_public);
        let mut ikm = [0u8; 32];
        Hkdf::<Sha256>::new(Some(auth), shared.raw_secret_bytes()).expand(&key_info, &mut ikm).unwrap();
        let hk = Hkdf::<Sha256>::new(Some(salt), &ikm);
        let (mut cek, mut nonce) = ([0u8; 16], [0u8; 12]);
        hk.expand(b"Content-Encoding: aes128gcm\0", &mut cek).unwrap();
        hk.expand(b"Content-Encoding: nonce\0", &mut nonce).unwrap();
        let mut pt = Aes128Gcm::new_from_slice(&cek).unwrap().decrypt(Nonce::from_slice(&nonce), ct).unwrap();
        assert_eq!(pt.pop(), Some(0x02), "padding delimiter");
        pt
    }

    fn fresh_sub(endpoint: &str) -> (Subscription, p256::SecretKey, [u8; 16]) {
        let sk = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let mut auth = [0u8; 16];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut auth);
        let pubkey = EncodedPoint::from(sk.public_key());
        (Subscription { endpoint: endpoint.into(), p256dh: B64.encode(pubkey.as_bytes()), auth: B64.encode(auth), created_at: "t".into(), origin: None }, sk, auth)
    }

    #[test]
    fn encryption_roundtrips_and_vapid_header_verifies() {
        let (sub, ua_sk, auth) = fresh_sub("https://push.example/x");
        let body = encrypt(&B64.decode(&sub.p256dh).unwrap(), &auth, "S3 DON上班 ✅".as_bytes()).unwrap();
        assert_eq!(decrypt(&ua_sk, &auth, &body), "S3 DON上班 ✅".as_bytes());
        let dir = crate::testing::scratch_dir("am-webpush-vapid");
        let sk = load_or_create_key(&dir).unwrap();
        let h = vapid_header(&sk, "https://push.example:8443/x/y", "https://agm.example.ts.net", 1_000).unwrap();
        let (t, k) = h.strip_prefix("vapid t=").unwrap().split_once(", k=").unwrap();
        assert_eq!(k, public_key_b64(&sk));
        let parts: Vec<&str> = t.split('.').collect();
        let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example:8443");
        assert_eq!(claims["exp"], 1_000 + 3600);
        assert_eq!(claims["sub"], "https://agm.example.ts.net");
        assert_eq!(parts[0], B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#));
        assert_eq!(B64.decode(parts[2]).unwrap().len(), 64, "raw r||s");
        assert_eq!(B64.decode(k).unwrap().len(), 65);
        assert_eq!(B64.decode(k).unwrap()[0], 0x04);
        assert!(!k.contains('=') && !parts[2].contains('='));
        use p256::ecdsa::signature::Verifier as _;
        let sig = Signature::from_slice(&B64.decode(parts[2]).unwrap()).unwrap();
        sk.verifying_key().verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).unwrap();
        // 私鑰檔 0600，重讀得到同一把。
        assert_eq!(std::fs::metadata(vapid_path(&dir)).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(public_key_b64(&load_or_create_key(&dir).unwrap()), k);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn apple_aud_and_subject_and_reason() {
        let sk = SigningKey::random(&mut rand::rngs::OsRng);
        let h = vapid_header(&sk, "https://web.push.apple.com/QOabc", FALLBACK_SUBJECT, 5).unwrap();
        let claims: Value = serde_json::from_slice(&B64.decode(h.strip_prefix("vapid t=").unwrap().split('.').nth(1).unwrap()).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://web.push.apple.com");
        assert_eq!(claims["sub"], "mailto:noreply@example.com");
        assert!(claims["exp"].as_u64().unwrap() - 5 <= 3600);
        assert_eq!(pick_subject(Some("https://cfg.example"), Some("https://o.example")), "https://cfg.example");
        assert_eq!(pick_subject(None, Some("https://agm.example.ts.net")), "https://agm.example.ts.net");
        assert_eq!(pick_subject(Some("mailto:noreply@agents-manager.invalid"), Some("http://insecure")), FALLBACK_SUBJECT);
        assert_eq!(pick_subject(None, None), FALLBACK_SUBJECT);
        assert_eq!(extract_reason(r#"{"reason":"BadJwtToken"}"#), "BadJwtToken");
        assert_eq!(extract_reason("plain\nbody"), "plainbody");
        assert!(extract_reason(&"x".repeat(500)).len() <= REASON_MAX);
    }

    #[test]
    fn payload_text_and_url() {
        let at = chrono::DateTime::parse_from_rfc3339("2026-11-09T00:58:00Z").unwrap().with_timezone(&chrono::Utc);
        let p = schedule_payload("S3", "DON上班", "success", "done", at, "Asia/Taipei", Some(47_000), "p1", "sch1", "occ1");
        assert_eq!(p["title"], "S3 DON上班");
        assert_eq!(p["body"], "✅ 08:58 完成（47 秒）");
        assert_eq!(p["url"], "/?schedule=sch1&project=p1&occurrence=occ1");
        assert!(schedule_payload("S3", "x", "failed", "done", at, "UTC", None, "p", "s", "o")["body"].as_str().unwrap().starts_with("❌ 失敗 00:58"));
        assert!(schedule_payload("S3", "x", "unknown", "unknown", at, "UTC", None, "p", "s", "o")["body"].as_str().unwrap().contains("結果不明"));
        assert!(schedule_payload("S3", "x", "refused", "done", at, "UTC", None, "p", "s", "o")["body"].as_str().unwrap().contains("拒絕"));
    }

    #[tokio::test]
    async fn subscription_crud_dedup_and_permissions() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let user = || Extension(RequestPrincipal::User);
        let (sub, _, _) = fresh_sub("https://push.example/a");
        let input = || SubInput { endpoint: sub.endpoint.clone(), keys: SubKeys { p256dh: sub.p256dh.clone(), auth: sub.auth.clone() }, origin: None };
        let r = subscribe(State(app.clone()), user(), Json(input())).await.unwrap().0;
        assert_eq!(r["created"], true);
        let r = subscribe(State(app.clone()), user(), Json(input())).await.unwrap().0;
        assert_eq!(r["created"], false, "同 endpoint 去重");
        assert_eq!(r["count"], 1);
        let listed = list(State(app.clone()), user()).await.unwrap().0;
        assert_eq!(listed["subscriptions"].as_array().unwrap().len(), 1);
        assert!(listed.to_string().find("p256dh").is_none(), "列表不回 keys");
        assert_eq!(std::fs::metadata(subs_path(&app.data_dir)).unwrap().permissions().mode() & 0o777, 0o600);
        let bad_ep = SubInput { endpoint: "http://insecure/x".into(), keys: SubKeys { p256dh: sub.p256dh.clone(), auth: sub.auth.clone() }, origin: None };
        assert!(matches!(subscribe(State(app.clone()), user(), Json(bad_ep)).await, Err(LcError::BadValue(_))));
        let bad_key = SubInput { endpoint: sub.endpoint.clone(), keys: SubKeys { p256dh: "AAAA".into(), auth: sub.auth.clone() }, origin: None };
        assert!(matches!(subscribe(State(app.clone()), user(), Json(bad_key)).await, Err(LcError::BadValue(_))));
        let r = unsubscribe(State(app.clone()), user(), Query(EndpointQuery { endpoint: sub.endpoint.clone() })).await.unwrap().0;
        assert_eq!(r["removed"], 1);
        assert_eq!(list(State(app.clone()), user()).await.unwrap().0["subscriptions"].as_array().unwrap().len(), 0);
        let bot = || Extension(RequestPrincipal::Bot("b1".into()));
        assert!(matches!(get_vapid_key(State(app.clone()), bot()).await, Err(LcError::Forbidden(_))));
        assert!(matches!(list(State(app.clone()), bot()).await, Err(LcError::Forbidden(_))));
        assert!(matches!(subscribe(State(app.clone()), bot(), Json(input())).await, Err(LcError::Forbidden(_))));
        assert!(matches!(unsubscribe(State(app.clone()), bot(), Query(EndpointQuery { endpoint: "x".into() })).await, Err(LcError::Forbidden(_))));
        assert!(matches!(test_push(State(app.clone()), bot()).await, Err(LcError::Forbidden(_))));
        assert!(get_vapid_key(State(app), user()).await.unwrap().0["public_key"].as_str().unwrap().len() >= 86);
    }

    /// 全域假 sender 只有一份：用到的測試在同一把鎖裡跑。
    static SENDER_TESTS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn gone_subscriptions_are_pruned_and_failures_kept() {
        let _g = SENDER_TESTS.lock().await;
        let dir = crate::testing::scratch_dir("am-webpush-gone");
        let subs: Vec<Subscription> = ["ok", "gone", "flaky"].iter().map(|n| fresh_sub(&format!("https://push.example/{n}")).0).collect();
        save_subs(&dir, &subs).unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let seen2 = seen.clone();
        set_test_sender(Some(Arc::new(move |sub: Subscription, _body: Vec<u8>| {
            seen2.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if sub.endpoint.ends_with("/gone") {
                    SendOutcome::Gone
                } else if sub.endpoint.ends_with("/flaky") {
                    SendOutcome::Failed("503".into())
                } else {
                    SendOutcome::Sent
                }
            })
        })));
        let (sent, removed, failed) = broadcast(&dir, None, &json!({"title": "t"})).await;
        set_test_sender(None);
        assert_eq!((sent, removed, failed), (1, 1, 1));
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        let left: Vec<String> = load_subs(&dir).into_iter().map(|s| s.endpoint).collect();
        assert_eq!(left, vec!["https://push.example/ok".to_string(), "https://push.example/flaky".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn terminal_transition_triggers_push_with_schedule_text() {
        let _g = SENDER_TESTS.lock().await;
        let env = crate::testing::env().await;
        let app = env.app.clone();
        save_subs(&app.data_dir, &[fresh_sub("https://push.example/t").0]).unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        set_test_sender(Some(Arc::new(move |_sub: Subscription, body: Vec<u8>| {
            let _ = tx.send(serde_json::from_slice(&body).unwrap());
            Box::pin(async { SendOutcome::Sent })
        })));
        let t = crate::schedules::store::Transition {
            schedule_id: "sch1".into(),
            schedule_name: "DON上班".into(),
            schedule_code: "S3".into(),
            project_id: "p1".into(),
            occurrence_id: "occ1".into(),
            status: "done".into(),
            notify: "all".into(),
            error: None,
            bot_id: "b".into(),
            clear_after: false,
            outcome: "success".into(),
            attempt_no: 1,
        };
        spawn_schedule_push(&app, &t);
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("應在逾時內收到推播").unwrap();
        set_test_sender(None);
        assert_eq!(got["title"], "S3 DON上班");
        assert!(got["body"].as_str().unwrap().starts_with("✅ "), "{got}");
        assert!(got["url"].as_str().unwrap().contains("schedule=sch1"), "{got}");
    }

    #[test]
    fn first_line_truncates_and_admit_dedups_throttles_and_respects_presence() {
        assert_eq!(first_line("\n  hello world \nsecond", 80), "hello world");
        let long = "字".repeat(100);
        assert_eq!(first_line(&long, 80).chars().count(), 81, "80 字加省略號");
        assert_eq!(first_line("", 80), "");
        let n = MainNotifier::default();
        assert!(n.admit("b", MainKind::Done, "t1", false));
        assert!(!n.admit("b", MainKind::Done, "t1", false), "同一回合只推一次");
        assert!(!n.admit("b", MainKind::Done, "t2", false), "30 秒內合併");
        assert!(n.admit("b", MainKind::Blocked, "r1:1", false), "done 與 blocked 分開算");
        assert!(n.admit("other", MainKind::Done, "t9", false), "別顆 bot 不受影響");
        assert!(!n.admit("c", MainKind::Done, "t1", true), "正在看就不推");
        assert!(!n.admit("c", MainKind::Done, "t1", false), "被 presence 擋掉的回合不補送");
        let p = main_payload("b1", "foo", MainKind::Blocked, "要繼續嗎？");
        assert_eq!((p["title"].as_str(), p["body"].as_str(), p["url"].as_str()), (Some("foo"), Some("需要你回應：要繼續嗎？"), Some("/bots/b1")));
        assert_eq!(main_payload("b1", "foo", MainKind::Done, "")["body"], "回覆完成");
    }

    #[test]
    fn presence_requires_visible_selected_and_recent() {
        let _ = SENDER_TESTS.try_lock();
        clear_presence();
        let at = |bot: Option<&str>, visible, idle| Presence { bot_id: bot.map(String::from), visible, idle_secs: idle, received: std::time::Instant::now() };
        record_presence("c1", at(Some("b1"), true, 5));
        assert!(user_is_watching("b1"));
        assert!(!user_is_watching("b2"), "選中別顆");
        record_presence("c1", at(Some("b1"), false, 5));
        assert!(!user_is_watching("b1"), "頁面不可見");
        record_presence("c1", at(Some("b1"), true, 61));
        assert!(!user_is_watching("b1"), "超過 60 秒沒互動");
        clear_presence();
    }

    async fn primary_env() -> (crate::testing::Env, String, Arc<MainNotifier>, tokio::sync::mpsc::UnboundedReceiver<Value>) {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "主力一號").await;
        sqlx::query("UPDATE bots SET is_primary = 1 WHERE id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        save_subs(&env.app.data_dir, &[fresh_sub("https://push.example/m").0]).unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Value>();
        set_test_sender(Some(Arc::new(move |_s: Subscription, body: Vec<u8>| {
            let _ = tx.send(serde_json::from_slice(&body).unwrap());
            Box::pin(async { SendOutcome::Sent })
        })));
        let n = Arc::new(MainNotifier::default());
        *n.settle.lock().unwrap() = Some(Duration::ZERO);
        (env, bot.id, n, rx)
    }

    fn turn_ev(bot_id: &str, turn: &str, status: &str) -> crate::state::WsEvent {
        crate::state::WsEvent { seq: 1, kind: "turn_updated".into(), data: json!({"bot_id": bot_id, "turn": {"id": turn, "status": status}}) }
    }

    #[tokio::test]
    async fn primary_turn_done_pushes_once_and_respects_switch_and_presence() {
        let _g = SENDER_TESTS.lock().await;
        clear_presence();
        let (env, bot_id, n, mut rx) = primary_env().await;
        let app = env.app.clone();
        handle_event(&app, &n, &turn_ev(&bot_id, "t1", "in_flight")).await;
        assert!(rx.try_recv().is_err(), "還在跑不推");
        handle_event(&app, &n, &turn_ev(&bot_id, "t1", "completed")).await;
        let got = rx.try_recv().expect("主力回合做完要推");
        assert_eq!((got["title"].as_str(), got["url"].as_str()), (Some("主力一號"), Some(&*format!("/bots/{bot_id}"))));
        handle_event(&app, &n, &turn_ev(&bot_id, "t1", "completed")).await;
        handle_event(&app, &n, &turn_ev(&bot_id, "t2", "completed")).await;
        assert!(rx.try_recv().is_err(), "同回合與 30 秒內都不再推");
        // 非主力不推。
        let plain = crate::testing::claude_bot(&app, &env.project_id, "一般").await;
        handle_event(&app, &n, &turn_ev(&plain.id, "t3", "completed")).await;
        assert!(rx.try_recv().is_err(), "非主力不推");
        // 開關關掉不推。
        let n2 = Arc::new(MainNotifier::default());
        write_private(&settings_path(&app.data_dir), br#"{"main_bots":false}"#).unwrap();
        handle_event(&app, &n2, &turn_ev(&bot_id, "t4", "completed")).await;
        assert!(rx.try_recv().is_err(), "開關關了不推");
        write_private(&settings_path(&app.data_dir), br#"{"main_bots":true}"#).unwrap();
        // presence：正在看就不推。
        record_presence("c", Presence { bot_id: Some(bot_id.clone()), visible: true, idle_secs: 1, received: std::time::Instant::now() });
        handle_event(&app, &n2, &turn_ev(&bot_id, "t5", "completed")).await;
        assert!(rx.try_recv().is_err(), "正在看不推");
        clear_presence();
        handle_event(&app, &n2, &turn_ev(&bot_id, "t6", "completed")).await;
        assert!(rx.try_recv().is_ok(), "沒在看就推");
        set_test_sender(None);
    }

    #[tokio::test]
    async fn primary_blocked_pushes_once_per_episode() {
        let _g = SENDER_TESTS.lock().await;
        clear_presence();
        let (env, bot_id, n, mut rx) = primary_env().await;
        let app = env.app.clone();
        let run_id = crate::testing::fake_run(&app, &bot_id).await;
        sqlx::query("UPDATE runs SET agent_status = 'blocked' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        let ev = |st: &str| crate::state::WsEvent { seq: 1, kind: "bot_status".into(), data: json!({"bot_id": bot_id, "run": {"agent_status": st}}) };
        handle_event(&app, &n, &ev("blocked")).await;
        handle_event(&app, &n, &ev("blocked")).await;
        let got = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("blocked 要推").unwrap();
        assert_eq!(got["title"], "主力一號");
        assert!(got["body"].as_str().unwrap().starts_with("需要你回應"), "{got}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(rx.try_recv().is_err(), "同一段 blocked 只推一次");
        set_test_sender(None);
    }

    // ---- 推播內文：譯文／摘要 ----

    struct PFake {
        tr_calls: AtomicUsize,
        sum_calls: AtomicUsize,
        tr_delay_ms: u64,
        sum_prompts: std::sync::Mutex<Vec<String>>,
    }

    impl crate::claude_api::MessagesClient for PFake {
        fn complete<'a>(&'a self, req: &'a crate::claude_api::MessagesRequest) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<crate::claude_api::ClientOutcome, crate::claude_api::ClientError>> + Send + 'a>> {
            let is_tr = req.system.as_deref().is_some_and(|s| s.contains("translation engine"));
            let prompt = req.prompt.clone();
            Box::pin(async move {
                let text = if is_tr {
                    self.tr_calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(self.tr_delay_ms)).await;
                    "已部署完成，測試全數通過".to_string()
                } else {
                    self.sum_calls.fetch_add(1, Ordering::SeqCst);
                    self.sum_prompts.lock().unwrap().push(prompt);
                    "摘要：部署完成並通過測試".to_string()
                };
                Ok(crate::claude_api::ClientOutcome { text, model: Some("claude-haiku-5-5".into()), stop_reason: Some("end_turn".into()), usage: None })
            })
        }
    }

    async fn push_env(enabled: bool, auto: bool, tr_delay_ms: u64) -> (crate::testing::Env, String, Arc<MainNotifier>, tokio::sync::mpsc::UnboundedReceiver<Value>, Arc<PFake>) {
        let (env, bot_id, n, rx) = primary_env().await;
        let key = env.app.data_dir.join("key");
        crate::key_file::stage_key(&key.to_string_lossy(), "sk-ant-test-push").unwrap().publish().unwrap();
        let key = key.to_string_lossy().into_owned();
        env.app
            .cfg
            .update(move |c| {
                c.claude_api.key_file = key;
                c.claude_api.enabled = enabled;
                c.claude_api.uses.insert("message_translate".into(), crate::config::ClaudeApiUseCfg { enabled: Some(enabled), auto: Some(auto), ..Default::default() });
                Ok(())
            })
            .await
            .unwrap();
        let f = Arc::new(PFake { tr_calls: AtomicUsize::new(0), sum_calls: AtomicUsize::new(0), tr_delay_ms, sum_prompts: Default::default() });
        env.app.claude_api.set_client(f.clone());
        (env, bot_id, n, rx, f)
    }

    async fn add_reply(env: &crate::testing::Env, bot_id: &str, turn: &str, text: &str) {
        let conv = crate::db::conversation_id(&env.app.db, bot_id).await.unwrap();
        let run: String = match sqlx::query_scalar("SELECT id FROM runs WHERE bot_id = ?").bind(bot_id).fetch_optional(&env.app.db).await.unwrap() { Some(r) => r, None => crate::testing::fake_run(&env.app, bot_id).await };
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','completed','ok',?)").bind(turn).bind(&conv).bind(&run).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO messages(id,conversation_id,turn_id,role,content,source,created_at) VALUES(?,?,?,'assistant',?,'hook',?)").bind(format!("m-{turn}")).bind(&conv).bind(turn).bind(text).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    }

    const EN_SHORT: &str = "Deployed, all tests pass fine";
    const EN_LONG: &str = "I finished the deployment of the new daemon build and all of the tests passed on the first try.";

    #[tokio::test]
    async fn english_reply_waits_for_translation_once_and_uses_it() {
        let _g = SENDER_TESTS.lock().await;
        clear_presence();
        let (env, bot_id, n, mut rx, f) = push_env(true, true, 150).await;
        add_reply(&env, &bot_id, "t1", EN_SHORT).await;
        let ev = turn_ev(&bot_id, "t1", "completed");
        tokio::join!(handle_event(&env.app, &n, &ev), handle_event(&env.app, &n, &ev));
        let got = rx.try_recv().expect("等到譯文後要推");
        assert_eq!(got["body"], "已部署完成，測試全數通過");
        assert!(rx.try_recv().is_err(), "等譯文時重複事件不會重複推");
        assert_eq!((f.tr_calls.load(Ordering::SeqCst), f.sum_calls.load(Ordering::SeqCst)), (1, 0), "短句不摘要");
        // 譯文快取與網頁共用：同一則再翻是命中。
        let again = crate::message_translate::translate(&env.app, "m-t1", EN_SHORT).await.unwrap();
        assert_eq!(again["cached"], true);
        set_test_sender(None);
    }

    #[tokio::test]
    async fn translation_timeout_falls_back_to_the_original_text() {
        let _g = SENDER_TESTS.lock().await;
        clear_presence();
        LINE_WAIT_MS.store(60, Ordering::Relaxed);
        let (env, bot_id, n, mut rx, f) = push_env(true, true, 2000).await;
        add_reply(&env, &bot_id, "t1", EN_SHORT).await;
        handle_event(&env.app, &n, &turn_ev(&bot_id, "t1", "completed")).await;
        LINE_WAIT_MS.store(20_000, Ordering::Relaxed);
        let got = rx.try_recv().expect("逾時也要推，不漏推");
        assert_eq!(got["body"], EN_SHORT, "逾時用原文（短句直接用）");
        assert_eq!(f.tr_calls.load(Ordering::SeqCst), 1);
        set_test_sender(None);
    }

    #[tokio::test]
    async fn long_or_multiline_reply_is_summarized_to_one_sentence() {
        let _g = SENDER_TESTS.lock().await;
        clear_presence();
        let (env, bot_id, n, mut rx, f) = push_env(true, false, 0).await;
        let long_zh = "第一行說明目前的進度，已經完成部署\n第二行是更多細節：測試全部通過\n第三行列出檔案路徑 /tmp/a/b/c";
        add_reply(&env, &bot_id, "t1", long_zh).await;
        handle_event(&env.app, &n, &turn_ev(&bot_id, "t1", "completed")).await;
        let got = rx.try_recv().expect("要推");
        assert_eq!(got["body"], "摘要：部署完成並通過測試");
        assert_eq!(f.sum_prompts.lock().unwrap().as_slice(), [long_zh.to_string()]);
        // auto 關：英文長句不翻，直接摘要原文。
        let n2 = Arc::new(MainNotifier::default());
        add_reply(&env, &bot_id, "t2", EN_LONG).await;
        handle_event(&env.app, &n2, &turn_ev(&bot_id, "t2", "completed")).await;
        assert_eq!(rx.try_recv().unwrap()["body"], "摘要：部署完成並通過測試");
        assert_eq!(f.tr_calls.load(Ordering::SeqCst), 0, "auto 關著不自動翻譯");
        assert_eq!(f.sum_prompts.lock().unwrap()[1], EN_LONG);
        set_test_sender(None);
    }

    #[tokio::test]
    async fn disabled_feature_falls_back_to_first_line_without_any_call() {
        let _g = SENDER_TESTS.lock().await;
        clear_presence();
        let (env, bot_id, n, mut rx, f) = push_env(false, true, 0).await;
        add_reply(&env, &bot_id, "t1", &format!("{EN_LONG}\nsecond line")).await;
        handle_event(&env.app, &n, &turn_ev(&bot_id, "t1", "completed")).await;
        let got = rx.try_recv().expect("功能關閉也要推");
        assert_eq!(got["body"], first_line(EN_LONG, BODY_MAX));
        assert_eq!((f.tr_calls.load(Ordering::SeqCst), f.sum_calls.load(Ordering::SeqCst)), (0, 0));
        set_test_sender(None);
    }

    #[test]
    fn english_detection_matches_the_web_rule() {
        use crate::message_translate::is_mostly_english;
        assert!(is_mostly_english(EN_LONG));
        assert!(!is_mostly_english("已經完成部署，測試全部通過，請放心 deployed ok"));
        assert!(!is_mostly_english("short"), "拉丁字母不足 20");
        assert!(!is_mostly_english("```\nlots of english code here and more and more\n```\n好"), "程式碼區塊不算");
        assert!(!is_mostly_english("see https://example.com/a/very/long/path/name/here"), "網址不算");
    }

    #[tokio::test]
    async fn settings_roundtrip_user_only() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let user = || Extension(RequestPrincipal::User);
        assert_eq!(get_settings(State(app.clone()), user()).await.unwrap().0["main_bots"], true, "預設開");
        put_settings(State(app.clone()), user(), Json(SettingsInput { main_bots: Some(false), updates: None, reset_credits: None })).await.unwrap();
        assert_eq!(get_settings(State(app.clone()), user()).await.unwrap().0["main_bots"], false);
        let bot = || Extension(RequestPrincipal::Bot("b1".into()));
        assert!(matches!(get_settings(State(app.clone()), bot()).await, Err(LcError::Forbidden(_))));
        assert!(matches!(put_settings(State(app), bot(), Json(SettingsInput { main_bots: Some(true), updates: None, reset_credits: None })).await, Err(LcError::Forbidden(_))));
        assert!(matches!(post_presence(bot(), Json(PresenceInput { client_id: "c".into(), bot_id: None, visible: true, idle_secs: 0 })).await, Err(LcError::Forbidden(_))));
    }

    #[test]
    fn update_notices_and_build_change_and_exhaustion() {
        let up = json!({"kind": "codex", "latest_version": "0.130.0", "notify": "update"});
        let (key, p) = update_notice("upstream_update", &up).unwrap();
        assert_eq!(key, "upstream:codex:0.130.0");
        assert_eq!(p["body"], "codex CLI 有新版 0.130.0");
        assert!(update_notice("upstream_update", &json!({"kind": "codex", "latest_version": "1", "notify": null})).is_none());
        assert!(update_notice("upstream_update", &json!({"kind": "codex", "latest_version": "1", "notify": "error"})).is_none());
        let done = json!({"ok": true, "kind": "claude", "host": "m4p", "target_version": "2.1.300"});
        assert_eq!(update_notice("cli_update_done", &done).unwrap().1["body"], "claude CLI 已更新到 2.1.300");
        assert!(update_notice("cli_update_done", &json!({"ok": false, "kind": "claude", "target_version": "2"})).is_none());
        let dir = crate::testing::scratch_dir("am-webpush-build");
        assert!(!build_changed(&dir, "aaa1111"), "第一次沒有紀錄不推");
        assert!(!build_changed(&dir, "aaa1111"), "沒換版不推");
        assert!(build_changed(&dir, "bbb2222"), "換版要推");
        assert!(!build_changed(&dir, "bbb2222"));
        let _ = std::fs::remove_dir_all(&dir);
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-10T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(exhaustion_marker(&json!({"limit_hit": {"at": "A", "until": "2026-11-10T05:00:00Z"}}), now).unwrap(), "hit:A");
        assert!(exhaustion_marker(&json!({"limit_hit": {"at": "A", "until": "2026-11-09T05:00:00Z"}}), now).is_none(), "撞限已過期");
        assert_eq!(exhaustion_marker(&json!({"five_hour": {"used_pct": 100, "resets_at": "2026-11-10T03:00:00Z"}}), now).unwrap(), "win:five_hour:2026-11-10T03:00:00Z");
        assert!(exhaustion_marker(&json!({"five_hour": {"used_pct": 80, "resets_at": "2026-11-10T03:00:00Z"}}), now).is_none());
    }

    #[tokio::test]
    async fn upstream_update_pushes_once_per_version_and_respects_switch() {
        let _g = SENDER_TESTS.lock().await;
        let (env, _bot, n, mut rx) = primary_env().await;
        let app = env.app.clone();
        let ev = |v: &str| crate::state::WsEvent { seq: 1, kind: "upstream_update".into(), data: json!({"kind": "codex", "latest_version": v, "notify": "update"}) };
        handle_event(&app, &n, &ev("0.130.0")).await;
        assert_eq!(rx.try_recv().unwrap()["body"], "codex CLI 有新版 0.130.0");
        handle_event(&app, &n, &ev("0.130.0")).await;
        assert!(rx.try_recv().is_err(), "同版本只推一次");
        handle_event(&app, &n, &ev("0.131.0")).await;
        assert!(rx.try_recv().is_ok(), "新版本再推");
        write_private(&settings_path(&app.data_dir), br#"{"updates":false}"#).unwrap();
        handle_event(&app, &n, &ev("0.132.0")).await;
        assert!(rx.try_recv().is_err(), "開關關了不推");
        set_test_sender(None);
    }

    #[tokio::test]
    async fn reset_credit_offer_pushes_once_per_exhaustion_and_never_redeems() {
        let _g = SENDER_TESTS.lock().await;
        let (env, _bot, _n, mut rx) = primary_env().await;
        let app = env.app.clone();
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-10T00:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let snap = |at: &str| json!({"kinds": {
            "m4p/codex:co0": {"limit_hit": {"at": at, "until": "2026-11-10T05:00:00Z"}, "reset_credits": {"available": 2}, "held_bots": ["a", "b"]},
            "codex": {"limit_hit": {"at": at, "until": "2026-11-10T05:00:00Z"}, "reset_credits": {"available": 0}},
            "claude": {"limit_hit": {"at": at}, "reset_credits": {"available": 3}},
        }});
        assert_eq!(scan_reset_credits(&app, &snap("T1"), now).await, 1, "只有 codex 且有券的帳號");
        let got = rx.try_recv().unwrap();
        assert_eq!(got["body"], "codex 額度用完，有 2 張重置券可用（影響：a、b）");
        assert_eq!(got["url"], "/?reset_offer=m4p%2Fcodex%3Aco0");
        assert_eq!(scan_reset_credits(&app, &snap("T1"), now).await, 0, "同一次停工只推一次");
        assert!(rx.try_recv().is_err());
        assert_eq!(scan_reset_credits(&app, &snap("T2"), now).await, 1, "新的一次停工再推");
        write_private(&settings_path(&app.data_dir), br#"{"reset_credits":false}"#).unwrap();
        assert_eq!(scan_reset_credits(&app, &snap("T3"), now).await, 0, "開關關了不推");
        set_test_sender(None);
    }
}
