//! 網頁推播（Web Push，RFC 8030／8291／8292）：先用在排程終態通知。
//!
//! 自行實作而不用 `web-push` crate：該 crate 的加密後端（`ece`）預設走 OpenSSL，會把系統 libssl 拉進建置；
//! 這裡用純 Rust 的 RustCrypto（`p256`、`hkdf`、`aes-gcm`）加既有的 `reqwest`，約百來行。
//! * VAPID 私鑰第一次需要時產生，存 `{data_dir}/vapid.json`（0600）；不進 config.toml、不進 log，API 只給公鑰。
//! * 訂閱存 `{data_dir}/push-subscriptions.json`（0600，原子寫入）；同 endpoint 去重；推送端回 404／410 就刪。
//! * 送出一律背景、有逾時，失敗只記 warn，不影響排程。

use std::path::{Path, PathBuf};
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
}
