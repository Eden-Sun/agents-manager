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

struct DefaultPort;
impl crate::lifecycle::api_bot_port::ApiBotPort for DefaultPort {
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
