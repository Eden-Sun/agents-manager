//! #1862 S3b：從 `crates/am-supervisor/src/supervisor/incidents.rs` 搬來的 daemon 整合測試；``supervisor::incidents::tests::…` → `supervisor_tests::incidents::tests::…``。
//! 測試本文與斷言不變；私有項由 am-supervisor 的 `*_for_test`（test-hooks）轉呼叫，下面用別名維持原名。

#![allow(unused_imports)]

use crate::supervisor::incidents::*;
use serde_json::{json, Value};
use std::collections::HashMap;
use crate::supervisor::store;
use crate::supervisor::incidents::notifiable_for_test as notifiable;

mod tests {
    use super::*;

    // 測試本地輔助（#1862 S3b）：與 am-supervisor crate-only 版同一份本體（`obs` 這 9 條沒用到，不複製）。
    fn thresholds() -> Thresholds {
        Thresholds {
            host_disconnected_secs: 120,
            bot_stopped_secs: 300,
            assignment_stalled_secs: 7200,
            notify_max_attempts: 5,
            approval_stalled_secs: 1800,
            classify_failures: CLASSIFY_FAILURE_LIMIT,
            spool_fold_stuck_rounds: SPOOL_FOLD_STUCK_LIMIT,
        }
    }

    /// #472：`observe` 讀的是 `App` 上那個計數器，所以「連續幾拍」這件事要真的走一遍。
    /// 門檻以下不開票（一次抖動不驚動人），到門檻才開，成功歸零之後下一拍就該消失。
        #[tokio::test]
    async fn the_classifier_incident_follows_the_consecutive_counter() {
        use std::sync::atomic::Ordering;
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let t = thresholds();

        let has = |p: &Probed| p.seen.iter().any(|o| o.kind == CLASSIFY_FAILING_KIND);

        // 沒失敗過：不該有。
        assert!(!has(&observe(&app, &t).await));

        // 差一次：還不到門檻。
        app.classify_failures.store(CLASSIFY_FAILURE_LIMIT - 1, Ordering::Relaxed);
        assert!(!has(&observe(&app, &t).await), "門檻以下不開票");

        // 到門檻：開，而且 detail 要帶得出次數（查事故的人要看得到連續幾拍）。
        app.classify_failures.store(CLASSIFY_FAILURE_LIMIT, Ordering::Relaxed);
        let probed = observe(&app, &t).await;
        let o = probed.seen.iter().find(|o| o.kind == CLASSIFY_FAILING_KIND).expect("到門檻要開");
        assert_eq!(o.severity, "critical", "期間所有新事件都送不出去");
        assert!(o.detail.contains("consecutive_failures"), "{}", o.detail);

        // 成功一次歸零：下一拍就不該再看到。
        app.classify_failures.store(0, Ordering::Relaxed);
        assert!(!has(&observe(&app, &t).await), "恢復之後不該還在");
    }

    /// #500 複看：遠端 spool 停收。門檻以下不開票（drain 是事件驅動的，一次磁碟忙不該驚動人），
    /// 到門檻才開；`resource` 要分得出是哪一台的哪一顆；併回去之後（計數被移除）就該消失。
        #[tokio::test]
    async fn a_stuck_remote_spool_opens_an_incident_per_bot() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let t = thresholds();
        let of = |p: &Probed| -> Vec<String> {
            p.seen.iter().filter(|o| o.kind == SPOOL_FOLD_STUCK_KIND).map(|o| o.resource.clone()).collect()
        };

        assert!(of(&observe(&app, &t).await).is_empty(), "沒卡住就不該有");

        app.spool_fold_stuck.lock().await.insert("mac2/b1".into(), (SPOOL_FOLD_STUCK_LIMIT - 1, 42));
        assert!(of(&observe(&app, &t).await).is_empty(), "門檻以下不開票");

        app.spool_fold_stuck.lock().await.insert("mac2/b1".into(), (SPOOL_FOLD_STUCK_LIMIT, 4096));
        let probed = observe(&app, &t).await;
        let o = probed.seen.iter().find(|o| o.kind == SPOOL_FOLD_STUCK_KIND).expect("到門檻要開");
        assert_eq!(o.resource, "mac2/b1", "一台機器上可以只有某顆 bot 卡住");
        assert_eq!(o.severity, "critical");
        assert!(o.detail.contains("claim_bytes"), "查事故的人要知道卡著多少：{}", o.detail);

        // 同一台的另一顆也卡住：各開各的。
        app.spool_fold_stuck.lock().await.insert("mac2/b2".into(), (SPOOL_FOLD_STUCK_LIMIT, 7));
        let mut both = of(&observe(&app, &t).await);
        both.sort();
        assert_eq!(both, vec!["mac2/b1".to_string(), "mac2/b2".to_string()]);

        // 併回去了：計數被移除，下一拍就不該再看到。
        app.spool_fold_stuck.lock().await.clear();
        assert!(of(&observe(&app, &t).await).is_empty(), "恢復之後不該還在");

        // 這一類要推進 inbox：壞的是那台機器的磁碟，不是通知那條路自己（對照 #472）。
        assert!(notifiable(SPOOL_FOLD_STUCK_KIND, "mac2/b1", false));
    }

    /// #534：遠端補版放棄之後要開票，補成之後要消失。`resource` 是 host 名。
        #[tokio::test]
    async fn a_host_whose_shims_could_not_be_refreshed_opens_an_incident() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let t = thresholds();
        let of = |p: &Probed| -> Vec<String> {
            p.seen.iter().filter(|o| o.kind == REMOTE_SHIM_STALE_KIND).map(|o| o.resource.clone()).collect()
        };

        assert!(of(&observe(&app, &t).await).is_empty(), "沒放棄過就不該有");

        app.remote_shim_stale.lock().await.insert("mac2".into(), "重試 4 次都失敗，最後一個錯誤：connection refused".into());
        let probed = observe(&app, &t).await;
        let o = probed.seen.iter().find(|o| o.kind == REMOTE_SHIM_STALE_KIND).expect("放棄了就要開票");
        assert_eq!(o.resource, "mac2");
        assert_eq!(o.severity, "degraded");
        assert!(o.detail.contains("connection refused"), "查的人要看得到最後那個錯誤：{}", o.detail);

        // 下一次補成了：計數被移除，這一拍就不該再看到。
        app.remote_shim_stale.lock().await.clear();
        assert!(of(&observe(&app, &t).await).is_empty(), "補成之後不該還在");

        // 壞的是那台機器，不是通知那條路：這一類要推進 inbox（對照 #472）。
        assert!(notifiable(REMOTE_SHIM_STALE_KIND, "mac2", false));
    }

    /// incident 開起來與叫醒 AGM 的 `incident_opened` 同一個交易：通知寫不進去，incident 就不開（下一輪再來）。
    /// 以前先 `open_incident`、再 `let _` 推事件；寫不進去時 incident 已是 open，之後每輪 `opened=false` 不再通知，
    /// 一個真的系統故障就永遠沒人被告知。
        #[tokio::test]
    async fn an_incident_and_its_notification_land_together() {
        use crate::supervisor_tests::flow_fixture as flow_tests;
        use crate::supervisor::roles;
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let max = app.cfg.get().await.supervisor.notify_max_attempts.max(1);
        let stuck = store::push_inbox(&app.db, "health:y", "health_changed", None, None, None, &json!({})).await.unwrap().unwrap();
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=? WHERE id=?").bind(max).bind(&stuck).execute(&app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER no_incident_event BEFORE INSERT ON supervisor_inbox WHEN NEW.kind='incident_opened' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let mut d = Detector::default();
        sweep(&app, &mut d).await;
        assert!(store::open_incidents(&app.db).await.unwrap().is_empty(), "通知寫不進去：incident 不開，下一輪再來");

        // Fault disappears before storage recovers. The threshold was already met; losing the
        // in-memory observation here must not erase the owed notification.
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=0 WHERE id=?")
            .bind(&stuck)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("DROP TRIGGER no_incident_event").execute(&app.db).await.unwrap();
        sweep(&app, &mut d).await;
        assert!(!store::open_incidents(&app.db).await.unwrap().is_empty(), "DB 好了：已達門檻的故障即使恢復也要補開 incident");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='incident_opened'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1, "而且通知一則");
        sweep(&app, &mut d).await;
        assert!(store::open_incidents(&app.db).await.unwrap().is_empty(), "下一拍確認恢復後就關閉 incident");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='incident_resolved'").fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 1, "補開後只推一則恢復通知");
    }

    /// issue #421：核准開著超過 30 分鐘沒人裁示 → incident，而且是走 inbox 喊人那條路（不是只留 log）。
    /// 裁示之後自己關掉。
        #[tokio::test]
    async fn a_stalled_approval_opens_an_incident_and_closes_when_someone_decides() {
        use crate::supervisor::failover;
        // `crate::testing::env()` 的 Env 一 drop 就把暫存目錄收掉，只留 app 會讓 DB 檔消失。
        let app = crate::supervisor_tests::flow_fixture::app().await;
        let a = store::create_approval(&app.db, "agm-kick", "rebuild", "release rebuild", Some("c1"), None, None).await.unwrap().approval.id;
        let t = thresholds();

        // 剛申請：沒有觀測。
        assert!(observe(&app, &t).await.seen.iter().all(|o| o.kind != failover::STALLED_KIND), "剛申請的不算卡住");

        sqlx::query("UPDATE supervisor_approvals SET created_at=? WHERE id=?")
            .bind(crate::db::iso_in(-t.approval_stalled_secs - 60))
            .bind(&a)
            .execute(&app.db)
            .await
            .unwrap();
        let seen = observe(&app, &t).await.seen;
        let obs = seen.iter().find(|o| o.kind == failover::STALLED_KIND).expect("30 分鐘沒裁示要被看到");
        assert_eq!(obs.resource, a, "resource 是核准 id，所以一筆核准只開一個 incident");
        assert_eq!(obs.severity, "critical", "卡住的是別人的部署");
        assert!(obs.detail.contains("waiting_secs"), "{}", obs.detail);

        // 真的寫進去，而且會叫醒人（`incident_opened` → 巡檢）。
        let mut detector = Detector::default();
        sweep(&app, &mut detector).await;
        // 寫入時不分類（`role`／`wake` 是 NULL），controller 每一拍先補上——跟其他 inbox 事件一樣。
        crate::supervisor::roles::classify(&app.db).await.unwrap();
        let woken: Vec<(String, Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT kind, role, wake FROM supervisor_inbox WHERE kind='incident_opened'").fetch_all(&app.db).await.unwrap();
        assert_eq!(woken.len(), 1, "{woken:?}");
        assert_eq!((woken[0].1.as_deref(), woken[0].2), (Some("patrol"), Some(1)), "使用者入口要被叫醒：{woken:?}");

        // 有人裁示了 → 觀測消失，sweep 把 incident 關掉。
        store::decide_approval(&app.db, &a, "approved", "AGM:patrol", None, None).await.unwrap();
        assert!(observe(&app, &t).await.seen.iter().all(|o| o.kind != failover::STALLED_KIND));
        sweep(&app, &mut detector).await;
        let open: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_incidents WHERE kind=? AND status='open'")
            .bind(failover::STALLED_KIND)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(open, 0, "裁示之後 incident 要自己關");
    }

    /// 巡檢自己的通知一直送不出去（它倒了）時，`notify_exhausted` 跟巡檢的 `watchdog_gave_up` 都要進
    /// **協調者**的佇列並叫醒它——送回巡檢等於送進已知壞掉的那條路（review 2026-09-16 c1 M2、L4）。
        #[tokio::test]
    async fn when_patrol_cannot_be_reached_the_responder_is_told() {
        use crate::supervisor_tests::flow_fixture as flow_tests;
        use crate::supervisor::roles::{self, Role};
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let max = app.cfg.get().await.supervisor.notify_max_attempts.max(1);
        let stuck = store::push_inbox(&app.db, "health:x", "health_changed", None, None, None, &json!({})).await.unwrap().unwrap();
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=? WHERE id=?").bind(max).bind(&stuck).execute(&app.db).await.unwrap();
        store::push_inbox(&app.db, "watchdog:gave_up:t", "watchdog_gave_up", None, None, None, &json!({"why": "CLI 起來就死"})).await.unwrap();

        let mut d = Detector::default();
        sweep(&app, &mut d).await;
        roles::classify(&app.db).await.unwrap();

        let exhausted = |e: &store::InboxEvent| e.kind == "incident_opened" && e.payload_json.contains("notify_exhausted");
        let responder = roles::due_for(&app.db, Role::Responder, true, "2999-01-01T00:00:00Z", 0).await.unwrap();
        assert!(responder.iter().any(|e| exhausted(e) && e.wake == Some(1)), "notify_exhausted 要推給協調者並叫醒它");
        assert!(responder.iter().any(|e| e.kind == "watchdog_gave_up" && e.wake == Some(1)));
        let patrol = roles::due_for(&app.db, Role::Patrol, true, "2999-01-01T00:00:00Z", 1_000).await.unwrap();
        assert!(patrol.iter().all(|e| !exhausted(e) && e.kind != "watchdog_gave_up"), "不送回倒下的巡檢");
    }

    /// issue #504 審核：整個佇列一起用盡額度時是**一張** critical incident 帶筆數，不是一則一張。
    ///
    /// 一則一張的話，巡檢停在登入失效那一晚（現場上百則）會開出上百張 critical，而每一張又各推一則
    /// `incident_opened` 給協調者——「把一則送不出去的通知變成 N 則」正是 `notifiable` 那段註解在防的事。
    /// 隔壁 `responder_undeliverable` 早就是聚合的，兩邊形狀現在一致。
        #[tokio::test]
    async fn a_whole_queue_that_ran_out_of_retries_is_one_incident_carrying_the_count() {
        use crate::supervisor_tests::flow_fixture as flow_tests;
        use crate::supervisor::roles;
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let max = app.cfg.get().await.supervisor.notify_max_attempts.max(1);
        for i in 0..5 {
            store::push_inbox(&app.db, &format!("health:{i}"), "health_changed", None, None, None, &json!({})).await.unwrap();
        }
        roles::classify(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisor_inbox SET notify_attempts=?").bind(max).execute(&app.db).await.unwrap();

        let mut d = Detector::default();
        sweep(&app, &mut d).await;

        let open = store::open_incidents(&app.db).await.unwrap();
        let mine: Vec<_> = open.iter().filter(|i| i.kind == "notify_exhausted").collect();
        assert_eq!(mine.len(), 1, "五則事件一張 incident，不是五張");
        assert_eq!(mine[0].resource, "patrol", "resource 是角色，不是事件 id（換一則事件不會留下關不掉的孤兒）");
        let detail: Value = serde_json::from_str(&mine[0].detail_json).unwrap();
        assert_eq!(detail["events"], 5, "筆數要寫在 detail 上：{detail}");
        assert_eq!(detail["max_attempts"], max);
        // 叫醒協調者的也只有一則。
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='incident_opened'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "一張 incident 一則通知");
    }

    /// issue #100（同 fence.rs／a4605b2 的根因）：一顆 bot 快速重啟時兩個 run 可能擠進同一毫秒。
    /// 「最後一個 run 是不是使用者自己停的」要看**寫入順序**（`rowid`），不是 ULID 字典序——故意讓
    /// 真正較舊的那個 run 用字典序比較大的假 ULID，證明不會把使用者主動停的（`stopped`）誤判成 outage。
        #[tokio::test]
    async fn a_bots_last_run_is_found_by_insertion_order_not_ulid_when_deciding_bot_stopped() {
        let app = crate::supervisor_tests::flow_fixture::app().await;
        let bot_id = "watched";
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,'p','watched','claude','[]',1,1,'tok-watched',?)",
        )
        .bind(bot_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let at = crate::db::now();
        // 真正較舊（先寫入）的用字典序比較大的假 ULID；真正最新（後寫入、使用者主動停的）用比較小的。
        for (id, state) in [("01ZZZZZZZZZZZZZZZZZZZZZZZZ", "exited"), ("01AAAAAAAAAAAAAAAAAAAAAAAA", "stopped")] {
            sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at) VALUES (?,?,?,'unknown',?)")
                .bind(id)
                .bind(bot_id)
                .bind(state)
                .bind(&at)
                .execute(&app.db)
                .await
                .unwrap();
        }

        let probed = observe(&app, &thresholds()).await;
        assert!(
            probed.seen.iter().all(|o| !(o.kind == "bot_stopped" && o.resource == bot_id)),
            "最後一個 run（寫入順序）其實是使用者主動停的，不該被誤判成 outage：{:?}",
            probed.seen
        );
    }

    /// #1477：Remote Control 的探針拿掉之後，舊版留下的 open `remote_entry` incident 不會再有人看見它，
    /// 下一拍 sweep 要把它當成已恢復收掉——不然側欄永遠掛著一張沒人清得掉的「遠端入口異常」。
        #[tokio::test]
    async fn a_legacy_remote_entry_incident_is_closed_by_the_next_sweep() {
        let app = crate::supervisor_tests::flow_fixture::app().await;
        store::open_incident_notifying(&app.db, "remote_entry", "AGM", "degraded", &json!({"status": "unavailable"}), false)
            .await
            .unwrap();
        let open_sql = "SELECT COUNT(*) FROM supervisor_incidents WHERE kind='remote_entry' AND status='open'";
        let open: i64 = sqlx::query_scalar(open_sql).fetch_one(&app.db).await.unwrap();
        assert_eq!(open, 1, "前提：舊版留下一張開著的");

        let mut detector = Detector::default();
        sweep(&app, &mut detector).await;

        let open: i64 = sqlx::query_scalar(open_sql).fetch_one(&app.db).await.unwrap();
        assert_eq!(open, 0, "沒有任何探針會再回報它：當成恢復收掉");
    }
}
