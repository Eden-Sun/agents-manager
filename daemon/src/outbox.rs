//! 給使用者的輸出目錄（SPEC §6.5f）：`<data_dir>/outbox/<bot_id>/`，bot 的 pane 拿到 `AM_OUTBOX` 指到這裡。
//!
//! 使用者 2026-09-16 裁示：scratchpad 暴露了私鑰與正式 DB 複本之後，**scratchpad 不再是輸出目錄**。
//! 要交給使用者的檔案一律放 outbox；只保留 [`TTL_SECS`]，清理由 AGM 的 launchd `com.agm.outbox-gc`
//! 每 10 分鐘做一次（依 mtime 刪超過一小時的檔、收空目錄）——daemon 不清。
//!
//! 空目錄會被那支清理收掉，所以 daemon 在 bot 啟動時建的目錄不保證還在：bot 寫檔前自己 `mkdir -p "$AM_OUTBOX"`。

use std::path::{Path, PathBuf};

/// 檔案在 outbox 裡保留多久（AGM 清理的門檻，跟 `outbox-gc.sh` 的 `MAX_AGE_MIN=60` 同一個數）。
pub(crate) const TTL_SECS: u64 = 3600;

/// 這顆 bot 的 outbox。bot id 會拼進路徑：只收英數（ULID），其他一律不給。
pub(crate) fn dir_for(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    if bot_id.is_empty() || !bot_id.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(data_dir.join("outbox").join(bot_id))
}

/// bot 啟動時把目錄建好。建不起來只記 warning：少一個目錄不該讓 bot 起不來，bot 寫之前本來就要 `mkdir -p`。
pub(crate) fn ensure(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    let dir = dir_for(data_dir, bot_id)?;
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(bot = bot_id, dir = %dir.display(), error = %e, "could not create the bot's outbox");
    }
    Some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_bot_id_is_the_only_thing_that_picks_the_directory() {
        let data = Path::new("/data");
        assert_eq!(dir_for(data, "01M2MC36YBQZWXCQN83RKD61TE"), Some(PathBuf::from("/data/outbox/01M2MC36YBQZWXCQN83RKD61TE")));
        for bad in ["", "..", "../x", "a/b", "b1 ", "b.1"] {
            assert_eq!(dir_for(data, bad), None, "{bad:?} 不能拼進路徑");
        }
    }
}
