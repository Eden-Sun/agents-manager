//! #1862 S3b：從 `crates/am-supervisor/src/supervisor/timing.rs` 搬來的 daemon 整合測試；``supervisor::timing::tests::…` → `supervisor_tests::timing::tests::…``。
//! 測試本文與斷言不變；私有項由 am-supervisor 的 `*_for_test`（test-hooks）轉呼叫，下面用別名維持原名。

#![allow(unused_imports)]

use crate::supervisor::timing::*;
use sqlx::SqlitePool;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

mod tests {
    use super::*;

    fn stats(key: &str) -> Value {
        snapshot()["stats"][key].clone()
    }

    fn logged(text: &str, needle: &str) -> bool {
        text.lines().any(|l| l.contains(needle))
    }

    /// 沒超標的一段**一個字都不寫**，但統計要記到；超標才寫一行，而且帶段名、bot 數、耗時。
        #[tokio::test(flavor = "current_thread")]
    async fn a_segment_is_only_logged_when_it_goes_over_the_budget() {
        let e = crate::testing::env().await;
        let (logs, _guard) = crate::config_audit::capture::start();
        const SLOW_LINE: &str = "supervisor tick segment was slow";

        seg(&e.app, "quick", async {}).await;
        assert!(!logged(&logs.text(), SLOW_LINE), "正常的一段不該寫那一行：{:?}", logs.text());
        assert_eq!(stats("tick:quick")["count"], json!(1), "沒寫 log 不代表沒量");

        note_segment(&e.app, "just-under", SLOW_SEGMENT - Duration::from_millis(1)).await;
        assert!(!logged(&logs.text(), SLOW_LINE), "差一毫秒也不寫：{:?}", logs.text());

        note_segment(&e.app, "slow", SLOW_SEGMENT).await;

        let text = logs.text();
        let line = text.lines().find(|l| l.contains(SLOW_LINE)).unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains("segment=\"slow\"") || line.contains("segment=slow"), "要寫哪一段：{line}");
        assert!(line.contains("bots="), "要帶 bot 數：{line}");
        assert!(line.contains("took_ms="), "要帶耗時：{line}");
        assert!(!text.contains("quick") && !text.contains("just-under"), "沒超標的那幾段不該出現：{text}");
        assert_eq!(stats("tick:just-under")["count"], json!(1), "沒超標的照樣進統計");
    }

    /// 整拍同理：沒超過一個心跳就不寫。
        #[tokio::test(flavor = "current_thread")]
    async fn a_tick_is_only_logged_when_it_outruns_one_heartbeat() {
        let e = crate::testing::env().await;
        let (logs, _guard) = crate::config_audit::capture::start();
        const SLOW_LINE: &str = "longer than one heartbeat";
        let before = stats(TICK_TOTAL)["count"].as_u64().unwrap_or(0);

        note_tick(&e.app, SLOW_TICK - Duration::from_millis(1)).await;
        assert!(!logged(&logs.text(), SLOW_LINE), "剛好沒超過就不寫：{:?}", logs.text());

        note_tick(&e.app, SLOW_TICK).await;
        let text = logs.text();
        let line = text.lines().find(|l| l.contains(SLOW_LINE)).unwrap_or_else(|| panic!("{text}"));
        assert!(line.contains("bots=") && line.contains("took_ms="), "{line}");
        assert_eq!(text.lines().filter(|l| l.contains(SLOW_LINE)).count(), 1, "只有超標那一次寫：{text}");
        let after = stats(TICK_TOTAL)["count"].as_u64().unwrap_or(0);
        assert_eq!(after - before, 2, "兩次都要進統計，只有一次進 log");
    }
}
