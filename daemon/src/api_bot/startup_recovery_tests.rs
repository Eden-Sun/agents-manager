use super::{
    adapter::AdapterIdentity,
    protocol::Response,
    receipt,
    registry::{RunTurnKey, TurnOutcome},
    runtime::{StartupError, StartupRestore},
    startup_recovery::{self, Error},
};
use am_base::db::api_bot::{self, Checkpoint, Fence};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const DEADLINE: Duration = Duration::from_secs(10);
struct Fixture {
    env: crate::testing::Env,
    identity: AdapterIdentity,
}
fn checkpoint() -> Checkpoint {
    Checkpoint::new(&json!({"messages":[{"role":"user","content":"PRIVATE_PROMPT_SENTINEL"}],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]})).unwrap()
}
fn response() -> Response {
    Response {
        id: "startup-fixture".into(),
        model: "fixture".into(),
        content: vec![
            json!({"type":"text","text":"VISIBLE_FINAL_SENTINEL"}),
            json!({"type":"thinking","thinking":"PRIVATE_THINKING_SENTINEL","signature":"PRIVATE_SIGNATURE_SENTINEL"}),
            json!({"type":"tool_use","id":"private-tool","input":{"command":"PRIVATE_TOOL_SENTINEL"}}),
        ],
        stop_reason: "end_turn".into(),
        usage: json!({}),
    }
}
async fn fixture(pending: bool) -> Fixture {
    let env = crate::testing::env().await;
    let bot = crate::db::ulid();
    let run = crate::db::ulid();
    let turn = crate::db::ulid();
    let conv = crate::db::ulid();
    let session = crate::db::ulid();
    let now = crate::db::now();
    sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at)VALUES(?,?,'startup','claude-api','fake',?)").bind(&bot).bind(&env.project_id).bind(&now).execute(&env.app.db).await.unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at)VALUES(?,?,'running',?)")
        .bind(&run)
        .bind(&bot)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO conversations(id,bot_id,created_at)VALUES(?,?,?)")
        .bind(&conv)
        .bind(&bot)
        .bind(&now)
        .execute(&env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,delivery,created_at)VALUES(?,?,?,'web','in_flight','ok',?)").bind(&turn).bind(&conv).bind(&run).bind(&now).execute(&env.app.db).await.unwrap();
    let identity = AdapterIdentity {
        session_id: session,
        bot_id: bot.clone(),
        run_id: run.clone(),
        turn_id: turn.clone(),
        generation: 1,
    };
    api_bot::create(
        &env.app.db,
        &Fence {
            session: &identity.session_id,
            bot: &bot,
            run: &run,
            turn: &turn,
            generation: 1,
            revision: 0,
        },
        &checkpoint(),
    )
    .await
    .unwrap();
    // A synthetic leftover reservation with a settled context: recovery must preserve its hold.
    sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,pricing_version,created_at)VALUES(?,?,?,?,?,1,0,'hash','fast','reserved',123,'fixture',?)")
        .bind(crate::db::ulid()).bind(&identity.session_id).bind(&bot).bind(&run).bind(&turn).bind(&now).execute(&env.app.db).await.unwrap();
    if pending {
        sqlx::query("CREATE TRIGGER stop_startup_completion BEFORE INSERT ON messages WHEN NEW.role='assistant' BEGIN SELECT RAISE(ABORT,'synthetic startup storage failure'); END").execute(&env.app.db).await.unwrap();
    }
    let key = crate::lifecycle::api_bot_port::ApiTurnKey {
        bot_id: bot,
        run_id: run,
        turn_id: turn,
    };
    let producer = env.app.api_bot_runtime.spawn_startup_fixture_owner(
        env.app.clone(),
        key,
        identity.clone(),
        response(),
    );
    if pending {
        tokio::time::timeout(DEADLINE, async {
            while producer.retries.load(std::sync::atomic::Ordering::SeqCst) < 3 {
                producer.wake.notified().await;
            }
        })
        .await
        .unwrap();
        producer.shutdown.cancel();
    }
    tokio::time::timeout(DEADLINE, producer.task)
        .await
        .unwrap()
        .unwrap();
    if pending {
        sqlx::query("DROP TRIGGER stop_startup_completion")
            .execute(&env.app.db)
            .await
            .unwrap();
    }
    Fixture { env, identity }
}
async fn restart(f: &Fixture) -> Arc<crate::state::App> {
    f.env.app.db.close().await;
    let app = crate::testing::restart_app(&f.env).await;
    app.set_startup_ready(false);
    app
}
async fn recover(app: &Arc<crate::state::App>) -> startup_recovery::Page {
    startup_recovery::recover_receipt_page(app, None, 64)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()))
}
async fn turn_status(app: &crate::state::App, id: &AdapterIdentity) -> String {
    sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
        .bind(&id.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap()
}
async fn assistant_count(app: &crate::state::App, id: &AdapterIdentity) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE turn_id=? AND role='assistant'")
        .bind(&id.turn_id)
        .fetch_one(&app.db)
        .await
        .unwrap()
}
async fn ledger(app: &crate::state::App) -> Vec<(String, Option<i64>, i64)> {
    sqlx::query_as(
        "SELECT state,estimated_microusd,reserved_microusd FROM api_bot_attempts ORDER BY id",
    )
    .fetch_all(&app.db)
    .await
    .unwrap()
}

#[tokio::test]
async fn fresh_runtime_recovers_true_join_pending_receipt_once_and_preserves_hold() {
    let f = fixture(true).await;
    let app = restart(&f).await;
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
    let first = recover(&app).await;
    assert_eq!(
        (
            first.counts.scanned,
            first.counts.applied,
            first.counts.restored
        ),
        (1, 1, 1)
    );
    assert_eq!(turn_status(&app, &f.identity).await, "completed");
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
    assert_eq!(ledger(&app).await, vec![("unknown".into(), None, 123)]);
    let second = recover(&app).await;
    assert_eq!(
        (
            second.counts.applied,
            second.counts.already_applied,
            second.counts.already_current
        ),
        (0, 1, 1)
    );
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
    let cursor = app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .unwrap();
    assert_eq!(cursor, (f.identity.clone(), 0));
    app.db.close().await;
}
#[tokio::test]
async fn applied_lost_ack_is_read_only_and_cursor_restore_is_idempotent() {
    let f = fixture(false).await;
    let app = restart(&f).await;
    let before = ledger(&app).await;
    let first = recover(&app).await;
    assert_eq!(
        (
            first.counts.applied,
            first.counts.already_applied,
            first.counts.restored
        ),
        (0, 1, 1)
    );
    assert_eq!(ledger(&app).await, before);
    assert_eq!(assistant_count(&app, &f.identity).await, 1);
    let again = recover(&app).await;
    assert_eq!(again.counts.already_current, 1);
    assert_eq!(ledger(&app).await, before);
}
#[tokio::test]
async fn unsafe_checkpoints_do_not_settle_cost_or_fabricate_completion() {
    for value in [
        json!({"messages":[],"responses":[],"api_pending":true,"tool_pending":null,"completed_tool_ids":[]}),
        json!({"messages":[],"responses":[],"api_pending":false,"tool_pending":"unknown-tool","completed_tool_ids":[]}),
        json!({"messages":[],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[],"PRIVATE_UNKNOWN_SENTINEL":"secret"}),
        json!({"messages":[false],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]}),
        json!({"messages":[{"role":"assistant","content":[{"type":"tool_use","id":"unfinished","name":"bash","input":{"command":"not executed"}}]}],"responses":[],"api_pending":false,"tool_pending":null,"completed_tool_ids":[]}),
    ] {
        let f = fixture(true).await;
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=? WHERE id=?")
            .bind(value.to_string())
            .bind(&f.identity.session_id)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        let app = restart(&f).await;
        let before = ledger(&app).await;
        let result = recover(&app).await;
        assert_eq!(result.counts.unresolved, 1);
        assert_eq!(ledger(&app).await, before);
        assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
        assert_eq!(assistant_count(&app, &f.identity).await, 0);
        assert!(app
            .api_bot_runtime
            .startup_cursor_for_test(&f.identity.bot_id)
            .is_none());
    }
}

#[tokio::test]
async fn absent_receipt_never_uses_checkpoint_as_completion_proof() {
    let f = fixture(true).await;
    sqlx::query("DELETE FROM api_bot_join_receipts")
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = restart(&f).await;
    let before = ledger(&app).await;
    let result = recover(&app).await;
    assert_eq!(result.counts.scanned, 0);
    assert_eq!(ledger(&app).await, before);
    let guard = app
        .api_bot_runtime
        .startup_guard(&app, &f.identity)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    let restored = app
        .api_bot_runtime
        .restore_startup_cursor(&app, &f.identity, 0, &guard)
        .await
        .unwrap_or_else(|e| panic!("{}", e.code()));
    assert!(matches!(restored, StartupRestore::Unresolved));
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
    assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
}
#[tokio::test]
async fn stale_owner_generation_revision_stop_and_expiry_only_fence_receipt() {
    for mutation in [
        "UPDATE api_bot_sessions SET generation=2",
        "UPDATE api_bot_sessions SET revision=1",
        "UPDATE api_bot_sessions SET owner_turn_id=NULL",
        "UPDATE api_bot_sessions SET retired_at='retired'",
        "UPDATE api_bot_sessions SET expires_at='2000-01-01T00:00:00Z'",
        "UPDATE runs SET state='stopping'",
        "UPDATE bots SET deleted_at='deleted'",
        "UPDATE bots SET kind='codex'",
    ] {
        let f = fixture(true).await;
        sqlx::query(mutation).execute(&f.env.app.db).await.unwrap();
        let app = restart(&f).await;
        let before = ledger(&app).await;
        let result = recover(&app).await;
        assert_eq!(result.counts.fenced, 1, "{mutation}");
        assert_eq!(ledger(&app).await, before, "{mutation}");
        assert_eq!(turn_status(&app, &f.identity).await, "in_flight");
        assert_eq!(assistant_count(&app, &f.identity).await, 0);
        assert_eq!(
            sqlx::query_scalar::<_, String>("SELECT state FROM api_bot_join_receipts")
                .fetch_one(&app.db)
                .await
                .unwrap(),
            "fenced"
        );
        assert!(app
            .api_bot_runtime
            .startup_cursor_for_test(&f.identity.bot_id)
            .is_none());
    }
}
#[tokio::test]
async fn forged_receipt_identity_cannot_claim_a_different_owner() {
    for field in 0..6 {
        let f = fixture(true).await;
        let now = crate::db::now();
        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at)VALUES('other-b',?,'other','claude-api','fake',?)").bind(&f.env.project_id).bind(&now).execute(&f.env.app.db).await.unwrap();
        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at)VALUES('other-r',?,'stopped',?)")
            .bind(&f.identity.bot_id)
            .bind(&now)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,created_at)SELECT 'other-t',conversation_id,run_id,'web','completed',? FROM turns WHERE id=?").bind(&now).bind(&f.identity.turn_id).execute(&f.env.app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_sessions(id,bot_id,owner_run_id,owner_turn_id,generation,checkpoint_json,created_at,updated_at,expires_at,retired_at)SELECT 'other-s',bot_id,owner_run_id,owner_turn_id,generation,checkpoint_json,created_at,updated_at,expires_at,'retired' FROM api_bot_sessions WHERE id=?").bind(&f.identity.session_id).execute(&f.env.app.db).await.unwrap();
        let mutation = match field {
            0 => "UPDATE api_bot_join_receipts SET bot_id='other-b'",
            1 => "UPDATE api_bot_join_receipts SET run_id='other-r'",
            2 => "UPDATE api_bot_join_receipts SET turn_id='other-t'",
            3 => "UPDATE api_bot_join_receipts SET session_id='other-s'",
            4 => "UPDATE api_bot_join_receipts SET generation=2",
            _ => "UPDATE api_bot_join_receipts SET revision=1",
        };
        sqlx::query(mutation).execute(&f.env.app.db).await.unwrap();
        let app = restart(&f).await;
        let before = ledger(&app).await;
        let result = recover(&app).await;
        assert_eq!(result.counts.fenced, 1, "{mutation}");
        assert_eq!(ledger(&app).await, before);
        assert_eq!(assistant_count(&app, &f.identity).await, 0);
    }
}
#[tokio::test]
async fn newer_active_run_and_live_registry_owner_are_rejected_without_cost_mutation() {
    let f = fixture(true).await;
    sqlx::query("UPDATE runs SET state='stopped' WHERE id=?")
        .bind(&f.identity.run_id)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    sqlx::query("INSERT INTO runs(id,bot_id,state,started_at)VALUES('successor',?,'running',?)")
        .bind(&f.identity.bot_id)
        .bind(crate::db::now())
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = restart(&f).await;
    let before = ledger(&app).await;
    let result = recover(&app).await;
    assert_eq!(result.counts.fenced, 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT state FROM runs WHERE id='successor'")
            .fetch_one(&app.db)
            .await
            .unwrap(),
        "running"
    );
    assert_eq!(ledger(&app).await, before);
    let f = fixture(true).await;
    let app = restart(&f).await;
    let before = ledger(&app).await;
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
    let unrelated_runtime = super::runtime::ApiBotRuntime::new();
    assert!(
        matches!(
            unrelated_runtime.startup_guard(&app, &f.identity).await,
            Err(StartupError::NotStartup)
        ),
        "an unrelated empty runtime cannot stand in for the App's live owner"
    );
    let result = recover(&app).await;
    assert_eq!(result.counts.unresolved, 1);
    assert_eq!(ledger(&app).await, before);
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
    ticket.cancel_and_join().await.unwrap();
}
struct NeverTools(std::sync::atomic::AtomicUsize);
impl super::runner::Tools for NeverTools {
    fn execute<'a>(
        &'a self,
        _call: &'a super::protocol::ToolCall,
        _cancel: &'a CancellationToken,
    ) -> futures::future::BoxFuture<'a, Result<String, String>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Err("fixture forbids tools".into()) })
    }
}
#[tokio::test]
async fn ready_app_or_provider_dependencies_forbid_startup_restore() {
    let f = fixture(true).await;
    let app = restart(&f).await;
    app.set_startup_ready(true);
    assert!(matches!(
        startup_recovery::recover_receipt_page(&app, None, 64).await,
        Err(Error::NotStartup)
    ));
    app.set_startup_ready(false);
    let tools = Arc::new(NeverTools(std::sync::atomic::AtomicUsize::new(0)));
    app.api_bot_runtime
        .startup_set_fake_dependencies_for_test(tools.clone());
    let before = ledger(&app).await;
    assert!(matches!(
        app.api_bot_runtime.startup_guard(&app, &f.identity).await,
        Err(StartupError::NotStartup)
    ));
    assert!(matches!(
        startup_recovery::recover_receipt_page(&app, None, 64).await,
        Err(Error::NotStartup)
    ));
    assert_eq!(ledger(&app).await, before);
    assert_eq!(tools.0.load(std::sync::atomic::Ordering::SeqCst), 0);
}
#[tokio::test]
async fn cursor_cas_does_not_overwrite_a_newer_cache_or_publish_after_startup() {
    let f = fixture(false).await;
    let app = restart(&f).await;
    let mut newer = f.identity.clone();
    newer.turn_id = "newer-turn".into();
    let expected = newer.clone();
    let target = app.clone();
    crate::lifecycle::race_point::arm(
        "api_startup_before_cursor_cas",
        &f.identity.bot_id,
        move || async move {
            target.api_bot_runtime.put_startup_cursor_for_test(newer, 7);
        },
    );
    let result = recover(&app).await;
    assert_eq!(result.counts.fenced, 1);
    assert_eq!(
        app.api_bot_runtime
            .startup_cursor_for_test(&f.identity.bot_id)
            .unwrap(),
        (expected, 7)
    );
    let f = fixture(false).await;
    let app = restart(&f).await;
    let target = app.clone();
    crate::lifecycle::race_point::arm(
        "api_startup_before_cursor_cas",
        &f.identity.bot_id,
        move || async move {
            target.set_startup_ready(true);
        },
    );
    let result = recover(&app).await;
    assert_eq!(result.counts.fenced, 1);
    assert!(app
        .api_bot_runtime
        .startup_cursor_for_test(&f.identity.bot_id)
        .is_none());
}
#[tokio::test]
async fn page_bounds_keyset_and_read_errors_are_not_silently_empty() {
    let f = fixture(true).await;
    let app = restart(&f).await;
    for limit in [0, 65] {
        assert!(matches!(
            startup_recovery::recover_receipt_page(&app, None, limit).await,
            Err(Error::Bounds)
        ));
    }
    // Copy only synthetic private rows, with the same timestamp to exercise tie-breaking.
    for turn in ["page-b", "page-c"] {
        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,created_at)SELECT ?,conversation_id,run_id,'web','completed',created_at FROM turns WHERE id=?").bind(turn).bind(&f.identity.turn_id).execute(&app.db).await.unwrap();
        sqlx::query("INSERT INTO api_bot_join_receipts(session_id,bot_id,run_id,turn_id,generation,revision,outcome,final_text,created_at)SELECT session_id,bot_id,run_id,?,generation,revision,outcome,final_text,created_at FROM api_bot_join_receipts WHERE turn_id=?").bind(turn).bind(&f.identity.turn_id).execute(&app.db).await.unwrap();
    }
    let first = receipt::startup_page(&app.db, None, 1).await.unwrap();
    assert_eq!(first.rows.len(), 1);
    let first_id = first.rows[0].item.identity.turn_id.clone();
    let cursor = first.next.unwrap();
    sqlx::query("DELETE FROM api_bot_join_receipts WHERE turn_id=?")
        .bind(&first_id)
        .execute(&app.db)
        .await
        .unwrap();
    let second = receipt::startup_page(&app.db, Some(&cursor), 2)
        .await
        .unwrap();
    assert_eq!(second.rows.len(), 2);
    assert!(second
        .rows
        .iter()
        .all(|row| row.item.identity.turn_id != first_id));
    let end = receipt::startup_page(&app.db, second.next.as_ref(), 2)
        .await
        .unwrap();
    assert!(end.rows.is_empty());
    sqlx::query("ALTER TABLE api_bot_join_receipts RENAME TO unreadable_receipts")
        .execute(&app.db)
        .await
        .unwrap();
    assert!(matches!(
        startup_recovery::recover_receipt_page(&app, None, 64).await,
        Err(Error::Storage)
    ));
}
#[tokio::test]
async fn malformed_oversized_checkpoint_and_unreadable_session_are_fail_closed() {
    for raw in [
        "not-json".into(),
        "x".repeat(api_bot::MAX_CHECKPOINT_BYTES + 1),
    ] {
        let f = fixture(true).await;
        sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=?")
            .bind(raw)
            .execute(&f.env.app.db)
            .await
            .unwrap();
        let app = restart(&f).await;
        let before = ledger(&app).await;
        assert_eq!(recover(&app).await.counts.unresolved, 1);
        assert_eq!(ledger(&app).await, before);
    }
    let f = fixture(true).await;
    let app = restart(&f).await;
    sqlx::query("ALTER TABLE api_bot_sessions RENAME TO unreadable_sessions")
        .execute(&app.db)
        .await
        .unwrap();
    assert!(matches!(
        startup_recovery::recover_receipt_page(&app, None, 64).await,
        Err(Error::Storage)
    ));
}
#[tokio::test]
async fn private_state_stays_out_of_projection_and_nonreserved_ledger_is_unchanged() {
    let f = fixture(true).await;
    for (ordinal, state, cost) in [
        (1, "recorded", Some(9i64)),
        (2, "failed", Some(0i64)),
        (3, "unknown", None),
    ] {
        sqlx::query("INSERT INTO api_bot_attempts(id,session_id,bot_id,run_id,turn_id,generation,ordinal,request_hash,requested_speed,state,reserved_microusd,estimated_microusd,pricing_version,created_at)VALUES(?,?,?,?,?,1,?,'extra-hash','standard',?,999,?,'unchanged',?)")
            .bind(format!("extra-{ordinal}")).bind(&f.identity.session_id).bind(&f.identity.bot_id).bind(&f.identity.run_id).bind(&f.identity.turn_id).bind(ordinal).bind(state).bind(cost).bind(crate::db::now()).execute(&f.env.app.db).await.unwrap();
    }
    let app = restart(&f).await;
    let query="SELECT json_object('id',id,'session_id',session_id,'bot_id',bot_id,'run_id',run_id,'turn_id',turn_id,'generation',generation,'ordinal',ordinal,'request_hash',request_hash,'requested_speed',requested_speed,'actual_speed',actual_speed,'state',state,'usage_json',usage_json,'reserved_microusd',reserved_microusd,'estimated_microusd',estimated_microusd,'pricing_version',pricing_version,'created_at',created_at,'finished_at',finished_at) FROM api_bot_attempts WHERE ordinal>0 ORDER BY id";
    let before: Vec<String> = sqlx::query_scalar(query).fetch_all(&app.db).await.unwrap();
    recover(&app).await;
    let after: Vec<String> = sqlx::query_scalar(query).fetch_all(&app.db).await.unwrap();
    assert_eq!(after, before);
    let projection = crate::api::state_json(&app).await.unwrap().to_string();
    for private in [
        "PRIVATE_PROMPT_SENTINEL",
        "PRIVATE_THINKING_SENTINEL",
        "PRIVATE_SIGNATURE_SENTINEL",
        "PRIVATE_TOOL_SENTINEL",
    ] {
        assert!(!projection.contains(private));
    }
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT final_text FROM api_bot_join_receipts")
            .fetch_one(&app.db)
            .await
            .unwrap(),
        "VISIBLE_FINAL_SENTINEL"
    );
}
#[tokio::test]
async fn restore_does_not_nest_pool_transactions_and_works_with_one_connection() {
    let f = fixture(true).await;
    f.env.app.db.close().await;
    let data = f.env.dir.join("data");
    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(data.join("db.sqlite3"))
                .foreign_keys(true),
        )
        .await
        .unwrap();
    let client = crate::herdr::HerdrClient::new(data.join("herdr.sock"));
    let cfg = crate::runners::app_ports_p2::load_config(data.join("config.toml"))
        .await
        .unwrap();
    let app = crate::state::App::new(
        pool,
        client.clone(),
        client,
        cfg,
        data.clone(),
        data.join("agents-managerd"),
        7799,
        "test-token".into(),
        "test".into(),
        false,
    );
    app.set_startup_ready(false);
    let page = tokio::time::timeout(DEADLINE, recover(&app)).await.unwrap();
    assert_eq!(page.counts.restored, 1);
    app.db.close().await;
}

#[tokio::test]
async fn completed_tool_history_is_preserved_without_replaying_tools() {
    let f = fixture(true).await;
    let tool =
        json!({"type":"tool_use","id":"finished","name":"bash","input":{"command":"fixture only"}});
    let state = json!({"messages":[{"role":"assistant","content":[tool.clone()]},{"role":"user","content":[{"type":"tool_result","tool_use_id":"finished","content":"PRIVATE_TOOL_OUTPUT","is_error":false}]}],"responses":[{"id":"tool-response","model":"claude-opus-5-5","content":[tool],"stop_reason":"tool_use","usage":{}}],"api_pending":false,"tool_pending":null,"completed_tool_ids":["finished"]});
    let original = state.to_string();
    sqlx::query("UPDATE api_bot_sessions SET checkpoint_json=?")
        .bind(&original)
        .execute(&f.env.app.db)
        .await
        .unwrap();
    let app = restart(&f).await;
    let page = recover(&app).await;
    assert_eq!((page.counts.applied, page.counts.restored), (1, 1));
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT checkpoint_json FROM api_bot_sessions")
            .fetch_one(&app.db)
            .await
            .unwrap(),
        original
    );
}
