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
