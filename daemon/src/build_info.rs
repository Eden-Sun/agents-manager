//! 這顆正在跑的 daemon 是哪一版、什麼時候上線的。
//!
//! 正式 daemon 的 binary 跟 repo 是兩件事：`origin/main` 動了不代表上線了，重建之後也要有人
//! 說得出「現在跑的是哪一版」。這兩個事實餵給 `GET /api/supervisor` 的 `last_deploy`，前端用它
//! 排除上次上線以前的舊申請，AGM 用它判斷該不該再排一次重建。
//!
//! `at` 是**這顆 binary 第一次跑起來**的時間，不是這個 process 起來的時間（review 2026-09-16 c3 L3）：
//! binary 沒換的重啟（launchd 拉回、restart 窗口、手動重啟）會把它往前推，上線前提出的重建申請
//! 就從 chip 與清單上消失，而腳本照 `daemon-update.built` 的 mtime 仍然數得到它們。
//! 記在資料目錄的 `last-deploy.json`；sha 跟上次一樣就沿用上次的時間。

use std::path::Path;
use std::sync::OnceLock;

/// 建置時由 `build.rs` 寫進來的 short sha；拿不到 git 時是 `unknown`。
pub const BUILD_SHA: &str = env!("AM_BUILD_SHA");

const FILE: &str = "last-deploy.json";

static STARTED_AT: OnceLock<String> = OnceLock::new();
static DEPLOYED_AT: OnceLock<String> = OnceLock::new();

/// `serve` 一啟動就記下來：process 起來的時間，以及這顆 binary 的上線時間。
/// 重複呼叫不會改寫（第一次的才算）。
pub fn mark_started(data_dir: &Path) {
    let _ = STARTED_AT.set(crate::db::now());
    let _ = DEPLOYED_AT.set(deployed_at(data_dir, BUILD_SHA, &crate::db::now()));
}

/// 這個 process 起來的時間。還沒記過（單元測試直接呼叫）就當場記一次。
pub fn started_at() -> String {
    STARTED_AT.get_or_init(crate::db::now).clone()
}

/// 這顆 binary 第一次跑起來的時間，持久化在 `<data_dir>/last-deploy.json`。
///
/// * 檔裡的 sha 跟現在這顆一樣 → 沿用檔裡的時間（binary 沒換的重啟不算新的上線）。
/// * 不一樣、讀不到、或壞掉 → 現在就是上線時間，寫回去。
/// * sha 是 `unknown`（沒有 git 的建置）→ 分不出版本，一律用 process 起來的時間，也不寫檔。
/// * 寫不進去（唯讀目錄）→ 照樣回這一次的時間，只是下次重啟會再算一次。
fn deployed_at(data_dir: &Path, sha: &str, now: &str) -> String {
    if sha.is_empty() || sha == "unknown" {
        return now.to_string();
    }
    let path = data_dir.join(FILE);
    let stored = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            let at = v.get("at").and_then(serde_json::Value::as_str)?.to_string();
            let seen = v.get("sha").and_then(serde_json::Value::as_str)?;
            (seen == sha && !at.trim().is_empty()).then_some(at)
        });
    if let Some(at) = stored {
        return at;
    }
    let body = serde_json::json!({"sha": sha, "at": now}).to_string();
    if let Err(e) = std::fs::write(&path, body) {
        tracing::warn!(path = %path.display(), error = %e, "could not record this binary's deploy time; it will be recomputed on the next start");
    }
    now.to_string()
}

/// `{sha, at}`：現在跑的是哪一版，以及它是什麼時候上線的。
pub fn last_deploy() -> serde_json::Value {
    serde_json::json!({"sha": BUILD_SHA, "at": DEPLOYED_AT.get().cloned().unwrap_or_else(started_at)})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 沒有 git 也要能編、能答：說 `unknown` 比編一個假 sha 好。
    #[test]
    fn the_build_sha_is_present_and_the_start_time_is_stable() {
        assert!(!BUILD_SHA.is_empty());
        assert_eq!(started_at(), started_at(), "同一個 process 只有一個啟動時間");
        let v = last_deploy();
        assert_eq!(v["sha"], BUILD_SHA);
    }

    /// binary 沒換的重啟不是新的上線：`at` 要沿用上一次的。換了 sha 才是新的上線。
    #[test]
    fn only_a_new_binary_moves_the_deploy_time() {
        let dir = std::env::temp_dir().join(format!("am-deploy-{}", crate::db::ulid()));
        std::fs::create_dir_all(&dir).unwrap();

        let first = deployed_at(&dir, "abc1234", "2026-09-16T10:00:00Z");
        assert_eq!(first, "2026-09-16T10:00:00Z", "第一次跑：現在就是上線時間");
        // 同一顆 binary 重啟兩次：時間不動。
        assert_eq!(deployed_at(&dir, "abc1234", "2026-09-16T10:20:00Z"), first);
        assert_eq!(deployed_at(&dir, "abc1234", "2026-09-16T18:00:00Z"), first);
        // 換了 binary：這一刻才是新的上線時間，而且寫回去。
        assert_eq!(deployed_at(&dir, "def5678", "2026-09-17T09:00:00Z"), "2026-09-17T09:00:00Z");
        assert_eq!(deployed_at(&dir, "def5678", "2026-09-17T09:30:00Z"), "2026-09-17T09:00:00Z");

        // 壞掉的檔案不能讓 daemon 起不來，也不該回一個空時間。
        std::fs::write(dir.join(FILE), "not json").unwrap();
        assert_eq!(deployed_at(&dir, "def5678", "2026-09-17T10:00:00Z"), "2026-09-17T10:00:00Z");
        // 沒有 git 的建置分不出版本：用這個 process 的時間，不寫檔。
        let before = std::fs::read_to_string(dir.join(FILE)).unwrap();
        assert_eq!(deployed_at(&dir, "unknown", "2026-09-17T11:00:00Z"), "2026-09-17T11:00:00Z");
        assert_eq!(std::fs::read_to_string(dir.join(FILE)).unwrap(), before);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
