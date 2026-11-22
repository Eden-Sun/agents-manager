#![allow(unused_imports)]

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::background_jobs::*;
    use crate::background_jobs::test_seams::*;
    use crate::runners::background_jobs::observe;
    use std::collections::HashMap;

    fn fixture(name: &str) -> String {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let candidates = [
            manifest.join("src/lifecycle/fixtures"),
            manifest.join("../crates/am-lifecycle/src/lifecycle/fixtures"),
            manifest.join("../am-lifecycle/src/lifecycle/fixtures"),
        ];
        let path = candidates.iter().map(|dir| dir.join(name)).find(|path| path.is_file()).expect("fixture exists");
        std::fs::read_to_string(path).unwrap()
    }

    #[tokio::test]
    async fn a_long_running_background_is_projected_with_its_age_and_flagged_in_health() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let run = Some(db::run(&app.db, &run_id).await.unwrap().unwrap());

        let v = run_json(&app, &run, Some(&run_id));
        assert_eq!((v["background_since"].clone(), v["background_secs"].clone(), v["background_stuck"].clone()), (Value::Null, Value::Null, false.into()), "沒看過");

        record_at(&mut app.background_jobs.lock().unwrap(), &run_id, 1, now_ms() - 1_800_000);
        let v = run_json(&app, &run, Some(&run_id));
        let secs = v["background_secs"].as_i64().unwrap();
        assert!((1800..1860).contains(&secs), "持續秒數：{secs}");
        assert!(v["background_since"].as_str().unwrap().ends_with('Z'));
        assert_eq!(v["background_stuck"], false);
        let health = crate::supervisor::health::snapshot(&app).await.unwrap();
        assert_eq!(health["background_stuck"], serde_json::json!([]), "沒到門檻不列");

        app.background_jobs.lock().unwrap().clear();
        record_at(&mut app.background_jobs.lock().unwrap(), &run_id, 2, now_ms() - 4 * 3_600_000);
        let v = run_json(&app, &run, Some(&run_id));
        assert_eq!(v["background_stuck"], true);
        let health = crate::supervisor::health::snapshot(&app).await.unwrap();
        let stuck = health["background_stuck"].as_array().unwrap();
        assert_eq!(stuck.len(), 1);
        assert_eq!(stuck[0]["bot_id"], bot.id.as_str());
        assert_eq!(stuck[0]["background_jobs"], 2);
        assert!(stuck[0]["secs"].as_i64().unwrap() >= 4 * 3600);

        record(&mut app.background_jobs.lock().unwrap(), &run_id, 0);
        let v = run_json(&app, &run, Some(&run_id));
        assert_eq!((v["background_since"].clone(), v["background_stuck"].clone()), (Value::Null, false.into()));
        let health = crate::supervisor::health::snapshot(&app).await.unwrap();
        assert_eq!(health["background_stuck"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn nr1892_run_json_explains_missing_identity_without_changing_status_or_errors() {
        let env = crate::testing::env().await;
        for (adopted, native, terminal, reason) in [
            (1, None, None, Some("missing_instance_identity")),
            (1, Some(" "), Some(""), Some("missing_instance_identity")),
            (1, Some("native"), None, None),
            (1, None, Some("terminal"), None),
            (0, None, None, None),
        ] {
            let input = Some(serde_json::json!({"adopted":adopted, "native_session_id":native,
                "terminal_id":terminal, "state":"running", "agent_status":"idle", "turn_error":"original error"}));
            let out = run_json(&env.app, &input, Some("test-run"));
            assert_eq!(out["identity_recovery_reason"], serde_json::json!(reason));
            assert_eq!(out["state"], "running");
            assert_eq!(out["agent_status"], "idle");
            assert_eq!(out["turn_error"], "original error");
        }
    }

    /// #1674：回合錯誤的種類跟著 run 一起出去（`/api/state` 與 `bot_status` 共用這支），下一回合清掉就沒有。
    #[tokio::test]
    async fn run_json_carries_the_turn_error_kind() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let kind_now = || async {
            let run = Some(db::run(&app.db, &run_id).await.unwrap().unwrap());
            run_json(&app, &run, Some(&run_id))["turn_error_kind"].clone()
        };
        assert_eq!(kind_now().await, Value::Null, "沒有錯誤：欄位在、值是 null");
        let capacity = "模型目前滿載，請換模型或稍後再試（Selected model is at capacity. Please try a different model.）";
        sqlx::query("UPDATE runs SET turn_error=? WHERE id=?").bind(capacity).bind(&run_id).execute(&app.db).await.unwrap();
        assert_eq!(kind_now().await, "model_at_capacity");
        sqlx::query("UPDATE runs SET turn_error=? WHERE id=?").bind("API Error: Connection lost mid-response.").bind(&run_id).execute(&app.db).await.unwrap();
        assert_eq!(kind_now().await, "api_error");
        sqlx::query("UPDATE runs SET turn_error=NULL WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        assert_eq!(kind_now().await, Value::Null);
    }

    #[tokio::test]
    async fn a_count_is_published_on_change_and_dropped_with_its_run() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "alfa").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let mut rx = app.subscribe();
        let live = fixture("claude-2.1.281-background-shell.txt");
        let client = app.herdr_for_run(&run).await.expect("mock herdr");
        let pane = run.pane_id.clone().unwrap_or_default();

        observe(&app, &run, "claude", &live, &client, &pane).await;
        assert_eq!(get(&app, &run_id), 1);
        let frame = rx.try_recv().expect("數字變了要推 bot_status");
        assert_eq!(frame.kind, "bot_status");
        let state = crate::api::state_json(&app).await.unwrap();
        assert_eq!(state["projects"][0]["bots"][0]["run"]["background_jobs"], 1, "GET /api/state 帶得出來");

        observe(&app, &run, "claude", &live, &client, &pane).await;
        assert!(rx.try_recv().is_err(), "沒變就不推");

        retain_runs(&app, &[]);
        assert_eq!(get(&app, &run_id), 0, "結束的 run 不留帳");
    }
}

mod stuck_notice_tests {
    use crate::background_jobs::*;
    use crate::background_jobs::test_seams::*;
    use std::collections::HashMap;

    const T: i64 = STUCK_AFTER_SECS * 1000;

    #[test]
    fn nothing_is_returned_before_the_threshold() {
        let mut m = HashMap::new();
        record_at(&mut m, "r", 1, 0);
        assert!(take_newly_stuck(&mut m, T - 1).is_empty());
        assert!(!m["r"].notified);
    }

    #[test]
    fn crossing_the_threshold_returns_once() {
        let mut m = HashMap::new();
        record_at(&mut m, "r", 1, 0);
        assert_eq!(take_newly_stuck(&mut m, T), vec![("r".to_string(), 0, 1)]);
        assert!(take_newly_stuck(&mut m, T + 1000).is_empty(), "同一段只送一次");
        assert!(m["r"].notified && m["r"].stuck);
    }

    #[test]
    fn a_new_segment_after_zero_notifies_again() {
        let mut m = HashMap::new();
        record_at(&mut m, "r", 1, 0);
        assert_eq!(take_newly_stuck(&mut m, T).len(), 1);
        record_at(&mut m, "r", 0, T + 1);
        record_at(&mut m, "r", 1, T + 2);
        assert!(take_newly_stuck(&mut m, T + 3).is_empty(), "新的一段還沒到門檻");
        assert_eq!(take_newly_stuck(&mut m, 2 * T + 2), vec![("r".to_string(), T + 2, 1)]);
    }

    #[test]
    fn a_number_change_within_the_same_segment_keeps_notified() {
        let mut m = HashMap::new();
        record_at(&mut m, "r", 1, 0);
        assert_eq!(take_newly_stuck(&mut m, T).len(), 1);
        record_at(&mut m, "r", 2, T + 5);
        assert!(m["r"].notified, "N→M 不清通知");
        assert!(take_newly_stuck(&mut m, T + 10).is_empty(), "同一段變動後不重送");
    }

    #[test]
    fn the_now_wrapper_uses_the_clock() {
        let mut m = HashMap::new();
        record_at(&mut m, "r", 1, now_ms() - T - 1000);
        assert_eq!(take_newly_stuck_now(&mut m).len(), 1);
        assert!(take_newly_stuck_now(&mut m).is_empty());
    }
}

