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
    use crate::api_bot::protocol::{self, Message, Options, Response};
    use crate::api_bot::quote::{
        QuoteError, QuoteProvider, RequestEnvelope, ReservationQuote, UsageQuote,
    };
    use crate::api_bot::registry::{ApiBotRegistry, RunTurnKey, TurnOutcome};
    use crate::api_bot::runner::{self, Halt, State};
    use axum::extract::State as AxumState;
    use axum::routing::post;
    use axum::{Json, Router};
    use sqlx::SqlitePool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::{oneshot, Notify};
    use tokio_util::sync::CancellationToken;

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

        am_base::db::migrate_with(&pool, &[])
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

        (pool, bot_id.into(), run_id.into(), turn_id.into(), session_id.into())
    }

    struct MockServerState {
        wire_calls: Arc<AtomicUsize>,
        response_json: serde_json::Value,
        pause_notify: Option<Arc<Notify>>,
        unpause_notify: Option<Arc<Notify>>,
    }

    /// Spawns a genuine Axum loopback HTTP server that fully reads the request body and returns typed JSON.
    async fn spawn_mock_server(
        state: MockServerState,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{}/v1/messages", port);

        let shared_state = Arc::new(state);

        let app = Router::new()
            .route(
                "/v1/messages",
                post(
                    |AxumState(st): AxumState<Arc<MockServerState>>,
                     Json(body): Json<serde_json::Value>| async move {
                        st.wire_calls.fetch_add(1, Ordering::SeqCst);
                        assert!(body.is_object(), "request body must be valid json object");

                        if let Some(ref p) = st.pause_notify {
                            p.notify_one();
                        }
                        if let Some(ref u) = st.unpause_notify {
                            u.notified().await;
                        }

                        Json(st.response_json.clone())
                    },
                ),
            )
            .with_state(shared_state);

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (endpoint, handle)
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
            _resp: &Response,
        ) -> Result<UsageQuote, QuoteError> {
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
        let (endpoint, _server) = spawn_mock_server(MockServerState {
            wire_calls: wire_calls.clone(),
            response_json: serde_json::json!({
                "id": "msg_permit",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Permit used"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }),
            pause_notify: None,
            unpause_notify: None,
        })
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
                endpoint,
                key: "sk-ant-test-key".into(),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
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
    }

    #[tokio::test]
    async fn test_fullhash_mismatch_prevents_wire_dispatch() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, _server) = spawn_mock_server(MockServerState {
            wire_calls: wire_calls.clone(),
            response_json: serde_json::json!({}),
            pause_notify: None,
            unpause_notify: None,
        })
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
                endpoint,
                key: "sk-ant-test-key".into(),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Original message"),
        }];
        state.api_pending = true;

        use runner::Journal;
        adapter.save(&state).await.unwrap();

        // Attempt complete with altered messages (hash mismatch)
        let altered_messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Tampered message"),
        }];

        use runner::Model;
        let res = adapter.complete(&altered_messages, &cancel).await;
        assert!(res.is_err());
        // Zero HTTP requests sent
        assert_eq!(wire_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_fail_closed_quote_provider_proves_zero_http_calls() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let (endpoint, _server) = spawn_mock_server(MockServerState {
            wire_calls: wire_calls.clone(),
            response_json: serde_json::json!({}),
            pause_notify: None,
            unpause_notify: None,
        })
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
            Arc::new(FailClosedQuoteProvider),
            AdapterHttpConfig {
                endpoint,
                key: "sk-ant-test-key".into(),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
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
    }

    #[tokio::test]
    async fn test_untrusted_usage_marks_attempt_unknown_in_db() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        // HTTP 200 but usage contains negative or invalid token count
        let (endpoint, _server) = spawn_mock_server(MockServerState {
            wire_calls: wire_calls.clone(),
            response_json: serde_json::json!({
                "id": "msg_untrusted",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Bad usage"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": -50, "output_tokens": 10}
            }),
            pause_notify: None,
            unpause_notify: None,
        })
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
                endpoint,
                key: "sk-ant-test-key".into(),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
        )
        .await
        .unwrap();

        let cancel = CancellationToken::new();
        let mut state = State::default();
        state.messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Untrusted test"),
        }];

        let res = runner::run(&adapter, &adapter, &adapter, &mut state, &cancel, 2).await;
        assert!(res.is_err());

        // Check DB: attempt must be marked unknown, estimated_microusd is NULL, reserved is retained
        let row: (String, Option<i64>, i64) = sqlx::query_as(
            "SELECT state, estimated_microusd, reserved_microusd FROM api_bot_attempts WHERE turn_id=?",
        )
        .bind(&turn)
        .fetch_one(&pool)
        .await
        .unwrap();

        assert_eq!(row.0, "unknown");
        assert_eq!(row.1, None); // estimated is NULL
        assert_eq!(row.2, 10_000); // reserved is retained
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
                key: "sk-ant-test-key".into(),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
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

    // =========================================================================
    // 32-Round Interleaving Stress Test (Registry + Adapter + Stop Race)
    // =========================================================================

    #[tokio::test]
    async fn test_adapter_registry_stop_race_32_rounds() {
        let wire_calls = Arc::new(AtomicUsize::new(0));
        let pause_notify = Arc::new(Notify::new());
        let unpause_notify = Arc::new(Notify::new());

        let (endpoint, _server) = spawn_mock_server(MockServerState {
            wire_calls: wire_calls.clone(),
            response_json: serde_json::json!({
                "id": "msg_race",
                "type": "message",
                "role": "assistant",
                "model": "claude-opus-5-5",
                "content": [{"type": "text", "text": "Racing response"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 20, "output_tokens": 10}
            }),
            pause_notify: Some(pause_notify.clone()),
            unpause_notify: Some(unpause_notify.clone()),
        })
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
                        endpoint: endpoint.clone(),
                        key: "sk-ant-test-key".into(),
                        timeout: std::time::Duration::from_secs(5),
                        max_response_bytes: 1024 * 1024,
                    },
                )
                .await
                .unwrap(),
            );

            let adapter_runner = adapter.clone();
            let key = RunTurnKey {
                run_id: run.clone(),
                turn_id: turn.clone(),
            };

            let _token = registry
                .spawn(key.clone(), move |cancel| async move {
                    let mut state = State::default();
                    state.messages = vec![Message {
                        role: "user".into(),
                        content: serde_json::json!("Race turn"),
                    }];
                    match runner::run(
                        &*adapter_runner,
                        &*adapter_runner,
                        &*adapter_runner,
                        &mut state,
                        &cancel,
                        3,
                    )
                    .await
                    {
                        Ok(Halt::EndTurn) => Ok(TurnOutcome::Completed),
                        Err(runner::Error::Cancelled) => Ok(TurnOutcome::Cancelled),
                        Err(e) => Ok(TurnOutcome::Failed(format!("{:?}", e))),
                        _ => Ok(TurnOutcome::Completed),
                    }
                })
                .unwrap();

            // Wait for HTTP server to receive the request
            pause_notify.notified().await;

            if i % 2 == 0 {
                // Stop wins branch: CAS run to stopping while HTTP is in-flight
                sqlx::query("UPDATE runs SET state='stopping' WHERE id=?")
                    .bind(&run)
                    .execute(&pool)
                    .await
                    .unwrap();

                // Unpause server
                unpause_notify.notify_one();

                // Cancel and join runner task
                let outcome = registry.cancel_and_join(&run).await.unwrap();
                assert!(!registry.is_active(&run));

                // Call recover_unknown as lifecycle would
                am_base::db::api_bot::recover_unknown(&pool, &run)
                    .await
                    .unwrap();

                // Attempt in DB must be recorded as unknown, estimated NULL, reserved intact
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
                // Record wins branch: Unpause server immediately to let record commit
                unpause_notify.notify_one();

                // Wait for task to finish
                let outcome = registry.cancel_and_join(&run).await.unwrap();
                assert_eq!(outcome, TurnOutcome::Completed);
                assert!(!registry.is_active(&run));

                // Attempt in DB must be recorded with estimated microUSD
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
    }
}
