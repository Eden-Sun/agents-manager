//! #1862 S1：從 `crates/am-supervisor/src/supervisor/watchdog.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::watchdog::tests::…` → `supervisor_tests::watchdog::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::watchdog::*`。

#[allow(unused_imports)]
mod tests {
    use crate::supervisor::watchdog::*;
    use crate::supervisor::watchdog::report_gave_up_for_test as report_gave_up;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    use crate::supervisor::store::{self, Supervisor};

    #[test]
    fn a_watchdog_deadline_remains_due_after_the_wall_clock_moves_back() {
        let mut cache = DeadlineCache::default();
        let wall = chrono::DateTime::parse_from_rfc3339("2026-10-03T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let mono = Instant::now();
        let at = "2026-10-03T12:00:30Z";
        cache.schedule_for_test("patrol", at, mono + Duration::from_secs(30));

        assert!(!cache.past_at_for_test("patrol", at, wall + chrono::Duration::seconds(29), mono + Duration::from_secs(29)));
        assert!(cache.past_at_for_test("patrol", at, wall - chrono::Duration::minutes(5), mono + Duration::from_secs(30)));
    }

    fn sup(wanted: bool, status: &str, attempts: i64, next: Option<&str>) -> Supervisor {
        Supervisor {
            id: "AGM".into(),
            bot_id: Some("b1".into()),
            project_id: None,
            cwd: None,
            identity: "cc0".into(),
            effort: "low".into(),
            active_model: "fable".into(),
            generation: 1,
            status: status.into(),
            status_detail: None,
            fallback_tries: 0,
            cooldown_until: None,
            quota_reset_at: None,
            summary: None,
            summary_version: 0,
            desired_running: i64::from(wanted),
            watchdog_attempts: attempts,
            watchdog_next_at: next.map(String::from),
            last_notify_at: None,
            persona_text: None,
            persona_version: 0,
            persona_hash: None,
            persona_source: None,
            persona_updated_at: None,
            persona_seed_hash: None,
            watchdog_gave_up_at: None,
            watchdog_last_error: None,
            created_at: "now".into(),
            updated_at: "now".into(),
        }
    }

    #[test]
    fn the_backoff_is_30_60_120_300_and_then_flat() {
        assert_eq!([0, 1, 2, 3, 4, 9].map(backoff_secs), [30, 60, 120, 300, 300, 300]);
    }

    #[test]
    fn a_manager_the_user_stopped_stays_stopped() {
        assert_eq!(plan(&sup(false, "", 0, None), "stopped", |_| true), Plan::Idle);
        // Never started at all (fresh setup): not the watchdog's to bring up.
        let mut s = sup(false, "", 0, None);
        s.generation = 0;
        assert_eq!(plan(&s, "stopped", |_| true), Plan::Idle);
    }

    #[test]
    fn waiting_for_quota_is_not_an_outage() {
        assert_eq!(plan(&sup(true, "waiting_quota", 0, None), "stopped", |_| true), Plan::Idle);
    }

    #[test]
    fn a_running_manager_needs_nothing() {
        for l in ["idle", "busy", "starting"] {
            assert_eq!(plan(&sup(true, "", 2, Some("x")), l, |_| true), Plan::Idle, "{l}");
        }
    }

    #[test]
    fn the_first_observation_schedules_and_the_deadline_starts() {
        assert_eq!(plan(&sup(true, "", 0, None), "stopped", |_| true), Plan::Wait { schedule: true });
        assert_eq!(plan(&sup(true, "", 0, Some("later")), "stopped", |_| false), Plan::Wait { schedule: false });
        assert_eq!(plan(&sup(true, "", 0, Some("earlier")), "stopped", |_| true), Plan::Start);
    }

    #[test]
    fn five_failures_and_it_stops_trying() {
        assert_eq!(plan(&sup(true, "", 4, Some("earlier")), "stopped", |_| true), Plan::Start);
        assert_eq!(plan(&sup(true, "", 5, None), "stopped", |_| true), Plan::GaveUp);
        assert_eq!(plan(&sup(true, "", 5, Some("earlier")), "stopped", |_| true), Plan::GaveUp);
    }

    /// The nastiest shape of giving up, and the one that used to be completely silent: the
    /// fifth start *succeeds*, so no error path runs, and then the CLI dies a second later.
    /// Every tick after that lands on `GaveUp` — which has to keep meaning "stop retrying"
    /// while the reporting happens exactly once (that half is `report_gave_up`, guarded by
    /// `store::mark_watchdog_gave_up`).
    #[test]
    fn a_start_that_succeeded_and_then_died_still_ends_in_give_up_on_every_later_tick() {
        // Attempt 5 returned Ok, so the row holds attempts=5 with a backoff deadline.
        let after_ok = sup(true, "", 5, Some("2026-09-13T00:00:00Z"));
        // The CLI died: the manager is stopped again, and the deadline has since passed.
        for _ in 0..10 {
            assert_eq!(plan(&after_ok, "stopped", |_| true), Plan::GaveUp, "no more automatic starts");
        }
        // Seen alive again → `tick` resets the streak (Plan::Idle), which is what clears the
        // give-up marker, so a later outage is reported as a new one.
        assert_eq!(plan(&after_ok, "idle", |_| true), Plan::Idle);
        assert_eq!(plan(&after_ok, "busy", |_| true), Plan::Idle);
        // And a human starting it by hand resets `watchdog_attempts` to 0 (see
        // `store::set_desired_running`), which puts it back in the ordinary flow.
        assert_eq!(plan(&sup(true, "", 0, None), "stopped", |_| true), Plan::Wait { schedule: true });
    }
    /// #249：讀不到 manager liveness（`runs`／`turns` 的 SELECT 壞掉）不能當成「已停止」。
    /// 修前：活著的 manager 遇到一次 DB 讀故障，看門狗就當它掛了、排定自動重啟（`watchdog_next_at`
    /// 被寫上）。修後這個 tick 什麼都不動，DB 恢復後下一個 tick 看到真的狀態。
    #[tokio::test]
    async fn an_unreadable_liveness_is_not_an_outage() {
        for table in ["runs", "turns"] {
            let env = crate::testing::env().await;
            let app = env.app.clone();
            let bot = crate::testing::claude_bot(&app, &env.project_id, "agm-mgr").await.id;
            crate::testing::fake_run(&app, &bot).await;
            store::get_or_init(&app.db).await.unwrap();
            sqlx::query("UPDATE supervisors SET bot_id=?, desired_running=1, generation=1, status='' WHERE id=?")
                .bind(&bot)
                .bind(store::SUPERVISOR_ID)
                .execute(&app.db)
                .await
                .unwrap();

            crate::testing::make_table_unreadable(&app, table).await;
            tick(&app).await;
            crate::testing::make_table_readable(&app, table).await;
            let s = store::get_or_init(&app.db).await.unwrap();
            assert_eq!(s.watchdog_next_at, None, "{table} 讀不到：不可排定重啟");
            assert_eq!(s.watchdog_attempts, 0, "{table} 讀不到：不可計次");
            assert!(s.watchdog_gave_up_at.is_none());

            // 恢復後下一個 tick 看到活著的 manager，照常沒事。
            tick(&app).await;
            let s = store::get_or_init(&app.db).await.unwrap();
            assert_eq!(s.watchdog_next_at, None, "{table} 恢復後 manager 是活的");
        }
    }
    /// #251：標記寫進去了、inbox 事件卻寫不進去（一次暫時性故障）——之後的 tick 要補上，而且只有一則、
    /// key 帶真正的標記時間。修前第二次呼叫看到標記已存在就直接 return，通知永遠少一則。
    #[tokio::test]
    async fn a_failed_inbox_write_is_retried_until_exactly_one_event_exists() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        store::get_or_init(&app.db).await.unwrap();

        crate::testing::make_table_unreadable(&app, "supervisor_inbox").await;
        report_gave_up(&app, "boom").await;
        crate::testing::make_table_readable(&app, "supervisor_inbox").await;
        let s = store::get_or_init(&app.db).await.unwrap();
        assert!(s.watchdog_gave_up_at.is_some(), "標記已經寫下");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='watchdog_gave_up'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0, "故障那一輪沒有事件");

        // 下一個 tick、沒有人介入：補上。再多幾輪也只有一則。
        for _ in 0..3 {
            report_gave_up(&app, "boom").await;
        }
        let keys: Vec<String> =
            sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE kind='watchdog_gave_up'").fetch_all(&app.db).await.unwrap();
        assert_eq!(keys, vec![format!("watchdog:gave_up:{}", s.watchdog_gave_up_at.unwrap())]);
    }

    /// 去重 key／kind／payload 是合約（改了去重就失效、巡檢讀的欄位也變）：快照。
    #[tokio::test]
    async fn gave_up_event_key_kind_and_payload_snapshot() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        store::get_or_init(&app.db).await.unwrap();
        report_gave_up(&app, "boom").await;
        let at = store::get_or_init(&app.db).await.unwrap().watchdog_gave_up_at.unwrap();
        let (key, kind, payload): (String, String, String) =
            sqlx::query_as("SELECT event_key, kind, payload_json FROM supervisor_inbox WHERE kind='watchdog_gave_up'").fetch_one(&app.db).await.unwrap();
        assert_eq!(key, format!("watchdog:gave_up:{at}"));
        assert_eq!(kind, "watchdog_gave_up");
        let want = serde_json::json!({
            "attempts": MAX_ATTEMPTS,
            "why": "boom",
            "gave_up_at": at,
            "action": "手動 `bin/agm supervisor-start`；自動重試不會再發生，直到有人重新啟動它",
        });
        assert_eq!(serde_json::from_str::<serde_json::Value>(&payload).unwrap(), want);
    }
}
