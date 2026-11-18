//! #1862 S1：從 `crates/am-supervisor/src/supervisor/health.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::health::tests::…` → `supervisor_tests::health::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::health::*`。

#[allow(unused_imports)]
mod tests {
    use crate::supervisor::health::*;
    use crate::lifecycle::LcError;
    use sqlx::SqlitePool;
    use serde_json::{json, Value};

    /// 真的把 `role_state` 跑起來：這四個值是 #421 決定「要不要把核准改派給巡檢」的唯一依據，
    /// 而最貴的錯誤是**在沒有證據的時候回 Unavailable**。用真的 DB 走一遍，不是只測純函式。
    #[tokio::test]
    async fn role_state_reads_the_evidence_and_says_unknown_when_there_is_none() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let role = crate::supervisor::roles::Role::Responder;

        // 還沒建立：核准本來就歸巡檢，沒有東西要改派。
        assert_eq!(role_state(&app, role).await, RoleState::NotConfigured);
        assert!(!role_state(&app, role).await.is_unavailable());

        crate::supervisor::roles::set_env(&app.db, role, "resp-bot", "proj", "/tmp").await.unwrap();
        // 登記了、但沒有 active run：它不會裁示任何東西。
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_NO_RUN));

        // #420 在送不出去時看畫面寫下的那個字，要被讀成 needs_login（字不一樣就永遠判不出來）。
        crate::supervisor::roles::set_status(&app.db, role, "needs_login", None, None).await.unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_NEEDS_LOGIN));
        // 撞限是另一個原因，但一樣是不可用。
        crate::supervisor::roles::set_status(&app.db, role, "waiting_quota", None, None).await.unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_WAITING_QUOTA));

        // 狀態清掉之後又回到「沒有 active run」——不是 Available：這一層只讀 DB，
        // 「沒有 needs_login」的意思是沒有證據說它壞了，不是證明它是好的。
        crate::supervisor::roles::set_status(&app.db, role, "", None, None).await.unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_NO_RUN));

        // 真的種一顆 bot＋一個 running 的 run，這時候才會是 Available。
        // project_id 要用 env 真的建出來的那一個：`bots.project_id` 有外鍵，寫死字串會在 INSERT 就炸。
        let bot = crate::testing::claude_bot(&app, &env.project_id, "AGM-responder").await;
        crate::testing::fake_run(&app, &bot.id).await;
        crate::supervisor::roles::set_env(&app.db, role, &bot.id, &env.project_id, "/tmp").await.unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Available);

        // bot 被軟刪、但 run 那一列還掛著 running（刪除到停機之間的窗口，#421 指出）：
        // `db::active_run` 只看 runs，不看 bot 還在不在，所以這裡如果不另外查一次，
        // 閘門會說「它可以裁示」而它根本不存在，核准就靜靜躺著——正是這支函式要擋的那種錯。
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?")
            .bind(crate::db::now())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_NO_RUN), "軟刪掉的 bot 不能被當成還能裁示");

        // 巡檢是另一列，不會被協調者的狀態汙染。
        assert_eq!(role_state(&app, crate::supervisor::roles::Role::Patrol).await, RoleState::NotConfigured);

        // #421 呼叫的是 responder_state()，它就是 role_state(Responder) 的別名，不能有第二套判斷。
        assert_eq!(responder_state(&app).await, role_state(&app, role).await);
    }

    /// #427：`role_state` 除了 DB，還要讀 health tick 每 30 秒看畫面得到的那張記憶體表。
    /// 三件事一起釘：**空表不是故障**（daemon 剛起來那一分鐘不能把等著的核准全部改派一輪）、
    /// 記憶體那份要**排在 `active_run` 之後**（停掉的角色要回 `no_run`，不是上一輪留下的
    /// `needs_login`——兩個都是不可用，但錯的字串會被原樣寫進 inbox payload 帶偏事後追查），
    /// 以及 `notify_stalled` 要真的走得通到 `is_unavailable()`（#421 靠它改派）。
    #[tokio::test]
    async fn role_state_also_reads_the_screen_conclusions_but_an_empty_table_is_not_a_fault() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let role = crate::supervisor::roles::Role::Responder;
        // `runs` 對 `bots` 有 FK，所以這裡要真的把那顆 bot 建出來（不像只讀 DB 那條測試）。
        sqlx::query("INSERT INTO projects (id,path,label,created_at) VALUES ('proj','/tmp','proj',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES ('resp-bot','proj','AGM-responder','claude','tok',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        crate::supervisor::roles::set_env(&app.db, role, "resp-bot", "proj", "/tmp").await.unwrap();
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,started_at) VALUES ('run-resp','resp-bot','running','idle',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();

        // 表還沒被填過＝還沒有結論，不是「它壞了」。
        assert_eq!(role_state(&app, role).await, RoleState::Available);

        // 記憶體說 notify 一直沒跑完（那條路不會為巡檢寫 DB，只有這張表看得到）。
        let failed = ["t1", "t2", "t3"].into_iter().map(str::to_string).collect();
        crate::supervisor::role_faults::note_notify_round(&app, "responder", failed, false).await;
        crate::supervisor::role_faults::refresh(&app).await;
        let state = role_state(&app, role).await;
        assert_eq!(state, RoleState::Unavailable(crate::supervisor::role_faults::REASON_NOTIFY_STALLED));
        assert!(state.is_unavailable(), "#421 靠這個改派");

        // bot 停掉：記憶體那份還留著上一輪的結論，但答案必須是 no_run，不是那個陳舊的原因。
        sqlx::query("UPDATE runs SET state='stopped' WHERE id='run-resp'").execute(&app.db).await.unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_NO_RUN));

        // DB 的 needs_login 仍然最權威（送不出去時寫的），排在記憶體那份前面。
        sqlx::query("UPDATE runs SET state='running' WHERE id='run-resp'").execute(&app.db).await.unwrap();
        crate::supervisor::roles::set_status(&app.db, role, "needs_login", None, None).await.unwrap();
        assert_eq!(role_state(&app, role).await, RoleState::Unavailable(REASON_NEEDS_LOGIN));
    }

    /// 這三個字串是 #421 要寫進 inbox payload（`reassigned_reason`）與 SPEC 的對外契約，改了要一起改。
    #[test]
    fn the_unavailable_reasons_are_a_stable_contract() {
        assert_eq!([REASON_NEEDS_LOGIN, REASON_WAITING_QUOTA, REASON_NO_RUN], ["needs_login", "waiting_quota", "no_run"]);
        // `needs_login` 與 `waiting_quota` 必須跟 `supervisor_roles.status` 寫進去的字一模一樣，
        // 否則 `role_state` 會永遠判不出不可用（#420 寫的是這兩個字）。
        assert_eq!(REASON_NEEDS_LOGIN, "needs_login");
        assert_eq!(REASON_WAITING_QUOTA, "waiting_quota");
    }

    /// 只有 `Unavailable` 能拿來當改派的依據。`Unknown`（讀不到）與 `NotConfigured`（沒建立）
    /// 都不行——沒有證據就把核准從一顆健康的協調者手上搬走，比晚幾分鐘更糟。
    #[test]
    fn only_unavailable_is_grounds_for_reassigning() {
        assert!(RoleState::Unavailable(REASON_NEEDS_LOGIN).is_unavailable());
        assert_eq!(RoleState::Unavailable(REASON_NEEDS_LOGIN).reason(), Some("needs_login"));
        for state in [RoleState::Unknown, RoleState::Available, RoleState::NotConfigured] {
            assert!(!state.is_unavailable(), "{state:?} 不該被當成不可用");
            assert_eq!(state.reason(), None, "{state:?} 不該有原因");
        }
    }

    #[test]
    fn counters_do_not_make_events_only_state_does() {
        let mut d = Debounce::default();
        assert!(observe_and_ack(&mut d, "healthy", "idle"), "the first reading after boot is news");
        // Ticks with different bot counts land here as the same (severity, state): nothing.
        assert!(!observe_and_ack(&mut d, "healthy", "idle"));
        assert!(!observe_and_ack(&mut d, "healthy", "busy"), "the manager's own busy/idle is not a state change");
        assert!(observe_and_ack(&mut d, "degraded", "idle"));
        assert!(observe_and_ack(&mut d, "healthy", "idle"));
        assert!(observe_and_ack(&mut d, "critical", "waiting_quota"));
    }

    #[test]
    fn a_health_transition_is_retried_until_its_inbox_write_succeeds() {
        let mut d = Debounce::default();
        assert!(d.observe("degraded", "idle"));
        assert!(d.observe("degraded", "idle"), "a failed durable write must remain due");
        let first_key = d.pending_event_key_for_test().unwrap().to_owned();
        assert_eq!(d.pending_event_key_for_test(), Some(first_key.as_str()), "retries must reuse the idempotency key");
        d.acknowledge_for_test();
        assert!(!d.observe("degraded", "idle"), "after persistence it is debounced");
    }

    fn observe_and_ack(d: &mut Debounce, severity: &str, supervisor_status: &str) -> bool {
        let due = d.observe(severity, supervisor_status);
        if due {
            d.acknowledge_for_test();
        }
        due
    }

    #[tokio::test]
    async fn a_failed_health_inbox_insert_retries_the_same_event_once() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        let snapshot = json!({"status":"degraded", "supervisor":{"status":"idle"}});
        let mut debounce = Debounce::default();
        sqlx::query("CREATE TRIGGER fail_health_changed BEFORE INSERT ON supervisor_inbox WHEN NEW.kind='health_changed' BEGIN SELECT RAISE(ABORT, 'injected health inbox failure'); END")
            .execute(&app.db)
            .await
            .unwrap();
        assert!(!queue_health_change(&app.db, &mut debounce, &snapshot, "degraded/healthy", "idle").await);
        let retry_key = debounce.pending_event_key_for_test().unwrap().to_owned();
        sqlx::query("DROP TRIGGER fail_health_changed").execute(&app.db).await.unwrap();
        assert!(queue_health_change(&app.db, &mut debounce, &snapshot, "degraded/healthy", "idle").await);
        let stored: Option<String> = sqlx::query_scalar("SELECT event_key FROM supervisor_inbox WHERE kind='health_changed'")
            .fetch_optional(&app.db)
            .await
            .unwrap();
        assert_eq!(stored.as_deref(), Some(retry_key.as_str()), "retry keeps the original idempotency key");
        assert!(!queue_health_change(&app.db, &mut debounce, &snapshot, "degraded/healthy", "idle").await);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='health_changed'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 1, "a successful retry is delivered once");
    }

    #[test]
    fn nothing_is_queued_while_the_manager_is_down_and_one_snapshot_when_it_is_back() {
        let mut d = Debounce::default();
        assert!(observe_and_ack(&mut d, "healthy", "idle"));
        // 5.5 hours of 30-second ticks against a dead manager: zero events, not 464.
        for _ in 0..660 {
            assert!(!observe_and_ack(&mut d, "degraded", "stopped"));
        }
        assert!(!observe_and_ack(&mut d, "degraded", "starting"));
        assert!(observe_and_ack(&mut d, "healthy", "idle"), "one snapshot once it is back, even to the same state");
        assert!(!observe_and_ack(&mut d, "healthy", "idle"));
    }

    /// 協調者卡在 `waiting_quota`（degraded）而巡檢好好的：以前 debounce 只看巡檢那一半，一則事件都不會有。
    #[test]
    fn a_responder_going_degraded_is_news_even_when_the_manager_is_fine() {
        let snap = |m: &str, r: Option<&str>| {
            let mut v = json!({"manager_health": {"status": m}});
            if let Some(r) = r {
                v["responder_health"] = json!({"status": r});
            }
            v
        };
        let mut d = Debounce::default();
        assert!(observe_and_ack(&mut d, &inbox_severity(&snap("healthy", Some("healthy"))), "idle"));
        assert!(observe_and_ack(&mut d, &inbox_severity(&snap("healthy", Some("degraded"))), "idle"), "responder waiting_quota must queue an event");
        assert!(!observe_and_ack(&mut d, &inbox_severity(&snap("healthy", Some("degraded"))), "busy"));
        assert!(observe_and_ack(&mut d, &inbox_severity(&snap("healthy", Some("healthy"))), "idle"), "and its recovery too");
        // 舊 snapshot 沒有 responder_health：當 healthy，不會憑空多一則。
        assert_eq!(inbox_severity(&snap("healthy", None)), inbox_severity(&snap("healthy", Some("healthy"))));
    }

    /// 協調者建立過但沒在跑（setup 完沒 start、start 失敗、手動 stop、bot 被刪）：佇列裡有會叫醒它的事件
    /// 就不是 healthy——沒有人會被叫醒（review 2026-09-16 c3 M1）。沒有待辦的停著仍是 healthy。
    #[test]
    fn a_responder_that_is_not_running_while_requests_wait_is_degraded() {
        let r = |status: &str, desired: bool, waiting: i64| json!({"status": status, "desired_running": desired, "wake_pending": waiting});
        assert_eq!(responder_severity(&r("stopped", false, 0)), "healthy", "停著、沒人在等：不是故障");
        assert_eq!(responder_severity(&r("stopped", false, 2)), "degraded");
        assert_eq!(responder_severity(&r("missing", false, 1)), "degraded");
        assert_eq!(responder_severity(&r("stopped", true, 0)), "degraded", "想要它跑卻停著，照舊是 degraded");
        assert_eq!(responder_severity(&r("idle", false, 5)), "healthy", "在跑就會被叫醒");
        assert_eq!(responder_severity(&r("not_configured", false, 3)), "healthy", "沒建立：事件歸巡檢");
        assert_eq!(responder_severity(&json!({"status": "stopped"})), "healthy", "舊形狀沒有 wake_pending");
    }

    /// #454：#427 判出來的不可用只在記憶體，`status` 不會變（`notify_stalled` 從來不寫 DB），
    /// 所以這一格要讀 `unavailable_reason`——不讀的話「核准正在被改派、incident 已經開了，
    /// 面板說 healthy」，正是 #420 要消滅的症狀換一條路又出現一次。
    #[test]
    fn a_responder_that_is_being_bypassed_is_never_healthy() {
        let with = |status: &str, reason: Value| json!({"status": status, "unavailable_reason": reason, "wake_pending": 0});
        // notify 連續沒完成：status 照樣是 idle，只有原因看得出來。
        assert_eq!(responder_severity(&with("idle", json!("notify_stalled"))), "degraded");
        assert_eq!(responder_severity(&with("busy", json!("notify_stalled"))), "degraded");
        // `no_run` 交還給既有那兩條規則：停著、沒有人在等，本來就不是故障（別把剛 setup 完的當壞的）。
        assert_eq!(responder_severity(&with("stopped", json!("no_run"))), "healthy");
        assert_eq!(
            responder_severity(&json!({"status": "stopped", "unavailable_reason": "no_run", "wake_pending": 2})),
            "degraded",
            "有人在等就還是 degraded"
        );
        // 停在登入要人動手、不會自己好：跟 incident 那邊一樣算 critical（兩個面板同一件事同一個嚴重度）。
        assert_eq!(responder_severity(&with("needs_login", json!("needs_login"))), "critical");
        assert_eq!(responder_severity(&with("idle", json!("needs_login"))), "critical", "原因先於 status");
        // 撞限是等得到的，維持 degraded。
        assert_eq!(responder_severity(&with("waiting_quota", json!("waiting_quota"))), "degraded");
        // 沒有原因就照舊。
        assert_eq!(responder_severity(&with("idle", Value::Null)), "healthy");
    }

    /// 端到端：setup 過、從沒 start，bot 送來一筆申請 → 健康頂層不再是 healthy。
    #[tokio::test]
    async fn a_request_queued_for_a_responder_that_never_started_shows_up_in_health() {
        use crate::supervisor::bot_requests::{self, flow_tests, ReplyMark};
        let app = flow_tests::app().await;
        flow_tests::configure_responder(&app).await;
        let snap = snapshot(&app).await.unwrap();
        assert_eq!(snap.pointer("/responder_health/status").and_then(Value::as_str), Some("healthy"));
        bot_requests::intercept(&app, "patrol", "w1", "請核准重建 abc", Some("r-1"), &[], true, "api", ReplyMark::default()).await.unwrap().unwrap();
        let snap = snapshot(&app).await.unwrap();
        assert_eq!(snap.pointer("/responder_health/status").and_then(Value::as_str), Some("degraded"), "{snap}");
        assert_eq!(snap.pointer("/responder_health/wake_pending").and_then(Value::as_i64), Some(1));
        assert_ne!(snap.get("status").and_then(Value::as_str), Some("healthy"));
    }

    #[test]
    fn a_manager_that_was_never_up_gets_its_first_snapshot_when_it_is() {
        let mut d = Debounce::default();
        assert!(!d.observe("degraded", "not_configured"));
        assert!(!d.observe("degraded", "stopped"));
        assert!(d.observe("healthy", "busy"));
    }
}
