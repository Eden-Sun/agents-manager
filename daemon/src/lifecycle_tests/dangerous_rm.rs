#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::dangerous_rm::*;
    use crate::testing as tt;
    use crate::tui_prompts::screens::{DANGEROUS_RM, DANGEROUS_RM_AUTO_DENIED};
    /// 記錄是全域的，[`forget_ended`] 會把「在這個測試 DB 讀不到」的別的測試的記錄清掉：這個模組的測試一個一個跑。
    async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
        static L: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        L.lock().await
    }
    async fn run_of(app: &impl crate::capabilities::Db, run_id: &str) -> db::Run {
        sqlx::query_as::<_, db::Run>("SELECT * FROM runs WHERE id=?").bind(run_id).fetch_one(app.db()).await.unwrap()
    }
    async fn system_messages(app: &impl crate::capabilities::Db, bot_id: &str) -> Vec<String> {
        let conv = db::conversation_id(app.db(), bot_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(app.db())
            .await
            .unwrap()
    }
    /// 真畫面：通知只寫一次、帶目標與指令；herdr 判成 idle 時補標 blocked；倒數到 0 框不見了 → 說明、還原成 idle；
    /// 從頭到尾一個鍵、一個字都沒送進 pane。
    #[tokio::test]
    async fn the_prompt_is_announced_once_marked_blocked_and_released_after_the_auto_deny() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        let msgs = system_messages(&app, &bot.id).await;
        assert_eq!(msgs.len(), 1, "{msgs:?}");
        assert!(msgs[0].contains("command substitution output") && msgs[0].contains("zz-nonexistent)/*"), "目標與指令都要寫：{}", msgs[0]);
        assert!(msgs[0].contains("不會替你按"), "{}", msgs[0]);
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "herdr 判 idle 時補標");
        assert!(is_open(&run_id));

        let later = DANGEROUS_RM.replace("in 1:52", "in 0:31");
        observe_screen(&app, &run_of(&app, &run_id).await, &later).await;
        assert_eq!(system_messages(&app, &bot.id).await.len(), 1, "同一個框只講一次");

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        let msgs = system_messages(&app, &bot.id).await;
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert_eq!(msgs[1], CLOSED_NOTE);
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle", "補標的 blocked 還回去");
        assert!(!is_open(&run_id));

        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(e.herdr.calls_to(m).is_empty(), "不能替使用者按、也不能打字進框：{m}");
        }
    }
    #[tokio::test]
    async fn stale_dialog_observation_does_not_mutate_ended_run() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-stale-run").await;
        let old_id = tt::fake_run(&app, &bot.id).await;
        let old = run_of(&app, &old_id).await;
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&old_id).execute(&app.db).await.unwrap();
        let current_id = tt::fake_run(&app, &bot.id).await;

        observe_screen(&app, &old, DANGEROUS_RM).await;

        assert_eq!(run_of(&app, &old_id).await.agent_status, "idle", "ended run must not be synthetically blocked");
        assert_eq!(run_of(&app, &current_id).await.agent_status, "idle", "new run remains untouched");
        assert!(!is_open(&old_id), "stale screen must not create an episode");
        assert!(system_messages(&app, &bot.id).await.is_empty(), "stale screen must not insert an alert");
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 0, "stale screen must not wake queued work");
    }
    /// herdr 自己判成 blocked 的（常見情形）不補標，框關掉時也不去改——那是 herdr 的狀態，它會自己報下一個。
    #[tokio::test]
    async fn a_blocked_status_that_herdr_set_is_left_to_herdr() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked");
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2);
    }
    /// 補標之後 herdr 報了別的狀態（例如使用者答完它開始 working）：還原是 CAS，不蓋掉 herdr 的新狀態。
    #[tokio::test]
    async fn restoring_does_not_overwrite_a_newer_herdr_status() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        crate::lifecycle::take_scheduled_flush_count(&bot.id);
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "working");
        assert!(!is_open(&run_id), "the newer herdr status authoritatively superseded our marker");
        assert_eq!(system_messages(&app, &bot.id).await.last().map(String::as_str), Some(CLOSED_NOTE));
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 1);
    }
    #[tokio::test]
    async fn a_herdr_blocked_status_after_the_synthetic_marker_is_not_restored_to_idle() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-herdr-blocked").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "dangerous-rm detection adds a synthetic blocked status");

        let event = |status: &str| crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: serde_json::json!({"pane_id": pane, "agent_status": status}),
        };
        crate::runners::events::handle_status(&app, crate::config::LOCAL_HOST, "test", &event("working")).await;
        crate::runners::events::handle_status(&app, crate::config::LOCAL_HOST, "test", &event("blocked")).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "herdr's question must remain blocked");

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;

        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "closing the old rm episode must not erase herdr's later blocked state");
        assert!(!is_open(&run_id));
    }
    #[tokio::test]
    async fn an_ended_run_releases_the_synthetic_blocked_episode() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-ended").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?")
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        crate::lifecycle::take_scheduled_flush_count(&bot.id);

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;

        let run = run_of(&app, &run_id).await;
        assert_eq!(run.state, "exited");
        assert_eq!(run.agent_status, "blocked", "don't rewrite an ended run");
        assert!(!is_open(&run_id), "the ended run authoritatively resolves the restore debt");
        assert_eq!(system_messages(&app, &bot.id).await.len(), 1, "an ended run gets no stale close message");
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 0, "an ended run cannot wake the queue");
    }
    /// A failed restore is still owed: keep the episode for the next patrol, and don't announce
    /// closure or wake queued prompts until the database accepts the restore.
    #[tokio::test]
    async fn a_failed_restore_keeps_the_episode_until_a_later_patrol_succeeds() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-restore").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let run = run_of(&app, &run_id).await;
        let conversation = db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','next prompt',?)",
        )
        .bind(db::ulid())
        .bind(&conversation)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        observe_screen(&app, &run, DANGEROUS_RM).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked");
        crate::lifecycle::take_scheduled_flush_count(&bot.id);
        sqlx::query(
            "CREATE TRIGGER refuse_dangerous_rm_restore BEFORE UPDATE OF agent_status ON runs
             WHEN OLD.agent_status='blocked' AND NEW.agent_status='idle' BEGIN SELECT RAISE(ABORT, 'database is locked'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "blocked", "restore did not commit");
        assert!(is_open(&run_id), "the patrol needs the retained episode to retry");
        assert_eq!(system_messages(&app, &bot.id).await.len(), 1, "don't announce closure before restore");
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 0, "don't wake the queue yet");

        sqlx::query("DROP TRIGGER refuse_dangerous_rm_restore")
            .execute(&app.db)
            .await
            .unwrap();
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle");
        assert!(!is_open(&run_id));
        assert_eq!(system_messages(&app, &bot.id).await, [notice(&crate::tui_prompts::dangerous_rm_prompt(DANGEROUS_RM).unwrap()), CLOSED_NOTE.to_string()]);
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 1);

        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2, "closure note is written once");
        assert_eq!(crate::lifecycle::take_scheduled_flush_count(&bot.id), 0, "resolved episode wakes the queue once");
    }
    /// 稽核：框開著時 run 結束（pane 被關、bot 停掉）。巡邏只掃 active run，這筆記錄再也等不到「框關了」，
    /// 不拿掉就一直留在行程記憶體裡。
    #[tokio::test]
    async fn an_episode_of_a_run_that_ended_while_the_box_was_open_is_forgotten() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-gone").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        assert!(is_open(&run_id));

        let active = vec![run_of(&app, &run_id).await];
        forget_ended(&app, &[]).await;
        assert!(is_open(&run_id), "DB 說還在進行中（只是這一輪名單沒帶到）：留著");
        forget_ended(&app, &active).await;
        assert!(is_open(&run_id));

        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run_id).execute(&app.db).await.unwrap();
        forget_ended(&app, &[]).await;
        assert!(!is_open(&run_id), "結束的 run 不留記錄");
    }
    /// 稽核：兩個不同的指令都落在同一句通用警語（`command substitution output`）時，以前只比警語，第二個框
    /// 沒人講——使用者看到的目標與指令還是第一個。框的內容不同就是新的一次，要再講一次，補標的 blocked 沿用。
    #[tokio::test]
    async fn a_second_box_with_the_same_warning_but_another_command_is_announced_again() {
        let _serial = serial().await;
        let e = tt::env().await;
        let app = e.app.clone();
        let bot = tt::claude_bot(&app, &e.project_id, "rm-two-cmds").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM).await;
        let second = DANGEROUS_RM.replace("which-dir", "other-dir");
        assert_eq!(
            crate::tui_prompts::dangerous_rm_prompt(DANGEROUS_RM).unwrap().warning,
            crate::tui_prompts::dangerous_rm_prompt(&second).unwrap().warning,
            "前提：警語一樣"
        );
        observe_screen(&app, &run_of(&app, &run_id).await, &second).await;
        let msgs = system_messages(&app, &bot.id).await;
        assert_eq!(msgs.len(), 2, "{msgs:?}");
        assert!(msgs[1].contains("other-dir"), "第二則要寫第二個指令：{}", msgs[1]);

        observe_screen(&app, &run_of(&app, &run_id).await, &second).await;
        assert_eq!(system_messages(&app, &bot.id).await.len(), 2);
        observe_screen(&app, &run_of(&app, &run_id).await, DANGEROUS_RM_AUTO_DENIED).await;
        assert_eq!(run_of(&app, &run_id).await.agent_status, "idle");
        assert!(!is_open(&run_id));
    }
}
