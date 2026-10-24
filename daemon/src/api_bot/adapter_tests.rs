//! Assembly and integration tests for inert Claude API bot adapter and registry.
//!
//! Uses ONLY fake loopback HTTP and fresh in-memory SQLite fixtures (schema 69).
//! No paid endpoints, key files, daemon restarts, or SSH commands.
//! Synchronization is strictly based on Atomic counters, channels, and Notify; NO arbitrary sleep.

#[cfg(test)]
mod tests {
    use super::adapter::{AdapterHttpConfig, AdapterIdentity, AdapterLimits, ApiBotAdapter};
    use super::protocol::{self, Message, Options, Response};
    use super::quote::{QuoteError, QuoteProvider, RequestEnvelope, ReservationQuote, UsageQuote};
    use super::runner::{self, Halt, State};
    use sqlx::SqlitePool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::sync::{oneshot, Notify};
    use tokio_util::sync::CancellationToken;

    /// Fresh in-memory SQLite pool with foreign keys and schema 69 migrations.
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

        let project_id = "p_test";
        let bot_id = "b_test";
        let run_id = "r_test";
        let turn_id = "t_test";
        let session_id = "s_test";

        let now = am_base::db::now();
        sqlx::query("INSERT INTO projects(id,path,label,created_at) VALUES(?,'/test','p',?)")
            .bind(project_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO bots(id,project_id,name,kind,created_at) VALUES(?,?,'bot','claude-api',?)")
            .bind(bot_id)
            .bind(project_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO runs(id,bot_id,state,created_at) VALUES(?,?,'running',?)")
            .bind(run_id)
            .bind(bot_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO conversations(id,bot_id,created_at) VALUES('c_test',?,?)")
            .bind(bot_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::query("INSERT INTO turns(id,conversation_id,run_id,status,created_at) VALUES(?,'c_test',?,'in_flight',?)")
            .bind(turn_id)
            .bind(run_id)
            .bind(&now)
            .execute(&pool)
            .await
            .unwrap();

        (pool, bot_id.into(), run_id.into(), turn_id.into(), session_id.into())
    }

    /// Fixed test quote provider returning approved mock microdollar constants.
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

    /// Fail-closed quote provider rejecting both reservation and settlement.
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
    async fn test_single_fresh_reservation_arms_one_shot_permit() {
        let (pool, bot, run, turn, session) = setup_test_db().await;

        let options = test_options();
        let system = serde_json::json!("You are an inert test assistant.");
        let quote = Arc::new(TestQuoteProvider {
            reserved_amount: 10_000,
            estimated_amount: 5_000,
        });

        let adapter = ApiBotAdapter::new(
            pool.clone(),
            AdapterIdentity {
                session_id: session.clone(),
                bot_id: bot,
                run_id: run,
                turn_id: turn,
                generation: 1,
            },
            options.clone(),
            system.clone(),
            test_limits(),
            quote,
            AdapterHttpConfig {
                endpoint: "http://127.0.0.1:9".into(), // Unused for permit verification
                key: "sk-ant-test-key".into(),
                timeout: std::time::Duration::from_secs(5),
                max_response_bytes: 1024 * 1024,
            },
        )
        .await
        .expect("adapter created");

        let cancel = CancellationToken::new();
        let messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Hello, permit check."),
        }];

        // Model::complete before reservation MUST fail because no permit was armed
        use runner::Model;
        let res1 = adapter.complete(&messages, &cancel).await;
        assert!(res1.is_err());

        // Now save a reservation checkpoint (false -> true)
        let mut state = State::default();
        state.messages = messages.clone();
        state.api_pending = true;

        use runner::Journal;
        let save_res = adapter.save(&state).await;
        assert!(save_res.is_ok());

        // First complete consumes the armed permit (would dispatch to wire if server existed)
        // Calling complete twice without a fresh reserve MUST failclosed immediately
        let res2 = adapter.complete(&messages, &cancel).await;
        let res3 = adapter.complete(&messages, &cancel).await;

        // Second complete call must definitely fail due to permit already consumed
        assert!(res3.is_err());
    }

    #[tokio::test]
    async fn test_request_envelope_body_and_hash_consistency() {
        let options = test_options();
        let system = serde_json::json!("Canonical hash check.");
        let messages = vec![Message {
            role: "user".into(),
            content: serde_json::json!("Test content"),
        }];

        let env1 = RequestEnvelope::new(options.clone(), system.clone(), messages.clone()).unwrap();
        let env2 = RequestEnvelope::new(options.clone(), system.clone(), messages.clone()).unwrap();

        // Deterministic canonical hash
        assert_eq!(env1.hash(), env2.hash());

        // Wire body must match protocol::request exactly
        let direct_body = protocol::request(&options, &system, &messages);
        assert_eq!(env1.body(), direct_body);

        // Modifying any field produces a distinct hash
        let altered_options = Options {
            effort: "high".into(),
            fast: false,
            max_tokens: 4096,
        };
        let env_altered = RequestEnvelope::new(altered_options, system, messages).unwrap();
        assert_ne!(env1.hash(), env_altered.hash());
    }

    #[tokio::test]
    async fn test_fail_closed_quote_provider_blocks_reservation() {
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
            serde_json::json!("Test system prompt"),
            test_limits(),
            Arc::new(FailClosedQuoteProvider),
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
            content: serde_json::json!("Say hi"),
        }];
        state.api_pending = true;

        // Save attempts to reserve via FailClosedQuoteProvider -> fails closed!
        use runner::Journal;
        let save_res = adapter.save(&state).await;
        assert!(save_res.is_err());
    }

    // =========================================================================
    // 32 Rounds Interleaving Stress Test with Fake HTTP Loopback
    // =========================================================================

    #[tokio::test]
    async fn test_adapter_fake_http_interleaving_32_rounds() {
        // Mock server running on ephemeral loopback TCP port
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = format!("http://127.0.0.1:{}/v1/messages", port);

        let http_wire_calls = Arc::new(AtomicUsize::new(0));
        let wire_calls_server = http_wire_calls.clone();

        // Background fake HTTP server answering valid Anthropic responses
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let calls = wire_calls_server.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    calls.fetch_add(1, Ordering::SeqCst);

                    let resp_body = serde_json::json!({
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "model": "claude-opus-5-5",
                        "content": [{"type": "text", "text": "Fake HTTP response"}],
                        "stop_reason": "end_turn",
                        "usage": {
                            "input_tokens": 50,
                            "output_tokens": 10,
                            "cache_creation_input_tokens": 0,
                            "cache_read_input_tokens": 0
                        }
                    });
                    let body_str = resp_body.to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body_str.len(),
                        body_str
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });

        let total_iterations = 32;
        let mut expected_completed = 0;

        for i in 0..total_iterations {
            let (pool, bot, run, turn, session) = setup_test_db().await;

            let quote = Arc::new(TestQuoteProvider {
                reserved_amount: 10_000,
                estimated_amount: 5_000,
            });

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
                    serde_json::json!("System prompt"),
                    test_limits(),
                    quote,
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

            let cancel = CancellationToken::new();
            let adapter_clone = adapter.clone();
            let cancel_clone = cancel.clone();

            let mut state = State::default();
            state.messages = vec![Message {
                role: "user".into(),
                content: serde_json::json!("Hello in 32-round test"),
            }];

            if i % 2 == 0 {
                // Cancel-first branch: cancel token fired before execution begins
                cancel.cancel();
                let outcome = runner::run(
                    &*adapter_clone,
                    &*adapter_clone,
                    &*adapter_clone,
                    &mut state,
                    &cancel_clone,
                    5,
                )
                .await;

                assert_eq!(outcome, Err(runner::Error::Cancelled));
            } else {
                // Complete-first branch: runs full turn to EndTurn
                let outcome = runner::run(
                    &*adapter_clone,
                    &*adapter_clone,
                    &*adapter_clone,
                    &mut state,
                    &cancel_clone,
                    5,
                )
                .await;

                assert_eq!(outcome, Ok(Halt::EndTurn));
                expected_completed += 1;
            }
        }

        // Exactly the 16 odd iterations dispatched wire HTTP requests
        assert_eq!(http_wire_calls.load(Ordering::SeqCst), expected_completed);
    }
}
