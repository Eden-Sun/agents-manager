mod tests {
    use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::lifecycle::poller::observed_status::*;
use crate::lifecycle::poller::observed_status::test_seams::*;
    use crate::capabilities::Db;
    use crate::testing as tt;

    const BUSY: &str = "❯ Work\n✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n──────\n❯\n──────\n";
    const IDLE: &str = "❯ Work\n⏺ Done\n✻ Cooked for 2s\n──────\n❯\n──────\n";
    const DRAFT: &str = "❯ Work\n⏺ Done\n✻ Cooked for 2s\n──────\n❯ ship it to prod now\n──────\n";

    struct Fixture {
        env: tt::Env,
        run: db::Run,
        turn: String,
        client: RunClient,
    }

    async fn fixture() -> Fixture {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "observed").await;
        let id = tt::fake_run(&env.app, &bot.id).await;
        let run = db::run(env.app.db(), &id).await.unwrap().unwrap();
        let conv = db::conversation_id(env.app.db(), &bot.id).await.unwrap();
        let turn = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(&turn).bind(conv).bind(&id).bind(db::now()).execute(env.app.db()).await.unwrap();
        let client = client_for_run(&env.app, &run).await.unwrap();
        env.herdr.agents.lock().unwrap().push(json!({
            "name": "agent", "agent": "claude", "pane_id": run.pane_id,
            "agent_status": "done", "revision": 1, "completion_seq": 30865,
        }));
        env.herdr.set_screen(run.pane_id.as_deref().unwrap(), BUSY);
        Fixture {
            env,
            run,
            turn,
            client,
        }
    }

    async fn status(f: &Fixture) -> String {
        db::run(f.env.app.db(), &f.run.id)
            .await
            .unwrap()
            .unwrap()
            .agent_status
    }

    async fn sample(f: &Fixture, text: &str) {
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        let stamp = Stamp::capture(&f.env.app, &run).await.unwrap();
        observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", text)
            .await
            .unwrap();
    }

    async fn complete(f: &Fixture) {
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&f.turn)
            .execute(f.env.app.db())
            .await
            .unwrap();
        f.env
            .herdr
            .set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
    }

    const PERMISSION: &str = "────────────────────────────────
 Bash command
 echo fixture
 Do you want to proceed?
 ❯ 1. Yes
   2. No
 Esc to cancel
";

    async fn permission_fixture() -> Fixture {
        let f = fixture().await;
        sqlx::query("UPDATE runs SET native_session_id='native-1',agent_status='working' WHERE id=?")
            .bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), PERMISSION);
        f
    }

    fn notification(f: &Fixture) -> crate::hookrecv::HookBody {
        crate::hookrecv::HookBody {
            bot_id: f.run.bot_id.clone(), provider: "claude".into(),
            payload: json!({"hook_event_name":"Notification", "notification_type":"permission_prompt", "session_id":"native-1"}),
            received_at: Some(db::now()), truncated: false, run_id: Some(f.run.id.clone()),
        }
    }

    #[tokio::test]
    async fn permission_notification_wakes_fresh_screen_and_replay_is_idempotent() {
        let f = permission_fixture().await;
        assert!(crate::tui_prompts::permission_prompt(PERMISSION).is_some());
        let body = notification(&f);
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        let since = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap().agent_status_since;
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        assert_eq!(db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap().agent_status_since, since);
        assert_eq!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap().unwrap().id, f.turn);
    }

    #[tokio::test]
    async fn remote_permission_notification_is_processed_once_from_inbox() {
        let f = permission_fixture().await;
        let body = notification(&f);
        assert!(crate::hook_inbox::accept(f.env.app.db(), &body, crate::hook_inbox::Source::Remote).await.unwrap().is_new());
        assert_eq!(crate::runners::hook_inbox::drain_once(&f.env.app).await.unwrap(), 1);
        assert_eq!(status(&f).await, "blocked");
        assert!(!crate::hook_inbox::accept(f.env.app.db(), &body, crate::hook_inbox::Source::Remote).await.unwrap().is_new());
        assert_eq!(crate::runners::hook_inbox::drain_once(&f.env.app).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn permission_notification_retries_when_bot_lock_is_busy() {
        let f = permission_fixture().await;
        let body = notification(&f);
        let lock = f.env.app.bot_lock(&f.run.bot_id).await;
        let guard = lock.lock().await;
        assert!(crate::hookrecv::process(&f.env.app, &body).await.is_err());
        assert_eq!(status(&f).await, "working");
        drop(guard);
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
    }

    #[tokio::test]
    async fn permission_notification_rejects_old_or_unrelated_input() {
        let f = permission_fixture().await;
        let body = notification(&f);
        for variant in 0..10 {
            let mut event = body.clone();
            match variant {
                0 => event.run_id = Some("old-run".into()),
                1 => event.run_id = None,
                2 => event.payload["session_id"] = json!("old-session"),
                3 => event.payload["session_id"] = json!(null),
                4 => event.received_at = Some(db::iso_in(-300)),
                5 => event.received_at = None,
                6 => event.payload["notification_type"] = json!("idle_prompt"),
                7 => event.payload["agent_id"] = json!("child"),
                8 => event.payload["agent_id"] = json!(1),
                _ => event.truncated = true,
            }
            crate::hookrecv::process(&f.env.app, &event).await.unwrap();
            assert_eq!(status(&f).await, "working", "{variant}");
        }
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        assert_eq!(status(&f).await, "working");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        assert_eq!(status(&f).await, "working");
    }

    #[tokio::test]
    async fn late_permission_notification_cannot_reopen_a_terminal_turn() {
        let f = permission_fixture().await;
        let body = notification(&f);
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        assert_eq!(status(&f).await, "working");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn permission_notification_cannot_reopen_a_turn_completed_during_read() {
        let f = permission_fixture().await;
        let app = f.env.app.clone();
        let turn = f.turn.clone();
        crate::lifecycle::race_point::arm("observed_menu_before_cas", &f.run.id, move || async move {
            sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
                .bind(db::now()).bind(&turn).execute(app.db()).await.unwrap();
        });
        crate::hookrecv::process(&f.env.app, &notification(&f)).await.unwrap();
        assert_eq!(status(&f).await, "working");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn permission_notification_cannot_cross_session_or_run_change_during_read() {
        for field in ["native_session_id", "state"] {
            let f = permission_fixture().await;
            let app = f.env.app.clone();
            let id = f.run.id.clone();
            crate::lifecycle::race_point::arm("observed_menu_before_cas", &f.run.id, move || async move {
                let query = if field == "state" { "UPDATE runs SET state='stopped' WHERE id=?" } else { "UPDATE runs SET native_session_id='new-session' WHERE id=?" };
                sqlx::query(query).bind(&id).execute(app.db()).await.unwrap();
            });
            crate::hookrecv::process(&f.env.app, &notification(&f)).await.unwrap();
            assert_eq!(status(&f).await, "working", "{field}");
            assert!(!menu_owned(&f.run.id));
        }
    }

    #[tokio::test]
    async fn prompt_submit_does_not_confirm_delivery() {
        let f = permission_fixture().await;
        sqlx::query("UPDATE turns SET delivery_verified=0 WHERE id=?").bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        let mut body = notification(&f);
        body.payload = json!({"hook_event_name":"UserPromptSubmit", "session_id":"native-1", "prompt":"fixture"});
        crate::hookrecv::process(&f.env.app, &body).await.unwrap();
        let verified: i64 = sqlx::query_scalar("SELECT delivery_verified FROM turns WHERE id=?").bind(&f.turn).fetch_one(f.env.app.db()).await.unwrap();
        assert_eq!(verified, 0);
        assert_eq!(status(&f).await, "working");
    }

    #[tokio::test]
    async fn p63_synthetic_in_flight_menu_is_blocked() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked", "in-flight menu should be visible");
    }

    #[tokio::test]
    async fn in_flight_menu_blocks_only_delivered_turn_and_clears_after_two_reads() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        sqlx::query("UPDATE turns SET delivery='pending' WHERE id=?").bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "idle");
        sqlx::query("UPDATE turns SET delivery='ok' WHERE id=?").bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        let now = std::time::Instant::now();
        crate::lifecycle::stuck_turns::observe_at(&f.run.id, "idle", now - std::time::Duration::from_secs(600));
        let closed = crate::lifecycle::stuck_turns::sweep_at(&f.env.app, None, now, std::time::Duration::from_secs(300)).await;
        assert!(!closed.contains(&f.turn));
        assert!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap().is_some());
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "__READ_ERROR__");
        observe_menu(&f.env.app, &run).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown");
        assert_eq!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id), 0);
    }

    #[tokio::test]
    async fn in_flight_menu_receipt_cannot_restore_a_new_turn() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        complete(&f).await;
        let conversation = db::conversation_id(f.env.app.db(), &f.run.bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(conversation).bind(&f.run.id).bind(db::iso_in(1))
            .execute(f.env.app.db()).await.unwrap();
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn delayed_in_flight_menu_read_cannot_block_a_new_turn() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let app = f.env.app.clone(); let turn = f.turn.clone(); let run_id = f.run.id.clone();
        let bot_id = f.run.bot_id.clone();
        crate::lifecycle::race_point::arm("observed_menu_before_cas", &f.run.id, move || async move {
            sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
                .bind(db::now()).bind(&turn).execute(app.db()).await.unwrap();
            let conversation = db::conversation_id(app.db(), &bot_id).await.unwrap();
            sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
                .bind(db::ulid()).bind(conversation).bind(&run_id).bind(db::iso_in(1))
                .execute(app.db()).await.unwrap();
        });
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "idle");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn in_flight_menu_allows_normal_turn_completion() {
        let f = fixture().await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.live_pane(f.run.pane_id.as_deref().unwrap(), crate::testing::LivePane { width: Some(120), ..Default::default() });
        let queued = crate::lifecycle::prompt_from_api_queue_if_busy(
            &f.env.app, &f.run.bot_id, "Synthetic next message", "p63-queue", &[],
            crate::lifecycle::RelaySrc::trusted(None), false, None,
        ).await.unwrap();
        assert_eq!(queued.delivery, "queued");
        let hook = crate::hookrecv::HookBody {
            bot_id: f.run.bot_id.clone(), provider: "claude".into(),
            payload: json!({"hook_event_name": "Stop", "last_assistant_message": "Synthetic completed answer"}),
            received_at: None, truncated: false, run_id: Some(f.run.id.clone()),
        };
        crate::hookrecv::process(&f.env.app, &hook).await.unwrap();
        assert!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap().is_none());
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown", "the queued turn must not invalidate the menu owner");
        crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id);
        let ev = crate::herdr::Event {
            event: "pane_agent_status_changed".into(),
            data: json!({"agent": "claude", "pane_id": f.run.pane_id, "agent_status": "idle"}),
        };
        crate::runners::events::handle_status(&f.env.app, "local", "test", &ev).await;
        assert_eq!(status(&f).await, "idle");
        assert!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id) > 0,
            "the observed idle transition must request a queue flush");
        crate::lifecycle::queue::flush_queued_locked(&f.env.app, &f.run.bot_id).await.unwrap();
        let delivered = tt::eventually!(db::in_flight_turn(f.env.app.db(), &f.run.id).await.unwrap()
            .is_some_and(|turn| turn.id == queued.turn_id && turn.delivery == "ok"));
        let row: (String, String, Option<String>, Option<String>, i64) = sqlx::query_as(
            "SELECT status,delivery,run_id,start_error,flush_retries FROM turns WHERE id=?")
            .bind(&queued.turn_id).fetch_one(f.env.app.db()).await.unwrap();
        eprintln!("queue metadata: {:?}, prompt calls: {}, pane send calls: {}", row,
            f.env.herdr.calls_to("agent.prompt").len(), f.env.herdr.calls_to("pane.send_text").len());
        assert!(delivered, "the exact queued turn must be delivered after the observed idle event");
    }

    #[tokio::test]
    async fn a_completed_fallback_with_a_live_menu_is_blocked_without_an_in_flight_turn() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked", "a live menu must be visible after the turn is closed");
    }

    #[tokio::test]
    async fn menu_requires_two_fresh_clear_reads_and_releases_to_unknown() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "__READ_ERROR__");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked", "failed reads reset the consecutive count");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown");
        assert_eq!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id), 0);
    }

    #[tokio::test]
    async fn late_working_finish_does_not_remove_menu_receipt() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?")
            .bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert!(menu_owned(&f.run.id));
        assert_eq!(status(&f).await, "blocked");
    }

    #[tokio::test]
    async fn menu_followed_by_working_also_releases_only_after_two_reads() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), BUSY);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "unknown");
        assert_eq!(crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id), 0);
    }

    #[tokio::test]
    async fn a_new_turn_fences_the_old_menu_receipt() {
        let f = fixture().await;
        sqlx::query("UPDATE turns SET status='completed_fallback',completed_at=? WHERE id=?")
            .bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "Choose an option\n❯ 1. First\n  2. Second\n");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        let conversation = db::conversation_id(f.env.app.db(), &f.run.bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid()).bind(conversation).bind(&f.run.id).bind(db::iso_in(1))
            .execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        observe_menu(&f.env.app, &run).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
        assert!(!menu_owned(&f.run.id));
    }

    #[tokio::test]
    async fn cached_idle_with_live_busy_is_working_then_completed_is_idle() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        assert_eq!(
            status(&f).await,
            "working",
            "Herdr's stale done must not hide a real busy pane"
        );
        complete(&f).await;
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "idle");
        assert_eq!(
            crate::lifecycle::queue::take_scheduled_flush_count(&f.run.bot_id),
            1,
            "restoring idle must wake the queue even without a Herdr idle edge"
        );
    }

    #[tokio::test]
    async fn an_unsent_draft_in_the_composer_does_not_block_the_restore() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        assert_eq!(status(&f).await, "working");
        complete(&f).await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), DRAFT);
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "idle", "composer draft must not keep the run working");
    }

    async fn stale_working(f: &Fixture, minutes_ago: i64, screen: &str) -> db::Run {
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        let old = db::iso_in(-minutes_ago * 60);
        sqlx::query("UPDATE runs SET agent_status_since=? WHERE id=?").bind(&old).bind(&f.run.id).execute(f.env.app.db()).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?").bind(&old).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), screen);
        db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_run_stuck_at_working_after_its_turn_ended_is_settled_by_the_sweep() {
        let f = fixture().await;
        let run = stale_working(&f, 10, DRAFT).await;
        assert!(settle_stale_working(&f.env.app, &run).await.unwrap());
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn the_sweep_leaves_fresh_busy_or_in_flight_runs_alone() {
        let f = fixture().await;
        let run = stale_working(&f, 1, IDLE).await;
        assert!(!settle_stale_working(&f.env.app, &run).await.unwrap(), "not stale long enough");
        let run = stale_working(&f, 10, BUSY).await;
        assert!(!settle_stale_working(&f.env.app, &run).await.unwrap(), "spinner still on screen");
        let run = stale_working(&f, 10, IDLE).await;
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT ?,conversation_id,run_id,'web','in_flight','ok',? FROM turns WHERE id=?")
            .bind(db::ulid()).bind(db::now()).bind(&f.turn).execute(f.env.app.db()).await.unwrap();
        assert!(!settle_stale_working(&f.env.app, &run).await.unwrap(), "turn still in flight");
        assert_eq!(status(&f).await, "working");
    }

    #[tokio::test]
    async fn unknown_can_be_repaired_but_a_new_working_status_is_not_ours_to_restore() {
        let f = fixture().await;
        sqlx::query("UPDATE runs SET agent_status='unknown' WHERE id=?")
            .bind(&f.run.id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        sample(&f, BUSY).await;
        assert_eq!(status(&f).await, "working");
        assert_eq!(
            f.env.herdr.agents.lock().unwrap()[0]["agent_status"],
            "done"
        );
        sqlx::query("UPDATE runs SET agent_status_since='2099-01-01T00:00:00Z' WHERE id=?")
            .bind(&f.run.id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        complete(&f).await;
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(
            status(&f).await,
            "working",
            "a fresher writer owns this status now"
        );
    }

    #[tokio::test]
    async fn the_real_progress_poller_repairs_and_restores_the_run() {
        let f = fixture().await;
        crate::lifecycle::poller::arm_progress(&f.env.app, &f.run.id, &f.run.bot_id, &f.turn).await;
        assert!(tt::eventually!(status(&f).await == "working"));
        let reads = f.env.herdr.calls_to("pane.read").len();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&f.turn)
            .execute(f.env.app.db())
            .await
            .unwrap();
        assert!(tt::eventually!(
            f.env.herdr.calls_to("pane.read").len() >= reads + 2
        ));
        assert_eq!(
            status(&f).await,
            "working",
            "Stop alone cannot declare a still-busy pane idle"
        );
        f.env
            .herdr
            .set_screen(f.run.pane_id.as_deref().unwrap(), IDLE);
        assert!(tt::eventually!(status(&f).await == "idle"));
        assert!(tt::eventually!(!f
            .env
            .app
            .progress_pollers
            .lock()
            .await
            .contains_key(&f.run.id)));
    }

    #[tokio::test]
    async fn blocked_unknown_delivery_and_menus_do_not_become_working() {
        for state in ["blocked", "idle"] {
            let f = fixture().await;
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=?")
                .bind(state)
                .bind(&f.run.id)
                .execute(f.env.app.db())
                .await
                .unwrap();
            if state == "idle" {
                sqlx::query("UPDATE turns SET delivery='unknown' WHERE id=?")
                    .bind(&f.turn)
                    .execute(f.env.app.db())
                    .await
                    .unwrap();
            }
            sample(&f, BUSY).await;
            assert_eq!(status(&f).await, state);
        }
        let f = fixture().await;
        let dialog = format!(
            "✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n{}",
            include_str!("../../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-bash-permission.txt")
        );
        assert!(menu(&dialog));
        sample(&f, &dialog).await;
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn idle_text_and_quoted_activity_are_not_positive_evidence() {
        let f = fixture().await;
        let quoted = "❯ Work\n⏺ Example:\n  ✻ Drizzling… (1m 24s · ↓ 6.2k tokens)\n\n⏺ Done\n\n──────\n❯\n──────\n";
        for text in [IDLE, quoted] {
            sample(&f, text).await;
            assert_eq!(status(&f).await, "idle");
        }
        sample(&f, BUSY).await;
        complete(&f).await;
        f.env
            .herdr
            .set_screen(f.run.pane_id.as_deref().unwrap(), quoted);
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(
            status(&f).await,
            "idle",
            "quoted activity must not prevent restoration"
        );
    }

    #[tokio::test]
    async fn old_session_pane_host_generation_and_turn_snapshots_are_rejected() {
        for change in [
            "UPDATE runs SET native_session_id='new-session' WHERE id=?",
            "UPDATE runs SET herdr_session='other' WHERE id=?",
            "UPDATE runs SET pane_id='other-pane' WHERE id=?",
            "UPDATE runs SET host_generation=123 WHERE id=?",
        ] {
            let f = fixture().await;
            let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
            sqlx::query(change)
                .bind(&f.run.id)
                .execute(f.env.app.db())
                .await
                .unwrap();
            observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
                .await
                .unwrap();
            assert_eq!(status(&f).await, "idle", "{change}");
        }
        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        f.env
            .app
            .hosts
            .bump_generation_for_test(&f.client.fence.conn().name)
            .await;
        observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
            .await
            .unwrap();
        assert_eq!(status(&f).await, "idle");

        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        observe(
            &f.env.app,
            &f.client,
            &stamp,
            "another-turn",
            "claude",
            BUSY,
        )
        .await
        .unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn stop_between_snapshot_and_status_write_cannot_resurrect_working() {
        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        let (app, turn) = (f.env.app.clone(), f.turn.clone());
        crate::lifecycle::race_point::arm(
            "observed_status_before_write",
            &f.run.id,
            move || async move {
                sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
                    .bind(db::now())
                    .bind(turn)
                    .execute(app.db())
                    .await
                    .unwrap();
            },
        );
        observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
            .await
            .unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn restoration_does_not_clobber_a_blocker_or_a_new_turn() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?")
            .bind(&f.run.id)
            .execute(f.env.app.db())
            .await
            .unwrap();
        complete(&f).await;
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "blocked");

        let f = fixture().await;
        sample(&f, BUSY).await;
        complete(&f).await;
        let conv = db::conversation_id(f.env.app.db(), &f.run.bot_id)
            .await
            .unwrap();
        let next = db::ulid();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES(?,?,?,'web','in_flight','ok',?)")
            .bind(&next).bind(conv).bind(&f.run.id).bind(db::now()).execute(f.env.app.db()).await.unwrap();
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "working");
        let run = db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap();
        let stamp = Stamp::capture(&f.env.app, &run).await.unwrap();
        observe(&f.env.app, &f.client, &stamp, &next, "claude", BUSY)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET status='completed',completed_at=? WHERE id=?")
            .bind(db::now())
            .bind(&next)
            .execute(f.env.app.db())
            .await
            .unwrap();
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(
            status(&f).await,
            "working",
            "old poller cannot erase the successor's ownership"
        );
        finish(&f.env.app, &f.run.id, &next).await.unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    #[tokio::test]
    async fn pane_read_failure_does_not_publish_idle_and_database_failure_does_not_mark_busy() {
        let f = fixture().await;
        sample(&f, BUSY).await;
        complete(&f).await;
        f.env.herdr.fail_next("pane.read", tt::Fault::Refuse);
        assert!(finish(&f.env.app, &f.run.id, &f.turn).await.is_err());
        assert_eq!(status(&f).await, "working");
        finish(&f.env.app, &f.run.id, &f.turn).await.unwrap();
        assert_eq!(status(&f).await, "idle");

        let f = fixture().await;
        let stamp = Stamp::capture(&f.env.app, &f.run).await.unwrap();
        sqlx::query("ALTER TABLE turns RENAME TO turns_unreadable")
            .execute(f.env.app.db())
            .await
            .unwrap();
        assert!(
            observe(&f.env.app, &f.client, &stamp, &f.turn, "claude", BUSY)
                .await
                .is_err()
        );
        sqlx::query("ALTER TABLE turns_unreadable RENAME TO turns")
            .execute(f.env.app.db())
            .await
            .unwrap();
        assert_eq!(status(&f).await, "idle");
    }

    const TOOL_RUNNING: &str = include_str!("../../../../crates/am-lifecycle/src/lifecycle/answerable_fixtures/claude-false-blocked-tool-running.txt");
    const BASH_PERMISSION: &str = include_str!("../../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-bash-permission.txt");

    async fn set_status(f: &Fixture, status: &str) {
        sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(status).bind(&f.run.id)
            .execute(f.env.app.db()).await.unwrap();
    }

    async fn fresh(f: &Fixture) -> db::Run {
        db::run(f.env.app.db(), &f.run.id).await.unwrap().unwrap()
    }

    async fn settle_twice(f: &Fixture) -> bool {
        assert!(!settle_false_blocked(&f.env.app, &fresh(f).await).await.unwrap(), "第一次只計數、等下一次讀取確認");
        assert_eq!(status(f).await, "blocked");
        tokio::time::sleep(Duration::from_millis(1100)).await;
        settle_false_blocked(&f.env.app, &fresh(f).await).await.unwrap()
    }

    #[tokio::test]
    async fn a_blocked_run_whose_pane_has_nothing_to_answer_is_settled_after_two_reads() {
        let f = fixture().await;
        set_status(&f, "blocked").await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), TOOL_RUNNING);
        assert!(settle_twice(&f).await);
        assert_eq!(status(&f).await, "working");
        assert!(false_blocked_owned(&f.run.id));
    }

    #[tokio::test]
    async fn a_real_prompt_keeps_the_run_blocked() {
        let f = fixture().await;
        set_status(&f, "blocked").await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), BASH_PERMISSION);
        assert!(!settle_false_blocked(&f.env.app, &fresh(&f).await).await.unwrap());
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!settle_false_blocked(&f.env.app, &fresh(&f).await).await.unwrap());
        assert_eq!(status(&f).await, "blocked");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), "__READ_ERROR__");
        settle_false_blocked(&f.env.app, &fresh(&f).await).await.unwrap();
        assert_eq!(status(&f).await, "blocked");
    }

    #[tokio::test]
    async fn a_prompt_that_shows_up_between_the_two_reads_cancels_the_settle() {
        let f = fixture().await;
        set_status(&f, "blocked").await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), TOOL_RUNNING);
        assert!(!settle_false_blocked(&f.env.app, &fresh(&f).await).await.unwrap());
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), BASH_PERMISSION);
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert!(!settle_false_blocked(&f.env.app, &fresh(&f).await).await.unwrap());
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), TOOL_RUNNING);
        assert!(!settle_false_blocked(&f.env.app, &fresh(&f).await).await.unwrap());
        assert_eq!(status(&f).await, "blocked");
    }

    #[tokio::test]
    async fn a_settled_run_goes_back_to_blocked_when_a_prompt_appears_and_herdr_stayed_silent() {
        let f = fixture().await;
        set_status(&f, "blocked").await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), TOOL_RUNNING);
        assert!(settle_twice(&f).await);
        assert_eq!(status(&f).await, "working");
        assert!(!reblock_if_prompt(&f.env.app, &fresh(&f).await).await.unwrap());
        assert_eq!(status(&f).await, "working");
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), BASH_PERMISSION);
        assert!(reblock_if_prompt(&f.env.app, &fresh(&f).await).await.unwrap());
        assert_eq!(status(&f).await, "blocked");
        assert!(!false_blocked_owned(&f.run.id));
    }

    #[tokio::test]
    async fn a_herdr_status_after_the_settle_wins_over_the_receipt() {
        let f = fixture().await;
        set_status(&f, "blocked").await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), TOOL_RUNNING);
        assert!(settle_twice(&f).await);
        set_status(&f, "idle").await;
        f.env.herdr.set_screen(f.run.pane_id.as_deref().unwrap(), BASH_PERMISSION);
        assert!(!reblock_if_prompt(&f.env.app, &fresh(&f).await).await.unwrap());
        assert_eq!(status(&f).await, "idle");
        assert!(!false_blocked_owned(&f.run.id));
    }

}
