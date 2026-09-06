//! v4.0 — rate-limit quota per kind (`GET /api/quota`, WS `quota_updated`).
//!
//! * codex: polled from the local `codex app-server` (`account/rateLimits/read`) at start-up,
//!   every 5 min and on `?refresh=1`.
//! * claude: pushed by the daemon-injected statusLine command (`StatusLine` hook event);
//!   keyed `claude`, plus `claude:<identity>` when the bot runs under an identity.
//! * grok: scraped from the TUI's `/usage` dialog in a throwaway pane every 30 s — see
//!   [`crate::quota_grok`]; grok exposes no CLI or RPC surface for it.

use crate::state::App;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const CODEX_POLL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Window {
    pub used_pct: f64,
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Quota {
    pub five_hour: Option<Window>,
    pub seven_day: Option<Window>,
    pub plan: Option<String>,
    pub updated_at: String,
    pub source: String,
    pub account: Option<String>,
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
        plan: rl.get("planType").and_then(|x| x.as_str()).map(String::from),
        updated_at: crate::db::now(),
        source: "codex-app-server".into(),
        account: None,
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
    if five.is_none() && seven.is_none() {
        return None;
    }
    Some(Quota {
        five_hour: five,
        seven_day: seven,
        plan: None,
        updated_at: crate::db::now(),
        source: "statusline".into(),
        account: account.map(String::from),
    })
}

/// Store + push `quota_updated`.
pub async fn set(app: &Arc<App>, kind_key: &str, q: Quota) {
    app.quotas.lock().await.insert(kind_key.to_string(), q.clone());
    app.emit("quota_updated", json!({"kind": kind_key, "quota": q})).await;
}

/// `GET /api/quota` body: every known key, with the three base kinds always present.
pub async fn snapshot(app: &Arc<App>) -> Value {
    let q = app.quotas.lock().await;
    let mut m = serde_json::Map::new();
    for k in crate::config::KINDS {
        m.insert(k.to_string(), q.get(k).map(|x| json!(x)).unwrap_or(Value::Null));
    }
    for (k, v) in q.iter() {
        m.insert(k.clone(), json!(v));
    }
    json!({"kinds": Value::Object(m)})
}

/// One codex refresh (local host). `Ok(false)` = codex not installed here (quota stays null).
pub async fn refresh_codex(app: &Arc<App>) -> Result<bool> {
    let r = crate::models::codex_rpc(app, crate::config::LOCAL_HOST, "account/rateLimits/read", json!({})).await;
    let r = match r {
        Ok(v) => v,
        Err(e) if e.to_string().contains("is not installed") => return Ok(false),
        Err(e) => return Err(e),
    };
    match quota_from_codex(&r) {
        Some(q) => {
            set(app, "codex", q).await;
            Ok(true)
        }
        None => anyhow::bail!("unexpected rateLimits shape: {r}"),
    }
}

/// Start-up + every 5 min.
pub fn spawn_codex_poller(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            match refresh_codex(&app).await {
                Ok(true) => {}
                Ok(false) => tracing::info!("codex not installed locally; codex quota stays null"),
                Err(e) => tracing::warn!(error = %e, "codex quota refresh failed"),
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
    fn statusline_maps() {
        let p = json!({"hook_event_name":"StatusLine","rate_limits":{
            "five_hour":{"used_percentage":3.5,"resets_at":1788650185},
            "seven_day":{"used_percentage":22,"resets_at":1789179340}}});
        let q = quota_from_statusline(&p, Some("cc1")).unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 3.5);
        assert_eq!(q.account.as_deref(), Some("cc1"));
        assert_eq!(q.source, "statusline");
        assert!(quota_from_statusline(&json!({"model": {}}), None).is_none());
    }
}
