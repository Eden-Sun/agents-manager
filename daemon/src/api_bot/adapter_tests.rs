//! Assembly and integration tests for inert Claude API bot adapter and registry.
//!
//! Uses ONLY fake loopback HTTP and fresh in-memory SQLite fixtures (schema 69).
//! No paid endpoints, key files, daemon restarts, or SSH commands.
//! Synchronization is strictly based on Atomic counters, channels, and Notify; NO arbitrary sleep.

#[cfg(test)]
mod tests {
    use crate::api_bot::adapter::{
        AdapterHttpConfig, AdapterIdentity, AdapterLimits, ApiBotAdapter,
    };
    use crate::api_bot::protocol::{self, Message, Options, Response, ToolCall};
    use crate::api_bot::quote::{
        QuoteError, QuoteProvider, RequestEnvelope, ReservationQuote, UsageQuote,
    };
    use crate::api_bot::registry::{ApiBotRegistry, RunTurnKey, TurnOutcome};
    use crate::api_bot::runner::{self, Halt, State};
    use axum::extract::State as AxumState;
    use axum::routing::post;
    use axum::{Json, Router};
    use sqlx::SqlitePool;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;
    use tokio::sync::{oneshot, Notify};
    use tokio_util::sync::CancellationToken;

    const DEADLOCK_TIMEOUT: Duration = Duration::from_secs(10);

    fn poll_once<F: Future>(mut fut: Pin<&mut F>) -> Poll<F::Output> {
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        fut.as_mut().poll(&mut cx)
    }

    /// Real in-memory SQLite pool matching Schema 69 fixture.
    async fn setup_test_db() -> (SqlitePool, String, String, String, String) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("sqlite memory pool");

        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&pool)
            .await
            .expect("foreign keys pragma");

        crate::runners::am_base_tests::migrate(&pool)
            .await
            .expect("schema migrations");

        let project_id = "p";
        let bot_id = "b";
        let run_id = "r";
        let turn_id = "t1";
        let session_id = "s";

        let now = am_base::db::now();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES(?,'/test','p',?)")
            .bind(project_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO bots(id,project_id,name,kind,hook_token,created_at) VALUES(?,?,'bot','claude-api','fixture',?)")
            .bind(bot_id)
            .bind(project_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO runs(id,bot_id,state,started_at) VALUES(?,?,'running',?)")
            .bind(run_id)
            .bind(bot_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('c',?,?)")
            .bind(bot_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,created_at) VALUES(?,'c',?,'web','in_flight',?)")
            .bind(turn_id)
            .bind(run_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        (
            pool,
            bot_id.into(),
            run_id.into(),
            turn_id.into(),
            session_id.into(),
        )
    }

    struct MockServerState {
        wire_calls: Arc<AtomicUsize>,
        response_json: serde_json::Value,
        pause_notify: Option<Arc<Notify>>,
        unpause_notify: Option<Arc<Notify>>,
        received_bodies: Option<Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>>,
        shutdown_token: CancellationToken,
    }

    struct MockServerHandle {
        endpoint: String,
        shutdown_token: CancellationToken,
        join_handle: Option<tokio::task::JoinHandle<()>>,
        unpause_notify: Option<Arc<Notify>>,
    }

    impl MockServerHandle {
        async fn shutdown(mut self) {
            self.shutdown_token.cancel();
            if let Some(ref u) = self.unpause_notify {
                u.notify_waiters();
            }
            if let Some(h) = self.join_handle.take() {
                tokio::time::timeout(DEADLOCK_TIMEOUT, h)
                    .await
                    .expect("mock server join timed out")
                    .expect("mock server panicked during shutdown");
            }
        }
    }

    impl Drop for MockServerHandle {
        fn drop(&mut self) {
            self.shutdown_token.cancel();
            if let Some(ref u) = self.unpause_notify {
                u.notify_waiters();
            }
        }
    }

    /// Spawns a genuine Axum loopback HTTP server with graceful shutdown RAII.
    async fn spawn_mock_server(
        wire_calls: Arc<AtomicUsize>,
        response_json: serde_json::Value,
        pause_notify: Option<Arc<Notify>>,
        unpause_notify: Option<Arc<Notify>>,
        received_bodies: Option<Arc<tokio::sync::Mutex<Vec<serde_json::Value>>>>,
    ) -> MockServerHandle {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{}/v1/messages", port);

        let shutdown_token = CancellationToken::new();
        let shutdown_child = shutdown_token.clone();
        let unpause_ref = unpause_notify.clone();

        let shared_state = Arc::new(MockServerState {
            wire_calls,
            response_json,
            pause_notify,
            unpause_notify,
            received_bodies,
            shutdown_token: shutdown_token.clone(),
        });

        let app = Router::new()
            .route(
                "/v1/messages",
                post(
                    |AxumState(st): AxumState<Arc<MockServerState>>,
                     Json(body): Json<serde_json::Value>| async move {
                        st.wire_calls.fetch_add(1, Ordering::SeqCst);
                        if let Some(ref b) = st.received_bodies {
                            b.lock().await.push(body.clone());
                        }

                        if let Some(ref p) = st.pause_notify {
                            p.notify_one();
                        }
                        if let Some(ref u) = st.unpause_notify {
                            tokio::select! {
                                _ = u.notified() => {}
                                _ = st.shutdown_token.cancelled() => {}
                            }
                        }

                        Json(st.response_json.clone())
                    },
                ),
            )
            .with_state(shared_state);

        let join_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_child.cancelled().await;
                })
                .await;
        });

        MockServerHandle {
            endpoint,
            shutdown_token,
            join_handle: Some(join_handle),
            unpause_notify: unpause_ref,
        }
    }

    struct TestQuoteProvider {
        reserved_amount: i64,
        estimated_amount: i64,
    }

    impl QuoteProvider for TestQuoteProvider {
        fn reserve(&self, _req: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
            Ok(ReservationQuote {
                reserved_microusd: self.reserved_amount,
                pricing_version: "test-v1".into(),
            })
        }
        fn settle(
            &self,
            _req: &RequestEnvelope,
            resp: &Response,
        ) -> Result<UsageQuote, QuoteError> {
            let u = resp.usage.as_object().ok_or(QuoteError::UntrustedUsage)?;
            let input = u.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(-1);
            let output = u
                .get("output_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            if input < 0 || output < 0 {
                return Err(QuoteError::UntrustedUsage);
            }
            Ok(UsageQuote {
                estimated_microusd: self.estimated_amount,
                actual_speed: Some("standard"),
            })
        }
    }

    struct FailClosedQuoteProvider;
    impl QuoteProvider for FailClosedQuoteProvider {
        fn reserve(&self, _: &RequestEnvelope) -> Result<ReservationQuote, QuoteError> {
            Err(QuoteError::Unapproved)
        }
        fn settle(&self, _: &RequestEnvelope, _: &Response) -> Result<UsageQuote, QuoteError> {
            Err(QuoteError::Unapproved)
        }
    }

    fn test_options() -> Options {
        Options {
            effort: "medium".into(),
            fast: false,
            max_tokens: 4096,
        }
    }

    fn test_limits() -> AdapterLimits {
        AdapterLimits {
            max_checkpoint_bytes: 8 * 1024 * 1024,
            max_turn_microusd: 50_000,
            max_requests_per_hour: 100,
        }
    }

    // =========================================================================
    // Safety Tests
    // =========================================================================

    #[tokio::test]
    async fn test_single_fresh_reservation_arms_one_shot_permit_verified_by_http() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let server = spawn_mock_server(
            wire_calls.clone(),
            serde_json::json!({
                "id": "msg_permit",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Permit used"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            None,
            None,
            None,
        )
        .await;

        let (pool, bot, run, turn, session) = setup_test_db().await;
        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run,
                turn_id: turn,
                generation: 1,
            },
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Permit message"),
        }];

        // Complete without reserve fails; exactly 0 HTTP calls
        use runner::Model;
        assert!(adapter.complete(&messages, &cancel).await.is_err());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 0);

        // Reserve checkpoint
        let mut state = State::default();
        state.messages = messages.clone();
        state.api_pending = true;

        use runner::Journal;
        adapter.save(&state).await.expect("reserve checkpoint");

        // First complete consumes permit and dispatches 1 HTTP call
        let resp1 = adapter.complete(&messages, &cancel).await;
        assert!(resp1.is_ok());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 1);

        // Second complete fails immediately; wire calls remain 1
        let resp2 = adapter.complete(&messages, &cancel).await;
        assert!(resp2.is_err());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 1);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_fullhash_and_exact_wire_body_match() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(tokio::sync::Mutex::new(Vec::new()));

        let server = spawn_mock_server(
            wire_calls.clone(),
            serde_json::json!({
                "id": "msg_wire",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Wire ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            None,
            None,
            Some(bodies.clone()),
        )
        .await;

        let (pool, bot, run, turn, session) = setup_test_db().await;
        let options = test_options();
        let system = serde_json::json!("System wire match");
        let messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Wire content"),
        }];

        let expected_envelope =
            RequestEnvelope::new(options.clone(), system.clone(), messages.clone()).unwrap();

        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run,
                turn_id: turn,
                generation: 1,
            },
            options.clone(),
            system.clone(),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::default();
        state.messages = messages.clone();
        state.api_pending = true;

        use runner::Journal;
        adapter.save(&state).await.unwrap();

        // 1. Attempt dispatch with mismatched message content -> zero wire calls
        let mismatched_messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Altered content"),
        }];
        use runner::Model;
        assert!(adapter
            .complete(&mismatched_messages, &cancel)
            .await
            .is_err());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 0);

        // 2. Dispatch with original messages -> 1 wire call, body exactly matches expected
        let resp = adapter.complete(&messages, &cancel).await;
        assert!(resp.is_ok());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 1);

        let received = bodies.lock().await;
        assert_eq!(received.len(), 1);
        assert_eq!(received[0], expected_envelope.body());

        drop(received);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_fail_closed_quote_provider_proves_zero_http_calls() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let server =
            spawn_mock_server(wire_calls.clone(), serde_json::json!({}), None, None, None).await;

        let (pool, bot, run, turn, session) = setup_test_db().await;
        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run,
                turn_id: turn,
                generation: 1,
            },
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(FailClosedQuoteProvider),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Hello"),
        }];
        state.api_pending = true;

        use runner::Journal;
        assert!(adapter.save(&state).await.is_err());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 0);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_db_failure_leaves_zero_http_side_effects() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let server =
            spawn_mock_server(wire_calls.clone(), serde_json::json!({}), None, None, None).await;

        let (pool, bot, run, turn, session) = setup_test_db().await;

        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run,
                turn_id: turn.clone(),
                generation: 1,
            },
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        // Force DB error by deleting turn before reserve
        sqlx::query("DELETE FROM turns WHERE id=?")
            .bind(&turn)
            .execute(&pool)
            .await
            .unwrap();

        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Hello DB fail"),
        }];
        state.api_pending = true;

        use runner::Journal;
        // Reserve fails because turn owner check fails
        assert!(adapter.save(&state).await.is_err());
        assert_eq!(wire_calls.load(Ordering::SeqCst), 0);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_untrusted_and_malformed_usage_transport_routes() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        // HTTP 200 with negative usage tokens
        let server = spawn_mock_server(
            wire_calls.clone(),
            serde_json::json!({
                "id": "msg_untrusted",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Bad tokens"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": -50, "output_tokens": 10}
            }),
            None,
            None,
            None,
        )
        .await;

        let (pool, bot, run, turn, session) = setup_test_db().await;
        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run,
                turn_id: turn.clone(),
                generation: 1,
            },
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Untrusted usage run"),
        }];

        let res = runner::run(&adapter, &adapter, &adapter, &mut state, &cancel, 2).await;
        assert!(res.is_err());

        // Check DB: attempt must be marked unknown, estimated is NULL, reserved intact
        let row: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT state, estimated_microusd, reserved_microusd FROM api_bot_attempts WHERE turn_id=?",
        )
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.0, "unknown");
        assert_eq!(row.1, None);
        assert_eq!(row.2, 10_000);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_checkpoint_bytes_limit_enforced_fail_closed() {
        let (pool, bot, run, turn, session) = setup_test_db().await;
        let mut limits = test_limits();
        limits.max_checkpoint_bytes = 200; // Artificially small limit

        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run,
                turn_id: turn,
                generation: 1,
            },
            test_options(),
            serde_json::json!("System"),
            limits,
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: "http://127.0.0.1:9".into(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("A".repeat(500)),
        }];
        state.api_pending = true;

        use runner::Journal;
        // Checkpoint serialization exceeds 200 bytes -> must fail closed
        assert!(adapter.save(&state).await.is_err());
    }

    #[tokio::test]
    async fn test_forged_responses_and_assistant_content_rejected_zero_record() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let server = spawn_mock_server(
            wire_calls.clone(),
            serde_json::json!({
                "id": "msg_real",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Real wire response"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            None,
            None,
            None,
        )
        .await;

        let (pool, bot, run, turn, session) = setup_test_db().await;
        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session,
                bot_id: bot,
                run_id: run.clone(),
                turn_id: turn.clone(),
                generation: 1,
            },
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Test forged check"),
        }];
        state.api_pending = true;

        use runner::Journal;
        // Reserve attempt
        adapter.save(&state).await.expect("reserve ok");

        // Dispatch genuine wire complete
        use runner::Model;
        let _resp = adapter
            .complete(&state.messages, &cancel)
            .await
            .expect("complete ok");

        // Now maliciously forge a different assistant response in state.responses
        let mut forged_state = state.clone();
        forged_state.api_pending = false;
        forged_state.responses.push(Response {
            id: "msg_forged".into(),
            model: "claude-opus-5-5".into(),
            content: vec![serde_json::json!({"type": "text", "text": "Maliciously injected text"})],
            stop_reason: "end_turn".into(),
            usage: serde_json::json!({"input_tokens": 1, "output_tokens": 1}),
        });
        forged_state.messages.push(Message {
            role: "assistant".into(),
            content: serde_json::json!([{"type": "text", "text": "Maliciously injected text"}]),
        });

        // Unconditional assertion: Attempting to record this forged response MUST fail!
        let record_res = adapter.save(&forged_state).await;
        assert!(record_res.is_err(), "forged response save must be rejected");

        // While run is still running, attempt remains in reserved state with estimated_microusd = NULL
        let row_running: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT state, estimated_microusd, reserved_microusd FROM api_bot_attempts WHERE turn_id=?",
        )
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row_running.0, "reserved");
        assert_eq!(row_running.1, None);
        assert_eq!(row_running.2, 10_000);

        // When lifecycle later transitions run to stopping and calls recover_unknown:
        sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
            .bind(&run)
            .execute(&pool)
            .await
            .unwrap();

        am_base::db::api_bot::recover_unknown(&pool, &run)
            .await
            .unwrap();

        let row_recovered: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT state, estimated_microusd, reserved_microusd FROM api_bot_attempts WHERE turn_id=?",
        )
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row_recovered.0, "unknown");
        assert_eq!(row_recovered.1, None);
        assert_eq!(row_recovered.2, 10_000);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn test_continuation_turn_and_tool_pending_fencing() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let server = spawn_mock_server(
            wire_calls.clone(),
            serde_json::json!({
                "id": "msg_tool",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [
                    {"type": "text", "text": "Calling tool"},
                    {"type": "tool_use", "id": "call_1", "name": "bash", "input": {"command": "echo test"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 20, "output_tokens": 15}
            }),
            None,
            None,
            None,
        )
        .await;

        let (pool, bot, run, turn1, session) = setup_test_db().await;
        let old_identity = AdapterIdentity {
            session_id: session.clone(),
            bot_id: bot.clone(),
            run_id: run.clone(),
            turn_id: turn1.clone(),
            generation: 1,
        };

        let adapter = ApiBotAdapter::new(
            pool.clone(),
            old_identity.clone(),
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await
        .unwrap();

        // Simulate tool pending checkpoint
        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Run tool"),
        }];
        state.api_pending = false;
        state.tool_pending = Some("call_1".into());

        use runner::Journal;
        // Save tool pending checkpoint
        adapter.save(&state).await.expect("save tool pending");
        assert_eq!(adapter.current_revision().await, 1);

        // Attempting to reserve while tool_pending is active MUST fail closed
        state.api_pending = true;
        assert!(adapter.save(&state).await.is_err());
        assert_eq!(adapter.current_revision().await, 1);

        // Resolve tool pending
        state.api_pending = false;
        state.tool_pending = None;
        adapter.save(&state).await.expect("settle tool pending");
        let settled_rev = adapter.current_revision().await;
        assert_eq!(settled_rev, 2);

        // Now setup second turn for rebind
        let now = am_base::db::now();
        sqlx::query("UPDATE turns SET status='completed' WHERE id=?")
            .bind(&turn1)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,origin,status,created_at) VALUES('t2','c',?,'web','in_flight',?)")
            .bind(&run)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        let new_identity = AdapterIdentity {
            session_id: session,
            bot_id: bot.clone(),
            run_id: run.clone(),
            turn_id: "t2".into(),
            generation: 2, // generation advances to old + 1
        };

        // Rebind constructor for turn 2 succeeds cleanly
        let rebind_res = ApiBotAdapter::rebind(
            pool.clone(),
            &old_identity,
            settled_rev, // dynamically retrieved settled revision (2)
            new_identity,
            test_options(),
            serde_json::json!("System"),
            test_limits(),
            Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            }),
            AdapterHttpConfig {
                endpoint: server.endpoint.clone(),
                key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
            None,
        )
        .await;

        assert!(rebind_res.is_ok());

        server.shutdown().await;
    }

    // =========================================================================
    // 32-Round Interleaving Stress Test (Registry + Adapter + Stop Race)
    // =========================================================================

    #[tokio::test]
    async fn test_adapter_registry_stop_race_32_rounds() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let pause_notify = Arc::new(Notify::new());
        let unpause_notify = Arc::new(Notify::new());

        let server = spawn_mock_server(
            wire_calls.clone(),
            serde_json::json!({
                "id": "msg_race",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Racing response"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 20, "output_tokens": 10}
            }),
            Some(pause_notify.clone()),
            Some(unpause_notify.clone()),
            None,
        )
        .await;

        let total_iterations = 32;

        for i in 0..total_iterations {
            let (pool, bot, run, turn, session) = setup_test_db().await;
            let registry = Arc::new(ApiBotRegistry::new());

            let adapter = Arc::new(
                ApiBotAdapter::new(
                    pool.clone(),
                    AdapterIdentity {
                        session_id: session,
                        bot_id: bot,
                        run_id: run.clone(),
                        turn_id: turn.clone(),
                        generation: 1,
                    },
                    test_options(),
                    serde_json::json!("System"),
                    test_limits(),
                    Arc::new(TestQuoteProvider {
                        reserved_amount: 10_000,
                        estimated_amount: 5_000,
                    }),
                    AdapterHttpConfig {
                        endpoint: server.endpoint.clone(),
                        key_source: Arc::new(super::super::adapter::FixedTestKey("sk-ant-test-key".into())),
                        timeout: std::time::Duration::from_secs(5),
                        max_response_bytes: 1024 * 1024,
                    },
                    None,
                )
                .await
                .unwrap(),
            );

            let adapter_runner = adapter.clone();
            let key = RunTurnKey {
                run_id: run.clone(),
                turn_id: turn.clone(),
            };

            let (tx_result, rx_result) = oneshot::channel();
            let release_gate = Arc::new(Notify::new());
            let release_gate_child = release_gate.clone();

            let _token = registry
                .spawn(key.clone(), move |cancel| async move {
                    let mut state = State::default();
                    state.messages = vec![Message {
                        role: "user".into(),
                        content: serde_json::json!("Race turn"),
                    }];
                    let outcome = runner::run(
                        &*adapter_runner,
                        &*adapter_runner,
                        &*adapter_runner,
                        &mut state,
                        &cancel,
                        3,
                    )
                    .await;

                    // Send the concrete runner::Result to coordinator BEFORE exiting
                    let _ = tx_result.send(outcome);
                    release_gate_child.notified().await;
                    Ok(TurnOutcome::Completed)
                })
                .unwrap();

            // Wait for HTTP server to receive the request
            tokio::time::timeout(DEADLOCK_TIMEOUT, pause_notify.notified())
                .await
                .expect("server received HTTP call within timeout");

            if i % 2 == 0 {
                // Stop wins branch:
                // 1. CAS run to stopping while HTTP is suspended
                sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
                    .bind(&run)
                    .execute(&pool)
                    .await
                    .unwrap();

                // 2. Poll cancel_and_join to Pending to guarantee cancel token is fired
                let mut join_fut = Box::pin(registry.cancel_and_join(&run));
                assert!(poll_once(join_fut.as_mut()).is_pending());

                // 3. Unpause server so HTTP completes, but adapter discovers owner is stopping
                unpause_notify.notify_one();

                // 4. Await runner result proof: must match Cancelled or Journal error
                let runner_res = tokio::time::timeout(DEADLOCK_TIMEOUT, rx_result)
                    .await
                    .expect("rx_result within timeout")
                    .expect("channel valid");

                assert!(
                    matches!(
                        runner_res,
                        Err(runner::Error::Cancelled)
                            | Err(runner::Error::Model(crate::api_bot::http::Error::Cancelled))
                            | Err(runner::Error::Journal)
                    ),
                    "runner must fail due to cancellation or journal owner CAS failure, got {:?}",
                    runner_res
                );

                // 5. Release task and await join completion
                release_gate.notify_one();
                let join_outcome = tokio::time::timeout(DEADLOCK_TIMEOUT, join_fut)
                    .await
                    .expect("join within timeout")
                    .expect("join outcome valid");
                assert_eq!(join_outcome, TurnOutcome::Completed);
                assert!(!registry.is_active(&run));

                // 6. Call recover_unknown and verify DB
                am_base::db::api_bot::recover_unknown(&pool, &run)
                    .await
                    .unwrap();

                let row: (String, Option<i64>, i64) = sqlx::query_as(
                    "SELECT state, estimated_microusd, reserved_microusd FROM api_bot_attempts WHERE turn_id=?",
                )
                .bind(&turn)
                .fetch_one(&pool)
                .await
                .unwrap();

                assert_eq!(row.0, "unknown");
                assert_eq!(row.1, None);
                assert_eq!(row.2, 10_000);
            } else {
                // Record wins branch:
                // 1. Unpause server immediately to let HTTP complete and record commit
                unpause_notify.notify_one();

                // 2. Await runner result proof: must be EndTurn and recorded in DB
                let runner_res = tokio::time::timeout(DEADLOCK_TIMEOUT, rx_result)
                    .await
                    .expect("rx_result within timeout")
                    .expect("channel valid");
                assert_eq!(runner_res, Ok(Halt::EndTurn));

                // 3. Poll cancel_and_join to Pending to ensure subscription
                let mut join_fut = Box::pin(registry.cancel_and_join(&run));
                assert!(poll_once(join_fut.as_mut()).is_pending());

                // 4. Release task to allow it to exit cleanly
                release_gate.notify_one();

                let outcome = tokio::time::timeout(DEADLOCK_TIMEOUT, join_fut)
                    .await
                    .expect("join within timeout")
                    .unwrap();
                assert_eq!(outcome, TurnOutcome::Completed);
                assert!(!registry.is_active(&run));

                // 5. Verify attempt in DB is recorded with estimated microUSD
                let row: (String, Option<i64>, i64) = sqlx::query_as(
                    "SELECT state, estimated_microusd, reserved_microusd FROM api_bot_attempts WHERE turn_id=?",
                )
                .bind(&turn)
                .fetch_one(&pool)
                .await
                .unwrap();

                assert_eq!(row.0, "recorded");
                assert_eq!(row.1, Some(5_000));
                assert_eq!(row.2, 10_000);
            }
        }

        // All 32 rounds sent genuine wire HTTP requests
        assert_eq!(wire_calls.load(Ordering::SeqCst), total_iterations);

        server.shutdown().await;
    }
}
