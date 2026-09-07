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

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    /// Max 方案才有的 Fable 週額度（`Current week (Fable)`）。跟 [`Quota::seven_day`] 同型
    /// 也同樣是週窗，只是只算 Fable 那一份；沒有這條桶子的方案／來源就是 `None`，UI 完全不畫。
    pub fable: Option<Window>,
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
    app.quotas.lock().await.insert(key.clone(), q.clone());
    app.emit("quota_updated", json!({"kind": key, "host": host, "quota": q})).await;
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
    use super::*;

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
        );
        let q = Quota {
            five_hour: Some(Window { used_pct: 10.0, resets_at: None }),
            seven_day: None,
            fable: None,
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
