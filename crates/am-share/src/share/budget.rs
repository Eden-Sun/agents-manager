//! 受限分享 bot 的沙箱總預算（#853，派工者決定）：工作目錄（含 `inbox/`）＋ outbox 合計 [`SANDBOX_MAX_BYTES`]（1 GiB）。
//!
//! 受限 bot 的 Write／Edit 可以無限長（`cage.rs` 給工作目錄與 outbox `/**`），上限原本只在 HTTP 上傳的 `inbox/`。
//! 這裡由 daemon 定期量測（[`REFRESH_EVERY`]；全程 fd-bound、不跟符號連結，見 [`crate::outbox::tree_usage`]），
//! 分享頁送訊息前（[`is_full`]）看最近一次量測值：滿了回 507 `share_storage_full`、分享頁顯示「空間滿了」，並通知擁有者
//! （`runners::share_budget`）。**不自動刪工作目錄的檔**——那是擁有者的東西；outbox 另有 `outbox-gc.sh` 的分享保留政策（#850）。
//!
//! 量測值只放記憶體（重啟後第一次送訊息或第一輪巡邏重新量）。量不到（目錄讀不出來）當作沒有資料，不擋人。

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::outbox::ShareStorage;

/// 一顆受限分享 bot 的沙箱總預算：工作目錄（含 `inbox/`）加 outbox。
pub const SANDBOX_MAX_BYTES: u64 = 1024 * 1024 * 1024;
/// 量測值多久重量一次。
pub const REFRESH_EVERY: Duration = Duration::from_secs(10 * 60);
/// 已經滿了的量測值只信這麼久：擁有者清掉檔案後，不必等 [`REFRESH_EVERY`] 才恢復。
const FULL_RECHECK: Duration = Duration::from_secs(60);

/// 一次量測的結果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Measured {
    pub workspace_bytes: u64,
    pub outbox_bytes: u64,
    pub files: u64,
    /// 樹太大、量到一半停下：數字是下限。
    pub truncated: bool,
}

impl Measured {
    pub fn total(&self) -> u64 {
        self.workspace_bytes.saturating_add(self.outbox_bytes)
    }

    /// 達到預算就算滿（量不完的樹只看已量到的位元組，不因為「太大」本身就擋人）。
    pub fn full(&self) -> bool {
        self.total() >= SANDBOX_MAX_BYTES
    }
}

fn cache() -> &'static Mutex<HashMap<String, (Measured, Instant)>> {
    static CACHE: OnceLock<Mutex<HashMap<String, (Measured, Instant)>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

/// 最近一次量測值與它的年紀。
pub fn cached(bot_id: &str) -> Option<(Measured, Duration)> {
    cache().lock().unwrap_or_else(|e| e.into_inner()).get(bot_id).map(|(m, at)| (*m, at.elapsed()))
}

fn record(bot_id: &str, m: Measured) -> Option<Measured> {
    cache().lock().unwrap_or_else(|e| e.into_inner()).insert(bot_id.to_string(), (m, Instant::now())).map(|(prev, _)| prev)
}

#[cfg(test)]
pub fn set_cached_for_test(bot_id: &str, m: Measured) {
    record(bot_id, m);
}

#[cfg(test)]
pub fn clear_cached_for_test(bot_id: &str) {
    cache().lock().unwrap_or_else(|e| e.into_inner()).remove(bot_id);
}

/// 量一顆受限 bot：工作目錄（`workspace`）一棵、`<data_dir>/outbox/<bot_id>` 一棵。blocking，呼叫端丟進 `spawn_blocking`。
/// 工作目錄不存在算 0；outbox 的可信檢查沒過（被換成符號連結等）回 `Err`。
pub fn measure_blocking(data_dir: &Path, workspace: &Path, bot_id: &str) -> Result<Measured, ()> {
    let mut m = Measured::default();
    match crate::trusted_open::open_bound_dir(workspace, &[], None) {
        Ok(fd) => {
            let u = crate::outbox::tree_usage(&fd).map_err(|_| ())?;
            m.workspace_bytes = u.bytes;
            m.files += u.files;
            m.truncated |= u.truncated;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(()),
    }
    if let Some(dir) = crate::outbox::dir_for(data_dir, bot_id) {
        if let Some(fd) = crate::outbox::open_trusted_dir(data_dir, &dir)? {
            let u = crate::outbox::tree_usage(&fd).map_err(|_| ())?;
            m.outbox_bytes = u.bytes;
            m.files += u.files;
            m.truncated |= u.truncated;
        }
    }
    Ok(m)
}

/// 重量一顆受限 bot 並記下。回 `(這次, 上次)`；不是受限分享 bot（沒有籠子工作目錄）或量不到回 `None`（不擋、不通知）。
pub async fn refresh<S: ShareStorage>(app: &S, bot_id: &str) -> Option<(Measured, Option<Measured>)> {
    let workspace = crate::share::store::restricted_workspace(app.db_pool(), bot_id).await.ok().flatten()?;
    let (data_dir, id) = (app.data_dir().to_path_buf(), bot_id.to_string());
    let measured = tokio::task::spawn_blocking(move || measure_blocking(&data_dir, Path::new(&workspace), &id)).await;
    match measured {
        Ok(Ok(m)) => Some((m, record(bot_id, m))),
        _ => {
            tracing::warn!(bot = %bot_id, "share sandbox usage could not be measured");
            None
        }
    }
}

/// 分享頁送訊息前問：這顆的沙箱滿了嗎。用最近一次量測值（沒有或太舊才現場量一次）；量不到當作沒滿。
pub async fn is_full<S: ShareStorage>(app: &S, bot_id: &str) -> bool {
    let fresh = cached(bot_id).filter(|(m, age)| *age < if m.full() { FULL_RECHECK } else { REFRESH_EVERY });
    let m = match fresh {
        Some((m, _)) => Some(m),
        None => refresh(app, bot_id).await.map(|(m, _)| m),
    };
    m.is_some_and(|m| m.full())
}

/// 巡邏用：重量所有本機的受限分享 bot，回「這一輪剛變滿」的（上一輪不是滿的，或這是第一次量）。
pub async fn sweep<S: ShareStorage>(app: &S) -> Vec<(String, Measured)> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT r.bot_id FROM shared_bots r JOIN bots b ON b.id = r.bot_id AND b.deleted_at IS NULL
           JOIN projects p ON p.id = b.project_id AND p.deleted_at IS NULL AND p.host = 'local'
          WHERE r.profile = 'restricted'",
    )
    .fetch_all(app.db_pool())
    .await
    .unwrap_or_else(|e| {
        tracing::warn!(error = %e, "could not list share bots for the sandbox budget sweep");
        Vec::new()
    });
    let mut newly_full = Vec::new();
    for id in ids {
        if let Some((now, before)) = refresh(app, &id).await {
            if now.full() && !before.is_some_and(|b| b.full()) {
                newly_full.push((id, now));
            }
        }
    }
    newly_full
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("am-share-budget-{tag}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn the_sandbox_counts_workspace_inbox_and_outbox_but_not_symlink_targets() {
        let data = dir("data");
        let ws = dir("ws");
        std::fs::create_dir_all(ws.join("inbox")).unwrap();
        std::fs::create_dir_all(ws.join("a/b")).unwrap();
        std::fs::write(ws.join("notes.md"), vec![0u8; 1000]).unwrap();
        std::fs::write(ws.join("inbox/up.bin"), vec![0u8; 2000]).unwrap();
        std::fs::write(ws.join("a/b/deep.txt"), vec![0u8; 300]).unwrap();
        // 指到工作目錄外的大檔與目錄：既不跟、也不算。
        let outside = dir("outside");
        std::fs::write(outside.join("huge.bin"), vec![0u8; 50_000]).unwrap();
        symlink(outside.join("huge.bin"), ws.join("link-file")).unwrap();
        symlink(&outside, ws.join("link-dir")).unwrap();
        let out = crate::outbox::dir_for(&data, "BOT1").unwrap();
        std::fs::create_dir_all(&out).unwrap();
        std::fs::write(out.join("card.png"), vec![0u8; 4000]).unwrap();

        let m = measure_blocking(&data, &ws, "BOT1").unwrap();
        assert_eq!((m.workspace_bytes, m.outbox_bytes, m.files), (3300, 4000, 4));
        assert!(!m.full() && !m.truncated);
        for d in [data, ws, outside] {
            std::fs::remove_dir_all(d).ok();
        }
    }

    #[test]
    fn a_missing_workspace_is_zero_and_the_budget_is_full_at_one_gib() {
        let data = dir("data2");
        let m = measure_blocking(&data, &data.join("no-such-folder"), "BOT2").unwrap();
        assert_eq!(m, Measured::default());
        let almost = Measured { workspace_bytes: SANDBOX_MAX_BYTES - 1, ..Default::default() };
        assert!(!almost.full());
        assert!(Measured { outbox_bytes: 1, ..almost }.full(), "workspace＋outbox 合計到 1 GiB 就滿");
        std::fs::remove_dir_all(data).ok();
    }

    #[test]
    fn a_symlinked_outbox_is_refused_not_measured() {
        let data = dir("data3");
        let elsewhere = dir("elsewhere");
        std::fs::create_dir_all(data.join("outbox")).unwrap();
        symlink(&elsewhere, data.join("outbox").join("BOT3")).unwrap();
        assert!(measure_blocking(&data, &data.join("ws"), "BOT3").is_err());
        for d in [data, elsewhere] {
            std::fs::remove_dir_all(d).ok();
        }
    }
}
