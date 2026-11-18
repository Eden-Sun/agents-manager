//! #1862 S1：從 `crates/am-supervisor/src/supervisor/role_faults.rs` 搬來的 daemon 整合測試（要 `state::App`／`testing::env`／runner）；
//! 測試名 `supervisor::role_faults::tests::…` → `supervisor_tests::role_faults::tests::…`。測試本文與斷言不變，只有 `use super::*` 改成 `use crate::supervisor::role_faults::*`。

#[allow(unused_imports)]
mod tests {
    use crate::supervisor::role_faults::*;
    use crate::supervisor::role_faults::quota_is_blank_for_test as quota_is_blank;
    use sqlx::SqlitePool;
    use crate::supervisor::roles::Role;
    use crate::supervisor::health::REASON_NEEDS_LOGIN;
    use crate::state::App;
    use std::sync::Arc;

    const RESPONDER: Role = Role::Responder;

    async fn configure(app: &Arc<App>, role: Role) {
        sqlx::query("INSERT OR IGNORE INTO projects (id,path,label,created_at) VALUES ('p','/tmp','p',?)")
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = format!("bot-{}", role.as_str());
        sqlx::query("INSERT OR IGNORE INTO bots (id,project_id,name,kind,hook_token,created_at) VALUES (?,'p',?,'claude',?,?)")
            .bind(&bot)
            .bind(&bot)
            .bind(format!("tok-{bot}"))
            .bind(crate::db::now())
            .execute(&app.db)
            .await
            .unwrap();
        crate::supervisor::roles::set_env(&app.db, role, &bot, "p", "/tmp").await.unwrap();
    }

    fn turns(ids: &[&str]) -> std::collections::BTreeSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    /// #427 第 3 項：門檻是「連續 [`NOTIFY_STALL_LIMIT`] 個**不同的**回合」。
    ///
    /// 這一條釘的是 i407 抓到的那個算錯（2026-09-24）：`recover_unacked` 對 inbox 事件**逐筆**呼叫，
    /// 同一個角色一輪裡可以有好幾筆事件、各自不同的 `notify_turn_id`。用「上一個算過的 id」去重時，
    /// 兩筆交錯的壞回合 A／B 兩輪就會湊到 3（A→1、B→2，第二輪 A 又跟上一個不同→3），
    /// 而這個數字直接餵 `role_state` → `is_unavailable()` → #421 的改派閘門。
    #[tokio::test]
    async fn two_interleaved_broken_turns_never_reach_the_threshold() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        for _ in 0..5 {
            note_notify_round(&app, "responder", turns(&["A", "B"]), false).await;
        }
        assert_eq!(
            snapshot(&app, RESPONDER).await.unwrap().failed_turns.len(),
            2,
            "只有兩個壞回合，掃幾輪都還是兩個"
        );

        // 第三個**不同的**回合才到門檻。
        note_notify_round(&app, "responder", turns(&["C"]), false).await;
        assert_eq!(snapshot(&app, RESPONDER).await.unwrap().failed_turns.len(), NOTIFY_STALL_LIMIT);
    }

    /// 同一輪裡一筆壞、一筆跑完時答案只有一個，不跟著迭代順序跑：有回合跑完就是這條路通了，整個歸零。
    #[tokio::test]
    async fn a_round_with_any_completed_turn_resets_regardless_of_order() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        note_notify_round(&app, "responder", turns(&["A", "B"]), false).await;
        note_notify_round(&app, "responder", turns(&["C"]), true).await;
        assert!(snapshot(&app, RESPONDER).await.unwrap().failed_turns.is_empty(), "這一輪有回合跑完＝通的");

        // 不是角色的事件（一般 bot）不要在表裡長出永遠不會被 refresh 清掉的項目。
        note_notify_round(&app, "some-bot", turns(&["t9"]), false).await;
        assert!(app.role_faults.lock().await.get("some-bot").is_none());
        // 什麼都沒看到的一輪不要無中生有一個項目。
        note_notify_round(&app, "patrol", turns(&[]), false).await;
        assert!(snapshot(&app, Role::Patrol).await.is_none());
    }

    /// 已知沒有 active run 時是 `no_run`，不是探針失敗；因此不會讓同一 kind 的其他角色 incident 卡在 blind。
    #[tokio::test]
    async fn a_configured_role_without_an_active_run_is_not_blind_and_resolves_incidents() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;

        for resource in ["responder", "patrol"] {
            crate::supervisor::store::open_incident(
                &app.db,
                crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND,
                resource,
                "degraded",
                &serde_json::json!({}),
            )
            .await
            .unwrap();
        }

        refresh(&app).await;
        let f = snapshot(&app, RESPONDER).await.expect("建立過的角色要有項目");
        assert!(!f.probe_failed, "沒有 active run 是已知的 no_run 狀態");
        assert_eq!(f.reason, None, "no_run 不會變成畫面探針故障原因");
        assert_eq!(reason(&app, RESPONDER).await, None);
        assert_eq!(
            crate::supervisor::health::role_state(&app, RESPONDER).await,
            crate::supervisor::health::RoleState::Unavailable(crate::supervisor::health::REASON_NO_RUN),
        );

        let mut detector = crate::supervisor::incidents::Detector::default();
        crate::supervisor::incidents::sweep(&app, &mut detector).await;
        let open = crate::supervisor::store::open_incidents(&app.db).await.unwrap();
        assert!(
            open.iter().all(|i| i.kind != crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND),
            "no_run must allow both role incidents to resolve"
        );
        let health = crate::supervisor::incidents::system_health(&app).await;
        assert!(
            !health["blind_probes"].as_array().unwrap().iter().any(|v| v == crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND),
            "a known no_run state must not be reported as blind"
        );
    }

    /// 畫面讀不到、但 notify 已經連續 [`NOTIFY_STALL_LIMIT`] 次沒完成：這是**另一個**訊號，
    /// 不依賴畫面，所以照樣要判不可用（#420 現場就是「看起來 idle、但每個回合都失敗」）。
    ///
    /// 「讀不到畫面」要用**有 active run、但 pane 不見了**來造：#685 起「成功查到沒有 run」是已知的
    /// `no_run`、不算 `probe_failed`，不能再拿沒有 run 來冒充。pane_id 清空會在 `probe_role` 讀 pane
    /// 之前就回 `None`，不碰 herdr，CI 與本機都穩定。
    #[tokio::test]
    async fn notify_stalling_is_a_fault_even_when_the_screen_cannot_be_read() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;
        let run = crate::testing::fake_run(&app, "bot-responder").await;
        sqlx::query("UPDATE runs SET pane_id = NULL WHERE id = ?").bind(&run).execute(&app.db).await.unwrap();

        note_notify_round(&app, "responder", turns(&["t1", "t2"]), false).await;
        refresh(&app).await;
        assert_eq!(reason(&app, RESPONDER).await, None, "還沒到門檻");

        note_notify_round(&app, "responder", turns(&["t3"]), false).await;
        refresh(&app).await;
        assert_eq!(reason(&app, RESPONDER).await, Some(REASON_NOTIFY_STALLED));
        assert!(snapshot(&app, RESPONDER).await.unwrap().since.is_some(), "要記下第一次看到的時間");

        // 回合跑完就歸零，下一拍自己恢復——不用任何人來按。**畫面照樣讀不到**：notify 那一半的證據
        // 在集合裡，不該因為看不到畫面就把已經解除的結論繼續留著。
        note_notify_round(&app, "responder", turns(&[]), true).await;
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.unwrap().probe_failed, "畫面仍然讀不到");
        assert_eq!(reason(&app, RESPONDER).await, None);
    }

    /// Production setup stores the patrol bot in `supervisors`, without a `supervisor_roles`
    /// patrol bot id. The health tick, health state, and incident probe must all use that source.
    #[tokio::test]
    async fn patrol_faults_use_the_bot_registered_by_supervisor_setup() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "AGM-patrol").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        crate::supervisor::store::set_env(&app.db, &bot.id, &env.project_id, "/tmp").await.unwrap();
        crate::testing::fake_run(&app, &bot.id).await;

        note_notify_round(&app, "patrol", turns(&["t1", "t2", "t3"]), false).await;
        refresh(&app).await;

        let fault = snapshot(&app, Role::Patrol).await.expect("configured patrol must be watched");
        assert_eq!(fault.reason, Some(REASON_NOTIFY_STALLED));
        assert_eq!(
            crate::supervisor::health::role_state(&app, Role::Patrol).await,
            crate::supervisor::health::RoleState::Unavailable(REASON_NOTIFY_STALLED),
        );

        let thresholds = crate::supervisor::incidents::Thresholds::from_cfg(&app.cfg.get().await.supervisor);
        let probed = crate::supervisor::incidents::observe(&app, &thresholds).await;
        let patrol_incident = probed.seen.iter().find(|observation| {
            observation.kind == crate::supervisor::incidents::ROLE_UNAVAILABLE_KIND
                && observation.resource == Role::Patrol.as_str()
        }).expect("patrol fault must appear in the incident probe");
        let detail: serde_json::Value = serde_json::from_str(&patrol_incident.detail).unwrap();
        assert_eq!(detail["bot_id"], bot.id);
        assert_eq!(detail["reason"], REASON_NOTIFY_STALLED);
    }

    /// 沒建立的角色**整個不留項目**：留著會讓 `incidents::observe` 每一拍把這個 kind 算成
    /// 「探針沒跑」，另一顆角色已經開著的 incident 就再也關不掉（blind 會擋住 resolve）。
    #[tokio::test]
    async fn a_role_that_was_never_set_up_leaves_no_entry_behind() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.is_some());
        assert!(snapshot(&app, Role::Patrol).await.is_none(), "巡檢還沒建立：不留紀錄");

        // 角色被拆掉（bot 刪了、登記清空）之後，上一輪留下的項目也要跟著消失。
        sqlx::query("UPDATE supervisor_roles SET bot_id=NULL").execute(&app.db).await.unwrap();
        refresh(&app).await;
        assert!(snapshot(&app, RESPONDER).await.is_none(), "不再建立就不留陳舊的結論");
    }

    /// #243：讀不到 bot 在哪台主機時不能拿本機的額度讀數去判——本機那把 key 對遠端 bot 永遠是空的，
    /// 會把一顆正常的遠端角色判成登入失效。回 `None`＝這一拍沒有證據。
    #[tokio::test]
    async fn an_unreadable_host_is_no_evidence_of_a_blank_quota() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        configure(&app, RESPONDER).await;
        sqlx::query("UPDATE projects SET host='mac_builder' WHERE id='p'").execute(&app.db).await.unwrap();
        let bot = crate::db::bot(&app.db, "bot-responder").await.unwrap().unwrap();

        crate::testing::make_table_unreadable(&app, "projects").await;
        let seen = quota_is_blank(&app, &bot).await;
        crate::testing::make_table_readable(&app, "projects").await;
        assert_eq!(seen, None, "讀不到主機不是「額度空的」");
        assert_eq!(quota_is_blank(&app, &bot).await, Some(true), "讀得到之後照常判（遠端這把 key 沒有讀數）");
    }
}
