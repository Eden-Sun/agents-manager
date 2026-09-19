//! claude 更新完只在 pane 底下印 `Update installed · Restart to update`，沒有事件；巡邏讀出來寫進
//! `runs.update_notice` 讓 web 畫重啟徽章。掛 run 不掛 bot：更新屬於這個 process，重啟後自然清空。
//! 掃所有 `running`（不只停著的）：使用者再送話 run 變 working，通知仍該看得見。

use crate::db;
use crate::state::App;
use crate::tui_prompts::update_notice;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// 每個 claude pane 都要一次 `pane.read`，所以比問卷那支（10 秒）鬆。
const SWEEP: Duration = Duration::from_secs(30);

/// `--version` 是 process spawn（遠端是 ssh），每輪跑太兇；晚幾分鐘亮徽章沒差。
const DISK_VERSION_TTL: Duration = Duration::from_secs(300);

type DiskCache = tokio::sync::Mutex<HashMap<String, (Instant, Option<String>)>>;

fn disk_cache() -> &'static DiskCache {
    static C: OnceLock<DiskCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

async fn disk_version(app: &Arc<App>, host: &str) -> Option<String> {
    let mut cache = disk_cache().lock().await;
    if let Some((at, v)) = cache.get(host) {
        if at.elapsed() < DISK_VERSION_TTL {
            return v.clone();
        }
    }
    let v = crate::changelog::installed_version(app, host, "claude").await.ok();
    cache.insert(host.to_string(), (Instant::now(), v.clone()));
    v
}

/// statusLine 回報的 process 版本（`runs.status_json.version`）。**跑著的**版本，不是磁碟上的；
/// 插隊送出的版本閘門（issue #103，`lifecycle::send_now`）也讀這一支。
pub(crate) fn running_version(status_json: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(status_json?).ok()?;
    crate::changelog::version_string(v.get("version")?.as_str()?)
}

/// 磁碟比跑著的新 = 重啟就換版。畫面那句會被推掉而漏報（2026-09-12 使用者實測 2.1.267 vs 2.1.269）。
fn version_notice(disk: &str, running: &str) -> Option<String> {
    let (d, r) = (crate::changelog::parse_version(disk)?, crate::changelog::parse_version(running)?);
    (d > r).then(|| format!("磁碟上已是 {disk}（這個 run 跑的是 {running}）· 重啟套用"))
}

async fn sweep(app: &Arc<App>) {
    let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if !matches!(db::bot(&app.db, &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
            continue;
        }
        let Some(pane) = run.pane_id.clone() else { continue };
        let Some(client) = app.herdr_for_run(&run).await else { continue };
        // 讀不到畫面就跳過，不要把已經看到的通知清掉。
        let Ok(read) = client.pane_read(&pane, "visible", 80).await else { continue };
        let mut seen = update_notice(&read.text);
        // 畫面原句優先（claude 自己說的較準），沒有才用版本比對。
        if seen.is_none() {
            if let Some(running) = running_version(run.status_json.as_deref()) {
                // 讀不到 host 就整輪跳過、不動既有通知：拿本機的 claude 去比遠端跑的版本會造出或清掉假通知（#243）。
                let Ok(host) = db::bot_host(&app.db, &run.bot_id).await else { continue };
                if let Some(disk) = disk_version(app, &host).await {
                    seen = version_notice(&disk, &running);
                }
            }
        }
        if seen.as_deref() == run.update_notice.as_deref() {
            continue;
        }
        if seen.is_some() {
            tracing::info!(run = %run.id, bot = %run.bot_id, "claude 有新版等著重啟套用");
        }
        let _ = sqlx::query("UPDATE runs SET update_notice = ? WHERE id = ?")
            .bind(seen.as_deref())
            .bind(&run.id)
            .execute(&app.db)
            .await;
        app.emit_bot_status(&run.bot_id).await;
    }
}

pub fn spawn_update_watcher(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP).await;
            sweep(&app).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-09-12 使用者實測：跑著 2.1.267、磁碟上 2.1.269。
    #[test]
    fn a_newer_version_on_disk_is_an_update_waiting_for_a_restart() {
        let n = version_notice("2.1.269", "2.1.267").expect("磁碟比較新 = 有更新");
        assert!(n.contains("2.1.269") && n.contains("2.1.267"), "兩個版本都要寫出來：{n}");
    }

    #[test]
    fn same_or_older_on_disk_is_not_an_update() {
        assert!(version_notice("2.1.267", "2.1.267").is_none());
        // 磁碟比較舊（降版、或探到別的 PATH）不是「有更新」，不要叫使用者重啟。
        assert!(version_notice("2.1.266", "2.1.267").is_none());
    }

    /// 版本號位數不同也要比得對：2.1.9 < 2.1.10（字串比較會給反的答案）。
    #[test]
    fn versions_compare_numerically_not_as_strings() {
        assert!(version_notice("2.1.10", "2.1.9").is_some());
        assert!(version_notice("2.1.9", "2.1.10").is_none());
    }

    #[test]
    fn unparsable_versions_are_silent() {
        assert!(version_notice("", "2.1.267").is_none());
        assert!(version_notice("2.1.269", "unknown").is_none());
    }

    #[test]
    fn running_version_comes_from_the_statusline_payload() {
        assert_eq!(running_version(Some(r#"{"version":"2.1.267 (Claude Code)","model_name":"Opus"}"#)).as_deref(), Some("2.1.267"));
        assert!(running_version(Some(r#"{"model_name":"Opus"}"#)).is_none());
        assert!(running_version(None).is_none());
    }
}
