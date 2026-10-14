//! claude 壓縮對話時「壓縮中…」（issue #1770）：手動 `/compact`、閒置壓縮、context 快滿的自動壓縮都一樣。
//!
//! 兩路訊號：claude 的 `PreCompact` hook（[`begin`]），以及終端備援讀到畫面上的 `Compacting conversation…` spinner
//! （[`observe_screen`]）。收掉的時機：`SessionStart`（`source: compact`）、`Stop`／`StopFailure`（[`end`]），或超過
//! [`STALE_SECS`] 還沒收（取消、hook 沒來）。畫面來的標記只由畫面消失收掉，不蓋掉 hook 來的。
//!
//! 跟 [`crate::prompt_suggestion`] 一樣記在記憶體（run id → 開始時間），不存 DB；投影進 run JSON 的 `compacting_since`，
//! 網頁在側欄與對話區顯示「壓縮中…」，跟 `working` 分開。

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// 超過這段還沒被收掉，就當作壓縮已經不在了（投影不帶）。大 context 的自動壓縮實測約 40 秒。
pub const STALE_SECS: i64 = 10 * 60;

#[derive(Clone, Copy)]
struct Mark {
    since: DateTime<Utc>,
    from_screen: bool,
}

fn store() -> &'static Mutex<HashMap<String, Mark>> {
    static S: OnceLock<Mutex<HashMap<String, Mark>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn locked() -> std::sync::MutexGuard<'static, HashMap<String, Mark>> {
    store().lock().unwrap_or_else(|e| e.into_inner())
}

/// `PreCompact` hook：這個 run 開始壓縮了。畫面先看到的話改記成 hook 來的，開始時間取先到的那個。
/// 回傳這次是不是新開的標記（網頁要重讀；畫面先標過的不算新的）。
pub fn begin(run_id: &str, now: DateTime<Utc>) -> bool {
    let mut m = locked();
    match m.get_mut(run_id) {
        Some(mark) => {
            mark.from_screen = false;
            false
        }
        None => {
            m.insert(run_id.to_string(), Mark { since: now, from_screen: false });
            true
        }
    }
}

/// 收掉（`SessionStart` source=compact、`Stop`、`StopFailure`）。畫面來的與 hook 來的一起收。回傳有沒有真的收掉。
pub fn end(run_id: &str) -> bool {
    locked().remove(run_id).is_some()
}

/// 終端備援：畫面上有 spinner 就記下來；畫面沒了只收掉自己記的（hook 來的等 hook 收）。回傳有沒有變。
pub fn observe_screen(run_id: &str, shown: bool, now: DateTime<Utc>) -> bool {
    let mut m = locked();
    if shown {
        if m.contains_key(run_id) {
            return false;
        }
        m.insert(run_id.to_string(), Mark { since: now, from_screen: true });
        return true;
    }
    match m.get(run_id) {
        Some(mark) if mark.from_screen => {
            m.remove(run_id);
            true
        }
        _ => false,
    }
}

/// 畫面上有沒有壓縮中的 spinner 行（`Compacting conversation…`，前面可以有 spinner 字符）。
/// 只看行首：對話裡引用這句話不會被當成正在壓縮。
pub fn screen_shows(text: &str) -> bool {
    text.lines().any(|line| {
        let s = line.trim().trim_start_matches(|c: char| !c.is_alphanumeric()).trim_start();
        s.starts_with("Compacting conversation") && (s.contains('…') || s.contains("..."))
    })
}

/// 這個 run 的壓縮開始時間（沒有標記、或超過 [`STALE_SECS`] 就是 `None`）。
pub fn since_at(run_id: &str, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    let mark = *locked().get(run_id)?;
    ((now - mark.since).num_seconds() <= STALE_SECS).then_some(mark.since)
}

/// 放進 run JSON 的 `compacting_since`（ISO 8601，`db::iso_at` 格式）；沒有就是 `null`。
pub fn json_at(run_id: &str, now: DateTime<Utc>) -> Value {
    since_at(run_id, now).map(crate::db::iso_at).into()
}

/// 不在 `active` 裡的 run（結束了）不留記錄。
pub fn retain_runs(active: &[String]) {
    locked().retain(|id, _| active.contains(id));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_790_000_000 + secs, 0).unwrap()
    }

    /// 記憶體帳是全域的：`retain_runs` 會清掉別的測試的標記，所以測試一個接一個跑。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_pre_compact_hook_marks_the_run_until_the_session_or_the_turn_ends() {
        let _serial = serial();
        let id = "compacting-hook-ends";
        assert!(begin(id, at(0)), "新開的標記要讓網頁重讀");
        assert_eq!(json_at(id, at(5)), Value::String(crate::db::iso_at(at(0))));
        assert!(end(id), "SessionStart(compact)／Stop 收掉");
        assert_eq!(json_at(id, at(5)), Value::Null);
        assert!(!end(id), "已經收掉就沒有變");
    }

    #[test]
    fn a_mark_goes_stale_after_ten_minutes_without_an_end_event() {
        let _serial = serial();
        let id = "compacting-stale";
        begin(id, at(0));
        assert!(since_at(id, at(STALE_SECS)).is_some(), "剛好十分鐘還算");
        assert_eq!(json_at(id, at(STALE_SECS + 1)), Value::Null, "超過就不帶");
    }

    #[test]
    fn the_screen_spinner_marks_and_only_the_screen_clears_its_own_mark() {
        let _serial = serial();
        let id = "compacting-screen";
        assert!(observe_screen(id, true, at(0)));
        assert!(!observe_screen(id, true, at(3)), "已經標過就不再算變");
        assert_eq!(json_at(id, at(3)), Value::String(crate::db::iso_at(at(0))), "開始時間取先到的");
        assert!(observe_screen(id, false, at(4)), "畫面的 spinner 消失就收掉自己標的");
        assert_eq!(json_at(id, at(4)), Value::Null);
    }

    #[test]
    fn a_screen_clear_does_not_drop_a_mark_the_hook_set() {
        let _serial = serial();
        let id = "compacting-hook-wins";
        begin(id, at(0));
        assert!(!observe_screen(id, false, at(1)), "hook 來的標記等 hook 收");
        assert!(json_at(id, at(2)).is_string());
    }

    #[test]
    fn a_hook_after_the_screen_mark_keeps_the_earlier_start() {
        let _serial = serial();
        let id = "compacting-screen-then-hook";
        observe_screen(id, true, at(0));
        assert!(!begin(id, at(7)), "畫面先標過，hook 來不算新的");
        assert_eq!(json_at(id, at(8)), Value::String(crate::db::iso_at(at(0))));
        assert!(!observe_screen(id, false, at(9)), "之後畫面沒了也不蓋掉 hook 來的");
        end(id);
    }

    #[test]
    fn the_spinner_line_is_recognised_and_a_quotation_is_not() {
        let _serial = serial();
        assert!(screen_shows("some reply\n✻ Compacting conversation… (12s · esc to interrupt)"));
        assert!(screen_shows("Compacting conversation..."));
        assert!(!screen_shows("我在解釋 Compacting conversation 這行是什麼意思"));
        assert!(!screen_shows("Compacted while idle, before the prompt cache expired"));
        assert!(!screen_shows(""));
    }

    #[test]
    fn a_run_that_ended_loses_its_mark() {
        let _serial = serial();
        let id = "compacting-retained";
        begin(id, at(0));
        retain_runs(&["someone-else".to_string()]);
        assert_eq!(json_at(id, at(1)), Value::Null);
    }
}
