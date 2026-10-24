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

/// 多台建置主機時每台一份（#1956）：`remote-cargo-health.<target 的 fnv1a64>.json`。分檔是為了好幾顆 cargo 同時連不同台時不互蓋。
pub fn target_path(data_dir: &Path, target: &str) -> PathBuf {
    let mut h = 0xcbf29ce484222325u64;
    for b in target.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    data_dir.join(format!("remote-cargo-health.{h:016x}.json"))
}

/// 記下這一次的結論（寫進那台自己的那份）。先寫暫存檔再 rename：同時有好幾顆 cargo 在跑時，讀的人不會讀到寫到一半的 JSON。
/// 寫不進去就算了（回報 `Err` 給想 log 的呼叫端，但呼叫端不該因此失敗）。
pub fn record(data_dir: &Path, h: &Health) -> std::io::Result<()> {
    let body = serde_json::to_vec(h).map_err(std::io::Error::other)?;
    let tmp = data_dir.join(format!("{FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, &body)?;
    match std::fs::rename(&tmp, target_path(data_dir, &h.target)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// 對 `target` 這台上一次的結論；沒有檔、讀不到、內容看不懂、或記的是別台都是 `None`（＝還沒有人試過，不是「連不上」）。
/// 這台自己的那份沒有時退回 #1956 之前的單一檔（`remote-cargo-health.json`）。
pub fn read(data_dir: &Path, target: &str) -> Option<Health> {
    let parse = |p: PathBuf| -> Option<Health> { serde_json::from_slice(&std::fs::read(p).ok()?).ok() };
    parse(target_path(data_dir, target)).or_else(|| parse(path(data_dir))).filter(|h| h.target == target)
}
