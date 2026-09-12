//! Claude Code 下載好新版之後，只會在 pane 最底下那行印一句
//!
//! ```text
//! ✔ Update installed · Restart to update
//! ```
//!
//! 就沒有別的動靜了——不是事件、不會消失、也不影響回合。使用者要一路點進終端才看得到，等於
//! 沒通知。這支巡邏把那句話讀出來掛到 run 上（`runs.update_notice`），web 才畫得出 header 上
//! 那顆點得下去的徽章（點了就是 `POST /api/bots/{id}/restart`，重啟就是套用更新）。
//!
//! 掛在 run 而不是 bot：等著被套用的更新是**這個 claude process** 的事，重啟後那個 process
//! 沒了，新 run 的欄位本來就是 NULL。
//!
//! `running` 的 claude run 全掃，不像 [`crate::tui_prompts::spawn_survey_watcher`] 只掃停著
//! 的：那句是在回合結束時印的，但使用者下一句話送出去之後 run 就變 `working`，通知還在畫面
//! 上、也還該看得見。

use crate::db;
use crate::state::App;
use crate::tui_prompts::update_notice;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

/// 巡邏間隔。更新一天撞不到幾次，晚 30 秒知道沒有差別，但每個 claude pane 都要一次
/// `pane.read`，所以比問卷那支（10 秒）鬆。
const SWEEP: Duration = Duration::from_secs(30);

/// 磁碟版本（`claude --version`）的快取壽命。它是一個 process spawn（遠端還是一次 ssh），
/// 每 30 秒一輪太兇；claude 自己更新完之後晚幾分鐘才亮徽章沒有差別。
const DISK_VERSION_TTL: Duration = Duration::from_secs(300);

type DiskCache = tokio::sync::Mutex<HashMap<String, (Instant, Option<String>)>>;

fn disk_cache() -> &'static DiskCache {
    static C: OnceLock<DiskCache> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// 那台主機磁碟上的 claude 版本，[`DISK_VERSION_TTL`] 內重用。探不到就是 `None`。
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

/// statusLine 回報的「這個 process 正在跑的版本」（`runs.status_json` 的 `version`）。
/// 吃字串而不是整個 `Run`：這支的輸入只有那一個欄位，測試也就不必造一整列。
fn running_version(status_json: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(status_json?).ok()?;
    crate::changelog::version_string(v.get("version")?.as_str()?)
}

/// 磁碟上比跑著的新 = 重啟就會換過去。回傳要寫進 `runs.update_notice` 的那句話。
///
/// 為什麼需要這條路：原本只認 pane 上那句 `Update installed · Restart to update`，但那句
/// 是 claude **自己更新完的當下**印在畫面最底下的一行——畫面被後續輸出推掉、或這個 run 是在
/// 更新之後才被看到，就再也讀不到了。2026-09-12 使用者實測：bot 跑著 2.1.267、磁碟上已經是
/// 2.1.269，UI 什麼都沒顯示。版本比較不依賴任何一瞬間的畫面，補的正是這一段。
fn version_notice(disk: &str, running: &str) -> Option<String> {
    let (d, r) = (crate::changelog::parse_version(disk)?, crate::changelog::parse_version(running)?);
    (d > r).then(|| format!("磁碟上已是 {disk}（這個 run 跑的是 {running}）· 重啟套用"))
}

/// 掃一輪：每個活著的 claude run 讀一次畫面，跟 DB 裡的值不同才寫回去並通知 web。
async fn sweep(app: &Arc<App>) {
    let runs = db::all_active_runs(&app.db).await.unwrap_or_default();
    for run in runs.into_iter().filter(|r| r.state == "running") {
        if !matches!(db::bot(&app.db, &run.bot_id).await, Ok(Some(b)) if b.kind == "claude") {
            continue;
        }
        let Some(pane) = run.pane_id.clone() else { continue };
        let Some(client) = app.herdr_for_run(&run).await else { continue };
        // 讀不到畫面（pane 沒了、主機斷線）就跳過，不要把已經看到的通知清掉。
        let Ok(read) = client.pane_read(&pane, "visible", 80).await else { continue };
        let mut seen = update_notice(&read.text);
        // 畫面上沒有那句話時，改用版本比對（見 `version_notice`）。畫面有就用畫面的原句——
        // 那是 claude 自己說的，比我們推出來的更準。
        if seen.is_none() {
            if let Some(running) = running_version(run.status_json.as_deref()) {
                let host = db::bot_host(&app.db, &run.bot_id).await.unwrap_or_else(|_| crate::config::LOCAL_HOST.to_string());
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

/// 每 [`SWEEP`] 掃一次。
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

    /// 2026-09-12 使用者實測的那一組：跑著 2.1.267、磁碟上 2.1.269，畫面上那句話早就被推掉了。
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

    /// statusLine 的 payload 只有這一個欄位重要；沒有 `version`（或根本沒 statusLine）
    /// 就退回「只認畫面那句話」的老路。
    #[test]
    fn running_version_comes_from_the_statusline_payload() {
        assert_eq!(running_version(Some(r#"{"version":"2.1.267 (Claude Code)","model_name":"Opus"}"#)).as_deref(), Some("2.1.267"));
        assert!(running_version(Some(r#"{"model_name":"Opus"}"#)).is_none());
        assert!(running_version(None).is_none());
    }
}
