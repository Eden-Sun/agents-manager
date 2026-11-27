//! OpenAI 帳戶的實際餘額（使用者 2026-11-27 核准做法 2）。
//!
//! platform.openai.com 前面有 Cloudflare challenge，headless 瀏覽器過不了，所以不比照 Claude 讀頁；改用使用者匯出的
//! platform 登入 token（`[openai_api] console_state_file`，`{access_token, refresh_token, id_token}`，**是憑證**）直接打 `api.openai.com`：
//! 1. `POST /dashboard/onboarding/login`（Bearer access_token）換短效 session key（`sess-…`）與 org 清單；
//! 2. `GET /v1/dashboard/billing/credit_grants`（Bearer sess key＋`OpenAI-Organization`）拿總額與每筆 grant。
//!
//! 規矩：
//! - 檔案不存在或關閉就不打網路（`not_configured`）。整個流程總逾時 30 秒。
//! - access_token 的 `exp` 剩不到 24 小時才用 refresh_token 續期，成功寫回同目錄 0600 暫存檔再 rename；失敗記 `logged_out`，
//!   同一把 refresh_token 不再重試。
//! - token、session key、回應本文一律不進 log、API 回應或錯誤訊息；錯誤只放類別與 HTTP code。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Datelike, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::state::App;

const TOTAL_BUDGET: Duration = Duration::from_secs(30);
/// access_token 剩不到這麼久才續期。
const REFRESH_WHEN_LEFT_SECS: i64 = 24 * 3600;

/// 三支端點；正式環境固定官方 HTTPS，測試換成本機 mock。
#[derive(Debug, Clone)]
pub struct Endpoints {
    pub login: String,
    pub credit_grants: String,
    pub token: String,
}

impl Endpoints {
    pub fn official() -> Self {
        Self {
            login: "https://api.openai.com/dashboard/onboarding/login".into(),
            credit_grants: "https://api.openai.com/v1/dashboard/billing/credit_grants".into(),
            token: "https://auth.openai.com/oauth/token".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Grant {
    pub remaining_usd: f64,
    pub total_usd: f64,
    /// `YYYY-MM-DD`（使用者時區）；沒有到期時間就是 `None`。
    pub expires_on: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConsoleData {
    pub total_available_usd: f64,
    pub total_granted_usd: f64,
    pub total_used_usd: f64,
    pub grants: Vec<Grant>,
    pub org_title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    LoggedOut,
    Error,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::LoggedOut => "logged_out",
            Status::Error => "error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConsoleSnapshot {
    pub status: Status,
    pub data: Option<ConsoleData>,
    pub fetched_at: String,
    pub error: Option<String>,
}

/// 記憶體裡的狀態：最近一次結果，與續期失敗過的 refresh_token 指紋（同一把不再重試）。
#[derive(Default)]
pub struct OpenAiConsoleState {
    pub snapshot: Mutex<Option<ConsoleSnapshot>>,
    refresh_failed_for: Mutex<Option<u64>>,
}

/// JWT payload 裡用得到的欄位。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct JwtClaims {
    pub exp: Option<i64>,
    pub client_id: Option<String>,
}

/// 解 JWT 的 payload（不驗簽，只讀 `exp`／`client_id`）；格式不對回 `None`。
pub fn jwt_claims(token: &str) -> Option<JwtClaims> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: Value = serde_json::from_slice(&bytes).ok()?;
    Some(JwtClaims { exp: v.get("exp").and_then(Value::as_i64), client_id: v.get("client_id").and_then(Value::as_str).map(str::to_string) })
}

/// `exp` 剩不到 24 小時（含已過期）才要續期；讀不到 `exp` 就不續，直接拿去用。
pub fn needs_refresh(exp: Option<i64>, now: DateTime<Utc>) -> bool {
    exp.is_some_and(|e| e - now.timestamp() < REFRESH_WHEN_LEFT_SECS)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Org {
    pub id: String,
    pub title: Option<String>,
    pub personal: bool,
}

/// 有設定就用指定的；否則唯一的非 personal org；再不然第一個。
pub fn pick_org<'a>(orgs: &'a [Org], configured: Option<&str>) -> Option<&'a Org> {
    if let Some(id) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        return orgs.iter().find(|o| o.id == id);
    }
    let business: Vec<&Org> = orgs.iter().filter(|o| !o.personal).collect();
    if business.len() == 1 {
        return Some(business[0]);
    }
    orgs.first()
}

/// login 回應 → (session key, orgs)。
pub fn parse_login(v: &Value) -> Option<(String, Vec<Org>)> {
    let user = v.get("user")?;
    let sess = user.pointer("/session/sensitive_id").and_then(Value::as_str).filter(|s| !s.is_empty())?.to_string();
    let orgs = user
        .pointer("/orgs/data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|o| {
                    Some(Org {
                        id: o.get("id")?.as_str()?.to_string(),
                        title: o.get("title").and_then(Value::as_str).map(str::to_string),
                        personal: o.get("personal").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some((sess, orgs))
}

fn usd(v: &Value, key: &str) -> Option<f64> {
    v.get(key).and_then(Value::as_f64).filter(|x| x.is_finite())
}

/// epoch 秒 → 使用者時區的 `YYYY-MM-DD`。
pub fn epoch_to_day(secs: f64, tz: chrono_tz::Tz) -> Option<String> {
    let dt = DateTime::<Utc>::from_timestamp(secs as i64, 0)?.with_timezone(&tz);
    Some(format!("{:04}-{:02}-{:02}", dt.year(), dt.month(), dt.day()))
}

/// credit_grants 回應 → 餘額。`total_available` 必須在；每筆 grant 的剩餘＝`grant_amount - used_amount`。
pub fn parse_credit_grants(v: &Value, tz: chrono_tz::Tz) -> Option<ConsoleData> {
    let total_available_usd = usd(v, "total_available")?;
    let grants = v
        .pointer("/grants/data")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|g| {
                    let total = usd(g, "grant_amount")?;
                    let used = usd(g, "used_amount").unwrap_or(0.0);
                    let expires_on = g.get("expires_at").and_then(Value::as_f64).and_then(|s| epoch_to_day(s, tz));
                    Some(Grant { remaining_usd: (total - used).max(0.0), total_usd: total, expires_on })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ConsoleData {
        total_available_usd,
        total_granted_usd: usd(v, "total_granted").unwrap_or(0.0),
        total_used_usd: usd(v, "total_used").unwrap_or(0.0),
        grants,
        org_title: None,
    })
}

/// `GET /api/openai-api/balance` 的 `console` 物件。
pub fn console_json(snapshot: Option<&ConsoleSnapshot>, configured: bool) -> Value {
    if !configured {
        return json!({"status": "not_configured"});
    }
    let Some(s) = snapshot else {
        return json!({"status": "error", "error": "pending"});
    };
    let mut v = json!({"status": s.status.as_str(), "fetched_at": s.fetched_at});
    if let (Status::Ok, Some(d)) = (s.status, s.data.as_ref()) {
        v["total_available_usd"] = json!(d.total_available_usd);
        v["total_granted_usd"] = json!(d.total_granted_usd);
        v["total_used_usd"] = json!(d.total_used_usd);
        v["grants"] = json!(d.grants);
        if let Some(t) = &d.org_title {
            v["org_title"] = json!(t);
        }
    }
    if let Some(e) = &s.error {
        v["error"] = json!(e);
    }
    v
}

/// 一次讀取的結果：狀態與只含類別／HTTP code 的錯誤。
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Ok(ConsoleData),
    LoggedOut(&'static str),
    Error(String),
}

fn fingerprint(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn send_err(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "timeout".into()
    } else {
        "network".into()
    }
}

/// 用 refresh_token 換新 token，寫回 `state_path`；回新的 access_token。錯誤字串只有類別／HTTP code。
async fn refresh_tokens(client: &reqwest::Client, ep: &Endpoints, state_path: &Path, state: &Value, client_id: &str, refresh_token: &str) -> Result<String, String> {
    let resp = client
        .post(&ep.token)
        .json(&json!({"grant_type": "refresh_token", "client_id": client_id, "refresh_token": refresh_token}))
        .send()
        .await
        .map_err(|e| send_err(&e))?;
    let code = resp.status();
    if !code.is_success() {
        return Err(format!("refresh_http_{}", code.as_u16()));
    }
    let body: Value = resp.json().await.map_err(|_| "refresh_parse".to_string())?;
    let access = body.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).ok_or("refresh_parse")?.to_string();
    let mut next = state.clone();
    next["access_token"] = json!(access);
    for k in ["refresh_token", "id_token"] {
        if let Some(t) = body.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()) {
            next[k] = json!(t);
        }
    }
    write_state(state_path, &next)?;
    Ok(access)
}

/// 同目錄 0600 暫存檔 → rename 蓋回。
pub fn write_state(state_path: &Path, v: &Value) -> Result<(), String> {
    use std::io::Write as _;
    let dir = state_path.parent().ok_or("state_path")?;
    let name = state_path.file_name().ok_or("state_path")?.to_string_lossy().into_owned();
    let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
    let result = (|| {
        crate::claude_console_balance::create_private_file(&tmp).map_err(|_| "state_write".to_string())?;
        let bytes = serde_json::to_vec(v).map_err(|_| "state_write".to_string())?;
        let mut f = std::fs::OpenOptions::new().write(true).open(&tmp).map_err(|_| "state_write".to_string())?;
        f.write_all(&bytes).and_then(|_| f.sync_all()).map_err(|_| "state_write".to_string())?;
        crate::claude_console_balance::set_mode_600(&tmp).map_err(|_| "state_write".to_string())?;
        std::fs::rename(&tmp, state_path).map_err(|_| "state_write".to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 讀一次（不含逾時包裝）。
pub async fn fetch_outcome(st: &OpenAiConsoleState, ep: &Endpoints, state_path: &Path, org_id: Option<&str>, tz: chrono_tz::Tz, now: DateTime<Utc>) -> Outcome {
    let Ok(raw) = std::fs::read_to_string(state_path) else { return Outcome::Error("state_read".into()) };
    let Ok(state) = serde_json::from_str::<Value>(&raw) else { return Outcome::Error("state_parse".into()) };
    let Some(mut access) = state.get("access_token").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string) else {
        return Outcome::Error("state_parse".into());
    };
    let client = match reqwest::Client::builder().user_agent(concat!("agents-manager/", env!("CARGO_PKG_VERSION"))).redirect(reqwest::redirect::Policy::none()).timeout(TOTAL_BUDGET).build() {
        Ok(c) => c,
        Err(_) => return Outcome::Error("client".into()),
    };

    let claims = jwt_claims(&access).unwrap_or_default();
    if needs_refresh(claims.exp, now) {
        let refresh = state.get("refresh_token").and_then(Value::as_str).filter(|s| !s.is_empty());
        let fp = refresh.map(fingerprint);
        let failed_before = fp.is_some() && *st.refresh_failed_for.lock().unwrap_or_else(|e| e.into_inner()) == fp;
        let expired = claims.exp.is_some_and(|e| e <= now.timestamp());
        if failed_before {
            if expired {
                return Outcome::LoggedOut("refresh_failed");
            }
        } else {
            let (Some(cid), Some(rt)) = (claims.client_id.as_deref(), refresh) else {
                return Outcome::LoggedOut("refresh_unavailable");
            };
            match refresh_tokens(&client, ep, state_path, &state, cid, rt).await {
                Ok(a) => access = a,
                Err(e) => {
                    *st.refresh_failed_for.lock().unwrap_or_else(|e| e.into_inner()) = fp;
                    tracing::warn!(error = e.as_str(), "openai console balance: token refresh failed");
                    return Outcome::LoggedOut("refresh_failed");
                }
            }
        }
    }

    let resp = match client.post(&ep.login).bearer_auth(&access).json(&json!({})).send().await {
        Ok(r) => r,
        Err(e) => return Outcome::Error(send_err(&e)),
    };
    let code = resp.status().as_u16();
    if code == 401 || code == 403 {
        return Outcome::LoggedOut("login_rejected");
    }
    if !resp.status().is_success() {
        return Outcome::Error(format!("login_http_{code}"));
    }
    let Ok(body) = resp.json::<Value>().await else { return Outcome::Error("login_parse".into()) };
    let Some((sess, orgs)) = parse_login(&body) else { return Outcome::Error("login_parse".into()) };
    let Some(org) = pick_org(&orgs, org_id) else { return Outcome::Error("no_org".into()) };

    let resp = match client.get(&ep.credit_grants).bearer_auth(&sess).header("OpenAI-Organization", &org.id).send().await {
        Ok(r) => r,
        Err(e) => return Outcome::Error(send_err(&e)),
    };
    if !resp.status().is_success() {
        return Outcome::Error(format!("credit_grants_http_{}", resp.status().as_u16()));
    }
    let Ok(body) = resp.json::<Value>().await else { return Outcome::Error("credit_grants_parse".into()) };
    match parse_credit_grants(&body, tz) {
        Some(mut d) => {
            d.org_title = org.title.clone();
            Outcome::Ok(d)
        }
        None => Outcome::Error("credit_grants_parse".into()),
    }
}

/// 讀一次，含 30 秒總逾時，轉成快照。
pub async fn fetch_once(st: &OpenAiConsoleState, ep: &Endpoints, state_path: &Path, org_id: Option<&str>, tz: chrono_tz::Tz) -> ConsoleSnapshot {
    let outcome = tokio::time::timeout(TOTAL_BUDGET, fetch_outcome(st, ep, state_path, org_id, tz, Utc::now())).await.unwrap_or_else(|_| Outcome::Error("timeout".into()));
    let fetched_at = am_base::db::iso_at(Utc::now());
    match outcome {
        Outcome::Ok(d) => ConsoleSnapshot { status: Status::Ok, data: Some(d), fetched_at, error: None },
        Outcome::LoggedOut(why) => ConsoleSnapshot { status: Status::LoggedOut, data: None, fetched_at, error: Some(why.into()) },
        Outcome::Error(e) => ConsoleSnapshot { status: Status::Error, data: None, fetched_at, error: Some(e) },
    }
}

pub struct Active {
    pub path: PathBuf,
    pub every: Duration,
    pub org_id: Option<String>,
    pub tz: chrono_tz::Tz,
}

/// 設定現在要不要跑；不跑回 `None`（OpenAI API 沒開、console 關閉、登入態檔不存在）。
pub async fn active(app: &Arc<App>) -> Option<Active> {
    let cfg = app.cfg.get().await;
    let c = &cfg.openai_api;
    if !c.enabled || !c.console_balance_enabled {
        return None;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let path = PathBuf::from(crate::config::expand_home(&c.console_state_file, &home));
    if !path.is_file() {
        return None;
    }
    let tz = cfg.agents.time_zone().parse().unwrap_or(chrono_tz::Asia::Taipei);
    Some(Active { path, every: Duration::from_secs(c.console_balance_interval_minutes.max(5) * 60), org_id: c.console_org_id.clone(), tz })
}

/// 讀一次並放進快照；不該跑就清掉快照。
pub async fn refresh(app: &Arc<App>) -> Option<Duration> {
    let st = &app.claude_api.openai_console;
    let Some(a) = active(app).await else {
        *st.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = None;
        return None;
    };
    let snap = fetch_once(st, &Endpoints::official(), &a.path, a.org_id.as_deref(), a.tz).await;
    if snap.status != Status::Ok {
        tracing::warn!(status = snap.status.as_str(), error = snap.error.as_deref().unwrap_or(""), "openai console balance: fetch did not succeed");
    }
    *st.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Some(snap);
    Some(a.every)
}

/// 開機後先讀一次，之後照間隔。測試不起。
pub fn spawn_ticker(app: &Arc<App>) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    am_base::background_loop::spawn_named(&tasks, "openai console balance", async move {
        loop {
            let wait = refresh(&app).await.unwrap_or(Duration::from_secs(300));
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = tokio::time::sleep(wait) => {}
            }
        }
    });
}

/// `GET /api/openai-api/balance` 用。
pub async fn current_json(app: &Arc<App>) -> Value {
    let configured = active(app).await.is_some();
    let snap = app.claude_api.openai_console.snapshot.lock().unwrap_or_else(|e| e.into_inner()).clone();
    console_json(snap.as_ref(), configured)
}

#[cfg(test)]
mod tests;
