//! `/api/build-slots` 的外部編譯人數與本機＋遠端合計，以及有變化時推 `build_slots_updated`。
//!
//! 每台建置主機的 `active`／`waiting` 來自 [`crate::remote_cargo_runs::slot_counts`]，一次最多 [`PROBE_LIMIT`]、
//! 結果快取 [`CACHE_FOR`]；讀不到是 `null`，不擋回應也不算進合計。

use crate::config::{BuildRemoteCfg, LOCAL_BUILD_HOST};
use crate::state::App;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

const CACHE_FOR: Duration = Duration::from_secs(5);
const PROBE_LIMIT: Duration = Duration::from_secs(2);
const PUSH_EVERY: Duration = Duration::from_secs(5);
pub const EVENT: &str = "build_slots_updated";

type Counts = Option<(u64, u64)>;
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Counts)>>> = LazyLock::new(Default::default);

fn cache_key(h: &BuildRemoteCfg) -> String {
    format!("{}|{}", h.target_label(), h.remote_root)
}

/// 指到 daemon 這台自己的熱池（同 `BuildRemoteCfg::same_machine_slot_pool` 的判斷）。
fn same_machine(h: &BuildRemoteCfg) -> bool {
    matches!(h.host.trim(), LOCAL_BUILD_HOST | "127.0.0.1" | "localhost")
}

async fn counts_of<F>(h: BuildRemoteCfg, data_dir: PathBuf, probe: Arc<F>) -> Counts
where
    F: Fn(&BuildRemoteCfg, &std::path::Path) -> anyhow::Result<(u64, u64)> + Send + Sync + 'static,
{
    let key = cache_key(&h);
    if let Some((at, c)) = CACHE.lock().ok().and_then(|m| m.get(&key).copied()) {
        if at.elapsed() < CACHE_FOR {
            return c;
        }
    }
    let job = tokio::task::spawn_blocking(move || probe(&h, &data_dir).ok());
    let c = tokio::time::timeout(PROBE_LIMIT + Duration::from_millis(500), job).await.ok().and_then(|r| r.ok()).flatten();
    if let Ok(mut m) = CACHE.lock() {
        m.insert(key, (Instant::now(), c));
    }
    c
}

/// 在 [`crate::build_scheduler::status`] 的結果上補 `remote.hosts[].active／waiting`、`remote_runs`、`total_active／total_waiting`。
pub async fn enrich(app: &App, v: &mut Value) {
    let probe = Arc::new(|h: &BuildRemoteCfg, d: &std::path::Path| crate::remote_cargo_runs::slot_counts(h, d, PROBE_LIMIT));
    enrich_with(app, v, probe).await
}

pub(crate) async fn enrich_with<F>(app: &App, v: &mut Value, probe: Arc<F>)
where
    F: Fn(&BuildRemoteCfg, &std::path::Path) -> anyhow::Result<(u64, u64)> + Send + Sync + 'static,
{
    let cfg = app.cfg.build_fresh().await;
    let targets = if cfg.remote.enabled { cfg.remote.targets() } else { Vec::new() };
    let local_pool = std::env::var_os("HOME")
        .and_then(|home| cfg.remote.same_machine_slot_pool(cfg.max_concurrent(), std::path::Path::new(&home)))
        .is_some();
    let counts = futures::future::join_all(
        targets.iter().map(|h| counts_of(h.clone(), app.data_dir.clone(), probe.clone())),
    )
    .await;
    let slots = v["slots"].as_array().cloned().unwrap_or_default();
    let local_active = slots.iter().filter(|s| s["status"] == "held").count() as u64;
    let local_waiting = slots.iter().filter(|s| s["status"] == "waiting").count() as u64;
    let (mut total_active, mut total_waiting) = (local_active, local_waiting);
    let mut pool_seen = false;
    if let Some(hosts) = v["remote"]["hosts"].as_array_mut() {
        for (row, (h, c)) in hosts.iter_mut().zip(targets.iter().zip(counts)) {
            if row["target"] != json!(h.target_label()) {
                continue;
            }
            let shares_local = local_pool && same_machine(h) && !pool_seen;
            pool_seen |= same_machine(h);
            let c = c.map(|(a, w)| (if shares_local { a.saturating_sub(local_active) } else { a }, w));
            row["active"] = json!(c.map(|c| c.0));
            row["waiting"] = json!(c.map(|c| c.1));
            if let Some((a, w)) = c {
                total_active += a;
                total_waiting += w;
            }
        }
    }
    v["remote_runs"] = json!(crate::remote_cargo_runs::list(&app.data_dir));
    v["total_active"] = json!(total_active);
    v["total_waiting"] = json!(total_waiting);
}

/// 推送要比的那幾個欄位：人數、每格的 holder／狀態、外部編譯明細。`expires_at`／`last_seen` 每次續約都變，不算。
fn digest(v: &Value) -> Value {
    let slots: Vec<Value> = v["slots"]
        .as_array()
        .map(|a| a.iter().map(|s| json!([s["holder"], s["status"]])).collect())
        .unwrap_or_default();
    let hosts: Vec<Value> = v["remote"]["hosts"]
        .as_array()
        .map(|a| a.iter().map(|h| json!([h["target"], h["active"], h["waiting"]])).collect())
        .unwrap_or_default();
    let runs: Vec<Value> = v["remote_runs"]
        .as_array()
        .map(|a| a.iter().map(|r| json!([r["call_id"], r["target"]])).collect())
        .unwrap_or_default();
    json!([v["total_active"], v["total_waiting"], slots, hosts, runs])
}

pub async fn snapshot(app: &App) -> anyhow::Result<Value> {
    let mut v = crate::build_scheduler::status(app).await?;
    enrich(app, &mut v).await;
    Ok(v)
}

/// 有人在聽 WS 時每 [`PUSH_EVERY`] 看一次，跟上次推的不同才推。
pub fn spawn_push(app: Arc<App>) {
    let last: Arc<tokio::sync::Mutex<Option<Value>>> = Arc::default();
    crate::background_loop::spawn_periodic(&app, "build slots push", PUSH_EVERY, PUSH_EVERY, move |app| {
        let last = last.clone();
        async move {
            if app.ws_listeners() == 0 {
                return;
            }
            let Ok(v) = snapshot(&app).await else { return };
            let d = digest(&v);
            let mut last = last.lock().await;
            if last.as_ref() != Some(&d) {
                *last = Some(d);
                app.emit(EVENT, v).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing as tt;

    async fn slot(app: &App, holder: &str, status: &str) {
        let held = status == "held";
        sqlx::query(
            "INSERT INTO build_slots (holder, token, bot_id, purpose, host, status, since, last_seen, expires_at)
             VALUES (?, 'tok', NULL, 'test', 'local', ?, ?, ?, ?)",
        )
        .bind(holder)
        .bind(status)
        .bind(crate::db::now())
        .bind(crate::db::now())
        .bind(held.then(|| (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true)))
        .execute(&app.db)
        .await
        .unwrap();
    }

    async fn two_remote_hosts(app: &App, first: &str, second: &str) {
        let (first, second) = (first.to_string(), second.to_string());
        app.cfg
            .update(move |cfg| {
                cfg.build.remote.enabled = true;
                cfg.build.remote.user = "me".into();
                cfg.build.remote.host = first;
                cfg.build.remote.hosts = vec![serde_json::from_value(json!({"host": second, "user": "me"})).unwrap()];
                Ok(())
            })
            .await
            .unwrap();
    }

    /// 合計＝本機 held／waiting＋每台遠端的鎖數；每台的數字寫在 `remote.hosts[]`。
    #[tokio::test]
    async fn totals_add_local_slots_and_every_remote_host() {
        let env = tt::env().await;
        let app = env.app.clone();
        two_remote_hosts(&app, "sum-a", "sum-b").await;
        slot(&app, "h1", "held").await;
        slot(&app, "h2", "waiting").await;
        let probe = Arc::new(|h: &BuildRemoteCfg, _: &std::path::Path| match h.host.as_str() {
            "sum-a" => Ok((2, 1)),
            _ => Ok((1, 0)),
        });
        let mut v = crate::build_scheduler::status(app.as_ref()).await.unwrap();
        enrich_with(&app, &mut v, probe).await;
        assert_eq!(v["remote"]["hosts"][0]["active"], json!(2));
        assert_eq!(v["remote"]["hosts"][0]["waiting"], json!(1));
        assert_eq!(v["remote"]["hosts"][1]["active"], json!(1));
        assert_eq!(v["total_active"], json!(4));
        assert_eq!(v["total_waiting"], json!(2));
        assert_eq!(v["remote_runs"], json!([]));
    }

    /// 一台讀不到（ssh 失敗、逾時）：那台是 `null`、不算進合計，其他照常回。
    #[tokio::test]
    async fn an_unreadable_remote_host_is_null_and_does_not_block() {
        let env = tt::env().await;
        let app = env.app.clone();
        two_remote_hosts(&app, "bad-a", "bad-b").await;
        slot(&app, "h1", "held").await;
        let probe = Arc::new(|h: &BuildRemoteCfg, _: &std::path::Path| match h.host.as_str() {
            "bad-a" => Err(anyhow::anyhow!("ssh 回 255")),
            _ => {
                std::thread::sleep(Duration::from_secs(3));
                Ok((9, 9))
            }
        });
        let t0 = Instant::now();
        let mut v = crate::build_scheduler::status(app.as_ref()).await.unwrap();
        enrich_with(&app, &mut v, probe).await;
        assert!(t0.elapsed() < Duration::from_secs(5), "卡住的那台不能擋回應：{:?}", t0.elapsed());
        assert_eq!(v["remote"]["hosts"][0]["active"], json!(null));
        assert_eq!(v["remote"]["hosts"][1]["active"], json!(null), "逾時也是 null");
        assert_eq!(v["remote"]["hosts"][1]["waiting"], json!(null));
        assert_eq!(v["total_active"], json!(1));
        assert_eq!(v["total_waiting"], json!(0));
    }

    /// 指到 daemon 自己這台的熱池：本機原生 cargo 也拿同一組 `.slots`，要扣掉才不會算兩次。
    #[tokio::test]
    async fn the_same_machine_pool_does_not_double_count_local_cargo() {
        let env = tt::env().await;
        let app = env.app.clone();
        app.cfg
            .update(|cfg| {
                cfg.build.remote.enabled = true;
                cfg.build.remote.user = "me".into();
                cfg.build.remote.host = "127.0.0.1".into();
                cfg.build.remote.remote_root = "dedup-root".into();
                Ok(())
            })
            .await
            .unwrap();
        slot(&app, "h1", "held").await;
        let probe = Arc::new(|_: &BuildRemoteCfg, _: &std::path::Path| Ok((3, 0)));
        let mut v = crate::build_scheduler::status(app.as_ref()).await.unwrap();
        enrich_with(&app, &mut v, probe).await;
        assert_eq!(v["remote"]["hosts"][0]["active"], json!(2));
        assert_eq!(v["total_active"], json!(3));
    }

    #[test]
    fn digest_ignores_lease_renewals() {
        let a = json!({"total_active":1,"total_waiting":0,"slots":[{"holder":"h","status":"held","expires_at":"1"}],"remote":{"hosts":[]},"remote_runs":[]});
        let mut b = a.clone();
        b["slots"][0]["expires_at"] = json!("2");
        assert_eq!(digest(&a), digest(&b));
        b["total_active"] = json!(2);
        assert_ne!(digest(&a), digest(&b));
    }
}
