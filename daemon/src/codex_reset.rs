//! Public reset announcements, separate from account-specific quota windows.
use crate::state::App;
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{path::Path, sync::Arc, time::Duration};

pub const SOURCE: &str = "https://codex-reset.com/";
const EVERY: Duration = Duration::from_secs(30 * 60);
const MAX_BYTES: usize = 1024 * 1024;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct State {
    initialized: bool,
    seen: Vec<String>,
    pub snapshot: Option<Snapshot>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Snapshot {
    pub source: String,
    pub checked_at: String,
    pub source_updated_at: String,
    pub last_reset_at: Option<String>,
    pub upcoming: bool,
    pub latest: Option<News>,
    pub notice: Option<News>,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct News {
    pub id: String,
    pub at: String,
    pub text: String,
    pub url: String,
}

pub fn load(dir: &Path) -> State {
    std::fs::read(dir.join("codex-reset.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn timestamp(v: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn iso(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn fresh(v: &Value, now: DateTime<Utc>) -> bool {
    timestamp(v).is_some_and(|d| {
        let age = now.signed_duration_since(d).num_seconds();
        (-300..=90 * 60).contains(&age)
    })
}

fn source_url(v: &Value) -> String {
    v.as_str()
        .and_then(|s| reqwest::Url::parse(s).ok())
        .filter(|u| {
            u.scheme() == "https"
                && matches!(
                    u.host_str(),
                    Some("x.com" | "twitter.com" | "codex-reset.com")
                )
        })
        .map(|u| u.to_string())
        .unwrap_or_else(|| SOURCE.into())
}

/// First successful read seeds the baseline. Subsequent reads announce each new item once.
fn update(old: &State, forecast: &Value, feed: &Value, now: DateTime<Utc>) -> Result<State> {
    ensure!(
        fresh(&forecast["updated_at"], now),
        "reset forecast is stale or has no timestamp"
    );
    ensure!(
        feed["stale"] == false && fresh(&feed["fetched_at"], now),
        "reset feed is stale"
    );
    let posts = feed["tweets"]
        .as_array()
        .context("reset feed has no tweets array")?;
    let mut news: Vec<News> = posts
        .iter()
        .take(200)
        .filter_map(|v| {
            if !matches!(
                v["kind"].as_str(),
                Some("signal" | "reset" | "boost" | "limits" | "credits" | "banked" | "codex")
            ) {
                return None;
            }
            let id = v["id"].as_str()?.to_owned();
            if id.is_empty() || id.len() > 160 {
                return None;
            }
            let at = timestamp(&v["at"])?;
            if at > now + chrono::Duration::minutes(5) {
                return None;
            }
            let text: String = v["text"]
                .as_str()?
                .chars()
                .filter(|c| !c.is_control() || *c == '\n')
                .take(280)
                .collect();
            if text.trim().is_empty() {
                return None;
            }
            Some(News {
                id,
                at: iso(at),
                text,
                url: source_url(&v["url"]),
            })
        })
        .collect();
    let last_reset = timestamp(&forecast["last_reset_at"]).filter(|d| *d <= now);
    if let Some(at) = last_reset {
        news.push(News {
            id: format!("reset:{}", iso(at)),
            at: iso(at),
            text: "Codex 全域重置已確認；個人額度請以 /status 為準".into(),
            url: SOURCE.into(),
        });
    }
    let signal = &forecast["official_signal"];
    let upcoming = match signal {
        Value::Object(v) => !v.is_empty(),
        Value::String(v) => !v.trim().is_empty(),
        _ => false,
    };
    if upcoming {
        // Age/probability fields may change every poll without a new announcement.
        let identity = match signal {
            Value::Object(v) => serde_json::json!({
                "id":v.get("id"), "url":v.get("url"), "at":v.get("at"),
                "source_at":v.get("source_at"), "text":v.get("text"), "summary":v.get("summary"),
                "reset":forecast["last_reset_at"],
            }),
            _ => signal.clone(),
        };
        let fingerprint = identity
            .to_string()
            .bytes()
            .fold(0xcbf29ce484222325u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
            });
        news.push(News {
            id: format!("upcoming:{fingerprint:x}"),
            at: iso(now),
            text: "Codex 有重置預告，尚未確認生效；請查看公告來源".into(),
            url: SOURCE.into(),
        });
    }
    news.sort_by(|a, b| b.at.cmp(&a.at));
    let new = news.iter().find(|n| !old.seen.contains(&n.id));
    let notice = if old.initialized {
        new.cloned()
            .or_else(|| old.snapshot.as_ref().and_then(|s| s.notice.clone()))
    } else {
        None
    };
    let mut seen = old.seen.clone();
    for n in &news {
        if !seen.contains(&n.id) {
            seen.push(n.id.clone());
        }
    }
    // Keep chronological insertion order; the upstream feed is bounded, as is our restart ledger.
    if seen.len() > 1000 {
        seen.drain(..seen.len() - 1000);
    }
    Ok(State {
        initialized: true,
        seen,
        snapshot: Some(Snapshot {
            source: SOURCE.into(),
            checked_at: iso(now),
            source_updated_at: iso(timestamp(&forecast["updated_at"]).unwrap()),
            last_reset_at: last_reset.map(iso),
            upcoming,
            latest: news.first().cloned(),
            notice,
        }),
    })
}

async fn get(client: &reqwest::Client, path: &str) -> Result<Value> {
    let mut r = client
        .get(format!("{SOURCE}{path}"))
        .send()
        .await?
        .error_for_status()?;
    let mut bytes = Vec::new();
    while let Some(chunk) = r.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= MAX_BYTES,
            "reset response exceeds size limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(serde_json::from_slice(&bytes)?)
}

async fn tick(app: Arc<App>) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("agents-manager/1.0 (+https://github.com/Eden-Sun/agents-manager)")
        .build()?;
    let (forecast, feed) =
        tokio::try_join!(get(&client, "api/forecast"), get(&client, "api/feed"))?;
    let mut state = app.codex_resets.lock().await;
    let next = update(&state, &forecast, &feed, Utc::now())?;
    let bytes = serde_json::to_vec(&next)?;
    let path = app.data_dir.join("codex-reset.json");
    tokio::task::spawn_blocking(move || {
        am_base::atomic_file::write(&path, &bytes, am_base::atomic_file::Mode::Preserve)
    })
    .await??;
    let view = serde_json::to_value(&next.snapshot)?;
    *state = next;
    drop(state);
    app.emit("codex_reset_updated", view).await;
    Ok(())
}

pub fn spawn(app: Arc<App>) {
    crate::background_loop::spawn_periodic(
        &app,
        "codex reset announcements",
        EVERY,
        Duration::ZERO,
        |app| async move {
            if let Err(e) = tick(app).await {
                tracing::warn!(error = %e, "Codex reset refresh failed; retaining previous announcements");
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn payloads(now: DateTime<Utc>, id: &str) -> (Value, Value) {
        (
            json!({"updated_at":iso(now),"last_reset_at":null,"official_signal":null,"probabilities":{"rounded_24h":99}}),
            json!({"fetched_at":iso(now),"stale":false,"tweets":[{"id":id,"kind":"signal","at":iso(now),"text":"Global reset later today","url":"https://x.com/thsottiaux/status/1"}]}),
        )
    }
    #[test]
    fn baseline_restart_and_repeat_do_not_repeat_notifications() {
        let now = Utc::now();
        let (f, b) = payloads(now, "1");
        let seeded = update(&State::default(), &f, &b, now).unwrap();
        assert!(seeded.snapshot.as_ref().unwrap().notice.is_none());
        let (_, b) = payloads(now, "2");
        let changed = update(&seeded, &f, &b, now).unwrap();
        assert_eq!(
            changed
                .snapshot
                .as_ref()
                .unwrap()
                .notice
                .as_ref()
                .unwrap()
                .id,
            "2"
        );
        let restarted: State =
            serde_json::from_slice(&serde_json::to_vec(&changed).unwrap()).unwrap();
        let repeated = update(&restarted, &f, &b, now).unwrap();
        assert_eq!(repeated.snapshot.unwrap().notice.unwrap().id, "2");
    }
    #[test]
    fn forecast_odds_and_banked_posts_do_not_promise_a_reset() {
        let now = Utc::now();
        let (mut f, mut b) = payloads(now, "1");
        b["tweets"][0]["kind"] = json!("banked");
        assert!(
            !update(&State::default(), &f, &b, now)
                .unwrap()
                .snapshot
                .unwrap()
                .upcoming
        );
        f["official_signal"] = json!({"summary":"Global reset later today"});
        assert!(
            update(&State::default(), &f, &b, now)
                .unwrap()
                .snapshot
                .unwrap()
                .upcoming
        );
        b["stale"] = json!(true);
        assert!(update(&State::default(), &f, &b, now).is_err());
    }
    #[test]
    fn announcement_age_changes_do_not_create_another_notice() {
        let now = Utc::now();
        let (mut f, b) = payloads(now, "1");
        let baseline = update(&State::default(), &f, &b, now).unwrap();
        f["official_signal"] = json!({"id":"post-2","summary":"reset soon","age_hours":1});
        let announced = update(&baseline, &f, &b, now).unwrap();
        let id = announced
            .snapshot
            .as_ref()
            .unwrap()
            .notice
            .as_ref()
            .unwrap()
            .id
            .clone();
        assert!(id.starts_with("upcoming:"));
        f["official_signal"]["age_hours"] = json!(2);
        let repeated = update(&announced, &f, &b, now).unwrap();
        assert_eq!(repeated.seen, announced.seen);
        assert_eq!(repeated.snapshot.unwrap().notice.unwrap().id, id);
    }
    #[tokio::test]
    async fn persisted_snapshot_and_state_api_survive_reloading() {
        let e = crate::testing::env().await;
        let now = Utc::now();
        let (f, b) = payloads(now, "1");
        let baseline = update(&State::default(), &f, &b, now).unwrap();
        let (_, b) = payloads(now, "2");
        let next = update(&baseline, &f, &b, now).unwrap();
        am_base::atomic_file::write(
            &e.app.data_dir.join("codex-reset.json"),
            &serde_json::to_vec(&next).unwrap(),
            am_base::atomic_file::Mode::Preserve,
        )
        .unwrap();
        let restored = load(&e.app.data_dir);
        assert_eq!(restored.seen, next.seen);
        *e.app.codex_resets.lock().await = restored;
        let view = crate::api::state_json(&e.app).await.unwrap();
        assert_eq!(view["codex_reset"]["notice"]["id"], "2");
        assert_eq!(view["codex_reset"]["source"], SOURCE);
        assert!(e.app.quotas.lock().await.is_empty());
    }
    #[test]
    fn confirmed_reset_notifies_and_untrusted_links_are_replaced() {
        let now = Utc::now();
        let (mut f, mut b) = payloads(now, "1");
        let baseline = update(&State::default(), &f, &b, now).unwrap();
        f["last_reset_at"] = json!(iso(now));
        b["tweets"][0]["url"] = json!("javascript:alert(1)");
        let next = update(&baseline, &f, &b, now).unwrap().snapshot.unwrap();
        assert!(next.notice.unwrap().id.starts_with("reset:"));
        assert_eq!(next.last_reset_at, Some(iso(now)));
        assert!(next.latest.unwrap().url.starts_with("https://"));
        f["updated_at"] = json!(iso(now - chrono::Duration::hours(2)));
        assert!(update(&baseline, &f, &b, now).is_err());
    }
}
