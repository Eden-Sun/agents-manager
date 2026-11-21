#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod prompt_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::prompt::rearm_queued_prompt_restamps;
    use crate::lifecycle::prompt::test_seams::*;
    // parent-uses
    use crate::lifecycle::*;
    use crate::state::App;
    use crate::lifecycle::send_now::ports::AttachConnPort;
    use crate::lc_error::PromptOut;
    use crate::lc_error::QueueClass;
    use crate::lifecycle::prompt::*;
    use crate::testing as tt;
    #[tokio::test]
    async fn macos_local_numbered_menu_is_reported_without_sending_keys() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-choice-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let screen = "Choose an option\n❯ 1. Example first\n  2. Example second\n";
        f.env.herdr.set_screen("pane-choice-test", screen);
        assert_eq!(agent_prompt_dialog_reason("claude", screen), Some("dialog_open"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "dialog_open");
                assert!(v["message"].as_str().unwrap().contains("終端"));
            }
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }
    #[tokio::test]
    async fn xreview_prompt_refuses_a_live_bot_after_its_project_is_deleted() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE projects SET deleted_at = ? WHERE id = ?")
            .bind(db::now())
            .bind(&f.env.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let err = prompt(&app, &f.bot_id, "must not reach the old pane", "xreview-project-deleted")
            .await
            .expect_err("a live child row under a deleted project must not accept prompt delivery");
        assert!(matches!(err, LcError::NotFound(ref what) if what == "project"), "unexpected error: {err:?}");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "the rejected prompt must not create a turn");
    }
    /// #341：排隊用的 turn INSERT 真的寫不進去（不是唯一約束衝突）不能被說成「已經有一筆在排」的 409；真的重複才是 409。
    #[tokio::test]
    async fn a_queue_insert_that_really_fails_is_not_reported_as_already_queued() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("CREATE TRIGGER am_test_no_turns BEFORE INSERT ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();
        let err = queue_for_next_turn(&app, &f.conv, &f.bot_id, "x", "x", "c-fail", RelaySrc::default()).await.unwrap_err();
        assert!(matches!(err, LcError::Upstream(_)), "真的寫入失敗要照實回，不是 409：{err:?}");
        sqlx::query("DROP TRIGGER am_test_no_turns").execute(&app.db).await.unwrap();

        queue_for_next_turn(&app, &f.conv, &f.bot_id, "one", "one", "c-1", RelaySrc::default()).await.unwrap();
        let err = queue_for_next_turn(&app, &f.conv, &f.bot_id, "two", "two", "c-2", RelaySrc::default()).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "真的已經有一筆在排才是 409：{err:?}");
    }
    /// A transient post-commit message lookup failure must be repaired by the same request's idempotent retry.
    #[tokio::test]
    async fn an_idempotent_retry_reemits_message_after_the_first_lookup_failed() {
        let f = fixture("codex", "test").await;
        let (turn_id, message_id, request_id) = (db::ulid(), db::ulid(), "retry-message-added");
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at)
             VALUES (?,?,?,'web','queued','pending',?,?)",
        )
        .bind(&turn_id)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(request_id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
             VALUES (?,?,?,'user','queued prompt','web',?)",
        )
        .bind(&message_id)
        .bind(&f.conv)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?")
            .bind(&turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
        let before = crate::capabilities::Emit::current_seq(&f.env.app);

        sqlx::query("ALTER TABLE messages RENAME TO messages_unreadable")
            .execute(&f.env.app.db)
            .await
            .unwrap();
        assert!(emit_prompt_message(&f.env.app, &f.bot_id, &message_id).await.is_err(), "the broken lookup must be observable");
        sqlx::query("ALTER TABLE messages_unreadable RENAME TO messages")
            .execute(&f.env.app.db)
            .await
            .unwrap();

        let retry = answer_for_turn(&f.env.app, &turn).await.unwrap();
        assert_eq!(retry.turn_id, turn_id);
        assert_eq!(retry.message_id, message_id);
        assert_eq!(crate::capabilities::Emit::current_seq(&f.env.app), before + 1, "retry publishes the missed message_added");
    }
    async fn fixture(kind: &str, session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'prompt-test',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, started_at)
             VALUES (?,?,'running','idle',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(session)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        // These exercise the agent.prompt path, which needs an agent herdr has a session bound to.
        env.herdr.set_agent("prompt-test", "pane-prompt-test", true);
        // This pane belongs to the mock's test session, not an unavailable session.
        if session == "test" {
            sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?")
                .bind(&run_id).execute(&app.db).await.unwrap();
        }
        Fixture { env, bot_id, conv, run_id }
    }
    /// 把 fixture 的 run A 收掉、起一顆新的 active run B，回 B 的 id（#872：計畫綁 A，送出時 active 已是 B）。
    async fn supersede_run(f: &Fixture) -> String {
        let app = &f.env.app;
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?").bind(db::now()).bind(&f.run_id).execute(&app.db).await.unwrap();
        tt::fake_run(app, &f.bot_id).await
    }
    /// #872：計畫綁 run A、鎖內 active 已是 B → 409 `superseded_run`，沒有 turn、沒有使用者訊息、herdr 沒收到任何字或鍵。
    #[tokio::test]
    async fn prompt_expect_run_refuses_a_different_active_run() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let run_b = supersede_run(&f).await;

        let err = prompt_expect_run(&app, &f.bot_id, "any updates", "keep-warm:anchor-a", &f.run_id)
            .await
            .expect_err("active run 已換成 B，不能送進去");
        match err {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "superseded_run", "{v}");
                assert_eq!((v["expected_run_id"].as_str(), v["run_id"].as_str()), (Some(f.run_id.as_str()), Some(run_b.as_str())));
            }
            other => panic!("{other:?}"),
        }
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        let messages: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='user'").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!((turns, messages), (0, 0), "沒有新 turn、沒有新 user message");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty() && f.env.herdr.calls_to("pane.send_keys").is_empty(), "一個字、一個鍵都沒打");
    }
    /// #1479：字打進去、Enter 按了、框空了，回音列卻看不到（`not_proven_submitted`）。回給呼叫端的仍是 `unknown`
    /// （不假裝證明了），但這一則要被背景認領——之後看到證據就升成 ok、解開輸入框，不必等 Stop hook。
    #[tokio::test]
    async fn an_empty_box_without_evidence_is_handed_to_the_background_watch() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE bots SET inject_hooks = 0 WHERE id = ?").bind(&f.bot_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), hide_echo: true, no_spinner: true, ..Default::default() });

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "late-proof-1").await.unwrap();
        assert_eq!(out.delivery, "unknown", "{out:?}");
        let armed = crate::lifecycle::late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| *t == out.turn_id);
        assert!(armed, "框已空、證據沒長出來的那一則要交給背景");
    }
    /// #1483：unknown 第一次寫入失敗、Pending 超過 60 秒後由 owed_delivery 補寫，仍須重新認領補證。
    #[tokio::test]
    async fn issue_1483_owed_unknown_write_rearms_expired_pending() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE bots SET inject_hooks=0 WHERE id=?").bind(&f.bot_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test', pane_typed=1 WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), hide_echo: true, no_spinner: true, ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER refuse_late_unknown BEFORE UPDATE OF delivery ON turns WHEN NEW.delivery='unknown'
             BEGIN SELECT RAISE(ABORT, 'injected: cannot record unknown delivery'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let err = prompt(&app, &f.bot_id, "Reply with PONG please", "issue-1483-owed").await.expect_err("第一次 unknown 寫入故意失敗");
        let LcError::Uncommitted(body) = err else { panic!("expected owed delivery response: {err:?}") };
        let turn_id = body["turn_id"].as_str().expect("turn id").to_string();
        assert_eq!(body["delivery"], "unknown");
        crate::lifecycle::late_proof::expire_pending_for_test(&f.run_id);
        sqlx::query("DROP TRIGGER refuse_late_unknown").execute(&app.db).await.unwrap();

        crate::lifecycle::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("DB 恢復後補寫 unknown");
        let delivery: String = sqlx::query_scalar("SELECT delivery FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(delivery, "unknown");
        assert!(crate::lifecycle::late_proof::armed_in_tests().lock().unwrap().iter().any(|(t, _)| *t == turn_id), "欠帳補寫不受原 Pending 的 60 秒期限影響");
    }
    /// #872：run 沒換、仍 idle 就照常往下走（建 turn）；不是被 `superseded_run` 擋掉。
    #[tokio::test]
    async fn prompt_expect_run_sends_on_the_same_idle_run() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });

        let out = prompt_expect_run(&app, &f.bot_id, "any updates", "keep-warm:anchor-a", &f.run_id).await;
        if let Err(LcError::Conflict(v)) = &out {
            assert_ne!(v["reason"], "superseded_run", "{v}");
        }
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND client_request_id='keep-warm:anchor-a'").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "同一顆 idle run：照常建 turn 往下送");
    }
    /// #872：active run 不再 idle（working）也不送。
    #[tokio::test]
    async fn prompt_expect_run_refuses_a_run_that_is_no_longer_idle() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        match prompt_expect_run(&app, &f.bot_id, "any updates", "keep-warm:anchor-a", &f.run_id).await.unwrap_err() {
            LcError::Conflict(v) => assert_eq!(v["reason"], "superseded_run", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    /// 帶附件的回合一個字都還沒打就撤回：要先解開 attachments.message_id，不能踩外鍵（#646）。
    #[tokio::test]
    async fn retracting_an_unsent_turn_with_an_attachment_withdraws_it() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let turn_id = db::ulid();
        let msg_id = db::ulid();
        let file_id = attachment(&app, &f.bot_id).await;
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, client_request_id, created_at)
             VALUES (?,?,?,'web','in_flight','pending','crid-attach',?)",
        )
        .bind(&turn_id)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
             VALUES (?,?,?,'user','看這張圖','web',?)",
        )
        .bind(&msg_id)
        .bind(&f.conv)
        .bind(&turn_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("UPDATE attachments SET message_id=? WHERE id=?")
            .bind(&msg_id)
            .bind(&file_id)
            .execute(&app.db)
            .await
            .unwrap();

        let out = retract_unsent_turn(&app, &f.bot_id, &turn_id, &msg_id).await;
        assert!(matches!(out, Ok(Retraction::Withdrawn)), "帶附件也要撤得掉");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        let msgs: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE id=?").bind(&msg_id).fetch_one(&app.db).await.unwrap();
        let bound: Option<String> = sqlx::query_scalar("SELECT message_id FROM attachments WHERE id=?").bind(&file_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 0);
        assert_eq!(msgs, 0);
        assert!(bound.is_none(), "附件列留著，但不再指著已刪的訊息：{bound:?}");
    }
    #[tokio::test]
    async fn nr_target_rename_before_turn_admission_preserves_the_same_run() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test', agent_name='prompt-test' WHERE id=?")
            .bind(&f.run_id).execute(&app.db).await.unwrap();
        let changing_app = app.clone();
        let run_id = f.run_id.clone();
        crate::lifecycle::race_point::arm("prompt_before_turn_commit", &f.bot_id, move || async move {
            sqlx::query("UPDATE runs SET agent_name='renamed' WHERE id=?")
                .bind(&run_id).execute(&changing_app.db).await.unwrap();
        });
        let out = prompt(&app, &f.bot_id, "same pane after rename", "nr-target-rename").await;
        assert!(out.is_ok(), "a display rename must not invalidate admission: {out:?}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?")
            .bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 1);
    }
    #[tokio::test]
    async fn nr_target_native_session_change_before_admission_sends_nothing() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let changing_app = app.clone();
        let run_id = f.run_id.clone();
        crate::lifecycle::race_point::arm("prompt_before_turn_commit", &f.bot_id, move || async move {
            sqlx::query("UPDATE runs SET native_session_id='replacement-session' WHERE id=?")
                .bind(&run_id).execute(&changing_app.db).await.unwrap();
        });
        let out = prompt(&app, &f.bot_id, "must not reach a replacement session", "nr-target-session-change").await;
        let Err(LcError::Conflict(body)) = out else { panic!("session replacement must invalidate admission: {out:?}") };
        assert_eq!(body["reason"], "run_ended");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?")
            .bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 0);
        for method in ["agent.prompt", "pane.send_text", "pane.send_keys"] {
            assert!(f.env.herdr.calls_to(method).is_empty(), "no side effect after session replacement: {method}");
        }
    }
    #[tokio::test]
    async fn a_run_exit_before_direct_turn_admission_sends_nothing_and_allows_a_clean_retry() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let app_for_exit = app.clone();
        let exiting_run = f.run_id.clone();
        crate::lifecycle::race_point::arm("prompt_before_turn_commit", &f.bot_id, move || async move {
            assert_eq!(mark_run_exited(&app_for_exit, &exiting_run, "pane exited").await, RunExit::Recorded);
        });

        let out = prompt(&app, &f.bot_id, "這句不能送進已結束的 run", "race-before-turn").await;
        let Err(LcError::Conflict(body)) = out else { panic!("losing the exit race must be retryable without creating a turn: {out:?}") };
        assert_eq!(body["reason"], "run_ended", "{body}");
        assert_eq!(body["run_id"], f.run_id, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(count, 0, "the lost admission leaves no in-flight or idempotency row");
        for method in ["agent.prompt", "pane.send_text", "pane.send_keys"] {
            assert!(f.env.herdr.calls_to(method).is_empty(), "the losing request has no delivery side effect: {method}");
        }

        let new_run = db::ulid();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at) VALUES (?, ?, 'running', 'idle', 'pane-prompt-test', 'test', ?)")
            .bind(&new_run).bind(&f.bot_id).bind(db::now()).execute(&app.db).await.unwrap();
        let retried = prompt(&app, &f.bot_id, "這句不能送進已結束的 run", "race-before-turn").await.unwrap();
        assert_eq!(retried.delivery, "unknown", "the mock accepts the new run's RPC path");
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND client_request_id='race-before-turn'")
            .bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(stored, 1, "the same client_request_id is reusable after the losing admission");
    }
    #[tokio::test]
    async fn a_run_exit_after_turn_admission_closes_the_new_turn() {
        let f = fixture("claude", "test").await;
        let app_for_exit = f.env.app.clone();
        let exiting_run = f.run_id.clone();
        let conversation = f.conv.clone();
        crate::lifecycle::race_point::arm("prompt_before_typing", &f.bot_id, move || async move {
            let status: String = sqlx::query_scalar("SELECT status FROM turns WHERE conversation_id=? AND client_request_id='exit-after-admission'")
                .bind(&conversation).fetch_one(&app_for_exit.db).await.unwrap();
            assert_eq!(status, "in_flight", "the prompt transaction committed before run exit");
            assert_eq!(mark_run_exited(&app_for_exit, &exiting_run, "pane exited").await, RunExit::Recorded);
        });

        let _ = prompt(&f.env.app, &f.bot_id, "在退出之前建立的回合", "exit-after-admission").await;
        let (status, notes): (String, i64) = sqlx::query_as(
            "SELECT t.status, (SELECT COUNT(*) FROM messages m WHERE m.turn_id=t.id AND m.role='system' AND m.content='run ended: pane exited')
               FROM turns t WHERE t.conversation_id=? AND t.client_request_id='exit-after-admission'",
        )
        .bind(&f.conv)
        .fetch_one(&f.env.app.db)
        .await
        .unwrap();
        assert_eq!(status, "failed", "run-exit cleanup sees and closes the admitted turn");
        assert_eq!(notes, 1, "run-exit cleanup leaves its normal explanation");
    }
    async fn attachment(app: &Arc<App>, bot_id: &str) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO attachments (id, bot_id, name, mime, size, local_path, agent_path, host, created_at)
             VALUES (?,?,'image.png','image/png',1,'/tmp/image.png','/tmp/image.png','local',?)",
        )
        .bind(&id)
        .bind(bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        id
    }
    /// 在對方回合中派工：AGM 那條路排隊（queued turn），使用者的 `/prompt` 仍是 409（AGM 2026-09-16 裁示）。
    #[tokio::test]
    async fn an_agm_dispatch_queues_behind_an_in_flight_turn_but_a_user_prompt_still_gets_409() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        // 先佔住一個回合。
        let busy = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&busy)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        // 使用者：照舊 409，不排隊。
        let user = prompt(&app, &f.bot_id, "使用者自己打的", "user-1").await;
        assert!(matches!(user, Err(LcError::Conflict(_))), "使用者的 /prompt 仍是 409");

        // AGM 派工：排一筆 queued，內容存在 prompt_text 等 flush。
        let out = prompt_relayed_queueable(&app, &f.bot_id, "AGM 派的工作", "agm-1", Some("agm-bot"), QueueClass::Relay).await.unwrap();
        assert_eq!(out.delivery, "queued");
        let (status, run_id, text): (String, Option<String>, Option<String>) =
            sqlx::query_as("SELECT status, run_id, prompt_text FROM turns WHERE id=?")
                .bind(&out.turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!((status.as_str(), run_id, text.as_deref()), ("queued", None, Some("AGM 派的工作")));

        // 同一筆交辦重試：回原本那一筆，不會疊第二筆。
        let again = prompt_relayed_queueable(&app, &f.bot_id, "AGM 派的工作", "agm-1", Some("agm-bot"), QueueClass::Relay).await.unwrap();
        assert_eq!(again.turn_id, out.turn_id);
        assert_eq!(again.delivery, "queued", "重問一筆還在排隊的，回答跟第一次一樣（不是 turn 欄位上的 pending）");
        // 另一筆交辦想排第二筆：擋下來，照舊 409（每個對話只留一筆 queued）。
        let other = prompt_relayed_queueable(&app, &f.bot_id, "另一件事", "agm-2", Some("agm-bot"), QueueClass::Relay).await;
        assert!(matches!(other, Err(LcError::Conflict(_))), "每個對話只留一筆 queued");
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(queued, 1);
    }
    /// 外部使用者可以提供合法的 `client_request_id`；保留 daemon 前綴也不能讓它跳到另一個 queued 槽。
    #[tokio::test]
    async fn an_external_child_done_prefix_still_occupies_the_user_queue_slot() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let busy = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&busy)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();

        let spoofed = prompt_from_api_queue_if_busy(
            &app,
            &f.bot_id,
            "使用者的一句",
            "child-done:user-controlled",
            &[],
            RelaySrc::trusted(None),
            false,
            None,
        )
        .await
        .expect("the first user prompt is queued");
        assert_eq!(spoofed.delivery, "queued");
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT queue_class FROM turns WHERE id=?")
                .bind(&spoofed.turn_id)
                .fetch_one(&app.db)
                .await
                .unwrap(),
            QueueClass::User.as_str(),
            "caller-controlled id prefixes never select a queue lane",
        );

        let second = prompt_from_api_queue_if_busy(
            &app,
            &f.bot_id,
            "使用者再一句",
            "user-second",
            &[],
            RelaySrc::trusted(None),
            false,
            None,
        )
        .await;
        assert!(matches!(second, Err(LcError::Conflict(_))), "a caller-controlled daemon prefix must not open a second user slot: {second:?}");
        let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(queued, 1);
    }
    /// AGM and other bot relays stay queued while a busy user can occupy the user slot ahead of them.
    #[tokio::test]
    async fn an_agm_or_bot_relay_does_not_occupy_the_user_queue_slot() {
        for (relay_from, relay_crid) in [("agm-bot", "dispatch-1"), ("child-bot", "relay-1")] {
            let f = fixture("codex", "test").await;
            let app = f.env.app.clone();
            let busy = db::ulid();
            sqlx::query(
                "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
                 VALUES (?,?,?,'web','in_flight','ok',?)",
            )
            .bind(&busy)
            .bind(&f.conv)
            .bind(&f.run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();

            let relay = prompt_relayed_queueable(&app, &f.bot_id, "自動轉送", relay_crid, Some(relay_from), QueueClass::Relay)
                .await
                .expect("the automation relay is queued");
            sqlx::query("UPDATE turns SET created_at='0001-01-01T00:00:00.000Z' WHERE id=?")
                .bind(&relay.turn_id)
                .execute(&app.db)
                .await
                .unwrap();
            let user = prompt_from_api_queue_if_busy(
                &app,
                &f.bot_id,
                "使用者的一句",
                "user-while-relay-queued",
                &[],
                RelaySrc::trusted(None),
                false,
                None,
            )
            .await
            .expect("the queued relay must not block the user's independent slot");

            let queued: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='queued'")
                .bind(&f.conv)
                .fetch_one(&app.db)
                .await
                .unwrap();
            assert_eq!(queued, 2, "both user and relay turns remain queued");
            assert_eq!(db::queued_turn(&app.db, &f.conv).await.unwrap().unwrap().id, user.turn_id, "user goes first");
            assert_eq!(db::queued_turn_for_bot(&app.db, &f.bot_id).await.unwrap().unwrap().id, user.turn_id, "the per-bot API projection picks the user turn");
            assert_eq!(db::queued_turns_by_bot(&app.db).await.unwrap()[&f.bot_id].id, user.turn_id, "GET /api/state picks the user turn");
            assert_ne!(user.turn_id, relay.turn_id);
        }
    }
    /// 打字前 `runs.pane_typed` 寫不進去（SQLite 鎖逾時、磁碟滿）：一個字都沒打，所以是 409＋撤回 turn，
    /// 不能留一筆 `unknown` 的 in_flight 擋住之後每一則、5 分鐘後又被收成 completed_fallback（review3 c4 L5）。
    #[tokio::test]
    async fn a_pane_typed_marker_that_cannot_be_written_is_a_retryable_409_with_no_turn_left() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER pane_typed_unwritable BEFORE UPDATE OF pane_typed ON runs
             BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-pane-typed").await;
        let Err(LcError::Conflict(body)) = out else { panic!("expected a retryable 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "pane_typed_unwritable", "{body}");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "沒送出的 turn 被撤回，不是留成 unknown");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }
    /// 窄窗：turn 已經 commit、第一個字還沒打的時候 pane 死掉，事件路徑 `mark_run_exited` →
    /// `fail_in_flight`（不拿 per-bot 鎖）把這筆標 failed 並插了「run ended」說明。撤回這時候
    /// 一個字都不能刪——訊息那句原本不帶條件，會把使用者的泡泡跟那則說明一起刪掉，只留一個空的
    /// failed 回合，還回可重試的 409 叫呼叫端重送，重送又從冪等分支拿到那筆失敗的回合（review3 L4）。
    ///
    /// `BEFORE UPDATE OF pane_typed` 的 trigger 正好落在那個窗裡：`RAISE(FAIL)` 只中止外層那句
    /// UPDATE，trigger 自己先做的兩筆寫入留著，等於另一條路徑插進來的效果。
    #[tokio::test]
    async fn a_turn_the_run_end_already_failed_is_left_alone_and_answered_like_a_resend() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?")
            .bind(&f.run_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        sqlx::query(
            "CREATE TRIGGER the_run_ends_mid_delivery BEFORE UPDATE OF pane_typed ON runs
             BEGIN
               INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at)
                 SELECT 'msg-run-ended', t.conversation_id, t.id, 'system', 'run ended: pane died', 'system', datetime('now')
                   FROM turns t WHERE t.run_id = NEW.id AND t.status = 'in_flight';
               UPDATE turns SET status = 'failed', completed_at = datetime('now')
                 WHERE run_id = NEW.id AND status = 'in_flight';
               SELECT RAISE(FAIL, 'run ended mid-delivery');
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced").await.expect("不是可重試的 409");

        // 那一筆回合留著，使用者的訊息與「run ended」說明都在。
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id = ?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(t.status, "failed", "收掉它的是 run 結束那條路，不是撤回");
        let msgs: Vec<(String, String)> = sqlx::query_as("SELECT role, content FROM messages WHERE turn_id = ? ORDER BY role")
            .bind(&out.turn_id)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert_eq!(
            msgs,
            vec![("system".to_string(), "run ended: pane died".to_string()), ("user".to_string(), "Reply with PONG please".to_string())],
            "使用者的泡泡與那則說明都不該被刪掉",
        );
        assert!(!out.message_id.is_empty(), "回的是那顆還在的泡泡");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");

        // 同一個 request id 重送：走冪等分支，答案必須跟剛剛那次一模一樣。
        let again = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced").await.unwrap();
        assert_eq!(
            (again.turn_id, again.message_id, again.delivery),
            (out.turn_id, out.message_id, out.delivery),
            "撤不掉時的回答要跟同一個 request id 重送拿到的一致",
        );
    }
    /// A missing run session is rejected before writing, so a retry reports the same upstream problem.
    #[tokio::test]
    async fn an_unavailable_run_session_does_not_create_a_turn() {
        let f = fixture("codex", "no-such-session").await;
        let app = f.env.app.clone();

        assert!(matches!(prompt(&app, &f.bot_id, "first", "prompt-1").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(turns, 0, "the unavailable client was checked before INSERT");

        assert!(matches!(prompt(&app, &f.bot_id, "second", "prompt-2").await, Err(LcError::Upstream(_))));
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
    }
    /// 別的 bot／排程送進來的 prompt 要留 `relay_from`，UI 才分得出來源（2026-09-12 使用者）。
    #[tokio::test]
    async fn a_relayed_prompt_records_who_sent_it() {
        // 一顆 bot 同時只有一個回合在飛：各用一個 fixture。
        let user = fixture("codex", "test").await;
        let user_app = user.env.app.clone();
        let mine = prompt_with(&user_app, &user.bot_id, "使用者自己打的", "prompt-user", &[]).await.unwrap();

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let relayed = prompt_relayed(&app, &f.bot_id, "排程派的", "prompt-daemon", &[], Some(crate::agent_relay::DAEMON_SENDER))
            .await
            .unwrap();

        let from = |db: sqlx::SqlitePool, id: &str| {
            let db = db.clone();
            let id = id.to_string();
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT relay_from FROM messages WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&db)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(from(user_app.db.clone(), &mine.message_id).await, None, "使用者自己打的不該有來源標");
        assert_eq!(
            from(app.db.clone(), &relayed.message_id).await,
            Some(crate::agent_relay::DAEMON_SENDER.to_string())
        );
    }
    /// 握住一個 `restart` 維護窗口（`mins` 為負＝已經過期）。
    async fn hold_restart_window(app: &Arc<App>, owner: &str, mins: i64) {
        let until = (chrono::Utc::now() + chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        crate::supervisor::store::acquire_lease(&app.db, "restart", owner, None, None, &until, false, None, &json!({}))
            .await
            .unwrap()
            .expect("沒有人握著，一定拿得到");
    }
    async fn turn_count(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(conv).fetch_one(&app.db).await.unwrap()
    }
    /// issue #86：`restart` 租約握著的期間，daemon 這一側不再開新回合。擋下來要講清楚**誰**握著、
    /// 到**什麼時候**——只說「送不出去」的話呼叫端只會一直重送。連一列都不建，所以同一個
    /// `client_request_id` 之後原樣重送是乾淨的。
    ///
    /// 這一支跟 `maintenance` 那邊的 `a_window_is_not_taken_while_a_prompt_is_in_the_delivery_critical_section`
    /// 是一對：租約先到就擋 prompt，prompt 先到就擋租約，兩個不會同時進送達臨界區。
    #[tokio::test]
    async fn a_held_restart_window_refuses_new_prompts_and_says_who_holds_it() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;
        let mut events = app.subscribe();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-in-window").await;

        let Err(LcError::Conflict(body)) = out else { panic!("expected 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "maintenance_window", "{body}");
        assert_eq!(body["held_by"], "k8bw2f", "{body}");
        assert_eq!(body["resource"], "restart", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(body["expires_at"].as_str().is_some_and(|s| !s.is_empty()), "要講到什麼時候：{body}");
        assert!(body["retry_after_secs"].as_i64().unwrap_or(0) > 0, "要講還要等多久：{body}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "連 turn 都沒建");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
        // 擋在 turn 之前，不是建完再撤：撤回那條路會先廣播 `message_added` 再補一次 `resync`，
        // 使用者看到的是一顆泡泡冒出來又消失。窗口開著時每一則 prompt 都這樣閃一下是不行的。
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), events.recv()).await.is_err(),
            "一則都不該廣播：沒有泡泡冒出來又消失",
        );
    }
    /// 窗口 release 之後自動恢復：同一個 `client_request_id` 原樣重送就真的送出去（前一次沒留下任何列）。
    #[tokio::test]
    async fn a_released_window_lets_the_same_request_id_through_again() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;
        assert!(prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-retry").await.is_err());

        let l = crate::supervisor::store::lease(&app.db, "restart").await.unwrap().unwrap();
        assert!(crate::supervisor::store::release_lease(&app.db, "restart", "k8bw2f", l.fence).await.unwrap());

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-retry").await.expect("窗口關了就照常送");
        assert_ne!(out.delivery, "failed", "{out:?}", out = out.delivery);
        assert_eq!(turn_count(&app, &f.conv).await, 1);
    }
    /// 過期沒 release 的租約**不會**把 daemon 鎖死：閘門看 `held_at`，`expires_at` 一到就自動不再擋，
    /// 不需要任何人來收尾（租約 TTL 上限一小時）。
    #[tokio::test]
    async fn an_expired_window_fences_nothing() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", -1).await;
        let l = crate::supervisor::store::lease(&app.db, "restart").await.unwrap().unwrap();
        assert!(l.released_at.is_none(), "沒有人 release，它只是過期了");

        prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-after-expiry").await.expect("過期的窗口不擋");
    }
    /// 控制面自己的送入（AGM／協調者啟動時的握手）不受閘門管：窗口就是為了「停下來再起來」而開的，
    /// 連起來時的握手都擋掉，控制面就被自己的閘門鎖在門外（issue #86 驗收第三條）。
    #[tokio::test]
    async fn the_control_plane_handshake_is_not_fenced() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;

        assert!(prompt(&app, &f.bot_id, "使用者的", "prompt-user").await.is_err(), "一般送入照擋");
        prompt_control_plane(&app, &f.bot_id, "AGM 啟動握手", "agm-bootstrap-v1", Some(crate::agent_relay::DAEMON_SENDER))
            .await
            .expect("控制面的握手照樣進得去");
    }
    /// 窄窗：閘門讀完之後、這一筆 turn commit 之前，窗口才被拿走。一個字都還沒打，所以這一筆要被
    /// 撤回再回 409——留著的話它就是一筆 `in_flight`＋`pending`，正好是窗口承諾「裡面沒有」的那種。
    /// 用 `AFTER INSERT ON turns` 的 trigger 精準插進那個時點。
    #[tokio::test]
    async fn a_window_that_opens_while_the_turn_commits_takes_the_turn_back_out() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        // 這一刻還沒有人握著（`released_at` 有值）：閘門的第一次讀會放行。
        sqlx::query("INSERT INTO supervisor_leases (resource, owner, fence, released_at) VALUES ('restart','k8bw2f',7,?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER a_window_opens_mid_commit AFTER INSERT ON turns
             BEGIN
               UPDATE supervisor_leases SET expires_at='2099-01-01T00:00:00Z', acquired_at='2026-01-01T00:00:00Z',
                      released_at=NULL WHERE resource='restart';
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-raced-window").await;

        let Err(LcError::Conflict(body)) = out else { panic!("expected 409, got {:?}", out.map(|o| o.delivery)) };
        assert_eq!(body["reason"], "maintenance_window", "{body}");
        assert_eq!(body["held_by"], "k8bw2f", "{body}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "搶進來的那一筆被撤回，不是留成 in_flight+pending");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }
    /// issue #127：讀不到 `restart` 租約 **不等於** 沒有窗口。窗口明明握著、只是那一刻 SELECT 失敗——
    /// 以前 `window_held` 把錯誤當成「沒有窗口」，新的 prompt 就在維護期間進了 pane。現在照窗口處理：
    /// 一個字都不送、連 turn 都不建；讀取恢復之後重新判斷，窗口還在就是「真的有窗口」的 409，
    /// 窗口收了就照常送（不會永久卡住）。
    #[tokio::test]
    async fn a_restart_lease_that_cannot_be_read_refuses_the_prompt_instead_of_letting_it_through() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        hold_restart_window(&app, "k8bw2f", 5).await;
        crate::supervisor::maintenance::fault::break_lease_reads(&app.db).await;

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unreadable").await;

        assert!(out.is_err(), "讀不到窗口狀態就不能放行：{:?}", out.as_ref().map(|o| &o.delivery));
        assert_eq!(turn_count(&app, &f.conv).await, 0, "連 turn 都沒建");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "agent.prompt").count(), 0, "一個字都沒送");

        // 讀取恢復、窗口還握著：現在是「真的有窗口」的 409，跟上面「讀不到」分得開。
        crate::supervisor::maintenance::fault::restore_lease_reads(&app.db).await;
        let Err(LcError::Conflict(body)) = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unreadable").await else {
            panic!("窗口還握著，要是 409")
        };
        assert_eq!(body["reason"], "maintenance_window", "{body}");

        // 窗口收了：同一個 request id 原樣重送就送得進去。
        let l = crate::supervisor::store::lease(&app.db, "restart").await.unwrap().unwrap();
        assert!(crate::supervisor::store::release_lease(&app.db, "restart", "k8bw2f", l.fence).await.unwrap());
        prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unreadable").await.expect("恢復＋窗口收了：照常送");
    }
    /// 同一條 fence 的第二道：第一次讀時窗口不在，turn commit 之後複查的那一刻才讀壞。這一筆已經 commit、
    /// 一個字都還沒打，所以要被撤回（不是留成 `in_flight`＋`pending`）再回錯誤——跟窗口在複查時
    /// 剛好被拿走是同一條路（`a_window_that_opens_while_the_turn_commits_takes_the_turn_back_out`）。
    #[tokio::test]
    async fn a_lease_that_becomes_unreadable_while_the_turn_commits_takes_the_turn_back_out() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("INSERT INTO supervisor_leases (resource, owner, fence, released_at) VALUES ('restart','k8bw2f',7,?)")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        // turn 一 commit，那一列就變成解不開的樣子（第一次讀已經過了）。
        sqlx::query(
            "CREATE TRIGGER the_lease_goes_bad_mid_commit AFTER INSERT ON turns
             BEGIN
               UPDATE supervisor_leases SET fence='corrupt' WHERE resource='restart';
             END",
        )
        .execute(&app.db)
        .await
        .unwrap();

        let out = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-lease-goes-bad").await;

        assert!(out.is_err(), "複查讀不到就不能放行：{:?}", out.as_ref().map(|o| &o.delivery));
        assert_eq!(turn_count(&app, &f.conv).await, 0, "已經 commit 的那一筆要撤回，不是留成 in_flight+pending");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "agent.prompt").count(), 0, "一個字都沒送");
    }
    /// #158 驗收一：附件綁不上（一個字都沒送），收成 failed 那一句又寫不進去——以前 helper 只記一行 error，照樣回普通的
    /// `delivery=failed`，DB 卻停在 in_flight＋pending。現在記成欠著、回 503（`sent:false`）；DB 好了同一個 request id
    /// 拿到 failed＋說明，一個字都沒送。
    #[tokio::test]
    async fn an_attachment_bind_failure_whose_close_cannot_be_written_is_not_a_plain_failed() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let attachment_id = attachment(&app, &f.bot_id).await;
        sqlx::query("CREATE TRIGGER fail_prompt_attachment_bind BEFORE UPDATE OF message_id ON attachments BEGIN SELECT RAISE(ABORT, 'bind failed'); END")
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let res = prompt_with(&app, &f.bot_id, "look", "prompt-attach-lost", std::slice::from_ref(&attachment_id)).await;
        let Err(LcError::Uncommitted(body)) = res else { panic!("收尾沒寫成，卻回了 {:?}", res.map(|o| o.delivery)) };
        assert_eq!((body["error"].as_str(), body["delivery"].as_str(), &body["sent"]), (Some("delivery_state_uncommitted"), Some("failed"), &json!(false)), "{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"), "寫不進去就是還沒收");

        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        let again = prompt_with(&app, &f.bot_id, "look", "prompt-attach-lost", &[attachment_id]).await.expect("DB 好了");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (turn_id.as_str(), "failed"));
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn_id).fetch_all(&app.db).await.unwrap();
        assert!(notes.len() == 1 && notes[0].contains("attachment binding failed"), "{notes:?}");
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none(), "不擋住下一則");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt" || m == "pane.send_text"), "一個字都沒送");
    }
    /// #158（同一類）：打字前才發現送不了（`runs.pane_typed` 寫不進去 → `NotAttempted`），撤回那一句寫不進去、改收成 failed
    /// 那一句也寫不進去——以前第二句被 `let _ =` 吞掉，那一筆停在 in_flight＋pending、沒人會收（擋住之後每一則）。
    /// 現在記成欠著、回 503（`sent:false`）；DB 好了同一個 request id 拿到 failed＋說明，一個字都沒打。
    #[tokio::test]
    async fn an_unsent_turn_that_can_neither_be_withdrawn_nor_failed_is_owed_not_left_in_flight() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id = 'pane-prompt-test', pane_typed = 1 WHERE id = ?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.live_pane("pane-prompt-test", tt::LivePane { width: Some(120), ..Default::default() });
        for ddl in [
            "CREATE TRIGGER pane_typed_unwritable BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'database or disk is full'); END",
            "CREATE TRIGGER lost_retract BEFORE DELETE ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END",
        ] {
            sqlx::query(ddl).execute(&app.db).await.unwrap();
        }

        let res = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unretractable").await;
        let Err(LcError::Uncommitted(body)) = res else { panic!("撤不回來也收不成，卻回了 {:?}", res.map(|o| o.delivery)) };
        assert_eq!((body["delivery"].as_str(), &body["sent"]), (Some("failed"), &json!(false)), "{body}");
        let turn_id = body["turn_id"].as_str().unwrap().to_string();
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"));

        sqlx::query("DROP TRIGGER lost_retract").execute(&app.db).await.unwrap();
        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        let again = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-unretractable").await.expect("DB 好了");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (turn_id.as_str(), "failed"));
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "不留永久 in_flight");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&turn_id).fetch_all(&app.db).await.unwrap();
        assert_eq!(notes, vec![UNRETRACTED_NOTE.to_string()]);
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 0, "一個字都沒打");
    }
    /// daemon 自己排的回合（提醒＝DaemonNotice、保溫探測＝`prompt_expect_run`）不是人打的字：不入帳，也不推 `ledger_updated`。
    #[tokio::test]
    async fn daemon_turns_are_never_ingested_as_user_commitments() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let notice = "承諾帳本提醒（系統排的，不是使用者訊息）：以下事項需要你處理";
        crate::lifecycle::prompt_relayed_queueable(&app, &f.bot_id, notice, "daemon-notice-1", None, crate::lifecycle::QueueClass::DaemonNotice).await.unwrap();
        let keep_warm = crate::lifecycle::prompt_expect_run(&app, &f.bot_id, "any updates", "keep-warm-1", &f.run_id).await;
        let _ = keep_warm;
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ledger_items WHERE project_id = ?").bind(&f.env.project_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(n, 0, "daemon 自己的回合不能變成承諾");
        let (msgs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM messages WHERE content LIKE '承諾帳本提醒%'").fetch_one(&app.db).await.unwrap();
        assert_eq!(msgs, 1, "提醒訊息本身照常寫入");
    }

    /// 管理入口收到使用者的一句：入帳，並推 `ledger_updated`（網頁不用重整就看得到「待分流」）。
    #[tokio::test]
    async fn a_user_prompt_to_the_management_bot_is_ingested_and_announced() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let mut events = app.subscribe();
        let out = prompt_with(&app, &f.bot_id, "請查 CI 失敗", "prompt-ledger-ingest", &[]).await.unwrap();
        let mut announced = None;
        while let Ok(Ok(ev)) = tokio::time::timeout(std::time::Duration::from_millis(500), events.recv()).await {
            if ev.kind == "ledger_updated" {
                announced = Some(ev.data.clone());
            }
        }
        let ev = announced.expect("ledger_updated is announced after the ingest");
        assert_eq!(ev["source_message_id"].as_str(), Some(out.message_id.as_str()));
        let (title, status): (String, String) = sqlx::query_as("SELECT title, status FROM ledger_items WHERE source_message_id = ?").bind(&out.message_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((title.as_str(), status.as_str()), ("請查 CI 失敗", "untriaged"));
    }
    /// Binding can fail after the turn commits; the UI must still get a terminal turn event.
    #[tokio::test]
    async fn an_attachment_bind_failure_closes_the_pending_turn() {
        let success = fixture("codex", "test").await;
        let success_app = success.env.app.clone();
        let success_attachment = attachment(&success_app, &success.bot_id).await;
        let mut success_events = success_app.subscribe();
        let success_out = prompt_with(&success_app, &success.bot_id, "look", "prompt-attachments-ok", &[success_attachment])
            .await
            .unwrap();
        let success_event = tokio::time::timeout(std::time::Duration::from_secs(1), success_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(success_event.kind, "message_added");
        assert_eq!(success_event.data["message"]["id"], success_out.message_id);
        assert!(!success_event.data["message"]["attachments_json"].is_null());

        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        let attachment_id = attachment(&app, &f.bot_id).await;
        sqlx::query(
            "CREATE TRIGGER fail_prompt_attachment_bind
             BEFORE UPDATE OF message_id ON attachments
             BEGIN SELECT RAISE(ABORT, 'bind failed'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        let mut ws_events = app.subscribe();
        let mut turn_events = app.subscribe_turns();

        let out = prompt_with(&app, &f.bot_id, "look", "prompt-attachments", &[attachment_id]).await.unwrap();
        assert_eq!(out.delivery, "failed");
        let user_event = tokio::time::timeout(std::time::Duration::from_secs(1), ws_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(user_event.kind, "message_added");
        assert_eq!(user_event.data["message"]["id"], out.message_id);
        assert_eq!(user_event.data["message"]["role"], "user");
        assert!(db::in_flight_turn(&app.db, &f.run_id).await.unwrap().is_none());
        let turn: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((turn.status.as_str(), turn.delivery.as_str()), ("failed", "failed"));
        let system: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert!(system.contains("attachment binding failed"));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), turn_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.turn_id, out.turn_id);
        assert_eq!((event.status.as_str(), event.delivery.as_str()), ("failed", "failed"));
    }
    /// 2.1.278 首次啟動的「Auto mode」框（2026-09-22）：送 Down＋Enter 選 keep bypass；mock 的畫面不會因為按鍵改變，
    /// 所以這裡驗的是「按了對的鍵、還在就 409 dialog_open、沒把 prompt 打進框裡」。
    #[tokio::test]
    async fn the_auto_mode_offer_is_answered_with_keep_bypass_and_still_open_is_a_409() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::AUTO_MODE);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        match err {
            LcError::Conflict(v) => assert_eq!(v["reason"], "dialog_open", "{v}"),
            other => panic!("{other:?}"),
        }
        let keys: Vec<String> = f.env.herdr.calls_to("pane.send_keys").iter().filter_map(|p| p.get("keys").and_then(|k| serde_json::to_string(k).ok())).collect();
        assert_eq!(keys, vec![r#"["Down","Enter"]"#.to_string()], "先替它選第二項 keep bypass");
    }
    #[tokio::test]
    async fn an_unreadable_auto_mode_recheck_refuses_to_continue() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::AUTO_MODE);
        let fail = f.env.herdr.fail_later();
        crate::lifecycle::race_point::arm("claude_auto_mode_before_answer", &f.bot_id, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        match err {
            LcError::Conflict(body) => assert_eq!(body["reason"], "pane_unreadable", "{body}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "unknown screen means no Auto answer");
    }
    #[tokio::test]
    async fn codex_model_migration_is_blocked_without_choosing_or_typing() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-model-migration.txt"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "dialog_open", "{body}");
                assert!(body["message"].as_str().unwrap().contains("不替你選"), "{body}");
            }
            other => panic!("{other:?}"),
        }
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "blocked", "herdr 判 idle 時也要顯示 blocked");
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    /// 啟動時的更新選單：預設選的是 `› 1. Update now`，畫面底下是 `Press enter to continue`。daemon 把 prompt 打進去、
    /// 最後那個 Enter 就等於替使用者按下「現在更新」（在 pane 裡跑 `npm install -g`），而且 prompt 本身被選單吃掉
    /// （2026-10-02 實際發生：Update available 吃掉 prompt）。要像其他 codex 對話框一樣擋下、不打字不按鍵、告訴使用者。
    #[tokio::test]
    async fn codex_update_menu_is_never_answered_by_typing_a_prompt_into_it() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-update-menu.txt"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "dialog_open", "{body}");
                let msg = body["message"].as_str().unwrap();
                assert!(msg.contains("更新") && msg.contains("Skip"), "要講清楚是更新選單、選 Skip：{msg}");
            }
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "不按任何鍵（Enter 會選到 Update now）");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty(), "不打字進選單");
        let status: String = sqlx::query_scalar("SELECT agent_status FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(status, "blocked", "herdr 判 idle 時也要顯示 blocked，網頁才看得到要去處理");
    }
    #[tokio::test]
    async fn a_scrolled_codex_update_menu_is_never_answered_by_a_prompt() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let menu = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-update-menu.txt");
        let partial = menu.lines().skip_while(|line| !line.trim_start().starts_with("› 1. Update now")).collect::<Vec<_>>().join("\n");
        f.env.herdr.set_screen("pane-prompt-test", &partial);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => assert_eq!(body["reason"], "dialog_open", "{body}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "更新選單上絕不按鍵");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty(), "更新選單上絕不送 prompt");
    }
    /// 非互動的更新方框（底下就是正常的輸入列）與對話裡引用選單原文，都不是擋路的對話框。
    #[tokio::test]
    async fn codex_update_banner_or_a_quoted_menu_does_not_block_delivery() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        let boxed = "╭──────╮\n│ >_ OpenAI Codex (v0.154.0) │\n╰──────╯\n\n  ✨ Update available! 0.154.0 -> 0.155.1\n  Run sh -c 'curl x/install.sh | sh' to update.\n\n› Explain this codebase\n";
        let quoted = format!("• 上次畫面長這樣：\n{}\n\n› ", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-update-menu.txt").lines().map(|l| format!("  > {l}")).collect::<Vec<_>>().join("\n"));
        for screen in [boxed.to_string(), quoted] {
            f.env.herdr.set_screen("pane-prompt-test", &screen);
            pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_or_else(|e| panic!("不該擋：{e:?}\n{screen}"));
        }
    }
    #[tokio::test]
    async fn codex_rate_limit_suggestion_waits_for_a_user_choice() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-rate-limit-switch.txt"));
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(body) => {
                assert_eq!(body["reason"], "dialog_open", "{body}");
                assert!(body["message"].as_str().unwrap().contains("切換模型"), "{body}");
            }
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    #[tokio::test]
    async fn a_closed_auto_mode_offer_does_not_get_a_stale_down_or_enter() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::AUTO_MODE);
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("claude_auto_mode_before_answer", &f.bot_id, move || async move {
            screens.lock().unwrap().insert("pane-prompt-test".into(), "Claude Code\n❯\n".into());
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap();
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "畫面已變，不能送 Down 或 Enter");
    }
    /// Trust setup can take time (especially on a remote host); if the prompt vanishes during it,
    /// the stale `y` must not hit the next screen.
    #[tokio::test]
    async fn a_grok_trust_prompt_that_closes_before_the_answer_gets_no_y() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("grok_trust_before_answer", &f.bot_id, move || async move {
            screens.lock().unwrap().insert("pane-prompt-test".into(), "Grok Build 1.0.34\n❯\n".into());
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap();
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "畫面已變，不能送 y");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    /// The screen can change from a trust dialog to printed lookalike text while host trust is prepared.
    /// A second read without the live `y` / `n` controls must never authorize the key.
    #[tokio::test]
    async fn a_grok_trust_lookalike_on_the_second_read_gets_no_y() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home-lookalike");
        std::fs::create_dir_all(&grok_home).unwrap();
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("grok_trust_before_answer", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                "⏺ Printed example:\nDo you trust the contents of this directory?\nYes, proceed\nNo, quit\nGrok Build 1.0.46\n".into(),
            );
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let _ = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await;
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "printed text is not live confirmation UI");
    }
    #[tokio::test]
    async fn an_unreadable_screen_after_grok_trust_answer_stops_prompt_delivery() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home-after-y");
        std::fs::create_dir_all(&grok_home).unwrap();
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let fail = f.env.herdr.fail_later();
        crate::lifecycle::race_point::arm("grok_trust_after_answer", &f.bot_id, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        match err {
            LcError::Conflict(body) => assert_eq!(body["reason"], "pane_unreadable", "{body}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(
            f.env.herdr.calls_to("pane.send_keys").iter().map(|p| p["keys"].clone()).collect::<Vec<_>>(),
            vec![json!(["y"])],
            "after the trust answer the unknown screen blocks further prompt effects"
        );
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    /// #243 重開：信任框要寫到**哪台**的信任紀錄，取決於 bot 在哪台主機。讀不到主機不能當成本機——
    /// 遠端 grok 會被寫進 daemon 這台的 `~/.grok`，再替它按 `y`。讀不到就一個鍵都不按、不寫信任，
    /// 回 409 讓排隊的 prompt 重試；讀得回來之後同一個 prompt 照常寫信任、按 `y`。
    #[tokio::test]
    async fn an_unreadable_host_leaves_the_grok_trust_dialog_alone_until_it_is_readable() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        let trust_file = grok_home.join("trusted_folders.toml");
        let cwd = f.env.repo.to_string_lossy().into_owned();
        sqlx::query("UPDATE bots SET cwd=?, env_json=? WHERE id=?")
            .bind(&cwd)
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        // pane 的 client 已經照 run 拿到了，之後才讀不到主機——正是現場的窗口。
        let app2 = app.clone();
        crate::lifecycle::race_point::arm("grok_trust_before_host", &f.bot_id, move || async move {
            crate::testing::make_table_unreadable(&app2, "projects").await;
        });
        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        crate::testing::make_table_readable(&app, "projects").await;
        match err {
            LcError::Conflict(v) => assert_eq!(v["reason"], "dialog_open", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "主機讀不到：不能按 y");
        assert!(!trust_file.exists(), "主機讀不到：不能把信任寫到 daemon 這台");

        // 讀得回來：同一個 prompt 再試一次就照常信任並按 y（mock 畫面不會消失，所以仍是 409）。
        assert!(pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.is_err());
        let keys: Vec<serde_json::Value> = f.env.herdr.calls_to("pane.send_keys").iter().filter_map(|p| p.get("keys").cloned()).collect();
        assert_eq!(keys, vec![json!(["y"])]);
        assert!(trust_file.exists(), "讀得到之後才寫信任");
    }
    /// 同一條的 cwd 那一半：bot 沒有自己的 cwd 時信任的是專案目錄。專案**讀不到**不等於「沒有目錄」——
    /// 後者只能按 y，前者要等讀得到再來（不然信任沒寫、y 卻按了）。用 BLOB 塞進 `path` 讓只有這一列解不開，
    /// 主機（同一張表的 `host`）照樣讀得到。
    #[tokio::test]
    async fn an_unreadable_project_dir_leaves_the_grok_trust_dialog_alone() {
        let f = fixture("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let grok_home = f.env.dir.join("grok-home");
        std::fs::create_dir_all(&grok_home).unwrap();
        let trust_file = grok_home.join("trusted_folders.toml");
        sqlx::query("UPDATE bots SET cwd=NULL, env_json=? WHERE id=?")
            .bind(json!({"GROK_HOME": grok_home.to_string_lossy()}).to_string())
            .bind(&f.bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        let path: String = sqlx::query_scalar("SELECT path FROM projects WHERE id=?").bind(&f.env.project_id).fetch_one(&app.db).await.unwrap();
        sqlx::query("UPDATE projects SET path=X'FF00' WHERE id=?").bind(&f.env.project_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Do you trust the contents of this directory?\n  Yes, proceed                 y\n  No, quit                      n\nGrok Build 1.0.34\n");
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        assert!(db::project(&app.db, &f.env.project_id).await.is_err(), "前提：專案那一列解不開");

        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(v) => assert_eq!(v["reason"], "dialog_open", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty(), "目錄讀不到：不能按 y");

        sqlx::query("UPDATE projects SET path=? WHERE id=?").bind(&path).bind(&f.env.project_id).execute(&app.db).await.unwrap();
        assert!(pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.is_err());
        let keys: Vec<serde_json::Value> = f.env.herdr.calls_to("pane.send_keys").iter().filter_map(|p| p.get("keys").cloned()).collect();
        assert_eq!(keys, vec![json!(["y"])]);
        assert!(trust_file.exists(), "讀得到之後信任的是專案目錄");
    }
    /// pt-hub mg1 的死結（2026-11-18）：child 的 Dangerous rm 框原文被當通知打進母 bot 的輸入框、沒送出（composer_unreadable）；
    /// 輸入框裡那段字不能讓母 bot 被當成「停在 Dangerous rm 框」——否則通知的重試與所有送達都被 409 擋死。
    /// 輸入框裡裝著通知（不論有沒有前綴）時，送達閘門不回 dangerous_rm_pending／dialog_open；真的框照舊擋（上一個測試）。
    #[tokio::test]
    async fn a_dialog_quoted_in_the_composer_does_not_block_delivery() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        let rule = "─".repeat(96);
        let notice = crate::child_alerts::message_for("otafable", crate::tui_prompts::screens::DANGEROUS_RM);
        for typed in [notice.as_str(), crate::tui_prompts::screens::DANGEROUS_RM] {
            let mut lines = typed.lines();
            let first = lines.next().unwrap_or("");
            let rest: String = lines.map(|l| format!("  {l}\n")).collect();
            let screen = format!("● earlier\n\n{rule}\n❯ {first}\n{rest}{rule}\n  status\n  ⏵⏵ bypass permissions on\n");
            f.env.herdr.set_screen("pane-prompt-test", &screen);
            if let Err(LcError::Conflict(v)) = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await {
                let reason = v["reason"].as_str().unwrap_or_default();
                assert!(!matches!(reason, "dangerous_rm_pending" | "dialog_open"), "輸入框裡的引文不是框：{v}");
            }
        }
    }
    /// 2.1.281 的防誤刪框（herdr 判成 idle 也一樣）：409 `dangerous_rm_pending` 帶目標，一個鍵都不按、一個字都不打；
    /// 通知寫在對話裡，同一個框被擋幾次都只寫一次（排隊的 flush 會一再撞這道閘門）。
    #[tokio::test]
    async fn a_dangerous_rm_prompt_is_never_typed_into_or_answered() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::DANGEROUS_RM);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        for _ in 0..2 {
            match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
                LcError::Conflict(v) => {
                    assert_eq!(v["reason"], "dangerous_rm_pending", "{v}");
                    assert_eq!(v["target"], "command substitution output", "{v}");
                }
                other => panic!("{other:?}"),
            }
        }
        for m in ["pane.send_keys", "pane.send_text", "agent.prompt"] {
            assert!(f.env.herdr.calls_to(m).is_empty(), "{m}");
        }
        let notices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system' AND content LIKE '%防誤刪確認框等你本人核准%'")
            .bind(&f.conv)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(notices, 1);
    }
    /// onboarding 的主題頁跟登入選單同一類：交給人（needs_login），一個鍵都不按。
    #[tokio::test]
    async fn the_onboarding_theme_page_is_needs_login_and_nothing_is_pressed() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::ONBOARDING_THEME);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();
        match pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err() {
            LcError::Conflict(v) => assert_eq!(v["reason"], "needs_login", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
    }
    #[tokio::test]
    async fn an_unreadable_pane_guard_does_not_authorize_a_prompt() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", crate::tui_prompts::screens::DANGEROUS_RM);
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        let bot = db::bot(&app.db, &f.bot_id).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &f.bot_id).await.unwrap().unwrap();

        let err = pane_ready_for_prompt(&app, &bot, &run, &f.conv).await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "unreadable guard must be retryable: {err:?}");
        assert!(f.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    #[tokio::test]
    async fn agent_prompt_pane_guard_rechecks_after_preflight() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "Claude Code\n❯\n");
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n".into(),
            );
        });

        let err = prompt(&app, &f.bot_id, "Reply with PONG please", "prompt-dialog-race").await.unwrap_err();
        assert!(matches!(err, LcError::Conflict(_)), "dialog race must be retryable: {err:?}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "an unsent direct turn is withdrawn");
        assert!(f.env.herdr.calls_to("agent.prompt").is_empty(), "the final fence must precede agent.prompt");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    /// #782：agent.prompt 那條路也會把字打進 pane。帳號安全提醒橫幅在前置檢查之後才出現：最後一道圍籬擋下、可重試，
    /// 一個鍵都不按（不按 Esc），對話留一則通知。
    #[tokio::test]
    async fn agent_prompt_is_fenced_by_a_codex_security_banner() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET pane_id='pane-prompt-test' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "• PONG\n\n› \n\n  gpt-6.1-sol default · /tmp/x\n");
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.159.3-security-setup-banner.txt").into(),
            );
        });

        let result = prompt(&app, &f.bot_id, "1. 先看 issue 782", "codex-banner-fence").await;
        let Err(LcError::Conflict(body)) = result else { panic!("banner must be a retryable 409: {result:?}") };
        assert_eq!(body["reason"], "codex_security_banner", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(f.env.herdr.calls_to("agent.prompt").is_empty());
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty() && f.env.herdr.calls_to("pane.send_keys").is_empty());
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_all(&app.db)
            .await
            .unwrap();
        assert!(notes.iter().any(|n| n.contains("安全提醒橫幅")), "{notes:?}");
    }
    #[tokio::test]
    async fn agent_prompt_is_fenced_by_a_codex_queued_followup() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET resume_session_id='session-2170' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "• PONG\n\n› \n\n  GPT-6.2-Sol default · /tmp/x\n");
        let queued_screen = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.171.0-queued-follow-up.txt")
            .replace("\\x1b", "\u{1b}");
        assert_eq!(crate::composer_parse::box_state("codex", &queued_screen), crate::composer_parse::BoxState::Empty);
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            screens.lock().unwrap().insert("pane-prompt-test".into(), queued_screen);
        });

        let result = prompt(&app, &f.bot_id, "Reply with PONG please", "codex-queued-followup-fence").await;
        let Err(LcError::Conflict(body)) = result else { panic!("queued follow-up must block direct delivery: {result:?}") };
        assert_eq!(body["reason"], "composer_busy", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(f.env.herdr.calls_to("agent.prompt").is_empty(), "do not resend into a queued follow-up");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    #[tokio::test]
    async fn agent_prompt_is_fenced_by_a_nonempty_codex_composer() {
        let f = fixture("codex", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET resume_session_id='session-2170' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();
        f.env.herdr.set_screen("pane-prompt-test", "• PONG\n\n› \n\n  GPT-6.2-Sol default · /tmp/x\n");
        let screens = f.env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            screens.lock().unwrap().insert(
                "pane-prompt-test".into(),
                "• PONG\n\n› UNSENT-DRAFT-2170\n\n  GPT-6.2-Sol default · /tmp/x\n".into(),
            );
        });

        let result = prompt(&app, &f.bot_id, "Reply with PONG please", "codex-draft-fence").await;
        let Err(LcError::Conflict(body)) = result else { panic!("nonempty composer must block direct delivery: {result:?}") };
        assert_eq!(body["reason"], "composer_busy", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(f.env.herdr.calls_to("agent.prompt").is_empty(), "do not append to a restored draft");
        assert!(f.env.herdr.calls_to("pane.send_text").is_empty());
    }
    #[tokio::test]
    async fn agent_prompt_is_refused_when_host_generation_changes_after_client_capture() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let conn = app.hosts.get(crate::config::LOCAL_HOST).await.unwrap();
        let calls = f.env.herdr.calls.clone();
        let guard_reads_before = Arc::new(std::sync::Mutex::new(None));
        let reads_before = guard_reads_before.clone();
        crate::lifecycle::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            let counts = {
                let calls = calls.lock().unwrap();
                (
                    calls
                        .iter()
                        .filter(|(method, _)| method == "agent.get")
                        .count(),
                    calls
                        .iter()
                        .filter(|(method, _)| method == "pane.read")
                        .count(),
                )
            };
            *reads_before.lock().unwrap() = Some(counts);
            conn.bump_generation_for_test();
        });

        let result = prompt(
            &app,
            &f.bot_id,
            "這句不能送到失效世代",
            "stale-host-generation",
        )
        .await;
        let Err(LcError::Conflict(body)) = result else {
            panic!("a prompt bound to a superseded host generation must be retryable: {result:?}");
        };
        assert_eq!(body["reason"], "host_changed", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(
            turn_count(&app, &f.conv).await,
            0,
            "an unsent direct turn is withdrawn"
        );
        assert!(
            f.env.herdr.calls_to("agent.prompt").is_empty(),
            "the stale client must not receive the prompt"
        );
        let (gets_before, reads_before) = guard_reads_before
            .lock()
            .unwrap()
            .expect("race point captured baseline");
        let calls = f.env.herdr.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "agent.get")
                .count(),
            gets_before,
            "a stale fence blocks final-guard RPCs"
        );
        assert_eq!(
            calls
                .iter()
                .filter(|(method, _)| method == "pane.read")
                .count(),
            reads_before,
            "a stale fence blocks final-guard RPCs"
        );
    }
    /// A same-name remote repoint must leave the captured RPC bound to A, but reject it before
    /// either A or B sees a request. Once reconcile publishes a run on the new authority, retry works.
    #[tokio::test]
    async fn a_repointed_remote_host_receives_no_prompt_from_the_superseded_generation() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let host = format!("prompt-repoint-{}", db::ulid());
        let host_cfg = |ssh: &str| crate::config::HostCfg { name: host.clone(), ssh: ssh.into(), herdr_session: "test".into(), ..Default::default() };
        let socket_a = f.env.dir.join("herdr-a.sock");
        let socket_b = f.env.dir.join("herdr-b.sock");
        let herdr_a = tt::MockHerdr::start(socket_a.clone());
        let herdr_b = tt::MockHerdr::start(socket_b.clone());
        for herdr in [&herdr_a, &herdr_b] {
            herdr.set_agent("prompt-test", "pane-prompt-test", true);
            herdr.set_screen("pane-prompt-test", "Claude Code\n❯\n");
        }
        let client_b = crate::herdr::HerdrClient::new(socket_b);
        app.hosts
            .insert_remote_with_client_for_test(
                host_cfg("target-a"),
                crate::herdr::HerdrClient::new(socket_a),
            )
            .await;
        sqlx::query("UPDATE projects SET host=? WHERE id=?")
            .bind(&host)
            .bind(&f.env.project_id)
            .execute(&app.db)
            .await
            .unwrap();

        let calls_at_repoint = Arc::new(std::sync::Mutex::new(None));
        let captured_calls = calls_at_repoint.clone();
        let app_to_repoint = app.clone();
        let replacement = host_cfg("target-b");
        let run_id = f.run_id.clone();
        let a_calls = herdr_a.calls.clone();
        let b_calls = herdr_b.calls.clone();
        crate::lifecycle::race_point::arm("agent_prompt_before_rpc", &f.bot_id, move || async move {
            assert!(
                db::in_flight_turn(&app_to_repoint.db, &run_id).await.unwrap().is_some(),
                "the direct turn already exists"
            );
            *captured_calls.lock().unwrap() = Some((a_calls.lock().unwrap().len(), b_calls.lock().unwrap().len()));
            app_to_repoint
                .hosts
                .replace_remote_with_client_for_test(&app_to_repoint, replacement, client_b)
                .await;
        });

        let result = prompt(&app, &f.bot_id, "同名主機改指後不能沿用舊 client", "remote-host-repoint").await;
        let Err(LcError::Conflict(body)) = result else {
            panic!("a repointed host generation must refuse delivery: {result:?}");
        };
        assert_eq!(body["reason"], "host_changed", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(turn_count(&app, &f.conv).await, 0, "the unsent direct turn is retracted");
        let (a_before, b_before) = calls_at_repoint
            .lock()
            .unwrap()
            .expect("captured the A/B baseline");
        assert_eq!(herdr_a.methods().len(), a_before, "the superseded operation sends no more RPCs to A");
        assert_eq!(herdr_b.methods().len(), b_before, "the superseded operation never switches over to B");
        assert!(herdr_a.calls_to("agent.prompt").is_empty());
        assert!(herdr_b.calls_to("agent.prompt").is_empty());

        // Model reconcile publishing a fresh active run against the now-authoritative B connection.
        assert_eq!(mark_run_exited(&app, &f.run_id, "host repointed").await, RunExit::Recorded);
        let new_run = db::ulid();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, started_at) VALUES (?, ?, 'running', 'idle', 'pane-prompt-test', 'test', ?)")
            .bind(&new_run)
            .bind(&f.bot_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let retry = prompt(&app, &f.bot_id, "同名主機改指後不能沿用舊 client", "remote-host-repoint").await.unwrap();
        // The test mock deliberately does not implement agent.prompt, so a received call is
        // recorded as unknown. What matters here is that the explicit retry reaches only B.
        assert_eq!(retry.delivery, "unknown", "the retry reaches B after the new run exists");
        assert!(herdr_a.calls_to("agent.prompt").is_empty(), "the old physical host remains untouched");
        assert_eq!(herdr_b.calls_to("agent.prompt").len(), 1, "only the newly authoritative host gets the retry");
    }
    #[tokio::test]
    async fn agent_prompt_rechecks_host_generation_after_final_guard() {
        let f = fixture("claude", "test").await;
        let app = f.env.app.clone();
        let conn = app.hosts.get(crate::config::LOCAL_HOST).await.unwrap();
        crate::lifecycle::race_point::arm("agent_prompt_after_guard", &f.bot_id, move || async move {
            conn.bump_generation_for_test();
        });

        let result = prompt(
            &app,
            &f.bot_id,
            "這句不能送到失效世代",
            "stale-host-after-guard",
        )
        .await;
        let Err(LcError::Conflict(body)) = result else {
            panic!("a generation superseded during the final reads must be retryable: {result:?}");
        };
        assert_eq!(body["reason"], "host_changed", "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(
            turn_count(&app, &f.conv).await,
            0,
            "an unsent direct turn is withdrawn"
        );
        assert!(
            f.env.herdr.calls_to("agent.prompt").is_empty(),
            "the stale client must not receive the prompt"
        );
    }
}

mod send_now_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::prompt::rearm_queued_prompt_restamps;
    use crate::lifecycle::prompt::test_seams::*;
    // parent-uses
    use crate::lifecycle::*;
    use crate::state::App;
    use crate::lifecycle::send_now::ports::AttachConnPort;
    use crate::lc_error::PromptOut;
    use crate::lc_error::QueueClass;
    // 插隊送出（issue #103）：daemon 這一側的記帳。端到端（pane 真的跑 2.1.275、CLI 真的收到）要等
    // §6.9 批次升級之後才能驗，這裡釘的是「哪些 run 可以插隊」與「被打斷的回合怎麼收」。
    use crate::lifecycle::prompt::*;
    use crate::testing as tt;
    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
    }
    /// 一顆會回話的 claude pane：`status_json` 帶 statusLine 回報的版本，transcript 檔就是送達證據。
    async fn fixture(kind: &str, version: Option<&str>) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'send-now-test',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        let transcript = if kind == "claude" {
            // Claude transcript evidence is trusted only inside this bot's configured project root.
            let config_dir = env.dir.join("claude-config");
            sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
                .bind(serde_json::json!({ "CLAUDE_CONFIG_DIR": config_dir.to_string_lossy() }).to_string())
                .bind(&bot_id)
                .execute(&app.db)
                .await
                .unwrap();
            let transcript_dir = config_dir.join("projects/-t");
            std::fs::create_dir_all(&transcript_dir).unwrap();
            transcript_dir.join(format!("session-{}.jsonl", db::ulid()))
        } else {
            env.dir.join(format!("session-{}.jsonl", db::ulid()))
        };
        std::fs::write(&transcript, "").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, native_session_id, transcript_path, status_json, started_at)
             VALUES (?,?,'running','working','pane-sn','test','sess-1',?,?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(transcript.to_str().unwrap())
        .bind(version.map(|v| format!(r#"{{"version":"{v} (Claude Code)","model_name":"Opus"}}"#)))
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.live_pane(
            "pane-sn",
            tt::LivePane { width: Some(120), transcript_file: Some(transcript), ..Default::default() },
        );
        Fixture { env, bot_id, conv, run_id }
    }
    /// 佔住一個回合，回傳那筆 turn 的 id。
    async fn busy(f: &Fixture) -> String {
        let id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at)
             VALUES (?,?,?,'web','in_flight','ok',?)",
        )
        .bind(&id)
        .bind(&f.conv)
        .bind(&f.run_id)
        .bind(db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
        id
    }
    async fn status_of(f: &Fixture, turn_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT status FROM turns WHERE id=?")
            .bind(turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap()
    }
    async fn in_flight_count(f: &Fixture) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=? AND status='in_flight'")
            .bind(&f.conv)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap()
    }
    fn keys_sent(f: &Fixture) -> Vec<String> {
        f.env
            .herdr
            .calls_to("pane.send_keys")
            .iter()
            .filter_map(|p| p.get("keys").and_then(|k| serde_json::to_string(k).ok()))
            .collect()
    }
    /// 驗收一：bot 正在跑時插隊送出——那句話進了對話，鍵真的按下去了，被打斷的回合收成 `failed`
    /// 並留下一則說明，不是卡在進行中。
    #[tokio::test]
    async fn a_send_now_closes_the_turn_it_interrupted_and_delivers_the_new_one() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let interrupted = busy(&f).await;

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-1", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"));
        assert_eq!(out.delivery, "ok", "打字進 pane 並由 transcript 證明送出");

        assert_eq!(status_of(&f, &interrupted).await, "failed", "被打斷的回合不能留在 in_flight");
        assert_eq!(status_of(&f, &out.turn_id).await, "in_flight", "新的那一筆才是在飛的");
        assert_eq!(in_flight_count(&f).await, 1);
        let note: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system' AND content=?")
            .bind(&interrupted)
            .bind(SEND_NOW_NOTE)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(note, 1, "對話裡看得到它是被插隊打斷的");
        assert_eq!(keys_sent(&f), vec![r#"["ctrl+x","ctrl+s"]"#.to_string()], "按的是 send-now 鍵，不是 Enter");
        assert_eq!(sent_via(&f, &out.message_id).await.as_deref(), Some("send_now"), "泡泡標「插隊」（使用者 2026-09-28）");
    }
    async fn sent_via(f: &Fixture, message_id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT sent_via FROM messages WHERE id=? AND role='user'").bind(message_id).fetch_one(&f.env.app.db).await.unwrap()
    }
    /// issue #748：一顆 codex 的 run。`version` 是巡邏從畫面讀到的跑著的版本，`flag` 是 `[codex] instant_interrupt`；
    /// 旗標開時當成這個 run 是旗標開著之後才起的（`runs.codex_instant_interrupt = 1`）。
    /// 沒有 native session：送達證據走一列回音，不去翻真的 ~/.codex。
    async fn codex_fixture(version: Option<&str>, flag: bool) -> Fixture {
        let f = fixture("codex", None).await;
        if flag {
            sqlx::query("UPDATE runs SET codex_instant_interrupt = 1 WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        }
        sqlx::query("UPDATE runs SET native_session_id = NULL, transcript_path = NULL WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        f.env.herdr.live_pane("pane-sn", tt::LivePane { width: Some(120), codex: true, ..Default::default() });
        if let Some(v) = version {
            crate::codex_update::remember_running(&f.run_id, &format!("│ >_ OpenAI Codex (v{v})   │"), None);
        }
        f.env.app.cfg.update(|c| { c.codex.instant_interrupt = flag; Ok(()) }).await.unwrap();
        f
    }
    async fn turn_count(f: &Fixture) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&f.env.app.db).await.unwrap()
    }
    fn typed(f: &Fixture) -> usize {
        f.env.herdr.calls_to("pane.send_text").len()
    }
    /// 旗標關（預設）：codex 的 send_now 跟以前一模一樣——409 說「只有 claude 有」，一個字都不打。
    #[tokio::test]
    async fn codex_send_now_is_refused_as_before_while_the_canary_is_off() {
        let f = codex_fixture(Some("0.159.0"), false).await;
        let running = busy(&f).await;
        let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-off", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["send_now_refused"], "send_now_unsupported_kind", "{body}");
        assert_eq!(typed(&f), 0, "沒插隊就一個字都不打");
        assert_eq!(status_of(&f, &running).await, "in_flight");
    }
    /// 旗標開，但 run 啟動時沒帶 instant_interrupt 參數（NULL＝旗標打開前就起的／adopt 來的、0＝起的時候旗標關著）：
    /// 409 `send_now_codex_restart_needed`、訊息講「重啟這顆 bot」，一個字都不打、回合照跑。
    #[tokio::test]
    async fn a_codex_run_launched_without_the_flag_is_refused_with_restart_needed() {
        for (stamp, crid) in [(None, "cx-null"), (Some(0), "cx-zero")] {
            let f = codex_fixture(Some("0.164.0"), true).await;
            sqlx::query("UPDATE runs SET codex_instant_interrupt = ? WHERE id = ?").bind(stamp).bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
            let running = busy(&f).await;
            let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", crid, &[], None).await.unwrap_err();
            let LcError::Conflict(body) = err else { panic!("{err:?}") };
            assert_eq!(body["send_now_refused"], "send_now_codex_restart_needed", "{stamp:?}: {body}");
            assert!(body.to_string().contains("重啟這顆 bot"), "{body}");
            assert_eq!(typed(&f), 0, "沒插隊就一個字都不打");
            assert_eq!(status_of(&f, &running).await, "in_flight");
        }
    }
    /// 旗標關（預設）時 run 記了什麼都不影響：就算啟動時帶了參數（1），send_now 仍是以前的 `send_now_unsupported_kind`。
    #[tokio::test]
    async fn the_launch_stamp_changes_nothing_while_the_canary_is_off() {
        let f = codex_fixture(Some("0.164.0"), false).await;
        sqlx::query("UPDATE runs SET codex_instant_interrupt = 1 WHERE id = ?").bind(&f.run_id).execute(&f.env.app.db).await.unwrap();
        busy(&f).await;
        let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-off-1", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["send_now_refused"], "send_now_unsupported_kind", "{body}");
        assert_eq!(typed(&f), 0);
    }
    /// 旗標開、版本夠：字打進忙碌的 pane，併進同一個回合——舊回合不收、不建新回合，泡泡掛在被 steer 的回合上，
    /// 回應是 `steered` 而不是 claude 式的 `interrupted`，按的是一般 Enter 不是 send-now 鍵。
    #[tokio::test]
    async fn a_codex_steer_joins_the_running_turn_without_interrupting_it() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        let running = busy(&f).await;
        let out = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-steer", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("steered"));
        assert_eq!(out.turn_id, running, "回的是被 steer 的那一回合");
        assert!(matches!(out.delivery.as_str(), "ok" | "unverified"), "{}", out.delivery);
        assert_eq!(status_of(&f, &running).await, "in_flight", "舊回合不收");
        assert_eq!(turn_count(&f).await, 1, "不建新回合");
        assert_eq!(in_flight_count(&f).await, 1);
        assert_eq!(sent_via(&f, &out.message_id).await.as_deref(), Some("supplement"));
        let msg_turn: Option<String> = sqlx::query_scalar("SELECT turn_id FROM messages WHERE id=?").bind(&out.message_id).fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(msg_turn.as_deref(), Some(running.as_str()));
        assert!(notes_on(&f, &running).await.is_empty(), "不留「被插隊打斷」的說明");
        assert_eq!(typed(&f), 1);
        assert_eq!(keys_sent(&f), vec![r#"["Enter"]"#.to_string()], "一般 Enter，沒有 send-now 鍵");
    }
    /// 同一個 request id 重送（逾時重試）：字已經在 pane 裡，不能再打一次，也不能多一則訊息。
    #[tokio::test]
    async fn retrying_a_codex_steer_does_not_type_it_twice() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        let first = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-retry", &[], None).await.unwrap();
        let again = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-retry", &[], None).await.unwrap();
        assert_eq!((again.message_id.as_str(), again.send_now), (first.message_id.as_str(), Some("steered")));
        assert_eq!(typed(&f), 1, "只打一次");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='user' AND sent_via='supplement'").fetch_one(&f.env.app.db).await.unwrap();
        assert_eq!(n, 1);
    }
    async fn supplement_count(f: &Fixture) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE role='user' AND sent_via='supplement'").fetch_one(&f.env.app.db).await.unwrap()
    }
    /// #916：記憶體帳沒了（daemon 重啟、被淘汰）之後同一個 request id 重送，以 `codex_steers` 為準：不再打字、回同一則訊息、不多補記。
    #[tokio::test]
    async fn a_steer_replayed_after_the_memo_is_lost_does_not_type_again() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        let first = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-lost", &[], None).await.unwrap();
        crate::lifecycle::codex_steer::forget_memo_for_test(&f.bot_id, "cx-lost");
        let again = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-lost", &[], None).await.unwrap();
        assert_eq!((again.turn_id.as_str(), again.message_id.as_str(), again.send_now), (first.turn_id.as_str(), first.message_id.as_str(), Some("steered")));
        assert_eq!(typed(&f), 1, "只打一次");
        assert_eq!(supplement_count(&f).await, 1);
        let row: (String, Option<String>) = sqlx::query_as("SELECT delivery, message_id FROM codex_steers WHERE bot_id=? AND client_request_id='cx-lost'")
            .bind(&f.bot_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
        assert_eq!((row.0.as_str(), row.1.as_deref()), (first.delivery.as_str(), Some(first.message_id.as_str())));
    }
    #[tokio::test]
    async fn a_steer_replayed_with_different_text_is_refused() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-diff", &[], None).await.unwrap();
        crate::lifecycle::codex_steer::forget_memo_for_test(&f.bot_id, "cx-diff");
        let err = prompt_send_now(&f.env.app, &f.bot_id, "完全不同的一句", "cx-diff", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "client_request_id was already used for a different prompt", "{body}");
        assert_eq!(typed(&f), 1, "沒有再打");
    }
    /// 字打進去了、訊息沒記成（記憶體帳也沒了）：重送只補記，不重打；帳上的 message_id 補上。
    #[tokio::test]
    async fn an_uncommitted_steer_is_only_recorded_on_replay() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        sqlx::query("CREATE TRIGGER no_user_msg2 BEFORE INSERT ON messages WHEN NEW.sent_via = 'supplement' BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&f.env.app.db).await.unwrap();
        let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-unc", &[], None).await.unwrap_err();
        assert!(matches!(err, LcError::Uncommitted(_)), "{err:?}");
        crate::lifecycle::codex_steer::forget_memo_for_test(&f.bot_id, "cx-unc");
        sqlx::query("DROP TRIGGER no_user_msg2").execute(&f.env.app.db).await.unwrap();
        let out = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-unc", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("steered"));
        assert_eq!(typed(&f), 1, "重送只補記，不重打");
        assert_eq!(supplement_count(&f).await, 1);
        let message_id: Option<String> = sqlx::query_scalar("SELECT message_id FROM codex_steers WHERE bot_id=? AND client_request_id='cx-unc'")
            .bind(&f.bot_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap();
        assert_eq!(message_id.as_deref(), Some(out.message_id.as_str()));
    }
    /// 字打進去了但訊息寫不進 DB：回 503（sent:true），重送只補記訊息，不重打。
    #[tokio::test]
    async fn a_codex_steer_whose_message_could_not_be_saved_is_not_typed_again_on_retry() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        sqlx::query("CREATE TRIGGER no_user_msg BEFORE INSERT ON messages WHEN NEW.sent_via = 'supplement' BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&f.env.app.db).await.unwrap();
        let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-db", &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("{err:?}") };
        assert_eq!((body["error"].as_str(), body["sent"].as_bool()), (Some("send_now_state_uncommitted"), Some(true)), "{body}");
        sqlx::query("DROP TRIGGER no_user_msg").execute(&f.env.app.db).await.unwrap();
        let out = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-db", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("steered"));
        assert!(!out.message_id.is_empty());
        assert_eq!(typed(&f), 1, "重送只補記，不重打");
    }
    /// 版本不夠或不知道：不賭，409 帶原因，一個字都不打。
    #[tokio::test]
    async fn a_codex_steer_needs_a_known_new_enough_version() {
        for (version, want) in [(Some("0.158.9"), "send_now_codex_too_old"), (None, "send_now_codex_version_unknown")] {
            let f = codex_fixture(version, true).await;
            busy(&f).await;
            let err = prompt_send_now(&f.env.app, &f.bot_id, "改方向", "cx-ver", &[], None).await.unwrap_err();
            let LcError::Conflict(body) = err else { panic!("{err:?}") };
            assert_eq!(body["send_now_refused"], want, "{body}");
            assert_eq!(typed(&f), 0);
        }
    }
    /// 普通 prompt／排隊不因這個功能變成插隊：旗標開著，沒帶 send_now 的 codex prompt 照舊 409。
    #[tokio::test]
    async fn a_plain_codex_prompt_still_waits_while_the_canary_is_on() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        busy(&f).await;
        let err = prompt_relayed(&f.env.app, &f.bot_id, "一般一句", "cx-plain", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("{err:?}") };
        assert_eq!(body["reason"], "a turn is already in flight", "{body}");
        assert_eq!(typed(&f), 0);
    }
    /// codex 閒著時按插隊＝一般送出（`idle`），建新回合，不走 steer。
    #[tokio::test]
    async fn a_codex_send_now_while_idle_is_a_plain_send() {
        let f = codex_fixture(Some("0.159.0"), true).await;
        let out = prompt_send_now(&f.env.app, &f.bot_id, "現在做這個", "cx-idle", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("idle"));
        assert_eq!(turn_count(&f).await, 1);
        assert_ne!(sent_via(&f, &out.message_id).await.as_deref(), Some("supplement"));
    }
    /// #120：插隊送出在**按任何鍵之前**就被擋下（這裡讓 `set_pane_typed` 寫不進去，`NotAttempted`）時，
    /// 什麼都沒打斷——正在跑的回合不能被收成「被插隊打斷」的 failed。以前計畫一成立就先收掉它，
    /// claude 其實還在跑，回覆之後變成一筆外部回合，重送還會退化成一般 Enter 打進忙碌的 pane。
    #[tokio::test]
    async fn a_send_now_refused_before_any_key_leaves_the_running_turn_alone() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query("CREATE TRIGGER no_pane_typed BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-x", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("按鍵前被擋是可重試的 409：{err:?}") };
        assert_eq!(body["sent"], false, "{body}");
        assert!(keys_sent(&f).is_empty(), "一個鍵都沒按");

        assert_eq!(status_of(&f, &running).await, "in_flight", "沒按鍵就沒有打斷：舊回合還在跑");
        let notes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='system'")
            .bind(&running)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(notes, 0, "也不留「被插隊打斷」的說明");
        assert_eq!(in_flight_count(&f).await, 1);
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "新的那一則沒有留下任何列，同一個 request id 可以原樣重送");
    }
    async fn transcript_of(f: &Fixture) -> String {
        let path: String =
            sqlx::query_scalar("SELECT transcript_path FROM runs WHERE id=?").bind(&f.run_id).fetch_one(&f.env.app.db).await.unwrap();
        std::fs::read_to_string(path).unwrap_or_default()
    }
    async fn delivery_of(f: &Fixture, turn_id: &str) -> String {
        sqlx::query_scalar::<_, String>("SELECT delivery FROM turns WHERE id=?").bind(turn_id).fetch_one(&f.env.app.db).await.unwrap()
    }
    async fn notes_on(f: &Fixture, turn_id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system' ORDER BY created_at")
            .bind(turn_id)
            .fetch_all(&f.env.app.db)
            .await
            .unwrap()
    }
    fn send_now_presses(f: &Fixture) -> usize {
        keys_sent(f).iter().filter(|k| k.contains("ctrl+s")).count()
    }
    /// #120 重開 regression 1：準備全過、第一個 `pane.send_text` 被 herdr **明確拒絕**（side effect 沒發生）。
    /// 一個字都沒進 pane：舊回合還在跑，不能被記成「被插隊打斷」；新的那一則撤掉，同一個 request id 可以重送。
    #[tokio::test]
    async fn a_send_now_whose_text_herdr_refused_leaves_the_running_turn_alone() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_text", tt::Fault::Refuse);

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-refused", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("herdr 拒收＝什麼都沒發生，是可重試的 409：{err:?}") };
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(body["retryable"], true, "{body}");
        assert!(keys_sent(&f).is_empty(), "一個鍵都沒按");

        assert_eq!(status_of(&f, &running).await, "in_flight", "字沒進去就沒有打斷：舊回合還在跑");
        assert!(notes_on(&f, &running).await.is_empty(), "不留「被插隊打斷」的說明");
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id=?").bind(&f.conv).fetch_one(&app.db).await.unwrap();
        assert_eq!(turns, 1, "新的那一則沒有留下任何列");
    }
    /// #647：長文字拆段，第 1 段已進框、第 2 段被拒。不能回「一個字都沒進去」的可重試 409。
    #[tokio::test]
    async fn a_send_now_whose_later_paste_chunk_was_refused_is_not_told_that_nothing_landed() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_after("pane.send_text", 1, tt::Fault::Refuse);
        let text = "x".repeat(1500);

        let out = prompt_send_now(&app, &f.bot_id, &text, "sn-partial", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(out.delivery, "failed", "{out:?}");
        assert_eq!(send_now_presses(&f), 0, "半段留在框裡就不按送出鍵");
        assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), 2, "第 1 段送出、第 2 段被拒");
        let box_rows = f.env.herdr.pane("pane-sn").unwrap().composer;
        assert_eq!(box_rows, vec!["x".repeat(1000)], "前 1000 字留在框裡：{box_rows:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight");
        assert_eq!(status_of(&f, &out.turn_id).await, "failed");
        assert!(notes_on(&f, &out.turn_id).await.iter().any(|n| n.contains("輸入框")));
    }
    /// 同一條，但這次 `pane.send_text` 的結果**不知道**（送出去之後連線斷了、沒回）。打字從來不會打斷 claude，
    /// 送出鍵也沒有按——舊回合照樣還在跑；新的那一則沒送出（字可能還留在框裡），說清楚、不按鍵。
    #[tokio::test]
    async fn a_send_now_whose_typing_went_unanswered_does_not_press_the_key_or_close_the_running_turn() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_text", tt::Fault::DropAfter);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-typing-unknown", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(out.delivery, "failed", "{out:?}");
        assert_eq!(send_now_presses(&f), 0, "不知道框裡是什麼就不按送出鍵");

        assert_eq!(status_of(&f, &running).await, "in_flight", "沒按送出鍵就沒有打斷");
        assert!(notes_on(&f, &running).await.is_empty());
        assert_eq!(status_of(&f, &out.turn_id).await, "failed");
        assert!(notes_on(&f, &out.turn_id).await.iter().any(|n| n.contains("輸入框")), "告訴人字可能還留在輸入框");
        assert_eq!(in_flight_count(&f).await, 1);
    }
    /// #120 重開 regression 2：準備做完之後、打第一個字之前，有人在終端裡打了字（不受 bot 鎖管）。
    /// 不能把兩段字接在一起送出——再看一次框、看到有字就放手。
    #[tokio::test]
    async fn someone_typing_after_the_send_now_was_prepared_never_gets_merged_into_it() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        let herdr = f.env.herdr.live.clone();
        crate::lifecycle::race_point::arm("prompt_before_typing", &f.bot_id, move || async move {
            herdr.lock().unwrap().get_mut("pane-sn").unwrap().composer.push("我自己的草稿".into());
        });

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-draft", &[], None).await.unwrap_err();
        let LcError::Conflict(body) = err else { panic!("框裡有別人的字：不送，可重試：{err:?}") };
        assert_eq!(body["reason"], "composer_busy", "{body}");
        assert_eq!(body["sent"], false, "{body}");
        assert_eq!(send_now_presses(&f), 0, "沒有按送出鍵");
        assert!(!transcript_of(&f).await.contains("先看這句"), "兩段字沒有被接在一起送出去：{}", transcript_of(&f).await);
        let box_rows = f.env.herdr.pane("pane-sn").unwrap().composer;
        assert_eq!(box_rows, vec!["我自己的草稿".to_string()], "別人的草稿原封不動，沒被接上我們的字");
        assert_eq!(status_of(&f, &running).await, "in_flight");
    }
    /// 送出鍵被 herdr **明確拒絕**：字在框裡、鍵沒按下去，claude 還在跑舊回合。不能記成被打斷。
    #[tokio::test]
    async fn a_send_now_key_that_herdr_refused_does_not_count_as_an_interruption() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::Refuse);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-refused", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(out.delivery, "failed", "{out:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight", "鍵沒按下去：舊回合照常");
        assert!(notes_on(&f, &running).await.is_empty());
        assert!(!transcript_of(&f).await.contains("先看這句"), "沒送出");
        assert_eq!(in_flight_count(&f).await, 1);
    }
    /// #158（同一類）：插隊送出的鍵被 herdr 拒收（沒生效），新的那一則收成 failed 那一句卻寫不進去——以前回 502，那一則停在
    /// run_id 為空的 in_flight＋pending（佔住維護窗口的送達臨界區、同一個 request id 重問拿不到結果）直到 daemon 重啟。
    /// 現在記成欠著、回 503；DB 好了同一個 request id 拿到 failed。正在跑的那一回合從頭到尾不動。
    #[tokio::test]
    async fn a_send_now_that_fell_through_and_could_not_be_closed_is_owed() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::Refuse);
        sqlx::query("CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.run_id IS NULL BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(&app.db)
            .await
            .unwrap();

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-fell-lost", &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("收不成，卻回了 {err:?}") };
        assert_eq!((body["error"].as_str(), body["delivery"].as_str(), &body["sent"]), (Some("delivery_state_uncommitted"), Some("failed"), &json!(false)), "{body}");
        let new_turn = body["turn_id"].as_str().unwrap().to_string();
        assert_eq!(status_of(&f, &new_turn).await, "in_flight", "寫不進去就是還沒收");

        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-fell-lost", &[], None).await.expect("DB 好了");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (new_turn.as_str(), "failed"));
        assert_eq!(status_of(&f, &new_turn).await, "failed");
        assert!(notes_on(&f, &new_turn).await.iter().any(|n| n.contains("插隊送出沒有送出")));
        assert_eq!(status_of(&f, &running).await, "in_flight", "鍵沒生效：舊回合照常");
        assert_eq!(in_flight_count(&f).await, 1);
    }
    /// #120 重開 regression 3：送出鍵**確定被 herdr 收下**之後，後面才失敗（這裡是 TUI 吃掉了那顆鍵、字一直留在框裡）。
    /// 第一個會打斷的 side effect 已經成立：舊回合照被插隊打斷收掉，新的那一則是「送出狀態不明」。
    #[tokio::test]
    async fn a_failure_after_the_send_now_key_was_accepted_still_closes_the_interrupted_turn() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.live.lock().unwrap().get_mut("pane-sn").unwrap().swallow_enter = true;

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-after-key", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "unknown", "{out:?}");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        assert_eq!(status_of(&f, &out.turn_id).await, "in_flight");
        assert_eq!(in_flight_count(&f).await, 1);
    }
    /// 送出鍵的 RPC 沒有回（herdr 其實按下去了）：看證據——transcript 裡有這一則，就是送出去了、舊回合被打斷。
    #[tokio::test]
    async fn an_unanswered_send_now_key_that_did_land_is_proven_by_the_transcript() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropAfter);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-landed", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "ok", "transcript 證明送出了：{out:?}");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        assert_eq!(send_now_presses(&f), 1, "證據已經說送出了，不再按");
    }
    /// 送出鍵的 RPC 沒有回、字還整個在框裡（鍵沒按下去）：再按一次，這次 herdr 收下了——照常插隊送出。
    #[tokio::test]
    async fn an_unanswered_send_now_key_that_did_not_land_is_pressed_once_more() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropBefore);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-again", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "ok", "{out:?}");
        assert_eq!(send_now_presses(&f), 2, "第一次沒進去，再按一次");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
    }
    /// 送出鍵的 RPC 沒有回、而且真的沒按下去（兩次都是）：字一直在框裡、transcript 沒有這一則——鍵沒生效，
    /// 舊回合照常。不假定打斷了。
    #[tokio::test]
    async fn an_unanswered_send_now_key_that_never_landed_leaves_the_running_turn_alone() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropBefore);
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropBefore);

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-key-lost", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("not_sent"), "{out:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight", "證據說沒送出：舊回合照常");
        assert!(notes_on(&f, &running).await.is_empty());
        assert_eq!(status_of(&f, &out.turn_id).await, "failed");
        assert!(!transcript_of(&f).await.contains("先看這句"));
    }
    /// 送出鍵的結果**真的不知道**：RPC 沒回、之後連 pane 都讀不到。不能假定打斷了——舊回合留在 in_flight，
    /// 新的那一則標成「結果不明」。之後的證據（transcript 裡確實有這一則）到的時候，hook 先把「被插隊打斷」補上，
    /// 回覆才不會掛到舊回合上。
    #[tokio::test]
    async fn an_unknown_send_now_is_settled_by_the_evidence_that_arrives_later() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        f.env.herdr.fail_next("pane.send_keys", tt::Fault::DropAfter);
        let transcript = f.env.herdr.pane("pane-sn").and_then(|p| p.transcript_file).unwrap();
        let hidden = transcript.with_extension("hidden");
        let (herdr_live, herdr_screens) = (f.env.herdr.live.clone(), f.env.herdr.screens.clone());
        let (from, to) = (transcript.clone(), hidden.clone());
        crate::lifecycle::race_point::arm("send_now_after_key", &f.bot_id, move || async move {
            // 鍵送出去之後什麼都看不到了：pane 讀不到、transcript 也暫時讀不到。
            herdr_live.lock().unwrap().remove("pane-sn");
            herdr_screens.lock().unwrap().insert("pane-sn".into(), "__READ_ERROR__".into());
            std::fs::rename(&from, &to).unwrap();
        });

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-unknown", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("unknown"), "{out:?}");
        assert_eq!(status_of(&f, &running).await, "in_flight", "結果不明＝不假定打斷");
        assert!(notes_on(&f, &running).await.is_empty());
        assert_eq!(status_of(&f, &out.turn_id).await, "failed", "新的那一則不佔 in_flight");
        assert_eq!(delivery_of(&f, &out.turn_id).await, "unknown");

        // transcript 又讀得到了，而新那一則的 user entry 確實在裡面；它的 Stop 到了：先補「被插隊打斷」，
        // 回覆才不會接到舊回合上。
        std::fs::rename(&hidden, &transcript).unwrap();
        let stop = crate::hookrecv::HookBody {
            bot_id: f.bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "sess-1", "prompt_id": "p-new",
                            "last_assistant_message": "看到了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();
        assert_eq!(status_of(&f, &running).await, "failed", "證據到了：舊回合確實被插隊打斷");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        let replies_on_old: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(&running)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(replies_on_old, 0, "新那一則的回覆不掛到被打斷的回合上");
        let answered: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM messages m JOIN turns t ON t.id = m.turn_id
              WHERE t.conversation_id=? AND m.role='assistant' AND m.content='看到了'",
        )
        .bind(&f.conv)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!(answered, 1, "回覆有地方放");
    }
    /// 送出鍵確定生效，但 DB 那一半（收舊回合、把新的那一則掛上 run）寫不進去：跟 #147 同一個模型——
    /// 不回普通成功，欠著的收尾之後補上，而且不再按鍵。
    #[tokio::test]
    async fn a_send_now_whose_bookkeeping_could_not_be_written_is_settled_later_without_pressing_again() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-owed", &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("不能是普通的成功或 502：{err:?}") };
        assert_eq!(body["error"], "send_now_state_uncommitted", "{body}");
        assert_eq!(body["sent"], true, "{body}");
        assert_eq!(body["interrupted_turn_id"], running.as_str(), "{body}");
        assert_eq!(send_now_presses(&f), 1);
        assert!(transcript_of(&f).await.contains("先看這句"), "鍵確實生效了");

        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        // 同一個 request id 重送：先補欠著的那一半，回的是那一則，不重打。
        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-owed", &[], None).await.unwrap();
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(notes_on(&f, &running).await, vec![SEND_NOW_NOTE.to_string()]);
        assert_eq!(status_of(&f, &again.turn_id).await, "in_flight");
        assert_eq!(in_flight_count(&f).await, 1);
        assert_eq!(send_now_presses(&f), 1, "補收尾不按鍵");
        let bound: Option<String> = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&again.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(f.run_id.as_str()), "新的那一則掛回 run 上");
    }
    /// 同一條，但 DB 只是一時寫不進去（送達證據等完時已經好了）：當場補上，回普通的成功，不必等重試。
    #[tokio::test]
    async fn a_send_now_whose_bookkeeping_failed_only_for_a_moment_is_settled_on_the_spot() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'database is locked'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let db = app.db.clone();
        crate::lifecycle::race_point::arm("send_now_owed", &f.bot_id, move || async move {
            sqlx::query("DROP TRIGGER lost_close").execute(&db).await.unwrap();
        });

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-owed-briefly", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("interrupted"), "{out:?}");
        assert_eq!(out.delivery, "ok", "{out:?}");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &out.turn_id).await, "in_flight");
        let bound: Option<String> = sqlx::query_scalar("SELECT run_id FROM turns WHERE id=?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(bound.as_deref(), Some(f.run_id.as_str()));
        assert_eq!(send_now_presses(&f), 1);
    }
    /// #149 重開的時序：插隊送出收舊回合那一半一時寫不進去（欠著），當場補上時交易 commit 了，緊接著寫新那一則的送達結果
    /// （送出的那一刻）又失敗——記成送達欠帳；回到 `prompt_inner` 再寫一次（此刻）時 DB 好了。舊帳要跟著清掉，送達時間
    /// 要是最早送出的那一刻；之後別的路寫進較新的證據，結清時不能被舊帳蓋回去。
    #[tokio::test]
    async fn a_send_now_whose_owed_delivery_write_lands_on_the_second_try_leaves_no_stale_debt() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let running = busy(&f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'database is locked'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let sent_by = Arc::new(std::sync::Mutex::new(String::new()));
        let (db, bot, seen) = (app.db.clone(), f.bot_id.clone(), sent_by.clone());
        crate::lifecycle::race_point::arm("send_now_owed", &f.bot_id, move || async move {
            // 收舊回合那一半好了，寫送達結果的那一句（只有它寫 `delivery_verified`）換成寫不進去。
            sqlx::query("DROP TRIGGER lost_close").execute(&db).await.unwrap();
            sqlx::query("CREATE TRIGGER lost_delivery BEFORE UPDATE OF delivery_verified ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
                .execute(&db)
                .await
                .unwrap();
            crate::lifecycle::race_point::arm("owed_delivery_recorded", &bot, move || async move {
                *seen.lock().unwrap() = db::now();
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                sqlx::query("DROP TRIGGER lost_delivery").execute(&db).await.unwrap();
            });
        });

        let out = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-stale-debt", &[], None).await.expect("第二次寫成了：普通的成功");
        assert_eq!((out.send_now, out.delivery.as_str()), (Some("interrupted"), "ok"));
        let sent_by = sent_by.lock().unwrap().clone();
        assert!(!sent_by.is_empty(), "送達結果的第一次寫入確實失敗、記成欠著");
        let at: Option<String> = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert!(at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "送達時間是送出的那一刻（{sent_by} 之前），拿到 {at:?}");

        sqlx::query("UPDATE turns SET delivery='unknown', delivery_verified=0 WHERE id=?").bind(&out.turn_id).execute(&app.db).await.unwrap();
        crate::lifecycle::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("沒有欠著的");
        assert_eq!(delivery_of(&f, &out.turn_id).await, "unknown", "舊帳不會把較新的結果蓋回去");
        assert_eq!(send_now_presses(&f), 1);
    }
    /// 驗收二：連續兩次插隊送出不會產生兩個 `in_flight`——第二次同樣先收掉第一次那筆。
    #[tokio::test]
    async fn two_send_nows_in_a_row_never_leave_two_in_flight_turns() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        busy(&f).await;

        let first = prompt_send_now(&app, &f.bot_id, "第一句", "sn-1", &[], None).await.unwrap();
        let second = prompt_send_now(&app, &f.bot_id, "第二句", "sn-2", &[], None).await.unwrap();

        assert_eq!(second.send_now, Some("interrupted"));
        assert_eq!(status_of(&f, &first.turn_id).await, "failed");
        assert_eq!(status_of(&f, &second.turn_id).await, "in_flight");
        assert_eq!(in_flight_count(&f).await, 1);
    }
    /// 驗收三：舊版 claude 與其他 kind 走原本的路——照樣 409、一個鍵都不按、在飛的那筆原封不動，
    /// 只是 body 多說一句為什麼沒插隊。
    #[tokio::test]
    async fn an_old_claude_or_another_kind_keeps_the_existing_behaviour() {
        for (kind, version, code) in [
            ("claude", Some("2.1.274"), "send_now_cli_too_old"),
            ("claude", None, "send_now_version_unknown"),
            ("codex", Some("2.1.275"), "send_now_unsupported_kind"),
            ("grok", Some("2.1.275"), "send_now_unsupported_kind"),
        ] {
            let f = fixture(kind, version).await;
            let app = f.env.app.clone();
            let interrupted = busy(&f).await;

            let err = prompt_send_now(&app, &f.bot_id, "插不進去", "sn-1", &[], None).await.unwrap_err();
            let LcError::Conflict(body) = err else { panic!("{kind}/{version:?} 應該照舊回 409") };
            assert_eq!(body["reason"], "a turn is already in flight", "{kind}/{version:?}");
            assert_eq!(body["send_now_refused"], code, "{kind}/{version:?}");
            assert!(body["send_now_message"].as_str().is_some_and(|m| !m.is_empty()), "{kind}/{version:?}");

            assert_eq!(status_of(&f, &interrupted).await, "in_flight", "{kind}/{version:?}：沒插隊就不准動人家的回合");
            assert_eq!(in_flight_count(&f).await, 1, "{kind}/{version:?}");
            assert!(keys_sent(&f).is_empty(), "{kind}/{version:?}：一個鍵都不該按");
        }
    }
    /// 閒著的 bot 帶 `send_now` 送出：沒有回合可以打斷，就照一般 Enter 送，不多綁一個版本前提。
    #[tokio::test]
    async fn a_send_now_with_nothing_in_flight_is_an_ordinary_send() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(&app.db).await.unwrap();

        let out = prompt_send_now(&app, &f.bot_id, "現在有空嗎", "sn-1", &[], None).await.unwrap();
        assert_eq!(out.send_now, Some("idle"));
        assert_eq!(in_flight_count(&f).await, 1);
        assert!(!keys_sent(&f).iter().any(|k| k.contains("ctrl+s")), "沒有回合可以打斷就不按那顆鍵");
        assert_eq!(sent_via(&f, &out.message_id).await, None, "什麼都沒打斷就不標插隊");
    }
    /// #157 的前半：送出鍵生效了，收舊回合那一半寫不進去（欠著），確認送出（`confirm_submitted`）又出錯——這裡讓它第一次
    /// 讀畫面就被 herdr 拒絕。回的是 `send_now_state_uncommitted`；帳上要記著新的那一則「鍵按過、證不出來」。
    async fn a_send_now_owed_while_its_confirmation_failed(f: &Fixture, crid: &str) -> (String, String, String) {
        let app = f.env.app.clone();
        let running = busy(f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let fail = f.env.herdr.fail_later();
        crate::lifecycle::race_point::arm("send_now_after_key", &f.bot_id, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        let err = prompt_send_now(&app, &f.bot_id, "先看這句", crid, &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("鍵生效、收尾沒寫成：{err:?}") };
        assert_eq!(body["error"], "send_now_state_uncommitted", "{body}");
        let sent_by = db::now();
        assert_eq!(send_now_presses(f), 1);
        assert!(transcript_of(f).await.contains("先看這句"), "鍵確實生效了");
        sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        (running, body["turn_id"].as_str().unwrap().to_string(), sent_by)
    }
    /// #157：確認送出出錯時，帳上以前記的是「沒有送達結果」——補收尾把新的那一則掛上 run 之後沒東西可寫，它停在
    /// in_flight＋pending 直到重啟，同一個 request id 重問只拿得到 503（#149）。要記成 `unknown`（鍵按過、證不出來），
    /// 送達時間是送出的那一刻；補的時候絕不再按鍵、不再打字。
    #[tokio::test]
    async fn a_send_now_whose_confirmation_failed_while_owed_is_settled_as_unknown_without_resending() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn, sent_by) = a_send_now_owed_while_its_confirmation_failed(&f, "sn-unconfirmed").await;

        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-unconfirmed", &[], None).await.expect("補上了：同一個 request id 拿到這一則");
        assert_eq!((again.turn_id.as_str(), again.delivery.as_str()), (new_turn.as_str(), "unknown"), "鍵按過、證不出來");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &new_turn).await, "in_flight");
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id=?").bind(&new_turn).fetch_one(&app.db).await.unwrap();
        assert!(t.delivered_at.as_deref().is_some_and(|at| at <= sent_by.as_str()), "送出的那一刻，不是補寫的時候：{:?}", t.delivered_at);
        assert_eq!(send_now_presses(&f), 1, "補的時候不再按鍵");
        assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), 1, "也不再打字");
        assert_eq!(transcript_of(&f).await.matches("先看這句").count(), 1, "只送了一次");
    }
    /// #157 同一個洞走 hook 那條：回覆到了，hook 先把欠著的收尾補上、新的那一則掛上 run，再認領它答完——
    /// 送達要跟著收成 `ok`（有回覆就是送到了），不是一筆已經完成、送達卻永遠 pending 的回合。
    #[tokio::test]
    async fn a_send_now_whose_confirmation_failed_while_owed_is_settled_by_the_reply_hook() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn, _) = a_send_now_owed_while_its_confirmation_failed(&f, "sn-unconfirmed-hook").await;

        let stop = crate::hookrecv::HookBody {
            bot_id: f.bot_id.clone(),
            provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "session_id": "sess-1", "prompt_id": "p-sn", "last_assistant_message": "看到了"}),
            received_at: None,
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &stop).await.unwrap();

        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &new_turn).await, "completed");
        assert_eq!(delivery_of(&f, &new_turn).await, "ok", "答完了：送達不留 pending");
        assert_eq!(send_now_presses(&f), 1);
        assert_eq!(f.env.herdr.calls_to("pane.send_text").len(), 1);
    }
    /// 送出鍵生效了（transcript 證明送到），收掉被插隊那一筆寫不進去：回 503、帳上欠著（新的那一則還沒掛上 run）。
    /// `healed`：回來之前 DB 就好了；否則收被插隊那一筆的寫入還壞著，由呼叫端之後自己拿掉。
    async fn an_owed_send_now(f: &Fixture, crid: &str, healed: bool) -> (String, String) {
        let app = f.env.app.clone();
        let running = busy(f).await;
        sqlx::query(&format!(
            "CREATE TRIGGER lost_close BEFORE UPDATE OF status ON turns WHEN OLD.id = '{running}' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"
        ))
        .execute(&app.db)
        .await
        .unwrap();
        let err = prompt_send_now(&app, &f.bot_id, "先看這句", crid, &[], None).await.unwrap_err();
        let LcError::Uncommitted(body) = err else { panic!("鍵生效、收尾沒寫成：{err:?}") };
        assert!(transcript_of(f).await.contains("先看這句"), "鍵確實生效了");
        if healed {
            sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        }
        (running, body["turn_id"].as_str().unwrap().to_string())
    }
    /// 插隊送出欠著收尾時 bot 被停掉：補收尾時新的那一則掛不上 run（run 已經不在），收成 failed——但送出鍵生效過、
    /// 送達結果也在帳上，不能丟掉只留 `pending`：503 叫呼叫端用同一個 request id 重問，拿到的要是送達結果（API 說不回 `pending`）。
    #[tokio::test]
    async fn a_send_now_owed_when_its_run_stops_keeps_its_delivery_result() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn) = an_owed_send_now(&f, "sn-stopped", true).await;
        stop_bot(&app, &f.bot_id).await.expect("停得掉");
        assert_eq!(status_of(&f, &running).await, "failed");

        let again = prompt_send_now(&app, &f.bot_id, "先看這句", "sn-stopped", &[], None).await.expect("補上了：同一個 request id 拿到這一則");
        assert_eq!(again.turn_id, new_turn);
        assert_eq!(status_of(&f, &new_turn).await, "failed", "run 已經不在：這一則接不上去");
        assert_eq!(again.delivery, "ok", "送出鍵生效過、有送達證據：不是 pending");
        assert_eq!(delivery_of(&f, &new_turn).await, "ok");
        assert_eq!(send_now_presses(&f), 1, "補的時候不再按鍵");
    }
    /// 插隊送出欠著收尾時 run 結束了（pane 關了）：run 結束那一路收的是被插隊的那一筆，帳上跟它綁在一起的新那一則不能跟著
    /// 被忘掉或蓋掉——不然它永遠是 `run_id` 為空的 in_flight＋pending：同一個 request id 重問永遠 503、維護窗口的送達臨界區
    /// 永遠有它，直到 daemon 重啟。run 結束的當下 DB 好了（收得掉）、還沒好（又記成欠著）各一條。
    async fn a_run_that_ends_while_a_send_now_is_owed(healed: bool) {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let crid = format!("sn-run-ended-{healed}");
        let (running, new_turn) = an_owed_send_now(&f, &crid, healed).await;
        mark_run_exited(&app, &f.run_id, "pane exited").await;
        if !healed {
            sqlx::query("DROP TRIGGER lost_close").execute(&app.db).await.unwrap();
        }

        let again = prompt_send_now(&app, &f.bot_id, "先看這句", &crid, &[], None)
            .await
            .unwrap_or_else(|e| panic!("healed={healed}：DB 好了，同一個 request id 要拿到這一則，拿到 {e:?}"));
        assert_eq!(again.turn_id, new_turn);
        assert_eq!(status_of(&f, &running).await, "failed", "healed={healed}");
        assert_eq!(status_of(&f, &new_turn).await, "failed", "healed={healed}：run 不在了，這一則接不上去，也不能一直在飛");
        assert_eq!(again.delivery, "ok", "healed={healed}");
        assert!(!crate::supervisor::store::delivery_critical_anywhere(&app.db).await.unwrap(), "healed={healed}：不再佔著送達臨界區");
        assert_eq!(send_now_presses(&f), 1, "healed={healed}");
    }
    #[tokio::test]
    async fn a_run_that_ends_while_a_send_now_is_owed_does_not_orphan_the_new_message() {
        a_run_that_ends_while_a_send_now_is_owed(true).await;
    }
    #[tokio::test]
    async fn a_run_that_ends_while_a_send_now_is_owed_and_the_db_is_still_down_does_not_orphan_the_new_message() {
        a_run_that_ends_while_a_send_now_is_owed(false).await;
    }
    /// 插隊送出欠著收尾（503）之後，使用者看 claude 還在忙、按 Esc（網頁不帶 turn_id）：補帳把新的那一則掛上 run，claude
    /// 正在處理的就是它——這一次 Esc 要打斷它，不是「上一次中斷的重試」：不能回 200 卻一個鍵都沒按。
    #[tokio::test]
    async fn an_esc_after_an_owed_send_now_interrupts_the_message_claude_is_working_on() {
        let f = fixture("claude", Some("2.1.275")).await;
        let app = f.env.app.clone();
        let (running, new_turn) = an_owed_send_now(&f, "sn-then-esc", true).await;

        interrupt_turn(&app, &f.bot_id, None).await.expect("Esc 進去了");
        let escs = f
            .env
            .herdr
            .calls_to("agent.send_keys")
            .iter()
            .filter(|p| p.get("keys").and_then(|k| k.as_array()).is_some_and(|k| k.iter().any(|k| k == "esc")))
            .count();
        assert_eq!(escs, 1, "使用者要停下 claude 正在做的：Esc 要按下去");
        assert_eq!(status_of(&f, &running).await, "failed");
        assert_eq!(status_of(&f, &new_turn).await, "failed", "被這次 Esc 打斷的是新的那一則");
        assert_eq!(notes_on(&f, &new_turn).await, vec![crate::lifecycle::interruption::INTERRUPT_NOTE.to_string()]);
    }
}

mod restamp_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::prompt::rearm_queued_prompt_restamps;
    use crate::lifecycle::prompt::test_seams::*;
    // parent-uses
    use crate::lifecycle::*;
    use crate::state::App;
    use crate::lifecycle::send_now::ports::AttachConnPort;
    use crate::lc_error::PromptOut;
    use crate::lc_error::QueueClass;
    use crate::lifecycle::prompt::*;
    use crate::lifecycle::delivery::DeliveryRecord;
    async fn turn_with_prompt(app: &Arc<App>, conv: &str, queued_at: &str, text: &str) -> (String, String) {
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, created_at) VALUES (?,?,'web','queued','pending',?)")
            .bind(&turn)
            .bind(conv)
            .bind(queued_at)
            .execute(&app.db)
            .await
            .unwrap();
        let msg = db::ulid();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user',?,'web',?)")
            .bind(&msg)
            .bind(conv)
            .bind(&turn)
            .bind(text)
            .bind(queued_at)
            .execute(&app.db)
            .await
            .unwrap();
        (turn, msg)
    }
    async fn created(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT created_at FROM messages WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }
    #[tokio::test]
    async fn d3b2_restamp_skips_api_keeps_missing_debt_and_preserves_cli() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "d3b2-restamp").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let (turn, message) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "fixture").await;
        sqlx::query("UPDATE turns SET restamp_pending=1,delivered_at='2026-09-30T13:38:55.000Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(am_config::CLAUDE_API_KIND).bind(&bot.id).execute(&app.db).await.unwrap();
        restamp_queued_prompt(&app, &turn).await.unwrap();
        assert_eq!(created(&app, &message).await, "2026-09-30T13:28:31.333Z");
        let pending: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(pending, 1);
        sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(&app.db).await.unwrap();
        assert!(restamp_queued_prompt(&app, &turn).await.is_err());
        sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET kind='claude' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        restamp_queued_prompt(&app, &turn).await.unwrap();
        assert_eq!(created(&app, &message).await, "2026-09-30T13:38:55.000Z");
    }
    #[tokio::test]
    async fn d3b2_mixed_restamp_scan_preserves_api_and_missing_while_processing_cli() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let api = crate::testing::claude_bot(&app, &env.project_id, "d3b2-mix-api").await;
        let cli = crate::testing::claude_bot(&app, &env.project_id, "d3b2-mix-cli").await;
        let missing = crate::testing::claude_bot(&app, &env.project_id, "d3b2-mix-missing").await;
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(am_config::CLAUDE_API_KIND).bind(&api.id).execute(&app.db).await.unwrap();
        let mut rows = Vec::new();
        for bot in [&api, &cli, &missing] {
            let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
            let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "fixture").await;
            sqlx::query("UPDATE turns SET restamp_pending=1,delivered_at='2026-09-30T13:38:55.000Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
            rows.push((turn,msg));
        }
        let mut conn = app.db.acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("DELETE FROM bots WHERE id=?").bind(&missing.id).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);
        assert!(db::bot(&app.db, &missing.id).await.unwrap().is_none());
        assert!(restamp_queued_prompt(&app, &rows[1].0).await.is_ok());
        // Restore CLI debt so the scan itself must process it.
        sqlx::query("UPDATE messages SET created_at='2026-09-30T13:28:31.333Z' WHERE id=?").bind(&rows[1].1).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET restamp_pending=1 WHERE id=?").bind(&rows[1].0).execute(&app.db).await.unwrap();
        assert!(restamp_queued_prompt(&app, &rows[2].0).await.is_err());
        sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(&app.db).await.unwrap();
        for (turn, msg) in &rows {
            assert!(restamp_queued_prompt(&app, turn).await.is_err());
            let marker: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap();
            assert_eq!(marker, 1);
            assert_eq!(created(&app,msg).await, "2026-09-30T13:28:31.333Z");
        }
        assert!(rearm_queued_prompt_restamps(&app).await.is_err());
        for (turn,msg) in &rows {
            let marker: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap();
            assert_eq!(marker, 1);
            assert_eq!(created(&app,msg).await, "2026-09-30T13:28:31.333Z");
        }
        sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(&app.db).await.unwrap();
        assert!(rearm_queued_prompt_restamps(&app).await.is_err());
        for index in [0,2] {
            let marker: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&rows[index].0).fetch_one(&app.db).await.unwrap();
            assert_eq!(marker,1);
            assert_eq!(created(&app,&rows[index].1).await,"2026-09-30T13:28:31.333Z");
        }
        let marker: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&rows[1].0).fetch_one(&app.db).await.unwrap();
        assert_eq!(marker,0);
        assert_eq!(created(&app,&rows[1].1).await,"2026-09-30T13:38:55.000Z");
    }
    /// 2026-09-30（cf-ox-2）：13:28 排進佇列、13:38:55 才送出的那一則，要排到送出的時間，回覆才接在它後面。
    #[tokio::test]
    async fn a_prompt_that_waited_in_the_queue_moves_to_when_it_was_sent() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };

        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "daemon 通知").await;
        mark_delivery(&app, &turn, ok, "2026-09-30T13:38:55.000Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, "2026-09-30T13:38:55.000Z");
        // 之後補證據再記一次：delivered_at 不變，訊息也不再動。
        mark_delivery(&app, &turn, ok, "2026-09-30T13:50:00.000Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, "2026-09-30T13:38:55.000Z");

        sqlx::query("UPDATE turns SET status='failed' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        // 沒排隊（幾百毫秒內就送出）的不動。
        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T14:00:00.000Z", "一般送出").await;
        mark_delivery(&app, &turn, ok, "2026-09-30T14:00:00.600Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, "2026-09-30T14:00:00.000Z");
    }
    /// SQLite may fail the post-delivery message update after the delivery transaction has committed.
    /// A later delivery observation must settle that debt with the original first-delivery time.
    #[tokio::test]
    async fn queued_prompt_restamp_retries_after_a_transient_message_update_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-restamp-retry").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "daemon 通知").await;
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };

        sqlx::query("CREATE TABLE issue_738_restamp_block (turn_id TEXT PRIMARY KEY)").execute(&app.db).await.unwrap();
        sqlx::query(
            "CREATE TRIGGER issue_738_fail_restamp BEFORE UPDATE OF created_at ON messages
             WHEN EXISTS (SELECT 1 FROM issue_738_restamp_block WHERE turn_id = OLD.turn_id)
             BEGIN SELECT RAISE(ABORT, 'issue 738 injected restamp failure'); END",
        )
        .execute(&app.db)
        .await
        .unwrap();
        sqlx::query("INSERT INTO issue_738_restamp_block (turn_id) VALUES (?)")
            .bind(&turn)
            .execute(&app.db)
            .await
            .unwrap();

        let first_at = "2026-09-30T13:38:55.000Z";
        mark_delivery(&app, &turn, ok, first_at).await.unwrap();
        let delivered_at: String = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?")
            .bind(&turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(delivered_at, first_at, "delivery stays durable even when restamping fails");
        assert_eq!(created(&app, &msg).await, "2026-09-30T13:28:31.333Z", "the injected update failed once");

        sqlx::query("DELETE FROM issue_738_restamp_block WHERE turn_id=?").bind(&turn).execute(&app.db).await.unwrap();
        assert!(crate::testing::eventually!(created(&app, &msg).await == first_at), "background retry uses first delivered_at");
        mark_delivery(&app, &turn, ok, "2026-09-30T14:00:00.000Z").await.unwrap();
        assert_eq!(created(&app, &msg).await, first_at, "repeated retries are idempotent");
    }
    /// 別的寫入者正拿著寫入鎖時，欠著的 restamp 要等它、不是讀完 SELECT 才在升級成寫入時直接 `database is locked`。
    #[tokio::test]
    async fn queued_prompt_restamp_waits_for_a_busy_writer_instead_of_failing_on_upgrade() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-restamp-busy").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let queued_at = "2026-09-30T13:28:31.333Z";
        let first_at = "2026-09-30T13:38:55.000Z";
        let (turn, msg) = turn_with_prompt(&app, &conv, queued_at, "daemon 通知").await;
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };
        mark_delivery(&app, &turn, ok, first_at).await.unwrap();
        sqlx::query("UPDATE messages SET created_at=? WHERE id=?").bind(queued_at).bind(&msg).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET restamp_pending=1 WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();

        let mut writer = app.db.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *writer).await.unwrap();
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(400)).await;
            sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
        });
        crate::lifecycle::prompt::rearm_queued_prompt_restamps(&app).await.expect("must wait out the other writer, not fail with database is locked");
        release.await.unwrap();
        assert_eq!(created(&app, &msg).await, first_at);
    }
    #[tokio::test]
    async fn queued_prompt_restamp_keeps_debt_when_bot_lookup_fails() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-restamp-bot-read").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let queued_at = "2026-09-30T13:28:31.333Z";
        let first_at = "2026-09-30T13:38:55.000Z";
        let (turn, msg) = turn_with_prompt(&app, &conv, queued_at, "daemon 通知").await;
        let ok = DeliveryRecord { stored: "ok", verified: true, auto_resend: false };
        mark_delivery(&app, &turn, ok, first_at).await.unwrap();

        // Recreate an uncompleted durable debt, then make the bot lookup fail once.
        sqlx::query("UPDATE messages SET created_at=? WHERE id=?").bind(queued_at).bind(&msg).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE turns SET restamp_pending=1 WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        let mut events = app.subscribe();
        crate::testing::make_table_unreadable(&app, "conversations").await;
        assert!(crate::lifecycle::prompt::rearm_queued_prompt_restamps(&app).await.is_err(), "bot lookup failure must remain an error");
        assert_eq!(created(&app, &msg).await, queued_at, "failed lookup rolls back the timestamp update");
        let pending: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(pending, 1, "the durable debt remains set");

        crate::testing::make_table_readable(&app, "conversations").await;
        crate::lifecycle::prompt::rearm_queued_prompt_restamps(&app).await.unwrap();
        assert_eq!(created(&app, &msg).await, first_at, "recovery uses first delivered_at");
        let pending: i64 = sqlx::query_scalar("SELECT restamp_pending FROM turns WHERE id=?").bind(&turn).fetch_one(&app.db).await.unwrap();
        assert_eq!(pending, 0);
        let event = events.recv().await.unwrap();
        assert_eq!(event.kind, "message_added");
        assert_eq!(event.data["bot_id"], bot.id);
        assert_eq!(event.data["message"]["id"], msg);
    }
    /// 在 first-delivery claim 前插入第二份 evidence，讓它先 claim；後到的 evidence 不能再移動訊息時間（#735）。
    #[tokio::test]
    async fn concurrent_delivery_evidence_only_restamps_for_the_first_claim() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "q-race").await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();

        let (turn, msg) = turn_with_prompt(&app, &conv, "2026-09-30T13:28:31.333Z", "daemon 通知").await;
        let turn2 = turn.clone();
        let app2 = app.clone();
        crate::lifecycle::race_point::arm("mark_delivery_before_claim", &turn, move || async move {
            // 這筆在第一筆 claim 前進來，並先完成原子 claim。
            mark_delivery(
                &app2,
                &turn2,
                Delivered::Unverified.record().unwrap(),
                "2026-09-30T13:38:55.000Z",
            )
            .await
            .unwrap();
        });

        // 外層呼叫先抵達 claim 點後暫停；注入的 evidence 先 claim 13:38:55，外層稍後以另一份 evidence 寫入。
        mark_delivery(
            &app,
            &turn,
            Delivered::Submitted.record().unwrap(),
            "2026-09-30T13:39:02.000Z",
        )
        .await
        .unwrap();

        let delivered_at: String = sqlx::query_scalar("SELECT delivered_at FROM turns WHERE id=?")
            .bind(&turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(delivered_at, "2026-09-30T13:38:55.000Z");
        assert_eq!(created(&app, &msg).await, delivered_at, "訊息時間跟著第一個成功 claim，不跟後到的 evidence 移動");

        let (delivery, verified, auto_resend): (String, i64, i64) = sqlx::query_as(
            "SELECT delivery, delivery_verified, auto_resend FROM turns WHERE id=?",
        )
        .bind(&turn)
        .fetch_one(&app.db)
        .await
        .unwrap();
        assert_eq!((delivery.as_str(), verified, auto_resend), ("ok", 1, 0), "第二條 evidence 仍更新送達欄位");
    }
}
