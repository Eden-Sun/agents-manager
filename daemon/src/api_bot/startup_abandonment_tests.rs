use super::*;

#[tokio::test]
async fn d3b1_driver_consumes_receipt_without_leaving_a_cursor() {
    let f = fixture(true).await;
    let app = restart(&f).await;
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
    let guard = app
        .api_bot_runtime
        .startup_guard(&app, &f.identity)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    drop(guard);
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    let guard = tokio::time::timeout(
        DEADLINE,
        app.api_bot_runtime.startup_guard(&app, &f.identity),
    )
    .await
    .unwrap()
    .unwrap_or_else(|e| panic!("{}", e.code()));
    drop(guard);
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    assert_eq!(turn_status(&app, &f.identity).await, "completed");
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
    let state: (String, Option<String>) =
        sqlx::query_as("SELECT state,exit_reason FROM runs WHERE id=?")
            .bind(&f.identity.run_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(
        state,
        ("exited".into(), Some("api_restart_abandoned".into()))
    );
}

#[tokio::test]
async fn d3b1_abandons_orphans_without_replay_and_preserves_unknown_hold() {
    for (state, checkpoint) in [
        ("starting", "bad-json"),
        (
            "running",
            r#"{"messages":[],"responses":[],"api_pending":true,"tool_pending":null,"completed_tool_ids":[]}"#,
        ),
        (
            "stopping",
            r#"{"messages":[],"responses":[],"api_pending":false,"tool_pending":"x","completed_tool_ids":[]}"#,
        ),
    ] {
        let f = fixture(true).await;
        sqlx::query("DELETE FROM api_bot_join_receipts WHERE run_id=?")
            .bind(&f.identity.run_id)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE id=?")
            .bind(checkpoint)
            .bind(&f.identity.session_id)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET state=? WHERE id=?")
            .bind(state)
            .bind(&f.identity.run_id)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        let app = restart(&f).await;
        startup_recovery::recover_all(&app)
            .await
            .unwrap_or_else(|e| panic!("{}", e.code()));
        assert_eq!(turn_status(&app, &f.identity).await, "failed");
        assert_eq!(assistant_count(&app, &f.identity).await, 0);
        assert_eq!(ledger(&app).await, vec![("unknown".into(), None, 123)]);
        let result: (String, Option<String>) =
            sqlx::query_as("SELECT state,exit_reason FROM runs WHERE id=?")
                .bind(&f.identity.run_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(
            result,
            ("exited".into(), Some("api_restart_abandoned".into()))
        );
        assert!(app
            .api_bot_runtime
            .startup_cursor_for_test(&f.identity.bot_id)
            .is_none());
        startup_recovery::recover_all(&app)
            .await
            .unwrap_or_else(|e| panic!("{}", e.code()));
        assert_eq!(ledger(&app).await, vec![("unknown".into(), None, 123)]);
    }
}

#[tokio::test]
async fn d3b1_unresolved_receipt_and_storage_failure_do_not_abandon() {
    let f = fixture(true).await;
    sqlx::query("UPDATE api_bot_sessions SET checkpoint_json='bad-json' WHERE id=?")
        .bind(&f.identity.session_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = restart(&f).await;
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_unresolved"
    );
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
    assert_eq!(ledger(&app).await, vec![("reserved".into(), None, 123)]);
    sqlx::query("DROP TABLE api_bot_join_receipts")
        .execute(&app.db)
        .await
        .unwrap();
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_storage"
    );
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
}

#[tokio::test]
async fn d3b1_stopping_pending_receipt_is_preserved_unresolved() {
    let f = fixture(true).await;
    sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
        .bind(&f.identity.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = restart(&f).await;
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_unresolved"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM api_bot_join_receipts WHERE run_id=?")
            .bind(&f.identity.run_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(state, "pending");
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
    assert_eq!(ledger(&app).await, vec![("reserved".into(), None, 123)]);
}

#[tokio::test]
async fn d3b1_abandons_all_runs_across_keyset_pages_and_is_idempotent() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    for n in 0..70 {
        let bot = format!("d3b1-bot-{n:03}");
        let run = format!("d3b1-run-{n:03}");
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,?,'claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&bot).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
            .bind(run)
            .bind(bot)
            .bind(&now)
            .execute(&env.app.db)
            .await
            .unwrap();
    }
    env.app.set_startup_ready(false);
    startup_recovery::recover_all(&env.app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    startup_recovery::recover_all(&env.app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    let count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM runs WHERE state='exited' AND exit_reason='api_restart_abandoned'",
    )
    .fetch_one(&env.app.db)
    .await
    .unwrap();
    assert_eq!(count, 70);
}

#[tokio::test]
async fn d3b1_busy_or_not_startup_cannot_abandon_orphan() {
    let f = fixture(true).await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE run_id=?")
        .bind(&f.identity.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = restart(&f).await;
    app.set_startup_ready(true);
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_not_exclusive"
    );
    app.set_startup_ready(false);
    let registry = app.api_bot_runtime.startup_registry_for_test();
    let ticket = registry
        .spawn_tracked(
            RunTurnKey {
                run_id: f.identity.run_id.clone(),
                turn_id: f.identity.turn_id.clone(),
            },
            |cancel| async move {
                cancel.cancelled().await;
                Ok(TurnOutcome::Cancelled)
            },
        )
        .unwrap();
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_unresolved"
    );
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
    assert_eq!(ledger(&app).await, vec![("reserved".into(), None, 123)]);
    ticket.cancel_and_join().await.unwrap();
}

#[tokio::test]
async fn d3b1_applied_receipt_is_read_only_and_write_failure_rolls_back() {
    let f = fixture(false).await;
    sqlx::query("CREATE TRIGGER reject_receipt_update BEFORE UPDATE ON api_bot_join_receipts BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&f.env.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER reject_new_assistant BEFORE INSERT ON messages WHEN NEW.role='assistant' BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&f.env.app.db).await.unwrap();
    let app = restart(&f).await;
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
    let f = fixture(true).await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE run_id=?")
        .bind(&f.identity.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query("CREATE TRIGGER reject_abandon BEFORE UPDATE OF state ON runs BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&f.env.app.db).await.unwrap();
    let app = restart(&f).await;
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_storage"
    );
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
    assert_eq!(ledger(&app).await, vec![("reserved".into(), None, 123)]);
}

#[tokio::test]
async fn d3b1_abandon_preserves_terminal_attempts_and_fails_queued_turn() {
    let f = fixture(true).await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE run_id=?")
        .bind(&f.identity.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT 'd3b1-queued',conversation_id,run_id,'web','queued','pending',created_at FROM turns WHERE id=?").bind(&f.identity.turn_id).execute(&f.env.app.db).await.unwrap();
    for (i, state) in ["recorded", "failed", "unknown"].iter().enumerate() {
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,estimated_microusd,pricing_version,created_at) SELECT ?,session_id,bot_id,run_id,turn_id,generation,?,'hash','fast',?,50,42,'fixture',created_at FROM api_bot_attempts WHERE run_id=? LIMIT 1").bind(format!("terminal-{i}")).bind((i+1) as i64).bind(state).bind(&f.identity.run_id).execute(&f.env.app.db).await.unwrap();
    }
    let app = restart(&f).await;
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    let terminal: Vec<(String, Option<i64>, i64)> = sqlx::query_as("SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts WHERE id LIKE 'terminal-%' ORDER BY id").fetch_all(&app.db).await.unwrap();
    assert_eq!(
        terminal,
        vec![
            ("recorded".into(), Some(42), 50),
            ("failed".into(), Some(42), 50),
            ("unknown".into(), Some(42), 50)
        ]
    );
    let queued: (String, String) =
        sqlx::query_as("SELECT status,delivery FROM turns WHERE id='d3b1-queued'")
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(queued, ("failed".into(), "failed".into()));
}

#[tokio::test]
async fn d3b1_retired_applied_artifact_does_not_authorize_successor_completion() {
    let f = fixture(false).await;
    let app = restart(&f).await;
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    let next_run = crate::db::ulid();
    let next_turn = crate::db::ulid();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(&next_run)
        .bind(&f.identity.bot_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) SELECT ?,conversation_id,?,'web','in_flight','ok',? FROM turns WHERE id=?").bind(&next_turn).bind(&next_run).bind(crate::db::now()).bind(&f.identity.turn_id).execute(&app.db).await.unwrap();
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    let state: (String, String) = sqlx::query_as(
        "SELECT r.state,t.status FROM runs r JOIN turns t ON t.run_id=r.id WHERE r.id=?",
    )
    .bind(&next_run)
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(state, ("exited".into(), "failed".into()));
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
            .bind(next_turn)
            .fetch_one(&app.db)
            .await
            .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn d3b1_final_scan_detects_active_run_before_cursor() {
    let f = fixture(true).await;
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE run_id=?")
        .bind(&f.identity.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('!earlier',?,'earlier','claude-api','fake',?)").bind(&f.env.project_id).bind(crate::db::now()).execute(&f.env.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER d3b1_insert_earlier AFTER UPDATE OF state ON runs WHEN NEW.exit_reason='api_restart_abandoned' BEGIN INSERT INTO runs(id,bot_id,state,started_at) VALUES('!earlier-run','!earlier','running','2026-01-01'); END").execute(&f.env.app.db).await.unwrap();
    let app = restart(&f).await;
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_unresolved"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id='!earlier-run'")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(state, "running");
}

#[tokio::test]
async fn d3b1_historical_applied_still_requires_startup_guard() {
    let f = fixture(false).await;
    let app = restart(&f).await;
    startup_recovery::recover_all(&app)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    app.api_bot_runtime
        .startup_set_fake_dependencies_for_test(Arc::new(NeverTools(
            std::sync::atomic::AtomicUsize::new(0),
        )));
    assert_eq!(
        startup_recovery::recover_all(&app)
            .await
            .err()
            .unwrap()
            .code(),
        "api_startup_not_exclusive"
    );
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
}
