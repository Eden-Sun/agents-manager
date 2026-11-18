#![allow(unused_imports)]
//! #1862 L2-2：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

// 兩邊重複的純函式測試（crate 側原樣留在 am-lifecycle，只在 crate-only 編）：daemon 側保留複本，維持各自一次執行。
mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::resume_context::test_seams::*;
    // parent-uses
    use crate::lifecycle::*;
    use serde_json::{json, Value};
    use sqlx::SqlitePool;
    use crate::lifecycle::resume_context::*;

    fn measured(session: &str) -> Value {
        json!({"session_id": session, "model": {"id": "opus"},
            "context_window": {"used_percentage": 42, "context_window_size": 1000000,
                "total_input_tokens": 9999999, "current_usage": {"input_tokens": 420000}},
            "prompt_cache": {"warm": true}, "cost": {"total_cost_usd": 12},
            "account_email": "old@example.com", "rate_limits": {"five_hour": {"used_percentage": 99}}})
    }

    #[test]
    fn restores_only_context_with_strict_payload_session_identity() {
        let old = measured("s1");
        let restored = snapshot(&old, "s1", "opus").unwrap();
        assert_eq!(restored, json!({"session_id": "s1", "context_window": {
            "used_percentage": 42, "context_window_size": 1000000, "total_input_tokens": 420000},
            (MARKER): "s1", (MODEL): "opus"}));
        assert!(snapshot(&old, "s2", "opus").is_none());
        let mut legacy = old.clone();
        legacy.as_object_mut().unwrap().remove("session_id");
        assert!(snapshot(&legacy, "s1", "opus").is_none(), "legacy data without payload identity is unknown");
        let mut conflicting = restored.clone();
        conflicting[MARKER] = json!("other");
        assert!(snapshot(&conflicting, "s1", "opus").is_none());
        assert!(snapshot(&restored, "s1", "opus").is_some(), "validated provisional data can survive another resume");
    }

    #[test]
    fn blank_redraw_preserves_context_until_real_input_even_rounded_zero() {
        let restored = snapshot(&measured("s1"), "s1", "opus").unwrap().to_string();
        for pct in [Value::Null, json!(0)] {
            let mut blank = json!({"session_id": "s1", "context_window": {
                "used_percentage": pct, "total_input_tokens": 0, "total_output_tokens": 0,
                "context_window_size": 1000000}, "model": {"id": "opus"}});
            retain_until_usage(&mut blank, Some(&restored));
            assert_eq!(blank["context_window"]["used_percentage"], 42);
            assert_eq!(blank[MARKER], "s1");
        }
        for pct in [0, 23] {
            let mut fresh = json!({"session_id": "s1", "context_window": {"used_percentage": pct, "total_input_tokens": 300}});
            let expected = fresh.clone();
            retain_until_usage(&mut fresh, Some(&restored));
            assert_eq!(fresh, expected);
        }
        let mut other = json!({"session_id": "other"});
        retain_until_usage(&mut other, Some(&restored));
        assert!(other.get("context_window").is_none());
    }

    #[test]
    fn a_new_window_or_model_invalidates_the_old_ratio_and_keeps_a_barrier() {
        let restored = snapshot(&measured("s1"), "s1", "opus").unwrap().to_string();
        for (model, size) in [("opus", 200000), ("sonnet", 1000000)] {
            let mut incoming = json!({"session_id": "s1", "model": {"id": model},
                "context_window": {"used_percentage": 0, "total_input_tokens": 0, "context_window_size": size}});
            retain_until_usage(&mut incoming, Some(&restored));
            assert_eq!(incoming["context_window"]["context_window_size"], size);
            assert_ne!(incoming["context_window"]["used_percentage"], 42);
            assert!(incoming.get(MARKER).is_none());
            assert_eq!(incoming[INVALIDATED], "s1");
            let mut blank = json!({"session_id": "s1"});
            retain_until_usage(&mut blank, Some(&incoming.to_string()));
            assert_eq!(blank[INVALIDATED], "s1", "blank redraw cannot erase the invalidation boundary");
            let mut unidentified = json!({"context_window": {"used_percentage": 0}});
            retain_until_usage(&mut unidentified, Some(&blank.to_string()));
            assert_eq!(unidentified[INVALIDATED], "s1", "an unidentified redraw cannot erase the barrier");
        }
        assert!(model_matches("opus", "claude-opus-4-6"));
        assert!(!model_matches("opus[1m]", "opus"));
        assert!(!model_matches("claude-opus-4-6", "claude-opus-4-5"));
    }
}
