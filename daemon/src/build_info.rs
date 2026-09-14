//! 這顆正在跑的 daemon 是哪一版、什麼時候起來的。
//!
//! 正式 daemon 的 binary 跟 repo 是兩件事：`origin/main` 動了不代表上線了，重建之後也要有人
//! 說得出「現在跑的是哪一版」。這兩個事實餵給 `GET /api/supervisor` 的 `last_deploy`，前端用它
//! 排除上次上線以前的舊申請，AGM 用它判斷該不該再排一次重建。

use std::sync::OnceLock;

/// 建置時由 `build.rs` 寫進來的 short sha；拿不到 git 時是 `unknown`。
pub const BUILD_SHA: &str = env!("AM_BUILD_SHA");

static STARTED_AT: OnceLock<String> = OnceLock::new();

/// `serve` 一啟動就記下來。重複呼叫不會改寫（第一次的時間才是這個 process 起來的時間）。
pub fn mark_started() {
    let _ = STARTED_AT.set(crate::db::now());
}

/// 這個 process 起來的時間。還沒記過（單元測試直接呼叫）就當場記一次。
pub fn started_at() -> String {
    STARTED_AT.get_or_init(crate::db::now).clone()
}

/// `{sha, at}`：上次成功上線的版本與時間。
pub fn last_deploy() -> serde_json::Value {
    serde_json::json!({"sha": BUILD_SHA, "at": started_at()})
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
        assert_eq!(v["at"], started_at());
    }
}
