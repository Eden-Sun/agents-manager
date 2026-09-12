//! v4.0 — rate-limit quota per host + kind (`GET /api/quota`, WS `quota_updated`).
//!
//! * codex: polled from that host's `codex app-server` (`account/rateLimits/read`) at start-up,
//!   every 5 min and on `?refresh=1`.
//! * claude: statusLine push while a bot is chatting, **plus** a background `/usage` pane probe
//!   every 60 s — see [`crate::quota_claude`]. Keyed `claude` for the default account, or
//!   `claude:<identity>` when probing / receiving under an identity (identity rows do not
//!   overwrite the default-account `claude` row).
//! * grok: scraped from the TUI's `/usage` dialog in a throwaway pane every 30 s — see
//!   [`crate::quota_grok`]; grok exposes no CLI or RPC surface for it.
//!
//! Every quota belongs to the host it was read on (SPEC §14): the local host keeps the bare
//! keys (`claude`, `claude:cc1`, `codex`, `grok`) and a remote host prefixes them with its name
//! (`m4p/claude`, `m4p/claude:cc1`, …), the same shape `GET /api/models` caches under. The
//! header strip shows one host at a time — the host of the bot / project being viewed — so a
//! remote bot's statusLine must never land on the local row.

use crate::config::LOCAL_HOST;
use crate::state::App;
use anyhow::Result;
use serde::ser::SerializeStruct;
use serde::{Serialize, Serializer};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const CODEX_POLL: Duration = Duration::from_secs(300);

/// Remaining % below which the strip shows the number, not just the bar (requirement: this
/// decision is the daemon's, not the UI's — the UI only reads [`Window::low`] off the wire).
pub const LOW_REMAINING_PCT: f64 = 30.0;

/// Remaining % below which the sidebar bot row surfaces a warning — see [`Window::critical`].
pub const CRITICAL_REMAINING_PCT: f64 = 5.0;

/// Serialises throwaway `/usage` probes **per host** so a `?refresh=1` and the background
/// pollers (claude + grok) never fight over the same pane. One lock per host: a slow ssh
/// probe on `m4p` must not hold up the local one.
pub async fn probe_lock(host: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static LOCKS: std::sync::OnceLock<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        std::sync::OnceLock::new();
    let map = LOCKS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    let lock = map.lock().await.entry(host.to_string()).or_default().clone();
    lock.lock_owned().await
}

/// Quota map key for a host: bare on `local`, `<host>/…` everywhere else.
pub fn quota_key(host: &str, base: &str) -> String {
    if host == LOCAL_HOST {
        base.to_string()
    } else {
        format!("{host}/{base}")
    }
}

/// The host a quota map key belongs to, given the hosts that exist (`local` for anything else).
pub fn host_of_key<'a>(key: &'a str, hosts: &[String]) -> (&'a str, &'a str) {
    match key.split_once('/') {
        Some((h, base)) if hosts.iter().any(|n| n == h) => (h, base),
        _ => (LOCAL_HOST, key),
    }
}

/// `local` plus every remote host that is currently connected — the hosts worth polling.
pub async fn pollable_hosts(app: &Arc<App>) -> Vec<String> {
    app.hosts
        .list()
        .await
        .into_iter()
        .filter(|c| c.is_local() || c.is_connected())
        .map(|c| c.name.clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    pub resets_at: Option<String>,
}

impl Window {
    fn remaining_pct(&self) -> f64 {
        (100.0 - self.used_pct).max(0.0)
    }

    /// Remaining < [`LOW_REMAINING_PCT`] — the strip should show the number.
    pub fn low(&self) -> bool {
        self.remaining_pct() < LOW_REMAINING_PCT
    }

    /// Remaining < [`CRITICAL_REMAINING_PCT`] — the sidebar bot row should warn.
    pub fn critical(&self) -> bool {
        self.remaining_pct() < CRITICAL_REMAINING_PCT
    }
}

/// Manual impl (rather than `#[derive(Serialize)]`) so `low` / `critical` go over the wire as
/// computed fields — every call site still just builds a plain `{used_pct, resets_at}` struct.
impl Serialize for Window {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut st = s.serialize_struct("Window", 4)?;
        st.serialize_field("used_pct", &self.used_pct)?;
        st.serialize_field("resets_at", &self.resets_at)?;
        st.serialize_field("low", &self.low())?;
        st.serialize_field("critical", &self.critical())?;
        st.end()
    }
}

/// Codex 的「額度重置券」（`account/rateLimits/read` 的 `rateLimitResetCredits`）。
///
/// 2026-09-10 使用者：codex 用完額度時，OpenAI 會送一張「立刻重置」的券，TUI 上寫成
/// `Reset usage`。它跟 5h／7d 兩條桶子是不同的事——桶子說「還剩多少、什麼時候回血」，
/// 這張券說「你可以現在就把它清掉，還有幾張」。額度用完的當下，這是使用者唯一還能做的動作，
/// 所以要看得到。daemon 只讀不用（按下去仍然在 codex 那邊做）。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ResetCredits {
    /// 現在可用的張數（`availableCount`）。0 = 有這個欄位但沒券可用。
    pub available: i64,
    /// 第一張可用券的名稱，例如 `Full reset (Weekly + 5 hr)`；沒有就 `None`。
    pub title: Option<String>,
    /// 第一張可用券的到期時間（RFC3339）。券會過期，所以這是「什麼時候用掉它」的依據。
    pub expires_at: Option<String>,
}

/// 「這個帳號現在被擋住了」——CLI 自己印出來的上限橫幅（codex：`You've hit your usage limit …`）。
///
/// 為什麼不能只靠 5h／7d 兩條桶子：那兩條是**速率**視窗，codex 的 credits 用完時它們可以是滿的，
/// app-server 也照樣回報 0% 已用。2026-09-12 使用者看到的就是這個矛盾——量表全滿，送出去卻一直
/// 回「hit your usage limit」。所以把 CLI 講的話單獨記一格，並且**黏住**：每 5 分鐘一次的
/// app-server 輪詢不帶這個欄位，[`set`] 會沿用舊值，直到 `until` 過了或下一回合真的跑成功。
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LimitHit {
    /// CLI 印的那一句（已經接好換行的完整橫幅）。
    pub message: String,
    /// 橫幅裡的 `try again at …`（RFC3339）；沒寫時間就 `None`，那就只能等下一次成功的回合清掉。
    pub until: Option<String>,
    /// 什麼時候撞到的（RFC3339）。
    pub at: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Max 方案才有的 Fable 週額度（`Current week (Fable)`）。跟 [`Quota::seven_day`] 同型
    /// 也同樣是週窗，只是只算 Fable 那一份；沒有這條桶子的方案／來源就是 `None`，UI 完全不畫。
    pub fable: Option<Window>,
    /// Codex 的額度重置券；只有 codex 這個來源會有，其餘一律 `None`。
    pub reset_credits: Option<ResetCredits>,
    /// CLI 說這個帳號現在被擋住了。見 [`LimitHit`]；沒撞到就是 `None`。
    pub limit_hit: Option<LimitHit>,
    pub plan: Option<String>,
    pub updated_at: String,
    pub source: String,
    pub account: Option<String>,
    /// Host this was read on. Parsers build `local`; [`set`] stamps the real one, so no call
    /// site can store a remote reading under a local key by forgetting a field.
    pub host: String,
}

fn unix_to_rfc3339(v: Option<&Value>) -> Option<String> {
    let secs = match v? {
        Value::Number(n) => n.as_f64()? as i64,
        Value::String(s) => {
            if let Ok(n) = s.parse::<i64>() {
                n
            } else {
                // Already a timestamp string? Keep it.
                return Some(s.clone());
            }
        }
        _ => return None,
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
}

/// `rateLimitResetCredits` → [`ResetCredits`]。`availableCount` 才是張數；`credits[]` 裡可能
/// 還有已經用掉／過期的，所以標題與到期時間只取第一張 `status == "available"` 的。
fn reset_credits(result: &Value) -> Option<ResetCredits> {
    let rc = result.get("rateLimitResetCredits")?;
    let available = rc.get("availableCount").and_then(|x| x.as_i64()).unwrap_or(0);
    let first = rc
        .get("credits")
        .and_then(|x| x.as_array())
        .and_then(|a| a.iter().find(|c| c.get("status").and_then(|s| s.as_str()) == Some("available")));
    Some(ResetCredits {
        available,
        title: first.and_then(|c| c.get("title")).and_then(|x| x.as_str()).map(String::from),
        expires_at: first.and_then(|c| unix_to_rfc3339(c.get("expiresAt"))),
    })
}

/// `account/rateLimits/read` result → Quota. Windows are matched by `windowDurationMins`
/// (300 → 5 h, 10080 → 7 d), falling back to primary/secondary order.
pub fn quota_from_codex(result: &Value) -> Option<Quota> {
    let rl = result.get("rateLimits")?;
    let window = |v: Option<&Value>| -> Option<Window> {
        let v = v?;
        Some(Window { used_pct: v.get("usedPercent")?.as_f64()?, resets_at: unix_to_rfc3339(v.get("resetsAt")) })
    };
    let mins = |v: Option<&Value>| v.and_then(|x| x.get("windowDurationMins")).and_then(|m| m.as_i64());
    let (p, s) = (rl.get("primary"), rl.get("secondary"));
    let mut five = None;
    let mut seven = None;
    for w in [p, s] {
        match mins(w) {
            Some(300) => five = window(w),
            Some(10080) => seven = window(w),
            _ => {}
        }
    }
    if five.is_none() && seven.is_none() {
        five = window(p);
        seven = window(s);
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        fable: None,
        reset_credits: reset_credits(result),
        limit_hit: None,
        plan: rl.get("planType").and_then(|x| x.as_str()).map(String::from),
        updated_at: crate::db::now(),
        source: "codex-app-server".into(),
        account: None,
        host: LOCAL_HOST.into(),
    })
}

/// Claude Code statusLine JSON (`rate_limits.five_hour / seven_day`) → Quota.
pub fn quota_from_statusline(payload: &Value, account: Option<&str>) -> Option<Quota> {
    let rl = payload.get("rate_limits")?;
    let window = |v: Option<&Value>| -> Option<Window> {
        let v = v?;
        Some(Window {
            used_pct: v.get("used_percentage")?.as_f64()?,
            resets_at: unix_to_rfc3339(v.get("resets_at")),
        })
    };
    let five = window(rl.get("five_hour"));
    let seven = window(rl.get("seven_day"));
    // Max 方案的 Fable 週桶。實測（2026-09-07）真的 statusLine payload 裡只有 `five_hour` /
    // `seven_day` 兩個 key，所以這裡只是「有就收」——桶名兩種寫法都認，沒有就維持 `None`。
    let fable = window(rl.get("fable")).or_else(|| window(rl.get("seven_day_fable")));
    if five.is_none() && seven.is_none() {
        return None;
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        fable,
        reset_credits: None,
        limit_hit: None,
        plan: None,
        updated_at: crate::db::now(),
        source: "statusline".into(),
        account: account.map(String::from),
        host: LOCAL_HOST.into(),
    })
}

/// Store + push `quota_updated`. `base` is the host-less key (`claude`, `claude:cc1`, …);
/// the stored key and `quota.host` are both derived from `host` here.
pub async fn set(app: &Arc<App>, host: &str, base: &str, mut q: Quota) {
    q.host = host.to_string();
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    // Sources do not all know the same windows. Claude's statusLine reports 5h / 7d only and
    // fires every few seconds while a bot is chatting, so without this it wiped the Fable
    // reading the `/usage` probe had just made (cc1 showed 5h / 7d and no F bar while the idle
    // accounts kept theirs). A window the new reading does not carry keeps the previous value.
    if q.fable.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.fable = prev.fable.clone();
        }
    }
    // 同理：重置券只有 codex 的 app-server 讀得到，別的來源（statusLine）寫進同一把 key 時
    // 不該把它抹掉。
    if q.reset_credits.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.reset_credits = prev.reset_credits.clone();
        }
    }
    // 「撞上限」只有 CLI 的橫幅看得到（§12.4）。app-server 的輪詢每 5 分鐘把同一把 key 蓋掉一次，
    // 沿用之前額度會在 CLI 還在拒絕的時候變回滿格——2026-09-12 使用者看到的就是這個。
    if q.limit_hit.is_none() {
        if let Some(prev) = quotas.get(&key) {
            q.limit_hit = prev.limit_hit.clone();
        }
    }
    if limit_hit_expired(q.limit_hit.as_ref()) {
        q.limit_hit = None;
    }

    quotas.insert(key.clone(), q.clone());
    drop(quotas);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": q})).await;
}

/// 這張「撞上限」已經過了它自己寫的恢復時間了嗎？沒寫時間的一律**不**過期——只能靠下一次
/// 成功的回合（[`clear_limit_hit`]）把它清掉。
fn limit_hit_expired(hit: Option<&LimitHit>) -> bool {
    let Some(until) = hit.and_then(|h| h.until.as_deref()) else { return false };
    match chrono::DateTime::parse_from_rfc3339(until) {
        Ok(t) => chrono::Utc::now() >= t.with_timezone(&chrono::Utc),
        Err(_) => false,
    }
}

/// 這個帳號又能跑了：一回合真的跑完就把「撞上限」拿掉，不必等它自己寫的時間。
pub async fn clear_limit_hit(app: &Arc<App>, host: &str, base: &str) {
    let key = quota_key(host, base);
    let mut quotas = app.quotas.lock().await;
    let Some(q) = quotas.get_mut(&key) else { return };
    if q.limit_hit.is_none() {
        return;
    }
    q.limit_hit = None;
    let out = q.clone();
    drop(quotas);
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": out})).await;
}

/// `GET /api/quota` body: every known key, with the three base kinds always present **per
/// host** (so the strip can show empty bars for a host that has not reported yet). Keys for
/// hosts that no longer exist are dropped rather than shown under the wrong host.
pub async fn snapshot(app: &Arc<App>) -> Value {
    let hosts = app.hosts.names().await;
    let q = app.quotas.lock().await;
    let mut m = serde_json::Map::new();
    for h in &hosts {
        for k in crate::config::KINDS {
            let key = quota_key(h, k);
            m.insert(key.clone(), q.get(&key).map(|x| json!(x)).unwrap_or(Value::Null));
        }
    }
    for (k, v) in q.iter() {
        // `foo/claude` with no `foo` host left in the config would otherwise be read as a
        // local key by everything downstream — drop it instead.
        let orphan = k.contains('/') && host_of_key(k, &hosts).0 == LOCAL_HOST;
        if !orphan {
            m.insert(k.clone(), json!(v));
        }
    }
    json!({"kinds": Value::Object(m)})
}

/// One codex refresh on `host`. `Ok(false)` = codex not installed there (quota stays null).
pub async fn refresh_codex(app: &Arc<App>, host: &str) -> Result<bool> {
    let r = crate::models::codex_rpc(app, host, "account/rateLimits/read", json!({})).await;
    let r = match r {
        Ok(v) => v,
        Err(e) if e.to_string().contains("is not installed") => return Ok(false),
        Err(e) => return Err(e),
    };
    match quota_from_codex(&r) {
        Some(q) => {
            set(app, host, "codex", q).await;
            Ok(true)
        }
        None => anyhow::bail!("unexpected rateLimits shape: {r}"),
    }
}

/// Start-up + every 5 min, for `local` and every connected remote host.
pub fn spawn_codex_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            for host in pollable_hosts(&app).await {
                match refresh_codex(&app, &host).await {
                    Ok(true) => {}
                    Ok(false) => tracing::info!(host = %host, "codex not installed; codex quota stays null"),
                    Err(e) => tracing::warn!(host = %host, error = %e, "codex quota refresh failed"),
                }
            }
            tokio::time::sleep(CODEX_POLL).await;
        }
    });
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn a_statusline_reading_keeps_the_probes_fable_window() {
        let app = crate::team::testing::env().await.app.clone();
        let probe = Quota {
            five_hour: Some(Window { used_pct: 10.0, resets_at: None }),
            seven_day: Some(Window { used_pct: 20.0, resets_at: None }),
            fable: Some(Window { used_pct: 66.0, resets_at: None }),
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "claude-usage".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", probe).await;
        let status = Quota {
            five_hour: Some(Window { used_pct: 11.0, resets_at: None }),
            seven_day: Some(Window { used_pct: 21.0, resets_at: None }),
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None, updated_at: crate::db::now(), source: "statusline".into(), account: Some("cc1".into()), host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", status).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "claude:cc1")).cloned().unwrap();
        assert_eq!(got.five_hour.unwrap().used_pct, 11.0, "the fresher 5h wins");
        assert_eq!(got.fable.unwrap().used_pct, 66.0, "the Fable window the statusLine cannot see survives");
    }

    use super::*;

    fn codex_q(source: &str, limit_hit: Option<LimitHit>) -> Quota {
        Quota {
            five_hour: Some(Window { used_pct: 0.0, resets_at: None }),
            seven_day: Some(Window { used_pct: 0.0, resets_at: None }),
            fable: None,
            reset_credits: None,
            limit_hit,
            plan: None,
            updated_at: crate::db::now(),
            source: source.into(),
            account: None,
            host: LOCAL_HOST.into(),
        }
    }

    /// 2026-09-12 使用者：codex 的量表全滿、送出去卻一直回「hit your usage limit」。
    /// CLI 的橫幅是唯一看得到這件事的地方（credits 用完時 5h／7d 兩條桶子可以是 0%），
    /// 所以它要黏過五分鐘一次的 app-server 輪詢，直到恢復時間到、或下一回合真的跑成功。
    #[tokio::test]
    async fn a_codex_limit_hit_outlives_the_app_server_poll() {
        let app = crate::team::testing::env().await.app.clone();
        let soon = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let hit = LimitHit { message: "ERROR: You've hit your usage limit.".into(), until: Some(soon), at: crate::db::now() };
        set(&app, LOCAL_HOST, "codex", codex_q("codex-limit-hit", Some(hit))).await;
        // 輪詢回來：桶子空的、而且它根本不知道有這回事。
        set(&app, LOCAL_HOST, "codex", codex_q("codex-app-server", None)).await;
        let got = |app: &std::sync::Arc<crate::state::App>| {
            let app = app.clone();
            async move { app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap() }
        };
        assert!(got(&app).await.limit_hit.is_some(), "量表滿了不代表 CLI 收得下一句話");
        // 一回合真的跑完就清掉。
        clear_limit_hit(&app, LOCAL_HOST, "codex").await;
        assert!(got(&app).await.limit_hit.is_none());
    }

    /// 恢復時間過了就自己消失——不必等下一回合，也不必使用者手動清。
    #[tokio::test]
    async fn a_limit_hit_past_its_reset_time_is_dropped() {
        let app = crate::team::testing::env().await.app.clone();
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        let hit = LimitHit { message: "ERROR: You've hit your usage limit.".into(), until: Some(past), at: crate::db::now() };
        set(&app, LOCAL_HOST, "codex", codex_q("codex-limit-hit", Some(hit))).await;
        let got = app.quotas.lock().await.get(&quota_key(LOCAL_HOST, "codex")).cloned().unwrap();
        assert!(got.limit_hit.is_none(), "過了恢復時間的橫幅不該再擋著畫面");
    }

    /// 沒寫恢復時間的橫幅不會自己過期：codex 當天撞上限時只寫 `try again at 5:07 AM`，
    /// 解析不出來時寧可留著，等下一回合成功再清。
    #[test]
    fn a_limit_hit_without_a_time_never_expires_on_its_own() {
        let hit = LimitHit { message: "ERROR: usage limit".into(), until: None, at: crate::db::now() };
        assert!(!limit_hit_expired(Some(&hit)));
        assert!(!limit_hit_expired(None));
    }

    #[test]
    fn codex_rate_limits_map_by_window() {
        let r = json!({"rateLimits": {
            "primary": {"usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1788650185},
            "secondary": {"usedPercent": 18, "windowDurationMins": 10080, "resetsAt": 1789179340},
            "planType": "plus"
        }});
        let q = quota_from_codex(&r).unwrap();
        assert_eq!(q.five_hour.as_ref().unwrap().used_pct, 0.0);
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 18.0);
        assert!(q.seven_day.unwrap().resets_at.unwrap().starts_with("2026-"));
        assert_eq!(q.plan.as_deref(), Some("plus"));
        assert_eq!(q.source, "codex-app-server");
    }

    /// 2026-09-10 使用者：codex 額度用完時 OpenAI 會給「立刻重置」的券。桶子只說什麼時候
    /// 回血，這張券說「現在就能清掉，還有幾張」——額度歸零的當下那是唯一還能做的事。
    #[test]
    fn codex_reset_credits_are_read_with_the_windows() {
        let r = json!({
            "rateLimits": {
                "primary": {"usedPercent": 100, "windowDurationMins": 300, "resetsAt": 1789074446},
                "secondary": {"usedPercent": 100, "windowDurationMins": 10080, "resetsAt": 1789450308},
                "planType": "plus"
            },
            "rateLimitResetCredits": {
                "availableCount": 1,
                "credits": [
                    {"status": "used", "title": "已經用掉的那張", "expiresAt": 1791173488},
                    {"status": "available", "title": "Full reset (Weekly + 5 hr)", "expiresAt": 1791173488}
                ]
            }
        });
        let c = quota_from_codex(&r).unwrap().reset_credits.unwrap();
        assert_eq!(c.available, 1);
        // 標題取的是**還能用**的那一張，不是陣列的第一個。
        assert_eq!(c.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert!(c.expires_at.unwrap().starts_with("2026-"));
    }

    /// 沒有這個欄位的舊 codex（或別的 kind）＝ `None`，UI 什麼都不畫。
    #[test]
    fn no_reset_credits_field_means_none() {
        let r = json!({"rateLimits": {"primary": {"usedPercent": 3, "windowDurationMins": 300}}});
        assert!(quota_from_codex(&r).unwrap().reset_credits.is_none());
    }

    #[test]
    fn keys_are_host_scoped() {
        assert_eq!(quota_key("local", "claude"), "claude");
        assert_eq!(quota_key("local", "claude:cc1"), "claude:cc1");
        assert_eq!(quota_key("m4p", "claude:cc1"), "m4p/claude:cc1");
        let hosts = vec!["local".to_string(), "m4p".to_string()];
        assert_eq!(host_of_key("claude", &hosts), ("local", "claude"));
        assert_eq!(host_of_key("m4p/claude:cc1", &hosts), ("m4p", "claude:cc1"));
        // A key for a host that is gone must not be read as a local one.
        assert_eq!(host_of_key("gone/claude", &hosts), ("local", "gone/claude"));
    }

    /// The snapshot always carries the three base kinds for every live host, and drops rows
    /// belonging to hosts that no longer exist (rather than folding them into `local`).
    #[tokio::test]
    async fn snapshot_covers_live_hosts_only() {
        let dir = std::env::temp_dir().join(format!("am-quota-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();
        let pool = crate::db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        let app = App::new(
            pool,
            client.clone(),
            client,
            cfg,
            dir.clone(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        );
        let q = Quota {
            five_hour: Some(Window { used_pct: 10.0, resets_at: None }),
            seven_day: None,
            fable: None,
            reset_credits: None,
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "test".into(),
            account: None,
            host: LOCAL_HOST.into(),
        };
        set(&app, LOCAL_HOST, "claude:cc1", q.clone()).await;
        app.quotas.lock().await.insert("gone/claude".into(), q);

        let snap = snapshot(&app).await;
        let kinds = snap["kinds"].as_object().unwrap().clone();
        for k in crate::config::KINDS {
            assert!(kinds.contains_key(k), "missing base kind {k}");
        }
        assert_eq!(kinds["claude:cc1"]["host"], "local");
        assert!(!kinds.contains_key("gone/claude"), "orphan host key was kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn statusline_maps() {
        let p = json!({"hook_event_name":"StatusLine","rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, Some("cc1")).unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 3.5);
        assert!(q.fable.is_none());
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "statusline");
        assert!(quota_from_statusline(&json!({"model": {}}), None).is_none());
    }

    /// 若哪天 statusLine 真的多一個 fable 桶，它要落在 `fable`，不能蓋掉週窗。
    #[test]
    fn statusline_picks_up_a_fable_bucket_if_it_appears() {
        let p = json!({"rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340},
            "fable":{"used_percentage":61,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, None).unwrap();
        assert_eq!(q.seven_day.unwrap().used_pct, 22.0);
        assert_eq!(q.fable.unwrap().used_pct, 61.0);
    }
}
