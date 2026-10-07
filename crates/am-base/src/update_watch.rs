//! claude 更新完只在 pane 底下印 `Update installed · Restart to update`，沒有事件；巡邏讀出來寫進
//! `runs.update_notice` 讓 web 畫重啟徽章。掛 run 不掛 bot：更新屬於這個 process，重啟後自然清空。
//! 掃所有 `running`（不只停著的）：使用者再送話 run 變 working，通知仍該看得見。
//! codex 也巡（issue #388）：它的新版**還沒安裝**，認法與文字見 [`crate::codex_update`]；claude 的批次重啟不收它。

use crate::db;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 每個 claude pane 都要一次 `pane.read`，所以比問卷那支（10 秒）鬆。
pub const SWEEP: Duration = Duration::from_secs(30);

/// `--version` 是 process spawn（遠端是 ssh），每輪跑太兇；晚幾分鐘亮徽章沒差。
pub const DISK_VERSION_TTL: Duration = Duration::from_secs(300);

/// 項目鍵是 `<kind>@<host>`，值還綁住讀取時的 HostFence；同名主機換代後，舊 observation 不能命中。
pub struct DiskVersionEntry {
    pub at: Instant,
    pub authority: crate::hosts::HostAuthorityKey,
    pub version: Option<String>,
}

pub struct DiskVersionObservation {
    pub version: Option<String>,
    pub fence: crate::hosts::HostFence,
}

/// 掛在 `App.disk_versions`，不放 process 全域：測試各自一個 App，別的測試裝完版本時的
/// [`forget_disk_version`] 不會清掉這個測試種的版本、害它改去問本機真的 `codex --version`（issue #759）。
pub type DiskCache = tokio::sync::Mutex<HashMap<String, DiskVersionEntry>>;

/// statusLine 回報的 process 版本（`runs.status_json.version`）。**跑著的**版本，不是磁碟上的；
/// 插隊送出的版本閘門（issue #103，`lifecycle::send_now`）也讀這一支。
pub fn running_version(status_json: Option<&str>) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(status_json?).ok()?;
    crate::changelog::version_string(v.get("version")?.as_str()?)
}

/// 磁碟比跑著的新 = 重啟就換版。畫面那句會被推掉而漏報（2026-09-12 使用者實測 2.1.267 vs 2.1.269）。
pub fn version_notice(disk: &str, running: &str) -> Option<String> {
    let (d, r) = (crate::changelog::parse_version(disk)?, crate::changelog::parse_version(running)?);
    (d > r).then(|| format!("磁碟上已是 {disk}（這個 run 跑的是 {running}）· 重啟套用"))
}

/// 行程級（static）的 per-run／per-bot 記憶體帳：run 或 bot 結束後沒人會再回頭清它們，長跑的 daemon 裡只增不減。
/// 這一輪的 active run 名單已經在手，順手把不在名單上的帶走。測試版不呼叫：帳是全域的，平行的測試會互相清掉對方剛記的東西
/// （每個模組的 `retain_*` 本身各有單元測試）。


/// 軟刪（含 child 退役）之後 per-bot 行程帳多留多久：退役的 child 常是 herdr 重啟後 reconcile 暫時收掉的，父 bot 會用 herdr 重開、
/// 復原（SPEC §6.5a）；這段時間帳清掉的話，復原後還停在同一個 blocked 問題會被再通知父 bot 一次。
pub const RETIRED_KEEP_SECS: i64 = 30 * 60;

/// per-bot 行程帳（`retain_bot_state`、child 通知指紋、per-bot 鎖…）要留著的 bot：
/// - 沒刪掉的；
/// - **軟刪了但還有 active run** 的：`delete_bot` 先定案 `deleted_at`、再停機，停機那幾秒 bot 還在用這些帳（欠著的收尾寫入、中斷標記…），
///   run 結束後下一輪才清；
/// - 剛軟刪／退役不久（[`RETIRED_KEEP_SECS`]）的：可能馬上復原。
pub async fn live_bot_ids(app: &impl crate::capabilities::Db) -> anyhow::Result<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM bots
          WHERE deleted_at IS NULL
             OR deleted_at >= ?
             OR EXISTS (SELECT 1 FROM runs r WHERE r.bot_id = bots.id AND r.state IN ('starting','running','stopping'))",
    )
    .bind(db::iso_in(-RETIRED_KEEP_SECS))
    .fetch_all(app.db())
    .await?)
}
