//! 刪 bot 時不直接 `rm -rf bots/<id>/`，而是搬到 `bots-trash/<id>.<毫秒>/`（issue #406）。
//!
//! 軟刪本來就是為了能還原（對話、設定都留著），偏偏 `bots/<id>/` 是當場 `remove_dir_all`——2026-09-23 13:28Z
//! `build` 與 triage 被誤刪時，AGM 還原得回 bot 列與 config，目錄裡的東西（spool 裡還沒重放的 hook、shim、
//! 手動放的檔）就沒了。搬走而不是刪：`POST /api/bots/{id}/restore` 時搬回來；開機時把放超過 [`KEEP_DAYS`] 天的清掉。
//! 只管本機：遠端目錄照舊由 `purge_bot_dir` 在遠端 `rm -rf`（那邊的還原要走 ssh，這次不做）。

use std::path::{Path, PathBuf};

/// 回收區保留幾天。
pub const KEEP_DAYS: u64 = 7;

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("bots-trash")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// 把 `dir`（某顆 bot 的 `bots/<id>/`）搬進回收區。`dir` 不存在＝`Ok(None)`。
pub fn move_in(data_dir: &Path, bot_id: &str, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    let root = root(data_dir);
    std::fs::create_dir_all(&root)?;
    let dest = root.join(format!("{bot_id}.{}", now_ms()));
    std::fs::rename(dir, &dest)?;
    Ok(Some(dest))
}

/// 回收區裡這顆 bot 最新的那一份（名字是 `<id>.<毫秒>`）。
fn latest(data_dir: &Path, bot_id: &str) -> Option<PathBuf> {
    let prefix = format!("{bot_id}.");
    std::fs::read_dir(root(data_dir))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let ms: u128 = name.strip_prefix(&prefix)?.parse().ok()?;
            Some((ms, e.path()))
        })
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, p)| p)
}

/// 還原：`bots/<id>/` 還不在時把回收區最新那份搬回去。已經在（重新啟動過、重建了）就不動，免得蓋掉新的。
pub fn restore(data_dir: &Path, bot_id: &str, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    if dir.exists() {
        return Ok(None);
    }
    let Some(src) = latest(data_dir, bot_id) else { return Ok(None) };
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&src, dir)?;
    Ok(Some(src))
}

/// 開機清掉放超過 `keep` 的（看名字裡的時間，不看 mtime：`rename` 不會更新目錄的 mtime）。
pub fn gc(data_dir: &Path, keep: std::time::Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(root(data_dir)) else { return 0 };
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let mut removed = 0;
    for e in entries.flatten() {
        let Some(ms) = e.file_name().to_str().and_then(|n| n.rsplit_once('.')).and_then(|(_, t)| t.parse::<u128>().ok()) else {
            continue;
        };
        if ms <= cutoff {
            match std::fs::remove_dir_all(e.path()) {
                Ok(()) => removed += 1,
                Err(err) => tracing::warn!(dir = %e.path().display(), error = %err, "could not remove an expired bots-trash entry"),
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_trashed_dir_comes_back_on_restore_and_expires_after_keep() {
        let data = std::env::temp_dir().join(format!("am-trash-{}", crate::db::ulid()));
        let dir = data.join("bots").join("b1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("keep.txt"), "x").unwrap();

        let moved = move_in(&data, "b1", &dir).unwrap().expect("moved");
        assert!(!dir.exists() && moved.join("keep.txt").exists());
        assert_eq!(restore(&data, "b1", &dir).unwrap(), Some(moved));
        assert_eq!(std::fs::read_to_string(dir.join("keep.txt")).unwrap(), "x");

        move_in(&data, "b1", &dir).unwrap().unwrap();
        assert_eq!(gc(&data, std::time::Duration::from_secs(3600)), 0, "還沒過期");
        assert_eq!(gc(&data, std::time::Duration::ZERO), 1);
        assert_eq!(restore(&data, "b1", &dir).unwrap(), None, "過期清掉之後沒得還原");
        std::fs::remove_dir_all(data).unwrap();
    }
}
