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


use std::ffi::{OsStr, OsString};
use std::fs::File;
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

pub fn open_root(data_dir: &Path) -> std::io::Result<File> {
    crate::trusted_open::open_bound_dir(data_dir, &[OsStr::new("bots-trash")], None)
}

fn create_root(data_dir: &Path) -> std::io::Result<File> {
    crate::trusted_open::create_bound_dirs(data_dir, &[OsStr::new("bots-trash")])
}

fn path_parent(data_dir: &Path, path: &Path, create: bool) -> std::io::Result<(File, OsString)> {
    if let Ok(relative) = path.strip_prefix(data_dir) {
        let components = crate::trusted_open::safe_relative_components(relative).ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no safe entry name"))?;
        let (name, parent) = components.split_last().expect("safe relative components are nonempty");
        let dir = if parent.is_empty() {
            crate::trusted_open::open_bound_dir(data_dir, &[], None)?
        } else if create {
            crate::trusted_open::create_bound_dirs(data_dir, parent)?
        } else {
            crate::trusted_open::open_bound_dir(data_dir, parent, None)?
        };
        return Ok((dir, name.to_os_string()));
    }

    if !path.is_absolute() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "path is outside data directory and not absolute"));
    }
    let parent = path.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent"))?;
    let name = path.file_name().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name"))?;
    if name == "." || name == ".." {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file name"));
    }
    let dir = if create {
        if !parent.is_dir() {
            let mut b = std::fs::DirBuilder::new();
            b.recursive(true);
            std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
            b.create(parent)?;
        }
        crate::trusted_open::open_dir(parent)?
    } else {
        crate::trusted_open::open_dir(parent)?
    };
    Ok((dir, name.to_os_string()))
}

pub fn now_ms() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()
}

/// 同一顆 bot 除了 `bots/<id>/` 之外還要收進回收區的目錄（issue #465、#828）。回收區的項目名：
/// `<id>.<毫秒>` 是 `bots/<id>/`，`<id>.<kind>.<毫秒>` 是這裡的其他目錄。兩種的結尾都是毫秒，
/// 所以 [`entries`]（過期與總量上限）一視同仁；[`latest`] 認的是前者，`<kind>` 那種 `parse::<u128>`
/// 會失敗、不會被當成 bot 目錄還原回去。
pub const ATTACHMENTS: &str = "attachments";
pub const SHARE_WORKSPACE: &str = "share_workspace";

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
    let (source_parent, source_name) = match path_parent(data_dir, dir, false) {
        Ok(parent) => parent,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if !crate::trusted_open::entry_exists_in(&source_parent, &source_name)? {
        return Ok(None);
    }
    let trash = create_root(data_dir)?;
    let name = OsString::from(entry_name(bot_id, kind));
    crate::trusted_open::rename_between(&source_parent, &source_name, &trash, &name)?;
    let dest = root(data_dir).join(name);
    Ok(Some(dest))
}

/// 回收區裡這顆 bot 最新的那一份（`<id>.<毫秒>`，或 `kind` 版的 `<id>.<kind>.<毫秒>`）。
/// `attach::read` 用得到：附件搬進回收區之後，已刪 bot 的對話仍要讀得到縮圖（#465）。
pub fn latest_kind(data_dir: &Path, bot_id: &str, kind: Option<&str>) -> Option<PathBuf> {
    latest(data_dir, bot_id, kind)
}

pub fn latest(data_dir: &Path, bot_id: &str, kind: Option<&str>) -> Option<PathBuf> {
    let root_dir = open_root(data_dir).ok()?;
    latest_name_in(&root_dir, bot_id, kind).map(|name| root(data_dir).join(name))
}

fn latest_name_in(root_dir: &File, bot_id: &str, kind: Option<&str>) -> Option<OsString> {
    let prefix = match kind {
        Some(k) => format!("{bot_id}.{k}."),
        None => format!("{bot_id}."),
    };
    crate::trusted_open::read_dir_bound(root_dir)
        .ok()?
        .into_iter()
        .filter_map(|e| {
            if !e.is_dir { return None; }
            let name = e.name.to_str()?;
            // `<id>.attachments.<ms>` 在 kind=None 時 strip 出來是 `attachments.<ms>`，parse 失敗＝不是 bot 目錄。
            let ms: u128 = name.strip_prefix(&prefix)?.parse().ok()?;
            Some((ms, e.name))
        })
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, name)| name)
}

/// 還原：`bots/<id>/` 還不在時把回收區最新那份搬回去。真的目錄或檔案已在就不動。symlink 只拆連結再搬回。
pub fn restore(data_dir: &Path, bot_id: &str, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    restore_kind(data_dir, bot_id, None, dir)
}

/// 同 [`restore`]，但還原的是 `kind` 那份。刪 bot 會把附件一起收進回收區，還原時要一起搬回來——
/// 不然還原後對話還在、縮圖卻全破（已刪 bot 的對話本來就讀得到，API.md §10.4）。
///
/// 跟清理搶的是同一個 `rename`（issue #513，見 [`remove`]）：gc 先把那一份改名走了，這裡的 `rename`
/// 就回 `NotFound`（`Err`），呼叫端記 warn、不擋還原——**不會**出現「回了 `Ok(Some(..))`、拿回來的目錄
/// 卻正在被清空」。
pub fn restore_kind(data_dir: &Path, bot_id: &str, kind: Option<&str>, dir: &Path) -> std::io::Result<Option<PathBuf>> {
    let Ok(trash) = open_root(data_dir) else { return Ok(None) };
    let Some(name) = latest_name_in(&trash, bot_id, kind) else { return Ok(None) };
    let (dest_parent, dest_name) = path_parent(data_dir, dir, true)?;
    if let Some(entry) = crate::trusted_open::read_dir_bound(&dest_parent)?.into_iter().find(|entry| entry.name == dest_name) {
        if entry.is_symlink {
            crate::trusted_open::unlink_in(&dest_parent, &dest_name)?;
        } else {
            return Ok(None);
        }
    }
    if crate::trusted_open::entry_exists_in(&dest_parent, &dest_name)? { return Ok(None); }
    crate::trusted_open::rename_between(&trash, &name, &dest_parent, &dest_name)?;
    let src = root(data_dir).join(name);
    Ok(Some(src))
}

/// 回收區裡的每一份：`(搬進來的毫秒, 路徑, 佔用位元組)`，最舊的在前。名字看不懂的（別人放的檔）不理。
#[cfg(any(test, feature = "test-hooks"))]
pub fn entries(data_dir: &Path) -> Vec<(u128, PathBuf, u64)> {
    let Ok(root_dir) = open_root(data_dir) else { return Vec::new() };
    entries_in(&root_dir).into_iter().map(|(ms, name, size)| (ms, root(data_dir).join(name), size)).collect()
}

fn entries_in(root_dir: &File) -> Vec<(u128, OsString, u64)> {
    let mut out: Vec<(u128, OsString, u64)> = crate::trusted_open::read_dir_bound(root_dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            if !e.is_dir && !e.is_symlink { return None; }
            let name = e.name.to_str()?;
            let ms: u128 = name.rsplit_once('.')?.1.parse().ok()?;
            let size = if e.is_dir {
                crate::trusted_open::open_dir_entry_in(root_dir, &e.name).map(|dir| bound_dir_size(&dir)).unwrap_or(0)
            } else {
                e.size
            };
            Some((ms, e.name, size))
        })
        .collect();
    out.sort_by_key(|(ms, _, _)| *ms);
    out
}

fn bound_dir_size(dir: &File) -> u64 {
    crate::trusted_open::read_dir_bound(dir).unwrap_or_default().into_iter().map(|entry| {
        if entry.is_dir {
            crate::trusted_open::open_dir_entry_in(dir, &entry.name).map(|child| bound_dir_size(&child)).unwrap_or(0)
        } else {
            entry.size
        }
    }).sum()
}

/// 目錄佔用的位元組（遞迴，只算檔案；讀不到的當 0——清理的判斷寧可低估也不要因為一個壞檔就整批不清）。
#[cfg(any(test, feature = "test-hooks"))]
pub fn dir_size(path: &Path) -> u64 {
    let Ok(md) = std::fs::symlink_metadata(path) else { return 0 };
    if !md.is_dir() {
        return md.len();
    }
    let Ok(entries) = std::fs::read_dir(path) else { return 0 };
    entries.flatten().map(|e| dir_size(&e.path())).sum()
}

/// 正在被清理的那一份改用這個結尾（issue #513）。[`entries`] 與 [`latest`] 都是拿 `rsplit_once('.')`
/// 的最後一段 `parse::<u128>`，所以 `.deleting` 結尾的一律 parse 不出來、兩邊都看不到它。
const DELETING_SUFFIX: &str = "deleting";

/// 清掉一份：**先 `rename` 成 `<原名>.<毫秒>.deleting`，成功了才 `remove_dir_all`**（issue #513）。
///
/// 直接 `remove_dir_all` 的問題是它先把裡面的檔一個個 unlink、最後才刪目錄本身，而且走的是已開啟的
/// dir fd（inode）：清到一半時 [`latest`] 還看得到這一份，[`restore_kind`] 的 `rename` 也照樣成功——
/// 目錄被搬到 `bots/<id>/` 之後這裡仍沿著同一個 inode 繼續刪，使用者拿回一個正在被清空的目錄，
/// 而還原那一步回的是 `Ok(Some(..))`。改成 rename-then-delete 之後，兩邊搶的是同一個 `rename`：
/// 還原先成功，這裡的 rename 就 `NotFound`、一個檔都不會動；這裡先成功，還原的 rename `NotFound`、
/// 回 `Err` 讓 `restore_bot` 記一行 warn（跟遠端那份的行為一致，`remote_trash::gc` 的 doc）。
pub fn remove(root_dir: &File, name: &OsStr) -> bool {
    let Some(staged) = take_aside(root_dir, name) else { return false };
    if let Err(err) = crate::trusted_open::remove_tree_in(root_dir, &staged) {
        tracing::warn!(dir = %name.to_string_lossy(), error = %err, "could not remove a bots-trash entry that was set aside");
    }
    // rename 成功就代表這一份已經不在回收區裡（還原也撈不到了）：即使 remove_dir_all 沒清乾淨，
    // 剩下的殘骸由下一輪的 [`sweep_leftovers`] 收，對呼叫端來說這一份確實清掉了。
    true
}

/// [`remove`] 的第一步：把這一份改名成看不見的 `*.deleting`。回 `None`＝沒拿到（多半是還原或另一輪 gc
/// 先把它搬走了），呼叫端**一個檔都不准動**。
pub fn take_aside(root_dir: &File, name: &OsStr) -> Option<OsString> {
    let name_str = name.to_str()?;
    let staged = OsString::from(format!("{name_str}.{}.{DELETING_SUFFIX}", now_ms()));
    match crate::trusted_open::rename_in(root_dir, name, &staged) {
        Ok(()) => Some(staged),
        // `NotFound`＝別人先拿走了，本來就不該由這裡清，不是錯。
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %name.to_string_lossy(), error = %err, "could not set a bots-trash entry aside for removal");
            }
            None
        }
    }
}

/// 上一輪在 `rename` 與 `remove_dir_all` 之間被砍掉時留下的 `*.deleting`：兩邊都看不到它們，
/// 沒有人收就會一直佔著磁碟。每輪 gc 開頭收一次（別輪正在清的那一份也可能被撈到，
/// 兩邊同時 `remove_dir_all` 同一棵樹只會讓其中一邊拿到 `NotFound`，不影響結果）。
///
/// **收掉之前它們不算進 [`MAX_BYTES`]**（[`entries`] 看不到＝`dir_size` 不會加到它們），而 gc 只在開機清掃
/// 與 [`spawn_gc`] 每天那一次跑：daemon 剛好死在那個窗口的話，磁碟上會多出一份總量上限沒算到的殘骸，
/// 最久到隔天才收。追磁碟對不上時先看回收區裡有沒有 `*.deleting`。
fn sweep_leftovers(root_dir: &File) {
    let Ok(entries) = crate::trusted_open::read_dir_bound(root_dir) else { return };
    for e in entries {
        let Some(name) = e.name.to_str() else { continue };
        if !name.ends_with(&format!(".{DELETING_SUFFIX}")) {
            continue;
        }
        if let Err(err) = crate::trusted_open::remove_tree_in(root_dir, &e.name) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(dir = %name, error = %err, "could not remove a leftover bots-trash .deleting entry");
            }
        }
    }
}

/// 清掉放超過 `keep` 的（看名字裡的時間，不看 mtime：`rename` 不會更新目錄的 mtime）。
#[cfg(any(test, feature = "test-hooks"))]
pub fn gc(data_dir: &Path, keep: Duration) -> usize {
    gc_with_cap(data_dir, keep, MAX_BYTES).0
}

/// 兩道一起跑：先清過期的，再看總量——還超過 `max_bytes` 就從**最舊的**開始清到降下來。
/// 回傳 `(過期清掉幾份, 因為超量再清掉幾份)`。
pub fn gc_with_cap(data_dir: &Path, keep: Duration, max_bytes: u64) -> (usize, usize) {
    let Ok(root_dir) = open_root(data_dir) else { return (0, 0) };
    sweep_leftovers(&root_dir);
    let cutoff = now_ms().saturating_sub(keep.as_millis());
    let mut live: Vec<(u128, OsString, u64)> = Vec::new();
    let (mut expired, mut evicted) = (0, 0);
    for (ms, name, size) in entries_in(&root_dir) {
        if ms <= cutoff && remove(&root_dir, &name) {
            expired += 1;
        } else {
            live.push((ms, name, size));
        }
    }
    let mut total: u64 = live.iter().map(|(_, _, size)| *size).sum();
    // 最舊的先走；最新那一份永遠留著（剛刪掉的那顆才是最可能要還原的，留著才有意義）。
    for (_, name, size) in live.iter().take(live.len().saturating_sub(1)) {
        if total <= max_bytes {
            break;
        }
        if remove(&root_dir, name) {
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
pub fn spawn_gc<H>(app: Arc<H>)
where
    H: crate::capabilities::DataDir + crate::capabilities::BgTasks + crate::capabilities::Shutdown + 'static,
{
    crate::background_loop::spawn_periodic(&app, "bots-trash gc", GC_EVERY, GC_EVERY, |app| async move {
        let (expired, evicted) = gc_with_cap(app.data_dir(), keep_duration(), MAX_BYTES);
        if expired > 0 || evicted > 0 {
            tracing::info!(expired, evicted, days = KEEP_DAYS, "daily bots-trash sweep");
        }
    });
}
