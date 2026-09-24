//! 刪 bot 時不直接 `rm -rf bots/<id>/`，而是搬到 `bots-trash/<id>.<毫秒>/`（issue #406）。
//!
//! 軟刪本來就是為了能還原（對話、設定都留著），偏偏 `bots/<id>/` 是當場 `remove_dir_all`——2026-09-23 13:28Z
//! `build` 與 triage 被誤刪時，AGM 還原得回 bot 列與 config，目錄裡的東西（spool 裡還沒重放的 hook、shim、
//! 手動放的檔）就沒了。搬走而不是刪：`POST /api/bots/{id}/restore` 時搬回來；放超過 [`KEEP_DAYS`] 天、
//! 或整個回收區超過 [`MAX_BYTES`] 就清掉（最舊的先）。
//! 這裡只管本機；遠端目錄的回收區在 `remote_trash`（#411）。
//!
//! 清理跑兩處：開機的清掃（`purge_deleted_bot_dirs`）與 [`spawn_gc`] 每天一次。只靠開機那一次不夠——
//! daemon 常駐好幾天很常見，回收區會一路長（review d77434c0 #2）。

use crate::state::App;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// 回收區保留幾天。
pub const KEEP_DAYS: u64 = 7;

/// 回收區的總量上限。超過就從最舊的開始清，直到降到上限以下——時間上限擋不住「短時間刪掉一堆大目錄」。
pub const MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 例行清理的間隔。
const GC_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

pub fn keep_duration() -> Duration {
    Duration::from_secs(KEEP_DAYS * 86_400)
}

pub fn root(data_dir: &Path) -> PathBuf {
    data_dir.join("bots-trash")
}

fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// 同一顆 bot 除了 `bots/<id>/` 之外還要收進回收區的目錄（issue #465）。回收區的項目名：
/// `<id>.<毫秒>` 是 `bots/<id>/`，`<id>.<kind>.<毫秒>` 是這裡的其他目錄。兩種的結尾都是毫秒，
/// 所以 [`entries`]（過期與總量上限）一視同仁；[`latest`] 認的是前者，`<kind>` 那種 `parse::<u128>`
/// 會失敗、不會被當成 bot 目錄還原回去。
pub const ATTACHMENTS: &str = "attachments";

/// 這顆 bot 在資料目錄裡的附件副本（`attach::local_copy_dir` 的同一條路）。
pub fn attachments_dir(data_dir: &Path, bot_id: &str) -> PathBuf {
    data_dir.join(ATTACHMENTS).join(bot_id)
}

fn entry_name(bot_id: &str, kind: Option<&str>) -> String {
    match kind {
        Some(k) => format!("{bot_id}.{k}.{}", now_ms()),
        None => format!("{bot_id}.{}", now_ms()),
    }
}

/// 把 `dir`（某顆 bot 的 `bots/<id>/`）搬進回收區。`dir` 不存在＝`Ok(None)`。
pub fn move_in(data_dir: &Path, bot_id: &str, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    move_in_kind(data_dir, bot_id, None, dir)
}

/// 同 [`move_in`]，但收的是這顆 bot 的其他目錄（`kind`，目前只有 [`ATTACHMENTS`]）。
pub fn move_in_kind(data_dir: &Path, bot_id: &str, kind: Option<&str>, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    if !dir.exists() {
        return Ok(None);
    }
    let root = root(data_dir);
    std::fs::create_dir_all(&root)?;
    let dest = root.join(entry_name(bot_id, kind));
    std::fs::rename(dir, &dest)?;
    Ok(Some(dest))
}

/// 回收區裡這顆 bot 最新的那一份（`<id>.<毫秒>`，或 `kind` 版的 `<id>.<kind>.<毫秒>`）。
/// `attach::read` 用得到：附件搬進回收區之後，已刪 bot 的對話仍要讀得到縮圖（#465）。
pub fn latest_kind(data_dir: &Path, bot_id: &str, kind: Option<&str>) -> Option<PathBuf> {
    latest(data_dir, bot_id, kind)
}

fn latest(data_dir: &Path, bot_id: &str, kind: Option<&str>) -> Option<PathBuf> {
    let prefix = match kind {
        Some(k) => format!("{bot_id}.{k}."),
        None => format!("{bot_id}."),
    };
    std::fs::read_dir(root(data_dir))
        .ok()?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            // `<id>.attachments.<ms>` 在 kind=None 時 strip 出來是 `attachments.<ms>`，parse 失敗＝不是 bot 目錄。
            let ms: u128 = name.strip_prefix(&prefix)?.parse().ok()?;
            Some((ms, e.path()))
        })
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, p)| p)
}

/// 還原：`bots/<id>/` 還不在時把回收區最新那份搬回去。已經在（重新啟動過、重建了）就不動，免得蓋掉新的。
pub fn restore(data_dir: &Path, bot_id: &str, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    restore_kind(data_dir, bot_id, None, dir)
}

/// 同 [`restore`]，但還原的是 `kind` 那份。刪 bot 會把附件一起收進回收區，還原時要一起搬回來——
/// 不然還原後對話還在、縮圖卻全破（已刪 bot 的對話本來就讀得到，API.md §10.4）。
pub fn restore_kind(data_dir: &Path, bot_id: &str, kind: Option<&str>, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    if dir.exists() {
        return Ok(None);
    }
    let Some(src) = latest(data_dir, bot_id, kind) else { return Ok(None) };
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::rename(&src, dir)?;
    Ok(Some(src))
}

/// 回收區裡的每一份：`(搬進來的毫秒, 路徑, 佔用位元組)`，最舊的在前。名字看不懂的（別人放的檔）不理。
fn entries(data_dir: &Path) -> Vec<(u128, PathBuf, u64)> {
    let Ok(dir) = std::fs::read_dir(root(data_dir)) else { return Vec::new() };
    let mut out: Vec<(u128, PathBuf, u64)> = dir
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let ms: u128 = name.rsplit_once('.')?.1.parse().ok()?;
            let path = e.path();
            let size = dir_size(&path);
            Some((ms, path, size))
        })
        .collect();
    out.sort_by_key(|(ms, _, _)| *ms);
    out
}

/// 目錄佔用的位元組（遞迴，只算檔案；讀不到的當 0——清理的判斷寧可低估也不要因為一個壞檔就整批不清）。
fn dir_size(path: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else { return 0 };
    if !md.is_dir() {
        return md.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries.flatten().map(|e| dir_size(&e.path())).sum()
}

fn remove(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(dir = %path.display(), error = %err, "could not remove a bots-trash entry");
            false
        }
    }
}

/// 清掉放超過 `keep` 的（看名字裡的時間，不看 mtime：`rename` 不會更新目錄的 mtime）。
pub fn gc(data_dir: &Path, keep: Duration) -> usize {
    gc_with_cap(data_dir, keep, MAX_BYTES).0
}

/// 兩道一起跑：先清過期的，再看總量——還超過 `max_bytes` 就從**最舊的**開始清到降下來。
/// 回傳 `(過期清掉幾份, 因為超量再清掉幾份)`。
pub fn gc_with_cap(data_dir: &Path, keep: Duration, max_bytes: u64) -> (usize, usize) {
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let mut live: Vec<(u128, PathBuf, u64)> = Vec::new();
    let (mut expired, mut evicted) = (0, 0);
    for (ms, path, size) in entries(data_dir) {
        if ms <= cutoff && remove(&path) {
            expired += 1;
        } else {
            live.push((ms, path, size));
        }
    }
    let mut total: u64 = live.iter().map(|(_, _, size)| *size).sum();
    // 最舊的先走；最新那一份永遠留著（剛刪掉的那顆才是最可能要還原的，留著才有意義）。
    for (_, path, size) in live.iter().take(live.len().saturating_sub(1)) {
        if total <= max_bytes {
            break;
        }
        if remove(path) {
            evicted += 1;
            total = total.saturating_sub(*size);
        }
    }
    if evicted > 0 {
        tracing::warn!(evicted, total, max_bytes, "bots-trash is over its size cap; removed the oldest entries");
    }
    (expired, evicted)
}

/// 每天清一次：開機那一次之外，常駐好幾天的 daemon 也要收（review d77434c0 #2）。
pub fn spawn_gc(app: Arc<App>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(GC_EVERY).await;
            let (expired, evicted) = gc_with_cap(&app.data_dir, keep_duration(), MAX_BYTES);
            if expired > 0 || evicted > 0 {
                tracing::info!(expired, evicted, days = KEEP_DAYS, "daily bots-trash sweep");
            }
        }
    });
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

    /// 在回收區放一份指定大小、指定「搬進來時刻」的目錄。
    fn seed(data: &Path, bot: &str, ms: u128, bytes: usize) -> PathBuf {
        let p = root(data).join(format!("{bot}.{ms}"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::write(p.join("blob"), vec![b'x'; bytes]).unwrap();
        p
    }

    /// review d77434c0 #2：時間上限擋不住「短時間刪掉一堆」。超過總量就從最舊的開始清，
    /// 最新那一份留著（剛刪掉的那顆才是最可能要還原的）。
    #[test]
    fn the_oldest_entries_go_first_once_the_trash_is_over_its_size_cap() {
        let data = std::env::temp_dir().join(format!("am-trash-cap-{}", crate::db::ulid()));
        let now = now_ms();
        let old = seed(&data, "b1", now - 3_000, 4_000);
        let mid = seed(&data, "b2", now - 2_000, 4_000);
        let new = seed(&data, "b3", now - 1_000, 4_000);

        // 沒超量就一個都不動（keep 很長，沒有東西過期）。
        assert_eq!(gc_with_cap(&data, Duration::from_secs(3600), 100_000), (0, 0));
        assert!(old.exists() && mid.exists() && new.exists());

        // 上限只容得下一份：最舊的兩份走，最新那份留著。
        assert_eq!(gc_with_cap(&data, Duration::from_secs(3600), 5_000), (0, 2));
        assert!(!old.exists() && !mid.exists(), "最舊的先清");
        assert!(new.exists(), "最新那一份永遠留著");
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 過期的先清；清完還超量才輪到按時間淘汰，兩個數字分開回報。
    #[test]
    fn expiry_runs_before_the_size_cap() {
        let data = std::env::temp_dir().join(format!("am-trash-both-{}", crate::db::ulid()));
        let now = now_ms();
        seed(&data, "b1", now - 10_000, 4_000); // 過期
        let mid = seed(&data, "b2", now - 2_000, 4_000);
        let new = seed(&data, "b3", now - 1_000, 4_000);

        assert_eq!(gc_with_cap(&data, Duration::from_millis(5_000), 5_000), (1, 1));
        assert!(!mid.exists() && new.exists());
        assert_eq!(entries(&data).len(), 1);
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 名字看不懂的（別人放進來的檔案、暫存）一律不碰，免得清理誤傷。
    #[test]
    fn entries_with_unparseable_names_are_never_touched() {
        let data = std::env::temp_dir().join(format!("am-trash-alien-{}", crate::db::ulid()));
        std::fs::create_dir_all(root(&data).join("not-a-trash-entry")).unwrap();
        std::fs::write(root(&data).join("README"), "x").unwrap();

        assert_eq!(gc_with_cap(&data, Duration::ZERO, 0), (0, 0));
        assert!(root(&data).join("not-a-trash-entry").exists() && root(&data).join("README").exists());
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// #465：附件副本要跟 `bots/<id>/` 一起進回收區、受同一套過期與總量上限，還原時一起搬回來，
    /// 而且 `<id>.attachments.<ms>` 不能被當成 bot 目錄還原到 `bots/<id>/`。
    #[test]
    fn attachments_ride_the_same_trash_lifecycle_without_being_mistaken_for_the_bot_dir() {
        let data = std::env::temp_dir().join(format!("am-trash-att-{}", crate::db::ulid()));
        let bots = data.join("bots/B1");
        let att = attachments_dir(&data, "B1");
        std::fs::create_dir_all(&bots).unwrap();
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(bots.join("config"), "bot").unwrap();
        std::fs::write(att.join("a.png"), "img").unwrap();

        assert!(move_in(&data, "B1", &bots).unwrap().is_some());
        assert!(move_in_kind(&data, "B1", Some(ATTACHMENTS), &att).unwrap().is_some());
        assert!(!bots.exists() && !att.exists(), "兩個都搬走了");
        assert_eq!(entries(&data).len(), 2, "兩份都要被 GC 看得到（過期與總量上限一視同仁）");

        // 還原 bot 目錄時不能撈到 attachments 那份。
        restore(&data, "B1", &bots).unwrap();
        assert_eq!(std::fs::read_to_string(bots.join("config")).unwrap(), "bot");
        restore_kind(&data, "B1", Some(ATTACHMENTS), &att).unwrap();
        assert_eq!(std::fs::read_to_string(att.join("a.png")).unwrap(), "img");
        std::fs::remove_dir_all(&data).unwrap();
    }

    /// 過期清理會把附件那份也收掉（以前 `attachments/<id>/` 永遠不會被任何人清）。
    #[test]
    fn expired_attachment_entries_are_collected_like_any_other() {
        let data = std::env::temp_dir().join(format!("am-trash-attgc-{}", crate::db::ulid()));
        let att = attachments_dir(&data, "B1");
        std::fs::create_dir_all(&att).unwrap();
        std::fs::write(att.join("a.png"), vec![b'x'; 32]).unwrap();
        let moved = move_in_kind(&data, "B1", Some(ATTACHMENTS), &att).unwrap().unwrap();
        assert!(moved.exists());
        let (expired, _) = gc_with_cap(&data, Duration::ZERO, u64::MAX);
        assert_eq!(expired, 1, "附件那份也要被過期清理收掉");
        assert!(!moved.exists());
        std::fs::remove_dir_all(&data).unwrap();
    }

    #[test]
    fn dir_size_adds_up_nested_files() {
        let data = std::env::temp_dir().join(format!("am-trash-size-{}", crate::db::ulid()));
        let d = data.join("x");
        std::fs::create_dir_all(d.join("a/b")).unwrap();
        std::fs::write(d.join("a/one"), vec![b'x'; 10]).unwrap();
        std::fs::write(d.join("a/b/two"), vec![b'x'; 5]).unwrap();
        assert_eq!(dir_size(&d), 15);
        std::fs::remove_dir_all(&data).unwrap();
    }
}
