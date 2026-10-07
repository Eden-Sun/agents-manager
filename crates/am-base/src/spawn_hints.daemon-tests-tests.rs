
    use super::*;
    use serde_json::json;

    fn bash_result(id: &str, stdout: Value) -> Value {
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": {"stdout": serde_json::to_string(&json!({"id": id, "result": stdout})).unwrap(), "stderr": ""},
        })
    }

    #[test]
    fn agent_start_and_pane_split_both_yield_the_pane_id() {
        let start = bash_result("cli:agent:start", json!({"agent": {"pane_id": "w1:p2", "name": "parent-kid"}}));
        assert_eq!(extract_pane_ids(&start), ["w1:p2"]);

        let split = bash_result("cli:pane:split", json!({"pane": {"pane_id": "w1:p3", "tab_id": "w1:t1"}}));
        assert_eq!(extract_pane_ids(&split), ["w1:p3"]);
    }

    #[test]
    fn several_json_responses_yield_every_distinct_spawned_pane() {
        let payload = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_response": {"stdout": format!(
                "{}\n{}\n{}\n{}\n",
                json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p2"}}}),
                json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p2"}}}),
                json!({"id": "cli:pane:split", "result": {"pane": {"pane_id": "w1:p3"}}}),
                json!({"id": "cli:agent:start", "result": {"agent": {"pane_id": "w1:p4"}}}),
            ), "stderr": ""},
        });
        assert_eq!(extract_pane_ids(&payload), ["w1:p2", "w1:p3", "w1:p4"]);
    }

    /// `pane:get`／`pane:current`／`pane:list` return the exact same `{"pane": {...}}` shape as
    /// `pane:split` — an agent merely *looking at* a pane it does not own must never be read as
    /// "I just created this". Only the request `id` tells the two apart.
    #[test]
    fn merely_inspecting_a_pane_is_not_a_spawn() {
        for id in ["cli:pane:get", "cli:pane:current", "cli:pane:list", "cli:agent:get", "cli:agent:list"] {
            let v = bash_result(id, json!({"pane": {"pane_id": "w1:p2"}}));
            assert!(extract_pane_ids(&v).is_empty(), "{id} must not be treated as a spawn");
        }
    }

    /// herdr's own error envelope (`{"error":{...},"id":"cli:agent:start"}`, e.g. a busy pane or a
    /// timeout) has no `result` at all — no pane was actually created, so there is nothing to hint.
    #[test]
    fn a_herdr_error_response_yields_no_hint() {
        let v = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": {"stdout": r#"{"error":{"code":"agent_pane_busy","message":"..."},"id":"cli:agent:start"}"#, "stderr": ""},
        });
        assert!(extract_pane_ids(&v).is_empty());
    }

    #[test]
    fn unrelated_bash_output_and_non_bash_tools_yield_no_hint() {
        let ls = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "ls -la"},
            "tool_response": {"stdout": "total 0\ndrwxr-xr-x  2 x  x  64 Jan  1 00:00 .\n", "stderr": ""},
        });
        assert!(extract_pane_ids(&ls).is_empty(), "plain command output is not JSON at all");

        let not_bash = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Read",
            "tool_input": {"file_path": "/tmp/x"},
            "tool_response": {"stdout": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#},
        });
        assert!(extract_pane_ids(&not_bash).is_empty(), "only the Bash tool is trusted");
    }

    /// A hook implementation that flattens `tool_response` to a bare string instead of `{stdout,
    /// stderr}` must still work — the field's shape is not part of this daemon's own contract.
    #[test]
    fn a_flattened_string_tool_response_still_works() {
        let v = json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "herdr agent start kid --kind claude --pane w1:p2"},
            "tool_response": r#"{"id":"cli:agent:start","result":{"agent":{"pane_id":"w1:p2"}}}"#,
        });
        assert_eq!(extract_pane_ids(&v), ["w1:p2"]);
    }

    #[tokio::test]
    async fn a_recorded_hint_is_visible_by_host_and_disappears_once_consumed() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;

        record(&env.app, &bot.id, "w1:p2").await.unwrap();
        let hints = for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(hints.get("w1:p2"), Some(&bot.id));
        assert!(for_host(&env.app, "some-other-host").await.unwrap().is_empty(), "scoped by host");

        consume(&env.app, crate::config::LOCAL_HOST, "w1:p2").await;
        assert!(for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().is_empty());
    }

    /// issue #635: pane IDs are only unique within a host, and consuming one host's hint must not
    /// erase another host's hint with the same ID.
    #[tokio::test]
    async fn the_same_pane_id_on_two_hosts_is_isolated_when_consumed() {
        let env = crate::testing::env().await;
        let local = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let remote_project_id = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, ?, ?, ?)")
            .bind(&remote_project_id)
            .bind(env.repo.to_string_lossy().to_string())
            .bind("remote-project")
            .bind("remote-host")
            .bind(db::now())
            .execute(&env.app.db)
            .await
            .unwrap();
        let remote = crate::testing::claude_bot(&env.app, &remote_project_id, "bravo").await;

        record(&env.app, &local.id, "w1:p2").await.unwrap();
        record(&env.app, &remote.id, "w1:p2").await.unwrap();

        assert_eq!(for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().get("w1:p2"), Some(&local.id));
        assert_eq!(for_host(&env.app, "remote-host").await.unwrap().get("w1:p2"), Some(&remote.id));

        consume(&env.app, crate::config::LOCAL_HOST, "w1:p2").await;
        assert!(for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().is_empty());
        assert_eq!(for_host(&env.app, "remote-host").await.unwrap().get("w1:p2"), Some(&remote.id));
    }

    /// Same pane id recorded twice (a retried Bash call, or the id recycled later): the row is
    /// replaced, not duplicated, and the newest claim wins.
    #[tokio::test]
    async fn recording_the_same_pane_twice_replaces_rather_than_duplicates() {
        let env = crate::testing::env().await;
        let first = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let second = crate::testing::claude_bot(&env.app, &env.project_id, "bravo").await;

        record(&env.app, &first.id, "w1:p2").await.unwrap();
        record(&env.app, &second.id, "w1:p2").await.unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_hints WHERE pane_id = 'w1:p2'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 1, "one row per pane_id");
        let hints = for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap();
        assert_eq!(hints.get("w1:p2"), Some(&second.id), "the newer claim wins");
    }

    /// A hint older than the staleness window must not surface, and `prune_stale` removes it.
    #[tokio::test]
    async fn a_stale_hint_is_invisible_and_gets_pruned() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let old = (chrono::Utc::now() - chrono::Duration::seconds(MAX_AGE_SECS + 60)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        sqlx::query("INSERT INTO spawn_hints (pane_id, host, bot_id, created_at) VALUES ('w1:p9', 'local', ?, ?)")
            .bind(&bot.id)
            .bind(&old)
            .execute(&env.app.db)
            .await
            .unwrap();

        assert!(!for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().contains_key("w1:p9"), "too old to trust");

        prune_stale(&env.app).await;
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_hints WHERE pane_id = 'w1:p9'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 0, "prune actually removes it");
    }
