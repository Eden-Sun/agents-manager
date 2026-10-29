use super::startup_gate;

#[tokio::test]
async fn d3b2_no_api_boot_opens_gate_without_api_tables() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    sqlx::query("DROP TABLE api_bot_join_receipts")
        .execute(&app.db)
        .await
        .unwrap();
    assert!(!app.api_startup_gate.ready());
    startup_gate::barrier(&app).await;
    assert!(
        app.api_startup_gate.ready(),
        "a confirmed no-API boot must open admission"
    );
}

#[tokio::test]
async fn d3b2_quarantine_keeps_cli_ready_and_refuses_stop_before_cas() {
    let env = crate::testing::env().await;
    let bot = crate::db::ulid();
    let run = crate::db::ulid();
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'api','claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
        .bind(&run)
        .bind(&bot)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    sqlx::query("DROP TABLE api_bot_join_receipts")
        .execute(&app.db)
        .await
        .unwrap();
    startup_gate::barrier(&app).await;
    app.set_startup_ready(true);
    assert!(app.is_startup_ready());
    assert!(!app.api_startup_gate.ready());
    let result = crate::lifecycle::stop_bot(&app, &bot).await;
    assert!(
        matches!(result, Err(crate::lifecycle::LcError::Unavailable(ref v)) if v["error"] == "api_startup_quarantined")
    );
    let state: String = sqlx::query_scalar("SELECT state FROM runs WHERE id=?")
        .bind(&run)
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(state, "running");
}

#[tokio::test]
async fn d3b2_boot_barrier_fits_one_mib_stack_with_api_bot() {
    let env = crate::testing::env().await;
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('stack-api',?,'stack-api','claude-api','fake',?)")
        .bind(&env.project_id).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    let worker = app.clone();
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(startup_gate::barrier(&worker));
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(app.api_startup_gate.ready());
}

struct NoTools;
impl super::runner::Tools for NoTools {
    fn execute<'a>(
        &'a self,
        _call: &'a super::protocol::ToolCall,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Err("fixture does not execute tools".into()) })
    }
}

#[tokio::test]
async fn d3b2_closed_gate_rejects_port_before_runtime_even_with_fake_dependencies() {
    use crate::lifecycle::api_bot_port::{ApiBotError, ApiBotPort, ApiTurnKey};
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app(&env).await;
    app.api_bot_runtime
        .startup_set_fake_dependencies_for_test(std::sync::Arc::new(NoTools));
    assert!(!app.api_recovery_ready());
    let key = ApiTurnKey {
        bot_id: "missing".into(),
        run_id: "missing".into(),
        turn_id: "missing".into(),
    };
    assert_eq!(app.api_turn_spawn(&key).await, Err(ApiBotError::Upstream));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_bot_attempts")
        .fetch_one(&app.db)
        .await
        .unwrap();
    assert_eq!(count, 0);
}

struct PortFixture(bool);
impl crate::lifecycle::api_bot_port::ApiBotPort for PortFixture {
    async fn api_run_begin_stop(&self, key: &crate::lifecycle::api_bot_port::ApiRunKey, mode: crate::lifecycle::api_bot_port::ApiBeginStopMode) -> Result<crate::lifecycle::api_bot_port::ApiBeginStopResult, crate::lifecycle::api_bot_port::ApiBeginStopError> {
        if key.bot_id == "forward-bot" && key.run_id == "forward-run" && matches!(mode, crate::lifecycle::api_bot_port::ApiBeginStopMode::User { withdraw: true }) {
            Err(crate::lifecycle::api_bot_port::ApiBeginStopError::Busy)
        } else {
            Err(crate::lifecycle::api_bot_port::ApiBeginStopError::Storage)
        }
    }

    fn api_recovery_ready(&self) -> bool {
        self.0
    }
    async fn api_turn_spawn(
        &self,
        _: &crate::lifecycle::api_bot_port::ApiTurnKey,
    ) -> Result<(), crate::lifecycle::api_bot_port::ApiBotError> {
        Err(crate::lifecycle::api_bot_port::ApiBotError::Upstream)
    }
    async fn api_run_cancel_and_join(
        &self,
        _: &crate::lifecycle::api_bot_port::ApiTurnKey,
    ) -> Result<
        crate::lifecycle::api_bot_port::ApiRunCompletion,
        crate::lifecycle::api_bot_port::ApiBotError,
    > {
        Err(crate::lifecycle::api_bot_port::ApiBotError::Upstream)
    }
    async fn api_run_stop_proof(
        &self,
        _: &crate::lifecycle::api_bot_port::ApiRunKey,
    ) -> Result<
        crate::lifecycle::api_bot_port::ApiRunStopProof,
        crate::lifecycle::api_bot_port::ApiBotError,
    > {
        Err(crate::lifecycle::api_bot_port::ApiBotError::Upstream)
    }
    async fn api_run_live(&self, _: &crate::lifecycle::api_bot_port::ApiTurnKey) -> bool {
        false
    }
}
#[test]
fn d3b2_arc_port_forwards_readiness() {
    use crate::lifecycle::api_bot_port::ApiBotPort;
    assert!(!std::sync::Arc::new(PortFixture(false)).api_recovery_ready());
    assert!(std::sync::Arc::new(PortFixture(true)).api_recovery_ready());
}

#[tokio::test]
async fn same_run_arc_forwards_begin_stop_key_and_mode() {
    use crate::lifecycle::api_bot_port::{ApiBeginStopError, ApiBeginStopMode, ApiBotPort, ApiRunKey};
    let port = std::sync::Arc::new(PortFixture(true));
    let key = ApiRunKey { bot_id: "forward-bot".into(), run_id: "forward-run".into() };
    assert!(matches!(port.api_run_begin_stop(&key, ApiBeginStopMode::User { withdraw: true }).await, Err(ApiBeginStopError::Busy)));
    assert!(matches!(port.api_run_begin_stop(&key, ApiBeginStopMode::Idle).await, Err(ApiBeginStopError::Storage)));
}

struct DefaultPort;
impl crate::lifecycle::api_bot_port::ApiBotPort for DefaultPort {
    async fn api_run_begin_stop(&self, _: &crate::lifecycle::api_bot_port::ApiRunKey, _: crate::lifecycle::api_bot_port::ApiBeginStopMode) -> Result<crate::lifecycle::api_bot_port::ApiBeginStopResult, crate::lifecycle::api_bot_port::ApiBeginStopError> {
        Err(crate::lifecycle::api_bot_port::ApiBeginStopError::Storage)
    }

    async fn api_turn_spawn(
        &self,
        _: &crate::lifecycle::api_bot_port::ApiTurnKey,
    ) -> Result<(), crate::lifecycle::api_bot_port::ApiBotError> {
        Err(crate::lifecycle::api_bot_port::ApiBotError::Upstream)
    }
    async fn api_run_cancel_and_join(
        &self,
        _: &crate::lifecycle::api_bot_port::ApiTurnKey,
    ) -> Result<
        crate::lifecycle::api_bot_port::ApiRunCompletion,
        crate::lifecycle::api_bot_port::ApiBotError,
    > {
        Err(crate::lifecycle::api_bot_port::ApiBotError::Upstream)
    }
    async fn api_run_stop_proof(
        &self,
        _: &crate::lifecycle::api_bot_port::ApiRunKey,
    ) -> Result<
        crate::lifecycle::api_bot_port::ApiRunStopProof,
        crate::lifecycle::api_bot_port::ApiBotError,
    > {
        Err(crate::lifecycle::api_bot_port::ApiBotError::Upstream)
    }
    async fn api_run_live(&self, _: &crate::lifecycle::api_bot_port::ApiTurnKey) -> bool {
        false
    }
}
#[test]
fn d3b2_port_default_is_closed() {
    use crate::lifecycle::api_bot_port::ApiBotPort;
    assert!(!DefaultPort.api_recovery_ready());
    assert!(!std::sync::Arc::new(DefaultPort).api_recovery_ready());
}

#[tokio::test]
async fn d3b2_deleted_api_bot_prevents_unsafe_fast_noop_and_shutdown_stays_closed() {
    let env = crate::testing::env().await;
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at,deleted_at) VALUES('d3b2-deleted',?,'d3b2-deleted','claude-api','fake',?,?)").bind(&env.project_id).bind(crate::db::now()).bind(crate::db::now()).execute(&env.app.db).await.unwrap();
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    sqlx::query("DROP TABLE api_bot_join_receipts")
        .execute(&app.db)
        .await
        .unwrap();
    startup_gate::barrier(&app).await;
    assert!(!app.api_startup_gate.ready());
    let second = crate::testing::restart_app(&env).await;
    second.set_startup_ready(false);
    second.shutdown.cancel();
    startup_gate::barrier(&second).await;
    assert!(!second.api_startup_gate.ready());
}

#[tokio::test]
async fn d3b2_bots_read_error_quarantines_without_blocking_global_ready() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    sqlx::query("ALTER TABLE bots RENAME TO d3b2_hidden_bots")
        .execute(&app.db)
        .await
        .unwrap();
    startup_gate::barrier(&app).await;
    assert!(!app.api_startup_gate.ready());
    app.set_startup_ready(true);
    assert!(app.is_startup_ready());
    sqlx::query("ALTER TABLE d3b2_hidden_bots RENAME TO bots")
        .execute(&app.db)
        .await
        .unwrap();
    startup_gate::barrier(&app).await;
    assert!(
        !app.api_startup_gate.ready(),
        "quarantine requires a fresh daemon boot"
    );
}

#[tokio::test]
async fn d3b2_one_mib_stack_recovers_true_join_pending_and_active_orphan() {
    use super::{adapter::AdapterIdentity, protocol::Response};
    use am_base::db::api_bot::{self, Checkpoint, Fence};
    use serde_json::json;
    use std::sync::atomic::Ordering;
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('d3b2-stack-api',?,'d3b2-stack-api','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('d3b2-stack-run','d3b2-stack-api','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('d3b2-stack-conv','d3b2-stack-api',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('d3b2-stack-turn','d3b2-stack-conv','d3b2-stack-run','web','in_flight','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
    let identity = AdapterIdentity {
        session_id: "d3b2-stack-session".into(),
        bot_id: "d3b2-stack-api".into(),
        run_id: "d3b2-stack-run".into(),
        turn_id: "d3b2-stack-turn".into(),
        generation: 1,
    };
    let checkpoint = Checkpoint::new(&json!({"messages":[{"role":"user","content":"fixture"}],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]})).unwrap();
    api_bot::create(
        &env.app.db,
        &Fence {
            session: &identity.session_id,
            bot: &identity.bot_id,
            run: &identity.run_id,
            turn: &identity.turn_id,
            generation: 1,
            revision: 0,
        },
        &checkpoint,
    )
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER d3b2_block_assistant BEFORE INSERT ON messages WHEN NEW.role='assistant' BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&env.app.db).await.unwrap();
    let response = Response {
        id: "fixture".into(),
        model: "fixture".into(),
        content: vec![json!({"type":"text","text":"fixture"})],
        stop_reason: "end_turn".into(),
        usage: json!({}),
    };
    let key = crate::lifecycle::api_bot_port::ApiTurnKey {
        bot_id: identity.bot_id.clone(),
        run_id: identity.run_id.clone(),
        turn_id: identity.turn_id.clone(),
    };
    let producer = env.app.api_bot_runtime.spawn_startup_fixture_owner(
        env.app.clone(),
        key,
        identity.clone(),
        response,
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while producer.retries.load(Ordering::SeqCst) < 3 {
            producer.wake.notified().await;
        }
    })
    .await
    .unwrap();
    producer.shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(10), producer.task)
        .await
        .unwrap()
        .unwrap();
    sqlx::query("DROP TRIGGER d3b2_block_assistant")
        .execute(&env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('d3b2-stack-orphan',?,'d3b2-stack-orphan','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('d3b2-orphan-run','d3b2-stack-orphan','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r1-stack-expired',?,'r1-stack-expired','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('r1-stack-expired-session','r1-stack-expired',1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    env.app.db.close().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    let worker = app.clone();
    std::thread::Builder::new()
        .stack_size(1024 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(startup_gate::barrier(&worker))
        })
        .unwrap()
        .join()
        .unwrap();
    assert!(app.api_startup_gate.ready());
    let expired: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_bot_sessions WHERE id='r1-stack-expired-session'").fetch_one(&app.db).await.unwrap();
    assert_eq!(expired, 0, "the 1 MiB barrier also executes retention");
    let receipt: String = sqlx::query_scalar(
        "SELECT state FROM api_bot_join_receipts WHERE turn_id='d3b2-stack-turn'",
    )
    .fetch_one(&app.db)
    .await
    .unwrap();
    assert_eq!(receipt, "applied");
    let turns: (String,i64) = sqlx::query_as("SELECT t.status,(SELECT COUNT(*) FROM messages m WHERE m.turn_id=t.id AND m.role='assistant') FROM turns t WHERE t.id='d3b2-stack-turn'").fetch_one(&app.db).await.unwrap();
    assert_eq!(turns, ("completed".into(), 1));
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT state FROM runs WHERE id IN ('d3b2-stack-run','d3b2-orphan-run') ORDER BY id",
    )
    .fetch_all(&app.db)
    .await
    .unwrap();
    assert_eq!(states, vec!["exited", "exited"]);
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&identity.bot_id)
        .is_none());
    // Only after the previous boot consumed the genuine join and retired the run, expire its
    // Applied historical session and verify the next boot may prune it without reinserting text.
    sqlx::query("UPDATE api_bot_sessions SET expires_at=? WHERE id=?").bind(crate::db::iso_in(-60)).bind(&identity.session_id).execute(&app.db).await.unwrap();
    app.db.close().await;
    let again = crate::testing::restart_app(&env).await;
    again.set_startup_ready(false);
    startup_gate::barrier(&again).await;
    assert!(again.api_startup_gate.ready());
    let remaining: (i64,i64,i64) = sqlx::query_as("SELECT (SELECT count(*) FROM api_bot_sessions WHERE id=?),(SELECT count(*) FROM api_bot_join_receipts WHERE session_id=?),(SELECT count(*) FROM messages WHERE turn_id=? AND role='assistant')")
        .bind(&identity.session_id).bind(&identity.session_id).bind(&identity.turn_id).fetch_one(&again.db).await.unwrap();
    assert_eq!(remaining, (0, 0, 1));
}

#[tokio::test]
async fn d3b2_closed_spawn_valid_owner_leaves_runtime_and_database_untouched() {
    use crate::lifecycle::api_bot_port::{ApiBotError, ApiBotPort, ApiTurnKey};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    struct CountingTools(Arc<AtomicUsize>);
    impl super::runner::Tools for CountingTools {
        fn execute<'a>(
            &'a self,
            _: &'a super::protocol::ToolCall,
            _: &'a tokio_util::sync::CancellationToken,
        ) -> futures::future::BoxFuture<'a, Result<String, String>> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("fixture".into()) })
        }
    }
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('d3b2-owner',?,'d3b2-owner','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('d3b2-owner-run','d3b2-owner','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query(
        "INSERT INTO conversations(id,bot_id,created_at) VALUES('d3b2-owner-conv','d3b2-owner',?)",
    )
    .bind(&now)
    .execute(&env.app.db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES('d3b2-owner-turn','d3b2-owner-conv','d3b2-owner-run','web','in_flight','ok','fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
    let app = crate::testing::restart_app(&env).await;
    app.cfg
        .update(|cfg| {
            cfg.claude_api.enabled = true;
            cfg.claude_api.api_bot.enabled = true;
            cfg.claude_api.api_bot.max_turn_cost_microusd = Some(1000);
            Ok(())
        })
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let http_calls = calls.clone();
    let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        loop {
            tokio::select! { biased;
                accepted = listener.accept() => match accepted {
                    Ok(_) => { http_calls.fetch_add(1, Ordering::SeqCst); }
                    Err(_) => break,
                },
                _ = &mut stop_rx => break,
            }
        }
    });
    let tool_calls = Arc::new(AtomicUsize::new(0));
    app.api_bot_runtime
        .startup_set_fake_dependencies_at_for_test(
            Arc::new(CountingTools(tool_calls.clone())),
            &format!("http://{address}/v1/messages"),
        );
    let key = ApiTurnKey {
        bot_id: "d3b2-owner".into(),
        run_id: "d3b2-owner-run".into(),
        turn_id: "d3b2-owner-turn".into(),
    };
    let before: (String,String,String,String,String,i64,i64,i64) = sqlx::query_as("SELECT b.kind,r.state,t.status,t.delivery,t.prompt_text,(SELECT COUNT(*) FROM api_bot_sessions),(SELECT COUNT(*) FROM api_bot_attempts),(SELECT COUNT(*) FROM api_bot_join_receipts) FROM bots b JOIN runs r ON r.bot_id=b.id JOIN turns t ON t.run_id=r.id WHERE r.id='d3b2-owner-run'").fetch_one(&app.db).await.unwrap();
    let controls = app.api_bot_runtime.control_count_for_test();
    assert_eq!(controls, 0);
    assert!(!app
        .api_bot_runtime
        .startup_registry_for_test()
        .is_active(&key.run_id));
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&key.bot_id)
        .is_none());
    assert_eq!(app.api_turn_spawn(&key).await, Err(ApiBotError::Upstream));
    let after: (String,String,String,String,String,i64,i64,i64) = sqlx::query_as("SELECT b.kind,r.state,t.status,t.delivery,t.prompt_text,(SELECT COUNT(*) FROM api_bot_sessions),(SELECT COUNT(*) FROM api_bot_attempts),(SELECT COUNT(*) FROM api_bot_join_receipts) FROM bots b JOIN runs r ON r.bot_id=b.id JOIN turns t ON t.run_id=r.id WHERE r.id='d3b2-owner-run'").fetch_one(&app.db).await.unwrap();
    assert_eq!(after, before);
    assert_eq!(app.api_bot_runtime.control_count_for_test(), controls);
    assert!(!app
        .api_bot_runtime
        .startup_registry_for_test()
        .is_active(&key.run_id));
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&key.bot_id)
        .is_none());
    stop_tx.send(()).unwrap();
    server.await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(tool_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn d3b2_fixture_endpoint_accepts_only_literal_loopback_with_port() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app(&env).await;
    for endpoint in ["http://127.0.0.1:1234/x", "http://[::1]:1234/x"] {
        app.api_bot_runtime
            .startup_set_fake_dependencies_at_for_test(std::sync::Arc::new(NoTools), endpoint);
    }
    for endpoint in [
        "http://localhost:1234/x",
        "https://127.0.0.1:1234/x",
        "http://127.1:1234/x",
        "http://127.0.0.1/x",
        "http://127.0.0.1:0/x",
        "http://192.0.2.1:1234/x",
        "http://user@127.0.0.1:1234/x",
    ] {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            app.api_bot_runtime
                .startup_set_fake_dependencies_at_for_test(std::sync::Arc::new(NoTools), endpoint)
        }));
        assert!(result.is_err(), "endpoint must be rejected: {endpoint}");
    }
}

#[tokio::test]
async fn r1_startup_prunes_expired_session_after_recovery() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r1-bot',?,'r1-bot','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r1-run','r1-bot','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('r1-session','r1-bot','r1-run',1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    env.app.db.close().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    startup_gate::barrier(&app).await;
    assert!(app.api_startup_gate.ready());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM api_bot_sessions WHERE id='r1-session'").fetch_one(&app.db).await.unwrap();
    assert_eq!(count, 0, "expired session must be pruned before startup becomes ready");
}

#[tokio::test]
async fn r1_no_api_bot_does_not_read_any_private_table() {
    let env = crate::testing::env().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    for table in ["api_bot_join_receipts", "api_bot_attempts", "api_bot_sessions"] {
        sqlx::query(&format!("DROP TABLE {table}")).execute(&app.db).await.unwrap();
    }
    startup_gate::barrier(&app).await;
    assert!(app.api_startup_gate.ready());
}

#[tokio::test]
async fn r1_unresolved_receipt_is_preserved_and_cli_remains_ready() {
    for (applied, soft_deleted) in [(false, false), (true, false), (false, true), (true, true)] {
        let env = crate::testing::env().await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r1-u-bot',?,'r1-u','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
        if soft_deleted { sqlx::query("UPDATE bots SET deleted_at=? WHERE id='r1-u-bot'").bind(&now).execute(&env.app.db).await.unwrap(); }
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r1-u-run','r1-u-bot','running',?)").bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r1-u-conv','r1-u-bot',?)").bind(&now).execute(&env.app.db).await.unwrap();
        let status = if applied { "completed" } else { "in_flight" };
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r1-u-turn','r1-u-conv','r1-u-run','web',?,'ok',?)").bind(status).bind(&now).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('r1-u-session','r1-u-bot','r1-u-run','r1-u-turn',1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
        let state = if applied { "applied" } else { "pending" };
        let processed = if applied { Some(now.as_str()) } else { None };
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,state,created_at,processed_at) VALUES('r1-u-session','r1-u-bot','r1-u-run','r1-u-turn',1,0,'completed','fixture',?,?,?)").bind(state).bind(&now).bind(processed).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('r1-u-attempt','r1-u-session','r1-u-bot','r1-u-run','r1-u-turn',1,0,'fixture','standard','reserved',123,'fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
        env.app.db.close().await;
        let app = crate::testing::restart_app(&env).await;
        app.set_startup_ready(false);
        startup_gate::barrier(&app).await;
        assert!(!app.api_startup_gate.ready());
        app.set_startup_ready(true);
        assert!(app.is_startup_ready());
        let snapshot: (String,String,i64,String,i64) = sqlx::query_as("SELECT j.state,a.state,a.reserved_microusd,r.state,(SELECT count(*) FROM api_bot_sessions WHERE id='r1-u-session') FROM api_bot_join_receipts j JOIN api_bot_attempts a ON a.session_id=j.session_id JOIN runs r ON r.id=j.run_id WHERE j.session_id='r1-u-session'").fetch_one(&app.db).await.unwrap();
        assert_eq!(snapshot, (state.into(), "reserved".into(), 123, "running".into(), 1));
    }
}

#[tokio::test]
async fn r1_prune_failure_rolls_back_and_quarantines() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r1-f-bot',?,'r1-f','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('r1-f-session','r1-f-bot',1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('r1-f-attempt','r1-f-session','r1-f-bot',1,0,'fixture','standard','reserved',123,'fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER r1_reject_delete BEFORE DELETE ON api_bot_sessions BEGIN SELECT RAISE(ABORT,'fixture'); END").execute(&env.app.db).await.unwrap();
    env.app.db.close().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    assert!(matches!(super::startup_retention::prune(&app).await, Err("api_startup_retention_storage")));
    startup_gate::barrier(&app).await;
    assert!(!app.api_startup_gate.ready());
    app.set_startup_ready(true);
    assert!(app.is_startup_ready());
    let row: (String,Option<i64>,i64,i64) = sqlx::query_as("SELECT a.state,a.estimated_microusd,a.reserved_microusd,(SELECT count(*) FROM api_bot_sessions WHERE id='r1-f-session') FROM api_bot_attempts a WHERE a.id='r1-f-attempt'").fetch_one(&app.db).await.unwrap();
    assert_eq!(row, ("reserved".into(), None, 123, 1));
}

#[tokio::test]
async fn r1_prune_preserves_accounting_and_handles_soft_deleted_bots() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    for bot in ["r1-exp", "r1-soft", "r1-live"] {
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at,deleted_at) VALUES(?,?,?,'claude-api','fake',?,?)")
            .bind(bot).bind(&env.project_id).bind(bot).bind(&now).bind(if bot == "r1-soft" { Some(now.as_str()) } else { None }).execute(&env.app.db).await.unwrap();
        let expires = if bot == "r1-exp" { crate::db::iso_in(-60) } else { crate::db::iso_in(3600) };
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES(?,?,1,'{}',?,?,?)")
            .bind(format!("{bot}-session")).bind(bot).bind(&now).bind(&now).bind(expires).execute(&env.app.db).await.unwrap();
    }
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r1-old-run','r1-live','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r1-old-conv','r1-live',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r1-old-turn','r1-old-conv','r1-old-run','web','failed','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) VALUES('r1-live-session','r1-live','r1-old-run','r1-old-turn',1,0,'failed','fenced',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    for (id,state,estimate) in [("r1-reserved","reserved",None),("r1-recorded","recorded",Some(44_i64)),("r1-failed","failed",None),("r1-unknown","unknown",None)] {
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,estimated_microusd,pricing_version,created_at) VALUES(?,'r1-exp-session','r1-exp',1,?,'fixture','standard',?,123,?,'fixture',?)")
            .bind(id).bind(id.len() as i64).bind(state).bind(estimate).bind(&now).execute(&env.app.db).await.unwrap();
    }
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('r1-null',NULL,'r1-live',1,0,'fixture','standard','reserved',321,'fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
    env.app.db.close().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    startup_gate::barrier(&app).await;
    assert!(app.api_startup_gate.ready());
    let sessions: Vec<String> = sqlx::query_scalar("SELECT id FROM api_bot_sessions ORDER BY id").fetch_all(&app.db).await.unwrap();
    assert_eq!(sessions, ["r1-live-session"]);
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM api_bot_join_receipts WHERE session_id='r1-live-session' AND state='fenced'").fetch_one(&app.db).await.unwrap();
    assert_eq!(receipts, 1, "non-candidate fenced receipt neither blocks nor is removed");
    let rows: Vec<(String,String,Option<String>,Option<i64>,i64)> = sqlx::query_as("SELECT id,state,session_id,estimated_microusd,reserved_microusd FROM api_bot_attempts ORDER BY id").fetch_all(&app.db).await.unwrap();
    assert_eq!(rows, vec![
        ("r1-failed".into(),"failed".into(),None,None,123),
        ("r1-null".into(),"unknown".into(),None,None,321),
        ("r1-recorded".into(),"recorded".into(),None,Some(44),123),
        ("r1-reserved".into(),"unknown".into(),None,None,123),
        ("r1-unknown".into(),"unknown".into(),None,None,123),
    ]);
    app.db.close().await;
    let again = crate::testing::restart_app(&env).await;
    again.set_startup_ready(false);
    startup_gate::barrier(&again).await;
    assert!(again.api_startup_gate.ready());
    let repeated: Vec<(String,String,Option<String>,Option<i64>,i64)> = sqlx::query_as("SELECT id,state,session_id,estimated_microusd,reserved_microusd FROM api_bot_attempts ORDER BY id").fetch_all(&again.db).await.unwrap();
    assert_eq!(repeated, rows);
}

#[tokio::test]
async fn r1_candidate_fenced_receipt_blocks_all_pruning_and_preserves_hold() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r1-b-bot',?,'r1-b','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES('r1-b-run','r1-b-bot','exited',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('r1-b-conv','r1-b-bot',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at) VALUES('r1-b-turn','r1-b-conv','r1-b-run','web','failed','ok',?)").bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('r1-b-session','r1-b-bot','r1-b-run','r1-b-turn',1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,state,created_at,processed_at) VALUES('r1-b-session','r1-b-bot','r1-b-run','r1-b-turn',1,0,'failed','fenced',?,?)").bind(&now).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at) VALUES('r1-b-attempt','r1-b-session','r1-b-bot',1,0,'fixture','standard','reserved',123,'fixture',?)").bind(&now).execute(&env.app.db).await.unwrap();
    env.app.db.close().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    assert!(matches!(super::startup_retention::prune(&app).await, Err("api_startup_retention_unresolved")));
    startup_gate::barrier(&app).await;
    assert!(!app.api_startup_gate.ready());
    app.set_startup_ready(true);
    assert!(app.is_startup_ready());
    let row: (String,String,Option<i64>,i64,i64) = sqlx::query_as("SELECT j.state,a.state,a.estimated_microusd,a.reserved_microusd,(SELECT count(*) FROM api_bot_sessions WHERE id='r1-b-session') FROM api_bot_join_receipts j JOIN api_bot_attempts a ON a.session_id=j.session_id WHERE j.session_id='r1-b-session'").fetch_one(&app.db).await.unwrap();
    assert_eq!(row, ("fenced".into(), "reserved".into(), None, 123, 1));
}

#[tokio::test]
async fn r1_guard_read_error_is_not_empty_receipts() {
    let env = crate::testing::env().await;
    sqlx::query("ALTER TABLE api_bot_join_receipts RENAME TO r1_hidden_receipts").execute(&env.app.db).await.unwrap();
    assert!(crate::db::api_bot::prune_unless_unresolved(&env.app.db).await.is_err());
    sqlx::query("ALTER TABLE r1_hidden_receipts RENAME TO api_bot_join_receipts").execute(&env.app.db).await.unwrap();
}

#[tokio::test]
async fn r1_prune_commit_failure_never_opens_api_gate() {
    let env = crate::testing::env().await;
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES('r1-c-bot',?,'r1-c','claude-api','fake',?)").bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,generation,checkpoint_json,created_at,updated_at,expires_at) VALUES('r1-c-session','r1-c-bot',1,'{}',?,?,?)").bind(&now).bind(&now).bind(crate::db::iso_in(-60)).execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TABLE r1_parent(id INTEGER PRIMARY KEY)").execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TABLE r1_child(parent INTEGER REFERENCES r1_parent(id) DEFERRABLE INITIALLY DEFERRED)").execute(&env.app.db).await.unwrap();
    sqlx::query("CREATE TRIGGER r1_commit_fault AFTER DELETE ON api_bot_sessions BEGIN INSERT INTO r1_child(parent) VALUES(3); END").execute(&env.app.db).await.unwrap();
    env.app.db.close().await;
    let app = crate::testing::restart_app(&env).await;
    app.set_startup_ready(false);
    startup_gate::barrier(&app).await;
    assert!(!app.api_startup_gate.ready());
    app.set_startup_ready(true);
    assert!(app.is_startup_ready());
}
