//! 外部編譯主機「上一次到底連不連得上」的結論（issue #428）。
//!
//! `remote-cargo` helper 是 shim 叫起來的**獨立行程**，daemon 看不到它的成敗：遠端整晚連不上、每顆 child
//! 都靜靜退回本機擠那兩個名額時，`/api/build-slots` 卻只看得到本機的隊伍，沒有一個地方說得出「遠端掛了」。
//! 每次真的嘗試連線之後把結論寫在 data-dir 的一份小檔，daemon 讀它回報 `remote_reachable`——用的是**真的跑過的
//! 那一次**，不是另外再 ssh 一次探測（探測本身要 15 秒 `ConnectTimeout`，而且探得到不代表編譯那條連得上）。
//!
//! 寫不進去一律當沒發生：這份檔是說明用的，不能讓它擋下一次編譯。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// data-dir 底下的檔名。跟密碼檔（`remote-cargo-password*`）同一個目錄，但這份沒有秘密，0644 就好。
pub const FILE: &str = "remote-cargo-health.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Health {
    /// 上一次嘗試有沒有連進遠端（拿到守門交出來的目錄＝連得上）。
    pub reachable: bool,
    /// 那一次是什麼時候（RFC3339，毫秒，UTC）。
    pub checked_at: String,
    /// 連的是誰（`user@host:port`）：設定換了主機之後，舊的結論才看得出來是別台的。
    pub target: String,
    /// 連不上時的原因，一句話。連得上就沒有這個欄位。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// 記下這一次的結論。先寫暫存檔再 rename：同時有好幾顆 cargo 在跑時，讀的人不會讀到寫到一半的 JSON。
/// 寫不進去就算了（回報 `Err` 給想 log 的呼叫端，但呼叫端不該因此失敗）。
pub fn record(data_dir: &Path, h: &Health) -> std::io::Result<()> {
    let body = serde_json::to_vec(h).map_err(std::io::Error::other)?;
    let tmp = data_dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &body)?;
    match std::fs::rename(&tmp, path(data_dir)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 上一次的結論；沒有檔、讀不到、或內容看不懂都是 `None`（＝還沒有人試過，不是「連不上」）。
pub fn read(data_dir: &Path) -> Option<Health> {
    let body = std::fs::read(path(data_dir)).ok()?;
    serde_json::from_slice(&body).ok()
}
