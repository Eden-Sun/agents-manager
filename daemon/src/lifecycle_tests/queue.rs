#![allow(unused_imports)]

mod run_exit_race_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::*;
    use crate::lifecycle::queue::*;
    use crate::lifecycle::send_now::ports::MaintenancePort;
    use crate::lifecycle::run_state as rs;
    use crate::testing as tt;

    /// 使用者 stop 一顆 bot：`stop_bot_locked` 自己關 pane，pane-exit 事件的 `mark_run_exited` 讀到的 run 還是
    /// `stopping`；stop 這時寫下 `stopped`，事件那邊接著寫 `exited` 的話就把「使用者要它停」蓋掉了——
    /// autostart 的 bot 會被 incident 探針（看最後一個 run 是不是 `stopped`）報成 `bot_stopped`。
    #[tokio::test]
    async fn a_pane_exit_that_read_the_run_before_the_user_stop_finished_does_not_overwrite_it() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stopped-on-purpose").await;
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run).execute(app.db()).await.unwrap();

        let (app2, run2) = (app.clone(), run.clone());
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &run, move || async move {
            sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?").bind(db::now()).bind(&run2).execute(app2.db()).await.unwrap();
        });
        mark_run_exited(&app, &run, "pane exited").await;

        let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?").bind(&run).fetch_one(app.db()).await.unwrap();
        assert_eq!(state, "stopped", "使用者停的就是停的，不是 pane 自己死掉");
    }

    /// 證據：`running → exited` 不是從 stopping；`stopping → exited`（stop／restart 自己關 pane 引來的 pane-exit）是；
    /// 兩步都輸（別的路徑先收了）是 `AlreadyEnded`、沒有證據。回傳值與原 `mark_run_exited` 一致。
    #[tokio::test]
    async fn the_exit_says_which_state_it_left() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "proof").await;

        let live = tt::fake_run(&app, &bot.id).await;
        let (exit, proof) = mark_run_exited_proved(&app, &live, "pane exited").await;
        assert_eq!(exit, RunExit::Recorded);
        let proof = proof.expect("自己寫下 exited");
        assert!(!proof.from_stopping);
        let ended: Option<String> = sqlx::query_scalar("SELECT ended_at FROM runs WHERE id=?").bind(&live).fetch_one(app.db()).await.unwrap();
        assert!(db::cmp_ts(&ended.unwrap(), &proof.at).is_le(), "證據的時間在 ended_at 之後（同一個毫秒也算）");

        let stopping = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&stopping).execute(app.db()).await.unwrap();
        let (exit, proof) = mark_run_exited_proved(&app, &stopping, "pane exited").await;
        assert_eq!((exit, proof.map(|p| p.from_stopping)), (RunExit::Recorded, Some(true)));
        assert_eq!(run_state(&app, &stopping).await, "exited");

        let (exit, proof) = mark_run_exited_proved(&app, &stopping, "pane exited").await;
        assert_eq!((exit, proof), (RunExit::AlreadyEnded, None));
        assert_eq!(mark_run_exited(&app, &stopping, "pane exited").await, RunExit::AlreadyEnded);

        let raced = tt::fake_run(&app, &bot.id).await;
        let (app2, raced2) = (app.clone(), raced.clone());
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &raced, move || async move {
            sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?").bind(db::now()).bind(&raced2).execute(app2.db()).await.unwrap();
        });
        assert_eq!(mark_run_exited_proved(&app, &raced, "pane exited").await, (RunExit::AlreadyEnded, None));
        assert_eq!(run_state(&app, &raced).await, "stopped");

        let flipped = tt::fake_run(&app, &bot.id).await;
        let (app3, flipped2) = (app.clone(), flipped.clone());
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &flipped, move || async move {
            sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&flipped2).execute(app3.db()).await.unwrap();
        });
        let (exit, proof) = mark_run_exited_proved(&app, &flipped, "pane exited").await;
        assert_eq!((exit, proof.map(|p| p.from_stopping)), (RunExit::Recorded, Some(true)));
    }

    /// 第一步寫不進去：不做第二步、照原路 `NotRecorded`＋排對帳，run 照舊。
    #[tokio::test]
    async fn a_first_step_that_cannot_be_written_does_not_try_the_second() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "proof-err").await;
        let run = tt::fake_run(&app, &bot.id).await;
        rs::refuse_run_state(&app, "exited").await;
        assert_eq!(mark_run_exited_proved(&app, &run, "pane exited").await, (RunExit::NotRecorded, None));
        assert_eq!(run_state(&app, &run).await, "running");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::Reconcile { stuck: "running".into() }]);
        rs::accept_run_state(&app, "exited").await;
    }

    /// 沒有人搶的時候照舊：還活著的 run 碰到 pane-exit 就是 `exited`。
    #[tokio::test]
    async fn a_pane_exit_on_a_live_run_still_ends_it_as_exited() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "died").await;
        let run = tt::fake_run(&app, &bot.id).await;
        mark_run_exited(&app, &run, "pane exited").await;
        let (state, ended): (String, Option<String>) =
            sqlx::query_as("SELECT state, ended_at FROM runs WHERE id=?").bind(&run).fetch_one(app.db()).await.unwrap();
        assert_eq!(state, "exited");
        assert!(ended.is_some());
    }

    async fn run_state(app: &Arc<App>, run: &str) -> String {
        db::run(app.db(), run).await.unwrap().unwrap().state
    }

    /// #135：`exited` 寫不進去（SQLite I/O／busy）就還不是 exited——依附這顆 run 的東西一樣都不能先動：
    /// in-flight 不收、排著的不撤、watcher 不拆。DB 恢復後再收一次，收尾照常、只做一次。
    #[tokio::test]
    async fn a_run_exit_that_cannot_be_recorded_leaves_the_run_and_its_work_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "db-hiccup").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let in_flight = rs::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;

        rs::refuse_run_state(&app, "exited").await;
        assert_eq!(mark_run_exited(&app, &run, "pane exited").await, RunExit::NotRecorded, "跟「別的路徑先收了」分得開");
        assert_eq!(run_state(&app, &run).await, "running", "寫不進去就還是 running");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight", "run 還沒結束，in-flight 不能先被收成 failed");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued", "run 還沒結束，排著的不能先被撤");
        assert!(rs::watched(&app, &watcher).await, "watcher 不能先拆");
        assert_eq!(rs::scheduled(&run), vec![rs::Settle::Reconcile { stuck: "running".into() }], "排了對帳重試，不是只留一行 warning");

        rs::accept_run_state(&app, "exited").await;
        assert_eq!(mark_run_exited(&app, &run, "pane exited").await, RunExit::Recorded);
        assert_eq!(run_state(&app, &run).await, "exited");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "failed");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed");
        assert_eq!((rs::system_notes(&app, &in_flight).await, rs::system_notes(&app, &queued).await), (1, 1), "各收一次");
        assert!(!rs::watched(&app, &watcher).await);
    }

    /// #198 同類：run 結束時讀不到它的主機，不拿 `local` 頂替去拆 watcher——本機剛好同名 session、同 pane id 的那顆
    /// bot 的 watcher 會被拆掉，它的狀態事件從此收不到。
    #[tokio::test]
    async fn a_run_exit_whose_host_cannot_be_read_leaves_other_hosts_watchers_alone() {
        let env = tt::env().await;
        let app = env.app.clone();
        let far = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, '/r/p', 'r', 'far', ?)").bind(&far).bind(db::now()).execute(app.db()).await.unwrap();
        let bot = tt::claude_bot(&app, &far, "remote").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let local = (LOCAL_HOST.to_string(), "test".to_string(), pane.clone());
        app.pane_watchers.lock().await.insert(local.clone(), tokio::spawn(std::future::pending::<()>()));

        sqlx::query("ALTER TABLE projects RENAME TO projects_unreadable").execute(app.db()).await.unwrap();
        mark_run_exited(&app, &run, "pane exited").await;
        sqlx::query("ALTER TABLE projects_unreadable RENAME TO projects").execute(app.db()).await.unwrap();
        assert!(rs::watched(&app, &local).await, "本機那顆的 watcher 不是這個 run 的");
    }

    /// #156：`exited` 記下了，in-flight 那一筆卻收不成。pane 已經沒了（這不是我們能不做的事），所以不回
    /// 「收尾做完了」：記成欠著的收尾、之後補上；佇列與 watcher 照樣收（那是 run 結束的事，跟回合收不收得成無關）。
    #[tokio::test]
    async fn a_run_exit_whose_in_flight_turn_cannot_be_closed_owes_it_and_settles_it_later() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "exit-turn-owed").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let in_flight = rs::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        rs::refuse_turn_close(&app, &in_flight).await;

        let exit = mark_run_exited(&app, &run, "pane exited").await;
        assert_ne!(exit, RunExit::Recorded, "回合沒收掉，不能說收尾做完了");
        assert_eq!(exit, RunExit::TurnOwed);
        assert_eq!(run_state(&app, &run).await, "exited");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight", "寫不進去就是還沒收");
        assert_eq!(rs::turn_status(&app, &queued).await, "failed", "run 結束了，排著的照撤");
        assert!(!rs::watched(&app, &watcher).await);

        rs::accept_turn_close(&app).await;
        crate::lifecycle::interruption::settle_locked(&app, &bot.id, crate::lifecycle::interruption::Evidence::Nothing).await.unwrap();
        assert_eq!(rs::turn_status(&app, &in_flight).await, "failed", "欠著的收尾補上了");
        assert_eq!(rs::system_notes(&app, &in_flight).await, 1);
        let note: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'").bind(&in_flight).fetch_one(app.db()).await.unwrap();
        assert_eq!(note, "run ended: pane exited");
    }

    /// #135 驗收第三條：CAS 輸給使用者的 stop 時照 #131——不覆寫 `stopped`，也不做第二份收尾（那是 stop 的）。
    #[tokio::test]
    async fn a_pane_exit_that_lost_the_race_to_a_user_stop_leaves_the_cleanup_to_the_stop() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "stopping").await;
        let run = tt::fake_run(&app, &bot.id).await;
        let in_flight = rs::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        let queued = rs::a_turn(&app, &bot.id, None, "queued").await;
        let watcher = rs::watch_run_pane(&app, &run).await;
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?").bind(&run).execute(app.db()).await.unwrap();
        let (app2, run2) = (app.clone(), run.clone());
        crate::lifecycle::race_point::arm("mark_run_exited_after_read", &run, move || async move {
            sqlx::query("UPDATE runs SET state='stopped', ended_at=? WHERE id=?").bind(db::now()).bind(&run2).execute(app2.db()).await.unwrap();
        });

        assert_eq!(mark_run_exited(&app, &run, "pane exited").await, RunExit::AlreadyEnded, "CAS 輸了");
        assert_eq!(run_state(&app, &run).await, "stopped");
        assert_eq!(rs::turn_status(&app, &in_flight).await, "in_flight", "不是這條路的收尾");
        assert_eq!(rs::turn_status(&app, &queued).await, "queued");
        assert!(rs::watched(&app, &watcher).await);
        assert!(rs::scheduled(&run).is_empty(), "CAS 輸了不是寫入失敗，不排重試");
    }
}


mod flush_queue_tests {
    //! Durable prompt queue claim step. `schedule_flush_queued` is a no-op in tests, so these call
    //! `flush_queued_locked` directly.
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::*;
    use crate::lifecycle::queue::*;
    use crate::lifecycle::send_now::ports::MaintenancePort;
    use crate::testing as tt;

    /// 上限橫幅寫進這顆 bot 身份的 key，且不把 5h `resets_at` 蓋成橫幅時間（2026-09-13）。
    #[tokio::test]
    async fn a_limit_hit_banner_lands_on_the_bots_own_quota_key() {
        let env = tt::env().await;
        let app = env.app.clone();
        let notice = "ERROR: You've hit your usage limit, or try again at 10:15 PM.";
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, "codex:astra", codex_limit_hit(notice, db::now())).await;

        let q = app.quotas.lock().await;
        let mine = q.get("codex:astra").expect("寫進帶身份的那把");
        assert!(mine.limit_hit.is_some());
        assert_eq!(mine.five_hour.as_ref().unwrap().used_pct, 100.0, "量表標成用完");
        assert!(mine.five_hour.as_ref().unwrap().resets_at.is_none(), "橫幅的時間只進 limit_hit.until");
        assert!(mine.limit_hit.as_ref().unwrap().until.is_some());
        assert!(q.get("codex").is_none(), "沒有身份的那把不該被動到");
    }

    /// 同一張橫幅重掃不是新證據，不可把 `at` 蓋成現在（2026-09-13：22:21 掃到 22:15 的舊橫幅）。
    #[tokio::test]
    async fn the_same_banner_seen_again_is_not_new_evidence() {
        let env = tt::env().await;
        let app = env.app.clone();
        let notice = "ERROR: You've hit your usage limit, or try again at 10:15 PM.";
        apply_codex_limit_hit_quota(&app, LOCAL_HOST, "codex", codex_limit_hit(notice, db::now())).await;
        let first = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();

        apply_codex_limit_hit_quota(&app, LOCAL_HOST, "codex", codex_limit_hit(notice, db::now())).await;
        let again = app.quotas.lock().await.get("codex").unwrap().limit_hit.clone().unwrap();
        assert_eq!(first.at, again.at, "同一張橫幅不會把時間戳往前推");
        assert_eq!(first.until, again.until);
    }

    struct Fixture {
        env: tt::Env,
        bot_id: String,
        conv: String,
        run_id: String,
        turn_id: String,
    }

    async fn link_schedule(f: &Fixture) -> String {
        let id = db::ulid();
        let now = db::now();
        sqlx::query("INSERT INTO schedules (id,project_id,bot_id,name,prompt,mode,tz,spec_json,created_at,updated_at) VALUES (?,?,?,'schedule','ping','once','UTC','{}',?,?)")
            .bind(&id).bind(&f.env.project_id).bind(&f.bot_id).bind(&now).bind(&now).execute(f.env.app.db()).await.unwrap();
        sqlx::query("INSERT INTO schedule_occurrences (id,schedule_id,source,scheduled_for,generation,bot_id,prompt,status,turn_id,conversation_id,created_at,updated_at) VALUES (?,?,'scheduled',?,1,?,'ping','queued',?,?,?,?)")
            .bind(db::ulid()).bind(&id).bind(&now).bind(&f.bot_id).bind(&f.turn_id).bind(&f.conv).bind(&now).bind(&now).execute(f.env.app.db()).await.unwrap();
        id
    }

    #[tokio::test]
    async fn a_cancelled_schedule_is_retracted_before_flush_and_frees_the_user_queue() {
        for change in ["enabled=0", "deleted_at='2026-10-13T00:00:00.000Z'", "generation=2"] {
            let f = queued("test").await;
            let id = link_schedule(&f).await;
            sqlx::query(&format!("UPDATE schedules SET {change} WHERE id=?")).bind(&id).execute(f.env.app.db()).await.unwrap();
            let lock = f.env.app.bot_lock(&f.bot_id).await;
            let _guard = lock.lock().await;
            flush_queued_locked(&f.env.app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "failed");
            assert!(db::queued_turn(f.env.app.db(), &f.conv).await.unwrap().is_none());
            let status: String = sqlx::query_scalar("SELECT status FROM schedule_occurrences WHERE turn_id=?")
                .bind(&f.turn_id).fetch_one(f.env.app.db()).await.unwrap();
            assert_eq!(status, "cancelled");
            assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt" || m == "pane.send_text"));
        }
    }

    #[tokio::test]
    async fn a_schedule_claim_checks_cancellation_atomically_and_keeps_active_occurrences_eligible() {
        let f = queued("test").await;
        let id = link_schedule(&f).await;
        sqlx::query("UPDATE schedules SET enabled=0 WHERE id=?").bind(&id).execute(f.env.app.db()).await.unwrap();
        let mut conn = f.env.app.db().acquire().await.unwrap();
        assert!(matches!(crate::lifecycle::turn_controller::claim_queued(&mut conn, &f.turn_id, &f.run_id).await.unwrap(), crate::lifecycle::turn_controller::Outcome::Fenced(_)));
        assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "queued");
        sqlx::query("UPDATE schedules SET enabled=1 WHERE id=?").bind(&id).execute(&mut *conn).await.unwrap();
        assert_eq!(crate::lifecycle::turn_controller::claim_queued(&mut conn, &f.turn_id, &f.run_id).await.unwrap(), crate::lifecycle::turn_controller::Outcome::Applied);
    }

    #[tokio::test]
    async fn unreadable_schedule_state_does_not_deliver_or_discard_a_queued_prompt() {
        let f = queued("test").await;
        link_schedule(&f).await;
        sqlx::query("ALTER TABLE schedule_occurrences RENAME TO unavailable_occurrences").execute(f.env.app.db()).await.unwrap();
        assert!(flush_queued_locked(&f.env.app, &f.bot_id).await.is_err());
        assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "queued");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt" || m == "pane.send_text"));
    }

    /// Running bot with one queued prompt. `session` other than `"test"` makes `client_for_run` fail
    /// (a host that dropped out between queueing and flush).
    async fn queued(session: &str) -> Fixture {
        queued_kind("claude", session).await
    }

    async fn queued_kind(kind: &str, session: &str) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        env.herdr.know_pane("pane-1");
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'q',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let conv = db::conversation_id(app.db(), &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-1','agent',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(session)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        let turn_id = db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,'web','queued','pending','ping',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(db::now())
        .execute(app.db())
        .await
        .unwrap();
        Fixture { env, bot_id, conv, run_id, turn_id }
    }

    #[tokio::test]
    async fn d3b2_queue_guards_keep_api_and_rearm_cli_in_same_database() {
        let f = queued_kind(am_config::CLAUDE_API_KIND, "test").await;
        let cli = tt::claude_bot(&f.env.app, &f.env.project_id, "d3b2-cli").await;
        let conv = db::conversation_id(f.env.app.db(), &cli.id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,origin,status,delivery,prompt_text,created_at) VALUES('d3b2-cli-turn',?,'web','queued','pending','ping',?)").bind(&conv).bind(db::now()).execute(f.env.app.db()).await.unwrap();
        let cli_run = db::ulid();
        sqlx::query("INSERT INTO runs(id,bot_id,state,agent_status,workspace_id,pane_id,agent_name,herdr_session,started_at) VALUES(?,?,'running','idle','ws-1','pane-1','agent','test',?)").bind(&cli_run).bind(&cli.id).bind(db::now()).execute(f.env.app.db()).await.unwrap();
        db::set_pane_typed(f.env.app.db(), &cli_run).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let pending = pending_queue_retries(&f.env.app).await.unwrap();
        assert!(pending.iter().any(|(id,_)| id == &cli.id));
        assert!(!pending.iter().any(|(id,_)| id == &f.bot_id));
        flush_queued_locked(&f.env.app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "queued");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt" || m == "pane.send_text"));
        flush_queued_locked(&f.env.app, &cli.id).await.unwrap();
        assert_eq!(turn(&f.env.app, "d3b2-cli-turn").await.status, "in_flight");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
        assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "queued");
        sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots").execute(f.env.app.db()).await.unwrap();
        assert!(pending_queue_retries(&f.env.app).await.is_err());
        assert!(flush_queued_locked(&f.env.app, &f.bot_id).await.is_err());
        assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "queued");
        sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots").execute(f.env.app.db()).await.unwrap();
    }

    #[tokio::test]
    async fn d3b2_missing_bot_flush_preserves_queue_and_retry_candidate() {
        let f = queued("test").await;
        let mut conn = f.env.app.db().acquire().await.unwrap();
        sqlx::query("PRAGMA foreign_keys=OFF").execute(&mut *conn).await.unwrap();
        sqlx::query("DELETE FROM bots WHERE id=?").bind(&f.bot_id).execute(&mut *conn).await.unwrap();
        sqlx::query("PRAGMA foreign_keys=ON").execute(&mut *conn).await.unwrap();
        drop(conn);
        assert!(db::bot(f.env.app.db(), &f.bot_id).await.unwrap().is_none());
        assert!(pending_queue_retries(&f.env.app).await.unwrap().iter().any(|(id,_)| id == &f.bot_id));
        assert!(flush_queued_locked(&f.env.app, &f.bot_id).await.is_err());
        assert_eq!(turn(&f.env.app, &f.turn_id).await.status, "queued");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"));
    }

    /// 真的走 flush：剛 interrupt 的 bot 排著的派工不動（不 claim、不算重試、掛好 timer）；
    /// 寬限過了就照常往下送。
    #[tokio::test]
    async fn a_queued_dispatch_waits_out_the_grace_after_the_user_interrupts() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        note_user_interrupt(&f.bot_id);
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued");
        assert_eq!((t.flush_retries, t.next_flush_at.clone()), (0, None), "寬限內連試都不試");
        assert!(queue_retry_timer_armed(&f.bot_id), "閒著的 bot 沒有邊會叫醒它，要掛 timer");

        crate::lifecycle::interrupt_grace::note_user_interrupt_at(&f.bot_id, chrono::Utc::now() - chrono::Duration::seconds(120));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "working", ago(121));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", ago(120));
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(t.flush_retries > 0 || t.status != "queued", "寬限過了要真的去送：{} retries={}", t.status, t.flush_retries);
        assert_eq!(crate::lifecycle::interrupt_grace::hold_of(&f.bot_id), None, "等完就收掉標記");
    }

    /// issue #86：維護窗口握著的時候，排隊的 prompt **留在佇列**，不進 pane。擋住它的是我們自己開的
    /// 窗口、不是 bot 的狀態，所以不算一次重試；窗口到期為止掛一個 timer，窗口提早關掉時
    /// `drain_queue` 會叫醒 flush，這個 timer 只是底線。
    #[tokio::test]
    async fn a_queued_prompt_waits_out_a_maintenance_window_instead_of_going_into_the_pane() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        crate::supervisor::store::acquire_lease(app.db(), "restart", "k8bw2f", None, None, &until, false, None, &json!({}))
            .await
            .unwrap()
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "留在佇列，而且不花它的重試額度");
        assert_eq!(t.run_id, None, "沒有被 claim");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text"), "一個字都沒打");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，窗口關掉之後有人會回來送");

        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(&past).execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(t.status != "queued" || t.flush_retries > 0, "窗口過期就照常往下送：{} retries={}", t.status, t.flush_retries);
    }

    /// issue #127：窗口握著、但讀不到它——排隊的這一筆要**留在佇列**，不 claim、不花重試、一個字都不送，
    /// 掛 timer 等下一輪重新判斷。跟窗口明確握著是同一個結果；讀取恢復、窗口過期之後才往下送。
    #[tokio::test]
    async fn a_queued_prompt_stays_queued_while_the_restart_lease_cannot_be_read() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        crate::supervisor::store::acquire_lease(app.db(), "restart", "k8bw2f", None, None, &until, false, None, &json!({}))
            .await
            .unwrap()
            .unwrap();
        crate::supervisor::maintenance::fault::break_lease_reads(app.db()).await;
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "留在佇列，而且不花它的重試額度");
        assert_eq!(t.run_id, None, "沒有被 claim");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒送");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，下一輪重新判斷；不然這筆會一直躺著");

        crate::supervisor::maintenance::fault::restore_lease_reads(app.db()).await;
        let past = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        sqlx::query("UPDATE supervisor_leases SET expires_at=? WHERE resource='restart'").bind(&past).execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert!(t.status != "queued" || t.flush_retries > 0, "恢復後照常往下送：{} retries={}", t.status, t.flush_retries);
    }

    /// 中斷寬限的 timer 比 `next_flush_at` 早燒：flush 看到退避還沒到就早退，這一刻起沒有任何 timer。
    /// 早退時一定要補一個到期才燒的 timer，否則閒著的 bot 再也不會有人來送，排隊的派工要等 30 分鐘
    /// 保險絲才被撤掉（review3 L1）。
    #[tokio::test]
    async fn an_early_wake_up_leaves_a_timer_for_when_the_backoff_is_actually_up() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let next = (chrono::Utc::now() + chrono::Duration::seconds(5)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET next_flush_at = ?, flush_retries = 1 WHERE id = ?")
            .bind(&next)
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 1), "退避還沒到：不動它、不花重試");
        assert!(queue_retry_timer_armed(&f.bot_id), "早退也要留下一個 timer，否則沒有人會再來送");
        let left = queue_retry_timer_left(&f.bot_id).expect("timer 掛著");
        assert!(left <= std::time::Duration::from_secs(5), "掛的是剩下的退避時間，不是整輪重來：{left:?}");
    }

    async fn turn(app: &Arc<App>, id: &str) -> db::Turn {
        sqlx::query_as::<_, db::Turn>("SELECT * FROM turns WHERE id = ?")
            .bind(id)
            .fetch_one(app.db())
            .await
            .unwrap()
    }

    /// 交辦掛上這筆 queued turn（AGM 派工遇到回合中就是這個形狀）。
    async fn queued_assignment(f: &Fixture) -> String {
        crate::supervisor::store::get_or_init(f.env.app.db()).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(f.env.app.db(), None, &f.bot_id, "crid-fence", "請釋放 fence 21", &[], None, true)
            .await
            .unwrap();
        crate::supervisor::store::mark_delivered(f.env.app.db(), &a.id, &f.turn_id, "queued").await.unwrap();
        a.id
    }

    async fn decide(app: &Arc<App>, id: &str, decision: &str) -> Value {
        let input: crate::runners::supervisor::api::ReviewIn =
            serde_json::from_value(json!({"decision": decision, "reason": "改主意了"})).unwrap();
        crate::runners::supervisor::api::post_review(axum::extract::State(app.clone()), axum::extract::Path(id.to_string()), axum::http::HeaderMap::new(), axum::Json(input))
            .await
            .map(|j| j.0)
            .unwrap()
    }

    /// cancel 當下就撤掉它排著的 queued turn：終態、寫明理由、名額立刻釋放，之後的 flush 也不會送出去。
    #[tokio::test]
    async fn cancelling_an_assignment_revokes_its_queued_turn_and_frees_the_slot() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let a = queued_assignment(&f).await;

        let out = decide(&app, &a, "cancel").await;
        assert_eq!(out["status"], "cancelled");
        assert_eq!(out["revoked_turn_id"], json!(f.turn_id));
        assert!(out.get("warning").is_none() && out.get("may_still_be_running").is_none(), "撤掉的是還沒送出的那則，不能再說它還在跑：{out}");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "終態，不佔名額");
        let why: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert!(why.contains(&a) && why.contains("已取消") && why.contains("改主意了"), "{why}");

        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','下一則',?)")
            .bind(db::ulid())
            .bind(&f.conv)
            .bind(db::now())
            .execute(app.db())
            .await
            .expect("queued 名額已經釋放");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let typed = f.env.herdr.pane("pane-1").unwrap().transcript;
        assert!(!typed.iter().any(|l| l.contains("fence 21")), "已取消的那則一個字都沒送：{typed:?}");
        assert!(typed.iter().any(|l| l.contains("下一則")), "排在後面的照常送：{typed:?}");
    }

    /// #200：cancel 的那一刻撤不掉排著的那一則——讀不到交辦（以前舊介面把讀取錯誤吞成「沒有要撤的」），或撤銷那一句寫不進去
    /// （以前只記一行）——回應看起來都跟「沒有要撤的」一樣。現在回應講明撤銷待補（`revoke_pending_turn_id`）、叫醒 flush；
    /// DB 好了 flush 把它撤掉，一個字都不送。
    #[tokio::test]
    async fn a_cancel_that_cannot_revoke_the_queued_prompt_right_now_says_it_is_pending() {
        for why in ["讀不到交辦", "撤銷寫不進去"] {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
            let a = queued_assignment(&f).await;
            forget_queue_retry_timer(&f.bot_id);
            if why == "讀不到交辦" {
                let app2 = app.clone();
                crate::lifecycle::race_point::arm("review_before_revoke", &a, move || async move { break_assignment_reads(&app2).await });
            } else {
                sqlx::query("CREATE TRIGGER lost_revoke BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
                    .execute(app.db())
                    .await
                    .unwrap();
            }

            let out = decide(&app, &a, "cancel").await;
            if why == "讀不到交辦" {
                restore_assignment_reads(&app).await;
            } else {
                sqlx::query("DROP TRIGGER lost_revoke").execute(app.db()).await.unwrap();
            }
            assert_eq!(out["status"], "cancelled", "{why}");
            assert!(out.get("revoked_turn_id").is_none(), "{why}：沒撤成：{out}");
            assert_eq!(out["revoke_pending_turn_id"], json!(f.turn_id), "{why}：講明撤銷待補：{out}");
            assert!(queue_retry_timer_armed(&f.bot_id), "{why}：叫醒 flush 再撤");
            assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "{why}");

            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "{why}：flush 撤掉");
            assert_eq!(typed(&f, "fence 21"), 0, "{why}：一個字都沒送");
        }
    }

    /// 已經 in_flight（送出去了）的撤不回來：cancel 不動 turn，也不說撤掉了。
    #[tokio::test]
    async fn an_assignment_whose_prompt_already_went_out_is_not_pretended_away() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let a = queued_assignment(&f).await;
        sqlx::query("UPDATE turns SET status = 'in_flight', delivery = 'ok', run_id = ? WHERE id = ?")
            .bind(&f.run_id)
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        let out = decide(&app, &a, "cancel").await;
        assert_eq!(out["status"], "cancelled");
        assert!(out.get("revoked_turn_id").is_none(), "{out}");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 繞過 API 改成 superseded／failed（或決定 commit 之後還沒撤就重啟）：flush 送出前也會撤掉，不送。
    #[tokio::test]
    async fn the_flush_never_sends_a_prompt_whose_assignment_was_withdrawn() {
        for status in ["superseded", "failed", "cancelled", "blocked"] {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            let a = queued_assignment(&f).await;
            sqlx::query("UPDATE supervisor_assignments SET status = ? WHERE id = ?").bind(status).bind(&a).execute(app.db()).await.unwrap();
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "{status}");
            assert!(f.env.herdr.pane("pane-1").map_or(true, |p| p.transcript.is_empty()), "{status}：一個字都沒打");
        }
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        queued_assignment(&f).await;
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// #1361：閒著的 bot 排著一則，flush 撞上暫時的讀取錯誤（`runs` 讀不到）：這一則不能躺到 30 分鐘保險絲才撤，要掛一個短 timer 再來。
    /// 不認領、不花重試；讀得到之後照常送出。不 sleep、不看時間。
    #[tokio::test]
    async fn a_flush_that_cannot_read_the_run_leaves_a_timer_behind() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        forget_queue_retry_timer(&f.bot_id);
        sqlx::query("ALTER TABLE runs RENAME TO runs_unreadable").execute(app.db()).await.unwrap();
        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "讀不到 run 要回錯誤");
        assert!(queue_retry_timer_armed(&f.bot_id), "閒著的 bot 沒有邊會叫醒它，要掛 timer");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 0), "留在佇列，不認領、不花重試");
        sqlx::query("ALTER TABLE runs_unreadable RENAME TO runs").execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "ping"), 1, "讀得到之後照常送出");
    }

    /// #1361 的另一條：讀不到對話（`conversations`）原本直接回 `Ok(())`、不掛 timer；現在要留一個短 timer。
    #[tokio::test]
    async fn a_flush_that_cannot_read_the_conversation_leaves_a_timer_behind() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        forget_queue_retry_timer(&f.bot_id);
        sqlx::query("ALTER TABLE conversations RENAME TO conversations_unreadable").execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert!(queue_retry_timer_armed(&f.bot_id), "讀不到對話也要留一個短 timer，閒著的 bot 不會再被叫醒");
        sqlx::query("ALTER TABLE conversations_unreadable RENAME TO conversations").execute(app.db()).await.unwrap();
    }

    /// #1431：DB 持續讀不到時，flush 失敗的重排要退避（10 秒起、每次加倍、上限 5 分鐘），不能每 10 秒永遠重來；
    /// DB 恢復後那次送出，排程表清空。只讀排程表上剩的時間，不 sleep、不看牆鐘。
    #[tokio::test]
    async fn a_flush_that_keeps_failing_backs_off_and_stops_once_it_succeeds() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("ALTER TABLE runs RENAME TO runs_unreadable").execute(app.db()).await.unwrap();
        let mut secs = Vec::new();
        for _ in 0..8 {
            forget_queue_retry_timer(&f.bot_id);
            assert!(flush_queued_locked(&app, &f.bot_id).await.is_err());
            secs.push(queue_retry_timer_left(&f.bot_id).expect("讀不到就要掛 timer").as_secs_f64().ceil() as u64);
        }
        assert_eq!(secs, vec![10, 20, 40, 80, 160, 300, 300, 300], "連續失敗：每次加倍，上限 5 分鐘");
        sqlx::query("ALTER TABLE runs_unreadable RENAME TO runs").execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "ping"), 1, "讀得到之後照常送出");
        assert!(!queue_retry_timer_armed(&f.bot_id), "成功之後不再排重試");
    }

    /// 之後每一句讀 `supervisor_assignments` 的都失敗（`no such table`）——DB 出錯的樣子（同 `maintenance::fault`）。
    async fn break_assignment_reads(app: &Arc<App>) {
        sqlx::query("ALTER TABLE supervisor_assignments RENAME TO supervisor_assignments_unreadable").execute(app.db()).await.unwrap();
    }

    async fn restore_assignment_reads(app: &Arc<App>) {
        sqlx::query("ALTER TABLE supervisor_assignments_unreadable RENAME TO supervisor_assignments").execute(app.db()).await.unwrap();
    }

    /// #159 驗收一、二、五：交辦已經不要了（cancelled／superseded／failed／blocked／quota_blocked），flush 卻讀不到它——
    /// 以前 `.ok()??` 把讀取錯誤當成「沒有撤銷理由」，照樣認領、打進 pane，被取消的工作真的開始跑。讀不到不等於還要：
    /// 留在佇列、不認領、不花重試、一個字都不送，掛短 timer；讀得到之後照它的狀態撤掉，永遠不送。
    #[tokio::test]
    async fn a_queued_prompt_whose_assignment_cannot_be_read_is_never_sent() {
        for status in WITHDRAWN_ASSIGNMENT {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
            let a = queued_assignment(&f).await;
            sqlx::query("UPDATE supervisor_assignments SET status = ? WHERE id = ?").bind(status).bind(&a).execute(app.db()).await.unwrap();
            forget_queue_retry_timer(&f.bot_id);
            break_assignment_reads(&app).await;

            assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "{status}：讀不到，不回普通的成功");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.flush_retries), ("queued", None, 0), "{status}：留在佇列、不認領、不花重試");
            assert_eq!(typed(&f, "fence 21"), 0, "{status}：一個字都沒送");
            assert!(queue_retry_timer_armed(&f.bot_id), "{status}：掛了 timer，稍後重新判斷");

            restore_assignment_reads(&app).await;
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "{status}：讀得到了就撤");
            assert_eq!(typed(&f, "fence 21"), 0, "{status}：永遠不送");
        }
    }

    /// #159：交辦已經取消、要撤排著的那一則時寫不進去——以前只記一行 error、回 Ok、不掛 timer：沒撤成卻當撤完了，那一筆
    /// 佔著佇列，直到下一次剛好有人叫醒 flush。現在留在佇列、回錯誤、掛短 timer；DB 好了就撤，一個字都不送。
    #[tokio::test]
    async fn a_withdrawn_prompt_that_cannot_be_revoked_stays_queued_and_is_retried() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let a = queued_assignment(&f).await;
        sqlx::query("UPDATE supervisor_assignments SET status = 'cancelled' WHERE id = ?").bind(&a).execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        sqlx::query("CREATE TRIGGER lost_revoke BEFORE UPDATE OF status ON turns WHEN NEW.status = 'failed' BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(app.db())
            .await
            .unwrap();

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "撤不掉：不回普通的成功");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，稍後再撤");
        assert_eq!(typed(&f, "fence 21"), 0);

        sqlx::query("DROP TRIGGER lost_revoke").execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert_eq!(typed(&f, "fence 21"), 0, "永遠不送");
    }

    /// #159（同一類）：flush 讀完交辦（還要）、還沒認領的那一瞬，交辦被取消（review API 不拿 bot 鎖）。認領那一句本身帶上
    /// 「掛的交辦此刻還要」：已經取消的那一則不被領走、不打進 pane；下一輪照它的狀態撤掉。
    #[tokio::test]
    async fn an_assignment_cancelled_between_the_check_and_the_claim_is_not_claimed() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '請釋放 fence 21' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let a = queued_assignment(&f).await;
        let (app2, a2) = (app.clone(), a.clone());
        crate::lifecycle::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
            sqlx::query("UPDATE supervisor_assignments SET status = 'cancelled' WHERE id = ?").bind(&a2).execute(app2.db()).await.unwrap();
        });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "取消了就不領走");
        assert_eq!(typed(&f, "fence 21"), 0, "一個字都沒打");

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed", "下一輪撤掉");
        assert_eq!(typed(&f, "fence 21"), 0);
    }

    /// run 結束（pane 不見、agent 退出）：它排著的 queued 收掉、名額釋放，交辦照一般流程變成失敗讓 AGM 看得到。
    #[tokio::test]
    async fn a_run_that_ends_takes_its_queued_prompt_with_it() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let a = queued_assignment(&f).await;
        let mut events = app.subscribe_turns();
        mark_run_exited(&app, &f.run_id, "pane exited").await;
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let why: String = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert!(why.contains("pane exited") && why.contains("沒有在跑"), "{why}");
        let ev = events.try_recv().expect("撤銷有推 turn 事件，交辦才會結案");
        assert_eq!((ev.turn_id.as_str(), ev.status.as_str()), (f.turn_id.as_str(), "failed"));
        crate::supervisor::controller::reconcile(&app).await;
        let row = crate::supervisor::store::assignment(app.db(), &a).await.unwrap().unwrap();
        assert_ne!(row.status, "delivered", "不留在 delivered 假裝還在路上");
    }

    /// bot 被停掉：同樣收掉。
    #[tokio::test]
    async fn stopping_a_bot_revokes_its_queued_prompt() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        stop_bot(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert!(db::active_run(app.db(), &f.bot_id).await.unwrap().is_none());
    }

    /// 還有活著的 run 就不動（flush 會送）；定時掃描收掉 run 早就不在的遺留（含上線前留下來的）。
    #[tokio::test]
    async fn only_a_queued_prompt_with_no_live_run_is_an_orphan() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        assert!(revoke_orphaned_queued_turns(&app, &f.bot_id, "測試").await.is_empty(), "run 還在");
        assert!(revoke_all_orphaned_queued_turns(&app).await.is_empty());
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");

        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?").bind(db::now()).bind(&f.run_id).execute(app.db()).await.unwrap();
        assert_eq!(revoke_all_orphaned_queued_turns(&app).await, vec![f.turn_id.clone()]);
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert!(revoke_all_orphaned_queued_turns(&app).await.is_empty(), "撤過的不再撤");
    }

    /// 在等額度的交辦：排著的那則不送（額度回來會用新的一則重送，送了就是做兩次）。
    #[tokio::test]
    async fn a_prompt_whose_assignment_waits_for_quota_is_not_sent() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let a = queued_assignment(&f).await;
        sqlx::query("UPDATE supervisor_assignments SET status = 'quota_blocked' WHERE id = ?").bind(&a).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
        assert!(f.env.herdr.pane("pane-1").map_or(true, |p| p.transcript.is_empty()));
    }

    fn ago(secs: u64) -> std::time::Instant {
        std::time::Instant::now().checked_sub(std::time::Duration::from_secs(secs)).unwrap_or_else(std::time::Instant::now)
    }

    fn typed(f: &Fixture, text: &str) -> usize {
        f.env.herdr.pane("pane-1").map_or(0, |p| p.transcript.iter().filter(|l| l.contains(text)).count())
    }

    /// 使用者剛按 Esc：排著的派工不立刻打進去，timer 掛好等寬限。
    #[tokio::test]
    async fn after_the_user_interrupts_the_queued_prompt_waits_for_the_grace() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        note_user_interrupt(&f.bot_id);
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "寬限內不送");
        assert_eq!(typed(&f, "AGM 的派工"), 0, "一個字都沒打");
        assert!(queue_retry_timer_armed(&f.bot_id), "寬限到了自己再來一次");
    }

    /// 連續 idle 滿寬限、期間沒有使用者新輸入才送；中斷很久但剛剛才又閒下來（有動靜）還是要等。
    #[tokio::test]
    async fn once_the_bot_has_been_quiet_for_the_whole_grace_the_queued_prompt_goes_out() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        crate::lifecycle::interrupt_grace::note_user_interrupt_at(&f.bot_id, chrono::Utc::now() - chrono::Duration::seconds(300));

        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "working", ago(11));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", ago(10));
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");

        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "working", ago(62));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", ago(61));
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
        assert_eq!(typed(&f, "AGM 的派工"), 1);
    }

    /// 寬限內使用者自己送了一則：在跑的時候派工排在後面；使用者那一回合結束後照一般規則馬上送，不再等寬限。
    #[tokio::test]
    async fn a_prompt_the_user_sends_inside_the_grace_goes_first() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        crate::lifecycle::interrupt_grace::note_user_interrupt_at(&f.bot_id, chrono::Utc::now() - chrono::Duration::seconds(5));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        let mine = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&mine).bind(&f.conv).bind(&f.run_id).bind(db::now()).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "使用者那則在跑，派工排在後面");

        sqlx::query("UPDATE turns SET status = 'completed' WHERE id = ?").bind(&mine).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "使用者那一回合結束：照一般規則，不等寬限");
    }

    /// 一般回合結束（沒有中斷）照舊立刻送。
    #[tokio::test]
    async fn an_ordinary_turn_end_still_flushes_at_once() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 使用者直接在 pane 裡按 Esc（daemon 沒有事件）：從 transcript 認出來，一樣先等。
    #[tokio::test]
    async fn an_escape_pressed_in_the_pane_itself_is_read_from_the_transcript() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let path = tt::trusted_transcript(&app, &f.bot_id, &f.env.dir, "t.jsonl").await;
        let write = |at: chrono::DateTime<chrono::Utc>| {
            let marker = json!({"type": "user", "timestamp": at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true), "interruptedMessageId": "m",
                                "message": {"role": "user", "content": [{"type": "text", "text": "[Request interrupted by user]"}]}});
            let prompt = json!({"type": "user", "message": {"role": "user", "content": "長回合"}});
            std::fs::write(&path, format!("{prompt}\n{marker}\n")).unwrap();
        };
        sqlx::query("UPDATE runs SET transcript_path = ? WHERE id = ?").bind(path.to_string_lossy()).bind(&f.run_id).execute(app.db()).await.unwrap();

        write(chrono::Utc::now());
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "transcript 說剛被中斷");

        write(chrono::Utc::now() - chrono::Duration::seconds(120));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "working", ago(121));
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", ago(120));
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "兩分鐘前中斷、之後一直閒著：送");
    }

    /// 寬限內 AGM 直接派新的一件（沒有 in_flight，本來會直接打字）：一樣排進佇列等寬限。
    #[tokio::test]
    async fn a_dispatch_arriving_inside_the_grace_is_queued_instead_of_typed() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("DELETE FROM turns WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        note_user_interrupt(&f.bot_id);
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        let out = prompt_relayed_queueable(&app, &f.bot_id, "新的派工", "crid-grace", None, crate::lc_error::QueueClass::Relay).await.unwrap();
        assert_eq!(out.delivery, "queued");
        assert_eq!(typed(&f, "新的派工"), 0);
        assert!(queue_retry_timer_armed(&f.bot_id));
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "新的派工"), 0);
    }

    /// 2026-10-02 prorosal：使用者按 Esc 之後馬上在網頁送一句——那就是他接管要打的字，不能跟 AGM 派工一樣等滿寬限。
    #[tokio::test]
    async fn the_users_own_prompt_right_after_an_interrupt_is_not_held() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("DELETE FROM turns WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        note_user_interrupt(&f.bot_id);
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        let out = crate::lifecycle::prompt_from_api_queue_if_busy(&app, &f.bot_id, "用cc1 sonnets", "crid-own", &[], RelaySrc::trusted(None), false, None)
            .await
            .unwrap();
        assert_ne!(out.delivery, "queued", "使用者自己送的不排隊等寬限");
        assert_eq!(typed(&f, "用cc1 sonnets"), 1, "當場打進 pane");
    }

    /// 使用者自己在 bot 沒在跑時送的那一則（`awaits_start`，等它起來才送）：中斷寬限不擋它，那正是接管後要打的字（#1362）。
    #[tokio::test]
    async fn the_users_own_start_if_stopped_prompt_is_not_held_by_an_earlier_interrupt() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET origin = 'web', awaits_start = 1, awaits_idle = 0, prompt_text = '改好了，繼續' WHERE id = ?")
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        user_message_of(&app, &f, None).await;
        note_user_interrupt(&f.bot_id);
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "改好了，繼續"), 1, "使用者自己的那一句不等寬限");
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight");
    }

    /// 對照（#1362）：同樣是等啟動的一則，但帶轉寄來源（AGM／別的 bot 送的）照舊受寬限擋、不撤。
    #[tokio::test]
    async fn a_relayed_start_if_stopped_prompt_is_still_held_by_the_interrupt() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET origin = 'web', awaits_start = 1, awaits_idle = 0, prompt_text = '改好了，繼續' WHERE id = ?")
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        user_message_of(&app, &f, Some("other-bot")).await;
        note_user_interrupt(&f.bot_id);
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "別人的派工照舊等寬限");
        assert_eq!(typed(&f, "改好了，繼續"), 0);
        assert!(queue_retry_timer_armed(&f.bot_id));
    }

    /// 這一則的使用者訊息（`role = 'user'`），`relay_from` 空＝使用者自己從網頁送的。
    async fn user_message_of(app: &Arc<App>, f: &Fixture, relay_from: Option<&str>) {
        sqlx::query("UPDATE turns SET queue_class=? WHERE id=?")
            .bind(if relay_from.is_some() { "relay" } else { "user" })
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at, relay_from) VALUES (?,?,?,'user','改好了，繼續','web',?,?)",
        )
        .bind(db::ulid())
        .bind(&f.conv)
        .bind(&f.turn_id)
        .bind(db::now())
        .bind(relay_from)
        .execute(app.db())
        .await
        .unwrap();
    }

    /// 網頁按 Esc（interrupt_bot）端到端：收掉回合觸發的 flush 不會把排著的派工打進去。
    #[tokio::test]
    async fn the_web_escape_button_holds_the_queue_behind_it() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(&f.conv).bind(&f.run_id).bind(db::now()).execute(app.db()).await.unwrap();
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        interrupt_bot(&app, &f.bot_id).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert_eq!(typed(&f, "AGM 的派工"), 0);
    }

    /// 強制中止（abort_turns）也是使用者要接手：不撤排著的派工（裁示不變），只是先等寬限。
    #[tokio::test]
    async fn the_abort_button_holds_the_queue_behind_it_without_revoking_it() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE turns SET prompt_text = 'AGM 的派工' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        crate::lifecycle::stuck_turns::observe_at(&f.run_id, "idle", std::time::Instant::now());

        abort_turns(&app, &f.bot_id).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "不撤，也不立刻送");
        assert_eq!(typed(&f, "AGM 的派工"), 0);
    }

    /// 框裡有字時排隊的 prompt 一個字都不打、放回隊列；框清空後再 flush 就送出去（第七輪 #2）。
    #[tokio::test]
    async fn a_queued_prompt_waits_for_a_busy_box_and_goes_out_once_it_clears() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = 'Reply with PONG please' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { composer: vec!["我自己在打的草稿".into()], width: Some(120), ..Default::default() },
        );

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued", "沒送出就回隊列，不是 in-flight unknown");
        let writes = |f: &Fixture| f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "pane.send_keys").count();
        assert_eq!(writes(&f), 0);

        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "still inside its backoff");
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));
        let pane = f.env.herdr.pane("pane-1").unwrap();
        assert_eq!(pane.transcript.iter().filter(|l| l.contains("Reply with PONG please")).count(), 1);
    }

    /// 已軟刪的 bot（child 退役、專案刪除時沒鎖住的晚到 child）可能還留著 running 的 run：排著的 prompt 不能再打進
    /// 使用者已經刪掉的東西（#338 說的是直送的 prompt；flush 是同一個道理，不能比直送寬鬆）。
    #[tokio::test]
    async fn a_soft_deleted_bot_never_gets_its_queued_prompt_typed_into_its_pane() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        sqlx::query("UPDATE bots SET deleted_at = ? WHERE id = ?").bind(db::now()).bind(&f.bot_id).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "failed", "已刪的 bot 不該領走排著的 prompt，而是撤掉它");
        let writes = f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text" || *m == "pane.send_keys").count();
        assert_eq!(writes, 0, "一個字都不該打進已刪 bot 的 pane");
    }

    /// 排隊中的 grok 多行 prompt：沒有無損證據也照樣送出，turn 標成 unverified（delivery ok＋delivery_verified 0）。
    #[tokio::test]
    async fn a_queued_prompt_without_lossless_proof_goes_out_marked_unverified() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.delivery_verified), ("in_flight", "ok", 0));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
    }

    /// 框永遠有字：每次放回都記次數與下次時間，額度用完就明確失敗並留說明（同一個 transaction）。
    #[tokio::test]
    async fn a_box_that_never_empties_ends_in_a_failed_turn_with_an_explanation() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        let mut events = app.subscribe();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        for n in 1..=QUEUE_RETRY_LIMIT {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            let t = turn(&app, &f.turn_id).await;
            assert_eq!(t.status, "queued", "put back #{n}");
            assert_eq!(t.flush_retries, n);
            assert!(t.next_flush_at.is_some());
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.flush_retries, n, "early wake-ups do not spend retries");
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        }
        assert!(std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
            event.kind != "message_added" || event.data["message"]["turn_id"] != f.turn_id
        }), "重試上限之前不會先推失敗說明");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"));
        let hints: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_all(app.db())
            .await
            .unwrap();
        assert_eq!(hints.len(), 1);
        assert!(hints[0].contains("已停止自動重試"));
        let message: db::Message = sqlx::query_as("SELECT *, rowid AS seq FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_one(app.db())
            .await
            .unwrap();
        let message_events: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.kind == "message_added" && event.data["message"]["turn_id"] == f.turn_id)
            .collect();
        assert_eq!(message_events.len(), 1, "failed 說明只推一次");
        assert_eq!(message_events[0].data["bot_id"], f.bot_id);
        assert_eq!(message_events[0].data["message"], serde_json::to_value(message).unwrap(), "event carries the committed DB message");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0, "從頭到尾沒打過字");
    }

    /// codex 已知 session、rollout 還沒寫出來：先放回隊列等，等滿 CODEX_LOG_WAIT_RETRIES 次才退回 unverified 送出。
    #[tokio::test]
    async fn a_queued_codex_prompt_waits_for_its_rollout_then_falls_back() {
        let f = queued_kind("codex", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        let home = f.env.dir.join("codex-home-q");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
            .bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string())
            .bind(&f.bot_id)
            .execute(app.db())
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-q' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });

        for n in 1..=CODEX_LOG_WAIT_RETRIES {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.flush_retries), ("queued", n), "waiting for the rollout #{n}");
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        }
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0, "等的時候一個字都沒打");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str(), t.delivery_verified), ("in_flight", "ok", 0), "等過了就照樣送、標 unverified");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
    }

    /// 退避中重啟：記憶體裡的 timer 沒了，啟動時依 next_flush_at 重建每 bot 唯一的 timer，到期恰好送一次（sol 第九輪 #3）。
    ///
    /// 兩段都不跟真實時間賽跑（#203）。以前退避只剩 150 ms：負載一高，兩次重建之間 timer 就先燒掉、把自己從表上拿掉，
    /// 第二次重建又掛一個，「不疊 timer」被判成錯的（偶發紅）。先把到期設在一小時後，驗「每顆 bot 一個、照 next_flush_at 掛」；
    /// 再把到期改成現在、重建一次（較早的取代較晚的），驗到期送出、只送一次。
    #[tokio::test]
    async fn a_backoff_survives_a_restart_and_the_prompt_goes_out_exactly_once() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        forget_queue_retry_timer(&f.bot_id);
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });
        let due_in = |d: chrono::Duration| (chrono::Utc::now() + d).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("UPDATE turns SET next_flush_at = ? WHERE id = ?")
            .bind(due_in(chrono::Duration::hours(1)))
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();

        let fire = {
            let app = app.clone();
            move |bot: String| {
                let app = app.clone();
                tokio::spawn(async move {
                    let lock = app.bot_lock(&bot).await;
                    let _g = lock.lock().await;
                    flush_queued_locked(&app, &bot).await.unwrap();
                });
            }
        };
        assert_eq!(rearm_queue_retries_with(&app, fire.clone()).await.unwrap(), 1, "one timer for the bot");
        assert_eq!(rearm_queue_retries_with(&app, fire.clone()).await.unwrap(), 0, "a second pass does not add another");
        let left = queue_retry_timer_left(&f.bot_id).expect("armed");
        assert!(left > std::time::Duration::from_secs(55 * 60), "armed for next_flush_at, not sooner: {left:?}");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "not before it is due");

        sqlx::query("UPDATE turns SET next_flush_at = ? WHERE id = ?")
            .bind(due_in(chrono::Duration::zero()))
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        assert_eq!(rearm_queue_retries_with(&app, fire).await.unwrap(), 1, "the due one replaces the later timer");

        let _ = crate::testing::eventually!({
            let t = turn(&app, &f.turn_id).await;
            t.delivery != "pending" && t.status != "queued"
        });
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1, "exactly once");
    }

    /// The new web queue entry waits for a real idle status; when it can flush, user turns
    /// precede queued AGM dispatches, with created_at ordering preserved within the user lane.
    #[tokio::test]
    async fn three_queue_lanes_flush_user_first_without_losing_automation() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET awaits_idle=1, prompt_text='user first', created_at='2026-10-14T00:00:00Z' WHERE id=?")
            .bind(&f.turn_id).execute(app.db()).await.unwrap();
        let relay = db::ulid();
        let notice = db::ulid();
        for (id, lane, text) in [(&relay, "relay", "relay second"), (&notice, "daemon", "notice third")] {
            sqlx::query("INSERT INTO turns (id,conversation_id,origin,status,delivery,prompt_text,created_at,queue_class,awaits_idle) VALUES (?,?,'web','queued','pending',?,'2020-01-01T00:00:00Z',?,1)")
                .bind(id).bind(&f.conv).bind(text).bind(lane).execute(app.db()).await.unwrap();
        }
        for (id, text) in [(&f.turn_id, "user first"), (&relay, "relay second"), (&notice, "notice third")] {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, id).await.status, "in_flight", "flush selected {text}");
            assert_eq!(typed(&f, text), 1);
            for pending in [&relay, &notice] {
                if pending != id && typed(&f, if pending == &relay { "relay second" } else { "notice third" }) == 0 {
                    assert_eq!(turn(&app, pending).await.status, "queued");
                }
            }
            sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?")
                .bind(db::now()).bind(id).execute(app.db()).await.unwrap();
        }
        assert!(db::queued_turn(app.db(), &f.conv).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn awaits_idle_queue_turns_wait_for_idle_and_keep_created_at_order() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("DROP INDEX turns_one_queued_user").execute(app.db()).await.unwrap();

        let created = |seconds: i64| {
            format!("2024-01-01T00:00:{seconds:02}.000Z")
        };
        let awaits_idle = &f.turn_id;
        let assignment_turn = db::ulid();
        let awaits_start = db::ulid();
        sqlx::query("UPDATE turns SET awaits_idle=1, prompt_text='web waits for idle', created_at=? WHERE id=?")
            .bind(created(0))
            .bind(awaits_idle)
            .execute(app.db())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, queue_class)
             VALUES (?,?,'web','queued','pending','AGM dispatch',?,'relay')",
        )
        .bind(&assignment_turn)
        .bind(&f.conv)
        .bind(created(1))
        .execute(app.db())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at, awaits_start)
             VALUES (?,?,'web','queued','pending','start_if_stopped',?,1)",
        )
        .bind(&awaits_start)
        .bind(&f.conv)
        .bind(created(2))
        .execute(app.db())
        .await
        .unwrap();
        crate::supervisor::store::get_or_init(app.db()).await.unwrap();
        let assignment = crate::supervisor::store::insert_assignment(
            app.db(),
            None,
            &f.bot_id,
            "queue-order-agm",
            "AGM dispatch",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        crate::supervisor::store::mark_delivered(app.db(), &assignment.id, &assignment_turn, "queued")
            .await
            .unwrap();

        sqlx::query("UPDATE runs SET agent_status='unknown' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, awaits_idle).await.status, "queued", "awaits_idle does not treat unknown as idle");
        assert_eq!(turn(&app, &assignment_turn).await.status, "queued", "a later AGM turn does not pass the queue head");
        assert_eq!(turn(&app, &awaits_start).await.status, "queued", "a later #122 turn does not pass the queue head");
        assert_eq!(typed(&f, "web waits for idle"), 0, "one character is not sent before idle");

        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        for (id, expected) in [
            (awaits_idle.as_str(), "web waits for idle"),
            (awaits_start.as_str(), "start_if_stopped"),
            (assignment_turn.as_str(), "AGM dispatch"),
        ] {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, id).await.status, "in_flight", "created_at order selected {expected}");
            assert_eq!(typed(&f, expected), 1, "the {expected} turn was typed once");
            sqlx::query("UPDATE turns SET status='completed', completed_at=? WHERE id=?")
                .bind(db::now())
                .bind(id)
                .execute(app.db())
                .await
                .unwrap();
        }
    }

    /// Queue timers are memory-only, so a persisted awaits_idle turn with no backoff must be
    /// rediscovered after restart and delivered once the run is idle again.
    #[tokio::test]
    async fn an_awaits_idle_turn_is_flushed_after_daemon_restart() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET awaits_idle=1, prompt_text='after restart', next_flush_at=NULL WHERE id=?")
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);

        drop(app);
        let fresh = tt::restart_app(&f.env).await;
        let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = fired.clone();
        assert_eq!(
            rearm_queue_retries_with(&fresh, move |bot: String| seen.lock().unwrap().push(bot))
                .await
                .unwrap(),
            1,
            "a queued awaits_idle turn with no backoff is rearmed after restart",
        );
        let _ = crate::testing::eventually!(!fired.lock().unwrap().is_empty());
        assert_eq!(fired.lock().unwrap().as_slice(), &[f.bot_id.clone()]);

        let lock = fresh.bot_lock(&f.bot_id).await;
        let _guard = lock.lock().await;
        flush_queued_locked(&fresh, &f.bot_id).await.unwrap();
        assert_eq!(turn(&fresh, &f.turn_id).await.status, "in_flight");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
    }

    /// 排隊送：herdr 拒絕 `format: ansi` 照樣送出；讀不到畫面就放回隊列（不是 delivery=unknown 卡住後面的訊息）。
    #[tokio::test]
    async fn queued_prompts_survive_a_herdr_without_styled_reads_and_unreadable_panes() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = 'Reply with PONG please' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        f.env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "ok"));

        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = 'Reply with PONG please' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        f.env.herdr.live.lock().unwrap().remove("pane-1");
        f.env.herdr.set_screen("pane-1", "__READ_ERROR__");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("queued", "pending"), "put back, not parked as unknown");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    /// 證據檔在打字前讀不到：排隊中的 prompt 放回隊列，不停在 unknown，pane 零寫入（sol 第十一輪）。
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_evidence_file_puts_the_queued_prompt_back() {
        use std::os::unix::fs::PermissionsExt;
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        let t = f.env.dir.join("locked-q.jsonl");
        std::fs::write(&t, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 's-q', transcript_path = ? WHERE id = ?")
            .bind(t.to_str().unwrap())
            .bind(&f.run_id)
            .execute(app.db())
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        let tr = turn(&app, &f.turn_id).await;
        assert_eq!((tr.status.as_str(), tr.delivery.as_str()), ("queued", "pending"));
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    fn codex_queue_env<'a>(f: &'a Fixture) -> impl std::future::Future<Output = ()> + 'a {
        async move {
            let app = &f.env.app;
            db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
            let home = f.env.dir.join("codex-home-w");
            std::fs::create_dir_all(home.join("sessions")).unwrap();
            sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?")
                .bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string())
                .bind(&f.bot_id)
                .execute(app.db())
                .await
                .unwrap();
            sqlx::query("UPDATE turns SET prompt_text = '第一行\n第二行' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        }
    }

    async fn clear_backoff(f: &Fixture) {
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(f.env.app.db()).await.unwrap();
    }

    /// 先因為框忙放回好幾次，之後 session 已知但 rollout 未寫：rollout 的等待額度不被框忙吃掉，還是要等滿 3 次（sol 第十輪 #2）。
    #[tokio::test]
    async fn busy_put_backs_do_not_spend_the_rollout_wait() {
        let f = queued_kind("codex", "test").await;
        codex_queue_env(&f).await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, composer: vec!["草稿".into()], ..Default::default() });
        for _ in 0..3 {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            clear_backoff(&f).await;
        }
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.flush_retries, t.rollout_waits), (3, 0), "busy put-backs are not rollout waits");

        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });
        sqlx::query("UPDATE runs SET native_session_id = 'sess-busy' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        for n in 1..=CODEX_LOG_WAIT_RETRIES {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.rollout_waits), ("queued", n), "still waiting for the rollout #{n}");
            clear_backoff(&f).await;
        }
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "waited its three, then went out");
    }

    /// 等到一半 bot 重啟／換了 session：等待次數跟著新的 run／session 重新算。
    #[tokio::test]
    async fn a_new_run_or_session_starts_the_rollout_wait_over() {
        let f = queued_kind("codex", "test").await;
        codex_queue_env(&f).await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });
        sqlx::query("UPDATE runs SET native_session_id = 'sess-a' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        for _ in 0..2 {
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            clear_backoff(&f).await;
        }
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.rollout_waits, 2);
        assert_eq!(t.rollout_wait_key.as_deref(), Some(format!("{}:sess-a", f.run_id).as_str()));

        sqlx::query("UPDATE runs SET native_session_id = 'sess-b' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.rollout_waits), ("queued", 1), "counted again from one");
        assert_eq!(t.rollout_wait_key.as_deref(), Some(format!("{}:sess-b", f.run_id).as_str()));
        clear_backoff(&f).await;
        sqlx::query("UPDATE turns SET rollout_waits = 9 WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-c' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert_eq!(f.env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }

    /// AGM 派工排進來的 queued turn 沒有 `next_flush_at`（靠回合結束的事件送）。daemon 重啟後那個事件不會再來，
    /// 所以重啟掃描也要收它，不然就是永遠不會送的孤兒（AGM 2026-09-16）。
    #[tokio::test]
    async fn a_queued_turn_without_a_backoff_is_still_rearmed_after_a_restart() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET status='queued', run_id=NULL, next_flush_at=NULL, prompt_text='AGM 派的工作' WHERE id=?")
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);
        let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = fired.clone();
        let armed = rearm_queue_retries_with(&app, move |bot: String| seen.lock().unwrap().push(bot)).await.unwrap();
        assert_eq!(armed, 1, "沒有 next_flush_at 的 queued turn 也要重新掛上");
        let _ = crate::testing::eventually!(!fired.lock().unwrap().is_empty());
        assert_eq!(fired.lock().unwrap().as_slice(), &[f.bot_id.clone()], "立刻補一次 flush");
    }

    /// 真的「重啟」：同一個 DB 開一個新的 App，走啟動時真正呼叫的 `reconcile::rearm_progress`，timer 被接回來。
    #[tokio::test]
    async fn the_startup_path_rearms_a_backoff_on_a_fresh_app() {
        let f = queued_kind("grok", "test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane(
            "pane-1",
            crate::testing::LivePane { width: Some(120), boxed: true, composer: vec!["草稿".into()], ..Default::default() },
        );
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert!(queue_retry_timer_armed(&f.bot_id), "the first process armed it");

        forget_queue_retry_timer(&f.bot_id);
        drop(app);
        let fresh = crate::testing::restart_app(&f.env).await;
        assert!(!queue_retry_timer_armed(&f.bot_id));
        crate::reconcile::rearm_progress(&fresh).await;
        assert!(queue_retry_timer_armed(&f.bot_id), "startup re-armed it from next_flush_at");
        let again = rearm_queue_retries(&fresh).await.unwrap();
        assert_eq!(again, 0, "a second startup pass does not add another timer");
    }

    /// Regression: a client lookup failing after the claim abandoned the turn `in_flight` +
    /// `delivery='pending'` (nothing finishes that), 409-ing every later prompt. It must be requeued.
    #[tokio::test]
    async fn a_claim_that_cannot_be_delivered_goes_back_on_the_queue() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();

        flush_queued_locked(&app, &f.bot_id).await.expect("the flush itself does not error");

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued", "the claim was undone, not left in flight");
        assert_eq!(t.delivery, "pending");
        assert_eq!(t.run_id, None, "an undelivered turn does not belong to that run");
        assert!(t.completed_at.is_none(), "it was requeued, not failed");
        assert!(
            db::in_flight_turn(app.db(), &f.run_id).await.unwrap().is_none(),
            "nothing is in flight, so the next prompt is not refused with 409",
        );
        assert_eq!(
            db::queued_turn(app.db(), &f.conv).await.unwrap().map(|q| q.id),
            Some(f.turn_id.clone()),
            "the durable queue still holds it, so a later transition retries the delivery",
        );
        assert_eq!(t.flush_retries, 1, "算進重試次數，有上限");
        assert!(t.next_flush_at.is_some(), "有下一次嘗試的時間");
        assert!(queue_retry_timer_armed(&f.bot_id), "timer 已經掛上");

        f.env.herdr.set_agent("agent", "pane-1", true);
        sqlx::query("UPDATE runs SET herdr_session = 'test' WHERE id = ?")
            .bind(&f.run_id)
            .execute(app.db())
            .await
            .unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued", "still inside its backoff");
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "in_flight", "the retry got to claim it");
        assert_eq!(t.run_id.as_deref(), Some(f.run_id.as_str()));
    }

    /// issue #125：flush 讀完 run 到認領之間，run 被不拿 bot 鎖的路徑收掉（`mark_run_exited`：pane 死掉、
    /// reconcile）。重啟中那一段孤兒撤銷刻意不撤排著的派工；這時認領下去，回合掛在死掉的 run 上、
    /// 字打進不存在的 pane。認領只認領到此刻還在跑的 run，沒認領到的留在佇列給下一個 run。
    #[tokio::test]
    async fn a_queued_prompt_is_not_claimed_onto_a_run_that_ended_mid_flush() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '給舊 run 的派工' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let (app2, run2) = (app.clone(), f.run_id.clone());
        crate::lifecycle::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
            sqlx::query("UPDATE runs SET state='exited', ended_at=? WHERE id=?").bind(db::now()).bind(&run2).execute(app2.db()).await.unwrap();
        });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "留在佇列，不掛到死掉的 run 上");
        assert_eq!(typed(&f, "給舊 run 的派工"), 0, "一個字都沒打進舊 pane");
        assert_eq!(t.flush_retries, 0, "不是這一筆送不出去，不花它的重試");
    }

    fn window_until(mins: i64) -> String {
        (chrono::Utc::now() + chrono::Duration::minutes(mins)).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
    }

    /// issue #1677：排隊訊息在 working 的 bot 上不算送達臨界區（flush 這時不會開始打字），窗口拿得到。
    /// 但 flush 讀完 run（idle）、還沒認領的那一瞬 bot 又開始 working、窗口就在這時被拿走的話，
    /// 認領下去之後必須再看一次窗口：一個字都不能打進一個正要被重啟的 pane，這一筆留在佇列。
    #[tokio::test]
    async fn a_window_taken_between_the_flush_check_and_the_claim_stops_the_typing() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '窗口開了還打字' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        let (app2, run2) = (app.clone(), f.run_id.clone());
        crate::lifecycle::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run2).execute(app2.db()).await.unwrap();
            let taken = crate::supervisor::store::acquire_lease(app2.db(), "restart", "ops", None, None, &window_until(5), true, None, &json!({}))
                .await
                .unwrap();
            assert!(taken.is_some(), "working 的 bot 上排著的訊息不擋窗口：租約拿得到");
        });

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        assert_eq!(typed(&f, "窗口開了還打字"), 0, "窗口握著，一個字都不能打進 pane");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "認領撤回，留在佇列");
        assert!(
            db::in_flight_turn(app.db(), &f.run_id).await.unwrap().is_none(),
            "不留 in_flight＋pending：那會把窗口永遠擋在門外",
        );
    }

    /// issue #1683：認領之後才看到窗口（或讀不到），一個字都不打，而且不花送達重試——`flush_retries` 原封不動，不判 failed。
    /// 以前這條路照一般 put-back 加一次重試，seed 在上限（12）時直接把還沒送出的訊息判成失敗、請使用者重送。
    /// 掛到租約到期（讀不到就短退避）；窗口放掉、退避到了之後照常送一次。
    #[tokio::test]
    async fn a_window_seen_after_the_claim_spends_no_delivery_retry() {
        for (seeded, unreadable) in [(0, false), (QUEUE_RETRY_LIMIT, false), (0, true), (QUEUE_RETRY_LIMIT, true)] {
            let f = queued("test").await;
            let app = f.env.app.clone();
            f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
            db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
            sqlx::query("UPDATE turns SET prompt_text = '認領後才看到窗口', flush_retries = ? WHERE id = ?")
                .bind(seeded)
                .bind(&f.turn_id)
                .execute(app.db())
                .await
                .unwrap();
            let (app2, run2) = (app.clone(), f.run_id.clone());
            crate::lifecycle::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
                sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run2).execute(app2.db()).await.unwrap();
                let taken = crate::supervisor::store::acquire_lease(app2.db(), "restart", "ops", None, None, &window_until(5), true, None, &json!({}))
                    .await
                    .unwrap();
                assert!(taken.is_some(), "認領前的窗口查詢是空的，這時才拿到租約");
                if unreadable {
                    sqlx::query("UPDATE supervisor_leases SET expires_at = 'not-a-time' WHERE resource = 'restart'")
                        .execute(app2.db())
                        .await
                        .unwrap();
                }
            });

            flush_queued_locked(&app, &f.bot_id).await.unwrap();

            assert_eq!(typed(&f, "認領後才看到窗口"), 0, "窗口握著（或讀不到），一個字都不能打進 pane");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!(
                (t.status.as_str(), t.run_id.as_deref(), t.flush_retries),
                ("queued", None, seeded),
                "放回佇列，送達重試一次都不花（seed={seeded}, unreadable={unreadable}）",
            );
            assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "掛了 timer 等租約到期或短退避");
            assert!(db::in_flight_turn(app.db(), &f.run_id).await.unwrap().is_none(), "不留 in_flight＋pending");

            crate::supervisor::maintenance::release_restart_on_startup(&app).await;
            sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
            forget_queue_retry_timer(&f.bot_id);
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "窗口放掉之後照常認領");
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(typed(&f, "認領後才看到窗口"), 1, "只送一次，不重複");
        }
    }

    /// issue #1683：認領之後的窗口放回佇列那一句寫不進去：記成欠著（flush 回 `Err`，不留 in_flight＋pending）。
    /// DB 好了補上，仍然不花重試；窗口放掉之後照常送一次。
    #[tokio::test]
    async fn a_window_release_that_cannot_be_written_is_owed_and_still_spends_no_retry() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        sqlx::query("UPDATE turns SET prompt_text = '欠著也不花重試', flush_retries = ? WHERE id = ?")
            .bind(QUEUE_RETRY_LIMIT)
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();
        forget_queue_retry_timer(&f.bot_id);
        sqlx::query("CREATE TRIGGER lost_window_release BEFORE UPDATE OF next_flush_at ON turns BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END")
            .execute(app.db())
            .await
            .unwrap();
        let (app2, run2) = (app.clone(), f.run_id.clone());
        crate::lifecycle::race_point::arm("flush_before_claim", &f.bot_id, move || async move {
            sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run2).execute(app2.db()).await.unwrap();
            let taken = crate::supervisor::store::acquire_lease(app2.db(), "restart", "ops", None, None, &window_until(5), true, None, &json!({}))
                .await
                .unwrap();
            assert!(taken.is_some(), "認領前的窗口查詢是空的，這時才拿到租約");
        });

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "放不回去，flush 不回普通的成功");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(
            (t.status.as_str(), t.delivery.as_str(), t.flush_retries),
            ("in_flight", "pending", QUEUE_RETRY_LIMIT),
            "寫不進去就還沒放回去，重試次數也還沒動",
        );
        assert_eq!(typed(&f, "欠著也不花重試"), 0, "窗口握著，一個字都沒打");

        sqlx::query("DROP TRIGGER lost_window_release").execute(app.db()).await.unwrap();
        crate::lifecycle::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("DB 好了：補上");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!(
            (t.status.as_str(), t.run_id.as_deref(), t.flush_retries),
            ("queued", None, QUEUE_RETRY_LIMIT),
            "補回佇列，重試次數不動",
        );
        assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "掛了下一次的 timer");

        crate::supervisor::maintenance::release_restart_on_startup(&app).await;
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "窗口放掉之後照常認領");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "欠著也不花重試"), 1, "只送一次，不重複");
    }

    /// issue #1677 (a)：換版前排隊在 working 的 bot 上、窗口拿到了，換版期間回合結束；新 daemon 起來（開機放掉窗口、
    /// 重掛排隊）之後照樣送出，而且只送一次。
    #[tokio::test]
    async fn a_prompt_queued_behind_a_working_turn_is_sent_once_after_a_restart_that_took_the_window() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        db::set_pane_typed(app.db(), &f.run_id).await.unwrap();
        f.env.herdr.live_pane("pane-1", crate::testing::LivePane { width: Some(120), ..Default::default() });
        sqlx::query("UPDATE turns SET prompt_text = '換版後才送', next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES ('busy', ?, ?, 'web', 'in_flight', 'ok', ?)")
            .bind(&f.conv).bind(&f.run_id).bind(db::now()).execute(app.db()).await.unwrap();

        let taken = crate::supervisor::store::acquire_lease(app.db(), "restart", "ops", None, None, &window_until(5), true, None, &json!({}))
            .await
            .unwrap();
        assert!(taken.is_some(), "working 的回合後面排著訊息：窗口拿得到");
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");

        sqlx::query("UPDATE turns SET status='completed' WHERE id='busy'").execute(app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&f.run_id).execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        drop(app);
        let fresh = tt::restart_app(&f.env).await;
        crate::supervisor::maintenance::release_restart_on_startup(&fresh).await;
        assert!(fresh.window_held().await.unwrap().is_none(), "新 daemon 起來就放掉窗口");
        let fired = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = fired.clone();
        assert_eq!(rearm_queue_retries_with(&fresh, move |bot: String| seen.lock().unwrap().push(bot)).await.unwrap(), 1, "重啟後重掛排隊");
        let _ = crate::testing::eventually!(!fired.lock().unwrap().is_empty());

        let lock = fresh.bot_lock(&f.bot_id).await;
        let _guard = lock.lock().await;
        flush_queued_locked(&fresh, &f.bot_id).await.unwrap();
        assert_eq!(turn(&fresh, &f.turn_id).await.status, "in_flight");
        flush_queued_locked(&fresh, &f.bot_id).await.unwrap();
        assert_eq!(typed(&f, "換版後才送"), 1, "只送一次，不重複");
    }

    /// issue #125：放回佇列之前，這一筆已經被不拿 bot 鎖的路徑收掉（run 結束的 `fail_in_flight`）。
    /// 以前 `defer_queued_turn` 不看 CAS 有沒有打中，照樣回報「放回去了」並掛 retry timer、記一行 put back。
    #[tokio::test]
    async fn a_put_back_that_loses_to_a_run_exit_does_not_pretend_it_requeued() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        let mut events = app.subscribe();
        forget_queue_retry_timer(&f.bot_id);
        let (app2, turn2) = (app.clone(), f.turn_id.clone());
        crate::lifecycle::race_point::arm("defer_before_return", &f.turn_id, move || async move {
            crate::lifecycle::turn_controller::fail(app2.db(), &turn2, crate::lifecycle::turn_controller::DeliveryOnFail::Keep, "run ended")
                .await
                .unwrap();
        });

        flush_queued_locked(&app, &f.bot_id).await.expect("the flush itself does not error");

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "failed", "別的路徑收掉的就是收掉了");
        assert_eq!((t.flush_retries, t.next_flush_at.clone()), (0, None), "沒有放回去，就不記一次重試");
        assert!(!queue_retry_timer_armed(&f.bot_id), "沒有東西在排隊，不掛 retry timer");
        assert!(std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
            event.kind != "message_added" || event.data["message"]["turn_id"] != f.turn_id
        }), "CAS 已輸掉時不會插入或廣播系統訊息");
    }

    /// #831：放回佇列讀完重試次數、還沒寫的那一瞬，一個不相干的 writer commit 了一筆。deferred 交易這時升級寫鎖直接 517，
    /// 這一筆放不回去；寫鎖從讀之前就拿著，插進來的那一筆等，照樣放回佇列、記一次重試。
    #[tokio::test]
    async fn an_unrelated_writer_between_the_retry_read_and_the_put_back_does_not_fail_it() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        let mut events = app.subscribe();
        forget_queue_retry_timer(&f.bot_id);
        let other = crate::testing::arm_app_foreign_writer(&app, "defer_after_retries_read", &f.turn_id);

        flush_queued_locked(&app, &f.bot_id).await.expect("the flush itself does not error");

        assert_eq!(*other.lock().unwrap(), Some(false), "the put-back holds the write lock from its read on; the other writer waits");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.flush_retries), ("queued", 1), "put back on the queue with one retry spent");
        assert!(queue_retry_timer_armed(&f.bot_id), "the retry timer is armed for it");
        assert!(std::iter::from_fn(|| events.try_recv().ok()).all(|event| {
            event.kind != "message_added" || event.data["message"]["turn_id"] != f.turn_id
        }), "尚未達到重試上限時不會推失敗說明");
    }

    /// Once the RPC went out, a failure is not requeued (could deliver twice); `delivery='unknown'`
    /// parks it. The mock's `unsupported` answer to `agent.prompt` is exactly that case.
    #[tokio::test]
    async fn a_failure_after_the_rpc_is_parked_as_unknown_not_requeued() {
        let f = queued("test").await;
        f.env.herdr.set_agent("agent", "pane-1", true);
        let app = f.env.app.clone();

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "in_flight");
        assert_eq!(t.delivery, "unknown");
        assert!(db::queued_turn(app.db(), &f.conv).await.unwrap().is_none(), "not put back on the queue");
    }

    /// The queue gets the live prompt's screen checks (review 2026-09-12 #6): codex on its `/model`
    /// picker (mock can't Esc) → back on the queue with the hint.
    #[tokio::test]
    async fn a_queued_prompt_waits_while_codex_shows_its_model_picker() {
        let f = queued_kind("codex", "test").await;
        let app = f.env.app.clone();
        f.env.herdr.set_screen("pane-1", "Select Model and Effort\n› 1. gpt-5 (current)\n  2. gpt-5-mini\n\nPress enter to confirm or esc to go back\n");

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued", "put back, not delivered into the menu");
        assert_eq!(t.run_id, None);
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt"), "nothing was typed");
        let hints: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_all(app.db())
            .await
            .unwrap();
        assert_eq!(hints.len(), 1, "{hints:?}");
        assert!(hints[0].contains("/model"), "{hints:?}");
    }

    /// claude parked on its login menu is the same story: `needs_login`, back on the queue.
    #[tokio::test]
    async fn a_queued_prompt_waits_while_claude_shows_its_login_menu() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.set_screen("pane-1", "Select login method:\n❯ 1. Claude account with subscription\n  2. Anthropic Console account\n");

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "queued");
        assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "畫面擋住也要自己掛 timer（L2）");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "agent.prompt"));
        let hints: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE conversation_id=? AND role='system'")
            .bind(&f.conv)
            .fetch_one(app.db())
            .await
            .unwrap();
        assert_eq!(hints, 1);
    }

    #[tokio::test]
    async fn a_queued_prompt_stays_queued_when_its_dialog_preflight_is_unreadable() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        f.env.herdr.set_agent("agent", "pane-1", true);
        f.env.herdr.set_screen("pane-1", crate::tui_prompts::screens::DANGEROUS_RM);
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        forget_queue_retry_timer(&f.bot_id);

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.run_id.as_deref()), ("queued", None), "the claimed prompt is put back");
        assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "pane uncertainty schedules another check");
        for method in ["agent.prompt", "pane.send_text", "pane.send_keys"] {
            assert!(f.env.herdr.calls_to(method).is_empty(), "unreadable preflight must not call {method}");
        }
    }

    /// 之後讓放回佇列（寫 `flush_retries`）或收成 failed 的寫入失敗（SQLite 這一刻寫不進去）；認領那一句照常。
    async fn lose(app: &Arc<App>, name: &str, what: &str) {
        let on = if what == "failed" { "status ON turns WHEN NEW.status = 'failed'" } else { "flush_retries ON turns" };
        sqlx::query(&format!("CREATE TRIGGER {name} BEFORE UPDATE OF {on} BEGIN SELECT RAISE(ABORT, 'disk I/O error'); END"))
            .execute(app.db())
            .await
            .unwrap();
    }

    async fn heal(app: &Arc<App>, name: &str) {
        sqlx::query(&format!("DROP TRIGGER {name}")).execute(app.db()).await.unwrap();
    }

    /// #158 驗收二：認領之後一個字都沒打就放棄（claude 停在登入選單、herdr client 拿不到），放回佇列那一句卻寫不進去——
    /// 以前只記一行 error、flush 回 Ok，那一筆停在 in_flight＋pending：之後每一則都被擋，交辦看起來已經離開佇列，其實沒送。
    /// 現在 flush 回錯誤、記成欠著；DB 好了補回佇列（照退避、算一次重試），擋住它的東西沒了就照常送，從頭到尾只送一次。
    #[tokio::test]
    async fn a_claim_whose_put_back_cannot_be_written_is_owed_not_left_in_flight() {
        for (why, session) in [("pane not ready", "test"), ("no herdr client", "no-such-session")] {
            let f = queued(session).await;
            let app = f.env.app.clone();
            if session == "test" {
                f.env.herdr.set_screen("pane-1", "Select login method:\n❯ 1. Claude account with subscription\n  2. Anthropic Console account\n");
            }
            forget_queue_retry_timer(&f.bot_id);
            lose(&app, "lost_put_back", "put_back").await;

            assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "{why}：放不回去，flush 不回普通的成功");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.delivery.as_str(), t.flush_retries), ("in_flight", "pending", 0), "{why}：寫不進去就是還沒放回去");

            heal(&app, "lost_put_back").await;
            crate::lifecycle::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("DB 好了：補上");
            let t = turn(&app, &f.turn_id).await;
            assert_eq!((t.status.as_str(), t.run_id.as_deref(), t.flush_retries), ("queued", None, 1), "{why}：補回佇列，算一次重試");
            assert!(t.next_flush_at.is_some() && queue_retry_timer_armed(&f.bot_id), "{why}：掛了下一次的 timer");
            assert!(db::in_flight_turn(app.db(), &f.run_id).await.unwrap().is_none(), "{why}：不再擋住下一則");

            f.env.herdr.set_screen("pane-1", "");
            f.env.herdr.set_agent("agent", "pane-1", true);
            sqlx::query("UPDATE runs SET herdr_session = 'test' WHERE id = ?").bind(&f.run_id).execute(app.db()).await.unwrap();
            sqlx::query("UPDATE turns SET next_flush_at = NULL WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
            flush_queued_locked(&app, &f.bot_id).await.unwrap();
            assert_eq!(turn(&app, &f.turn_id).await.status, "in_flight", "{why}");
            assert_eq!(f.env.herdr.calls_to("agent.prompt").len(), 1, "{why}：只送一次");
        }
    }

    /// #158 驗收三：排隊的那一則在這個 bot 上永遠送不出去（太長、證明不了，`NotAttempted { retry: false }`），收成 failed
    /// 那一句卻寫不進去——以前只記一行 error、flush 回 Ok，那一筆停在 in_flight＋pending。現在記成欠著、flush 回錯誤；
    /// DB 好了收成 failed＋一則說明，一個字都沒打。
    #[tokio::test]
    async fn an_unsendable_queued_prompt_whose_failure_cannot_be_written_is_owed() {
        let f = queued("test").await;
        let app = f.env.app.clone();
        let huge = "x".repeat(crate::lifecycle::delivery::MAX_PROVABLE_CHARS + 1);
        sqlx::query("UPDATE turns SET prompt_text = ? WHERE id = ?").bind(&huge).bind(&f.turn_id).execute(app.db()).await.unwrap();
        lose(&app, "lost_close", "failed").await;

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "收不成 failed：flush 不回普通的成功");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("in_flight", "pending"));

        heal(&app, "lost_close").await;
        crate::lifecycle::owed_delivery::settle_locked(&app, &f.bot_id).await.expect("DB 好了：補上");
        let t = turn(&app, &f.turn_id).await;
        assert_eq!((t.status.as_str(), t.delivery.as_str()), ("failed", "failed"), "不留永久 in_flight");
        let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id = ? AND role = 'system'")
            .bind(&f.turn_id)
            .fetch_all(app.db())
            .await
            .unwrap();
        assert!(notes.len() == 1 && notes[0].contains("prompt_too_long_to_prove"), "{notes:?}");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text" || m == "agent.prompt"), "一個字都沒打");
    }

    /// #158（同一類，還沒認領的那一段）：空的 prompt 收成 failed 那一句寫不進去——以前 `let _ =` 吞掉、回 Ok、不掛 timer，
    /// 那一筆佔著這個對話唯一的 queued 名額，直到有人剛好叫醒 flush。現在回錯誤、留在佇列、掛 timer；DB 好了就收掉。
    #[tokio::test]
    async fn an_empty_queued_prompt_that_cannot_be_dropped_stays_queued_and_is_retried() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET prompt_text = '   ' WHERE id = ?").bind(&f.turn_id).execute(app.db()).await.unwrap();
        forget_queue_retry_timer(&f.bot_id);
        lose(&app, "lost_drop", "failed").await;

        assert!(flush_queued_locked(&app, &f.bot_id).await.is_err(), "收不掉：不回普通的成功");
        assert_eq!(turn(&app, &f.turn_id).await.status, "queued");
        assert!(queue_retry_timer_armed(&f.bot_id), "掛了 timer，稍後再收");

        heal(&app, "lost_drop").await;
        flush_queued_locked(&app, &f.bot_id).await.unwrap();
        assert_eq!(turn(&app, &f.turn_id).await.status, "failed");
    }

    /// An empty queued prompt is dropped, not requeued ("always put it back" would loop forever).
    #[tokio::test]
    async fn an_empty_queued_prompt_is_still_failed_not_requeued() {
        let f = queued("no-such-session").await;
        let app = f.env.app.clone();
        sqlx::query("UPDATE turns SET prompt_text = '   ' WHERE id = ?")
            .bind(&f.turn_id)
            .execute(app.db())
            .await
            .unwrap();

        flush_queued_locked(&app, &f.bot_id).await.unwrap();

        let t = turn(&app, &f.turn_id).await;
        assert_eq!(t.status, "failed");
        assert!(db::queued_turn(app.db(), &f.conv).await.unwrap().is_none());
    }
}
