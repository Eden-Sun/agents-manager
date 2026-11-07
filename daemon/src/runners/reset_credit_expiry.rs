//! codex 重置券到期前一天提示（2026-11-07 使用者：「在要到期的前一天提示我」）。
//!
//! 不依賴網頁開著：daemon 每 [`EVERY`] 掃一次記憶體裡的額度讀數，每張 `available`／`redeeming` 的券
//! 進到到期前 [`WINDOW_HOURS`] 小時內就推**一則** AGM inbox `ops_alert`（`source=daemon`，
//! `reason=codex_reset_credit_expiring`），由 AGM 轉告使用者。去重靠 `supervisor_inbox.event_key`
//! （`ops_alert:daemon:codex_reset_credit_expiring:<券 id>`，`INSERT OR IGNORE`）：重啟、多次讀數、
//! 同一張券出現在好幾個 key 都只推一次。沒有 id 的券（舊讀數）不推，免得去重失效變成洗版。
//! 網頁開著時另有 `resetCreditsNotice` 跳 toast（localStorage 去重，兩邊互不依賴）。

use crate::state::App;
use crate::quota::{self, Quota};
use std::collections::BTreeSet;
use std::sync::Arc;

/// 掃描週期。窗口是 24 小時，10 分鐘一輪綽綽有餘，也不必等額度輪詢。
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(600);
/// 到期前多久開始提示。
pub const WINDOW_HOURS: i64 = 24;
/// 使用者時區（UTC+8，使用者 2026-10-31 裁示；目前固定）。
const USER_TZ_SECS: i32 = 8 * 3600;

#[derive(Debug, Clone, PartialEq)]
pub struct Due {
    pub key: String,
    pub id: String,
    pub title: Option<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

/// 哪些券該提示：`codex` 的額度 key、非 stale、有 id、尚未過期且 24 小時內到期。同一個 id 只回一筆（取字典序最小的 key）。
pub fn due_credits(quotas: &std::collections::BTreeMap<String, Quota>, stale: &BTreeSet<String>, now: chrono::DateTime<chrono::Utc>) -> Vec<Due> {
    let horizon = now + chrono::Duration::hours(WINDOW_HOURS);
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (key, q) in quotas {
        let kind = key.rsplit('/').next().unwrap_or(key).split(':').next().unwrap_or("");
        if kind != "codex" || stale.contains(key) {
            continue;
        }
        let Some(rc) = &q.reset_credits else { continue };
        for c in &rc.credits {
            if c.id.is_empty() || (c.status != "available" && c.status != "redeeming") {
                continue;
            }
            let Some(at) = c.expires_at.as_deref().and_then(quota::parse_utc) else { continue };
            if at <= now || at > horizon || !seen.insert(c.id.clone()) {
                continue;
            }
            out.push(Due { key: key.clone(), id: c.id.clone(), title: c.title.clone(), expires_at: at });
        }
    }
    out
}

/// `m4p/codex:co0` → `co0 @m4p`；`codex` → `codex`。
fn label(key: &str) -> String {
    let (host, rest) = key.split_once('/').map_or(("", key), |(h, r)| (h, r));
    let identity = rest.split_once(':').map(|(_, i)| i);
    match (identity, host) {
        (Some(i), "") => i.to_string(),
        (Some(i), h) => format!("{i} @{h}"),
        (None, "") => rest.to_string(),
        (None, h) => format!("{rest} @{h}"),
    }
}

pub fn message(d: &Due, now: chrono::DateTime<chrono::Utc>) -> String {
    let tz = chrono::FixedOffset::east_opt(USER_TZ_SECS).expect("valid offset");
    let when = d.expires_at.with_timezone(&tz).format("%m/%d %H:%M");
    let hours = ((d.expires_at - now).num_minutes() + 59) / 60;
    format!(
        "codex 重置券『{}』{when} (+8) 到期（約 {hours} 小時後；{}），在 codex 用 /status → Reset usage",
        d.title.as_deref().unwrap_or("Reset"),
        label(&d.key),
    )
}

pub fn event_key(id: &str) -> String {
    format!("ops_alert:daemon:codex_reset_credit_expiring:{id}")
}

/// 掃一輪，回這輪新推的張數。
pub async fn tick(app: &Arc<App>) -> usize {
    let now = chrono::Utc::now();
    let due = {
        let q = app.quotas.lock().await;
        let stale = app.quota_stale.lock().await;
        due_credits(&q, &stale, now)
    };
    let mut pushed = 0;
    for d in due {
        let text = message(&d, now);
        let payload = serde_json::json!({
            "source": "daemon",
            "reason": "codex_reset_credit_expiring",
            "subject": d.key,
            "detail": text,
            "action": "只是通知（每張券只推一次）：請把這句話轉告使用者；daemon 不會替使用者用掉重置券。",
        });
        match crate::supervisor::store::push_inbox(&app.db, &event_key(&d.id), "ops_alert", None, None, None, &payload).await {
            Ok(Some(_)) => pushed += 1,
            Ok(None) => {}
            Err(e) => tracing::warn!(key = %d.key, credit = %d.id, error = %e, "could not queue the codex_reset_credit_expiring ops_alert"),
        }
    }
    pushed
}

/// 開機後起一條背景迴圈。測試不起。
pub fn spawn_ticker(app: &Arc<App>) {
    if cfg!(test) {
        return;
    }
    let app = app.clone();
    let tasks = app.background_tasks.clone();
    crate::background_loop::spawn_named(&tasks, "codex reset credit expiry ticker", async move {
        let mut every = tokio::time::interval(EVERY);
        loop {
            tokio::select! {
                _ = app.shutdown.cancelled() => return,
                _ = every.tick() => {}
            }
            tick(&app).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{ResetCredit, ResetCredits};

    fn credit(id: &str, status: &str, expires: Option<chrono::DateTime<chrono::Utc>>) -> ResetCredit {
        ResetCredit {
            id: id.into(),
            title: Some("Full reset".into()),
            status: status.into(),
            granted_at: None,
            expires_at: expires.map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
        }
    }

    fn quota(credits: Vec<ResetCredit>) -> Quota {
        Quota {
            five_hour: None,
            seven_day: None,
            fable: None,
            reset_credits: Some(ResetCredits { available: credits.len() as i64, title: None, expires_at: None, credits }),
            limit_hit: None,
            plan: None,
            updated_at: crate::db::now(),
            source: "codex-app-server".into(),
            account: None,
            host: "local".into(),
        }
    }

    fn at(h: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc::now() + chrono::Duration::minutes(h * 60)
    }

    #[test]
    fn only_credits_expiring_within_24h_are_due() {
        let now = chrono::Utc::now();
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "m4p/codex:co0".to_string(),
            quota(vec![
                credit("soon", "available", Some(at(23))),
                credit("later", "available", Some(at(25))),
                credit("expired", "available", Some(at(-1))),
                credit("used", "used", Some(at(5))),
                credit("noexp", "available", None),
                credit("", "available", Some(at(2))),
            ]),
        );
        let due = due_credits(&m, &BTreeSet::new(), now);
        assert_eq!(due.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), vec!["soon"]);
    }

    #[test]
    fn stale_and_non_codex_readings_are_ignored_and_an_id_is_listed_once() {
        let now = chrono::Utc::now();
        let c = credit("a", "available", Some(at(3)));
        let mut m = std::collections::BTreeMap::new();
        m.insert("codex".to_string(), quota(vec![c.clone()]));
        m.insert("codex:co0".to_string(), quota(vec![c.clone()]));
        m.insert("claude".to_string(), quota(vec![c.clone()]));
        m.insert("codex:old".to_string(), quota(vec![credit("b", "available", Some(at(3)))]));
        let stale: BTreeSet<String> = ["codex:old".to_string()].into();
        let due = due_credits(&m, &stale, now);
        assert_eq!(due.len(), 1);
        assert_eq!((due[0].id.as_str(), due[0].key.as_str()), ("a", "codex"));
    }

    #[test]
    fn message_names_the_credit_identity_and_how_to_use_it() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-12-01T10:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let d = Due { key: "m4p/codex:co0".into(), id: "x".into(), title: Some("Full reset".into()), expires_at: now + chrono::Duration::hours(17) };
        let m = message(&d, now);
        assert!(m.contains("『Full reset』12/02 11:00 (+8) 到期"), "{m}");
        assert!(m.contains("co0 @m4p") && m.contains("/status → Reset usage"), "{m}");
    }

    #[tokio::test]
    async fn each_credit_is_pushed_once_even_across_ticks() {
        let e = crate::testing::env().await;
        e.app.quotas.lock().await.insert("m4p/codex:co0".into(), quota(vec![credit("c1", "available", Some(at(20))), credit("c2", "available", Some(at(60)))]));
        assert_eq!(tick(&e.app).await, 1);
        assert_eq!(tick(&e.app).await, 0, "同一張不再推");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE event_key = ?")
            .bind(event_key("c1"))
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(n, 1);
        let outside: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE event_key = ?")
            .bind(event_key("c2"))
            .fetch_one(&e.app.db)
            .await
            .unwrap();
        assert_eq!(outside, 0, "24h 外不推");
    }
}
