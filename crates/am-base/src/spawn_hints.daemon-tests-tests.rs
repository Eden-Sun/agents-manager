
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

    /// issue #1790：claim 不像 hint——不過期、認領後不消耗，pane 還活著就一直算；同一個 pane 再登記以新的為準。
    #[tokio::test]
    async fn a_claim_outlives_the_hint_window_and_the_newest_claim_wins() {
        let env = crate::testing::env().await;
        let first = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        let second = crate::testing::claude_bot(&env.app, &env.project_id, "bravo").await;
        crate::testing::fake_run(&env.app, &first.id).await;
        crate::testing::fake_run(&env.app, &second.id).await;
        let old = (chrono::Utc::now() - chrono::Duration::seconds(MAX_AGE_SECS * 100)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        claim(&env.app, &first.id, "w1:p2").await.unwrap();
        sqlx::query("UPDATE spawn_claims SET created_at=? WHERE pane_id='w1:p2'")
            .bind(&old)
            .execute(&env.app.db)
            .await
            .unwrap();
        assert_eq!(claims_for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().get("w1:p2"), Some(&first.id), "再久也算數");

        claim(&env.app, &second.id, "w1:p2").await.unwrap();
        assert_eq!(claims_for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().get("w1:p2"), Some(&second.id), "新的登記覆蓋舊的");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM spawn_claims WHERE pane_id = 'w1:p2'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(n, 1);
        assert!(for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().is_empty(), "claim 不混進 hint 表");
    }

    /// issue #1790：pane 不在 herdr 了才清 claim（pane id 會被回收）；剛登記的、或 snapshot 是空的，都不清。
    #[tokio::test]
    async fn dead_claims_are_pruned_but_young_and_live_ones_stay() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "alfa").await;
        crate::testing::fake_run(&env.app, &bot.id).await;
        let old = (chrono::Utc::now() - chrono::Duration::seconds(CLAIM_GRACE_SECS + 60)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        for pane in ["w1:live", "w1:dead"] {
            claim(&env.app, &bot.id, pane).await.unwrap();
            sqlx::query("UPDATE spawn_claims SET created_at=? WHERE pane_id=?")
                .bind(&old)
                .bind(pane)
                .execute(&env.app.db)
                .await
                .unwrap();
        }
        claim(&env.app, &bot.id, "w1:young").await.unwrap();

        prune_dead_claims(&env.app, crate::config::LOCAL_HOST, "test", &[]).await;
        assert_eq!(claims_for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap().len(), 3, "snapshot 是空的一個都不清");

        prune_dead_claims(&env.app, crate::config::LOCAL_HOST, "test", &["w1:live".to_string(), "w1:other".to_string()]).await;
        let left = claims_for_host(&env.app, crate::config::LOCAL_HOST).await.unwrap();
        let mut panes: Vec<&str> = left.keys().map(String::as_str).collect();
        panes.sort();
        assert_eq!(panes, ["w1:live", "w1:young"], "死掉的清掉，活著的與剛登記的留著");
    }

    #[tokio::test]
    async fn nr_claim_legacy_without_session_is_unknown_but_parent_run_changes_are_safe() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "original").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET herdr_session='session-a' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO spawn_claims (pane_id,host,bot_id,created_at) VALUES ('legacy','local',?,'2000-01-01T00:00:00.000Z')")
            .bind(&bot.id).execute(&env.app.db).await.unwrap();
        assert!(claims_for_host(&env.app, "local").await.unwrap().is_empty(), "claims without a host session stay unknown until migration backfills evidence");
        claim(&env.app, &bot.id, "w1:p2").await.unwrap();
        sqlx::query("UPDATE bots SET name='renamed' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let _reused_name = crate::testing::claude_bot(&env.app, &env.project_id, "original").await;
        assert_eq!(claims_for_host(&env.app, "local").await.unwrap().get("w1:p2"), Some(&bot.id));
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        let next = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET herdr_session='session-a' WHERE id=?").bind(next).execute(&env.app.db).await.unwrap();
        assert_eq!(claims_for_host(&env.app, "local").await.unwrap().get("w1:p2"), Some(&bot.id), "the child still belongs to the same bot after a new parent run");
    }

    #[tokio::test]
    async fn nr_claim_sessions_do_not_overwrite_or_prune_each_other() {
        let env = crate::testing::env().await;
        let first = crate::testing::claude_bot(&env.app, &env.project_id, "first").await;
        let second = crate::testing::claude_bot(&env.app, &env.project_id, "second").await;
        // These rows describe two host sessions, independently of either parent's run state.
        for (bot, session) in [(&first, "session-a"), (&second, "test")] {
            sqlx::query("INSERT INTO spawn_claims (pane_id,host,bot_id,created_at,herdr_session) VALUES ('w1:p2','local',?,'2000-01-01T00:00:00.000Z',?)")
                .bind(&bot.id).bind(session).execute(&env.app.db).await.unwrap();
        }
        assert_eq!(claims_for_host(&env.app,"local").await.unwrap().get("w1:p2"), Some(&second.id), "host lookups only use the configured session");
        for (session, bot) in [("session-a", &first), ("test", &second)] {
            assert_eq!(claims_for_session(&env.app, "local", session).await.unwrap().get("w1:p2"), Some(&bot.id));
        }
        sqlx::query("UPDATE spawn_claims SET created_at='2000-01-01T00:00:00.000Z'").execute(&env.app.db).await.unwrap();
        prune_dead_claims_before(&env.app, "local", "session-a", &["w1:other".into()], "2001-01-01T00:00:00.000Z").await;
        assert!(claims_for_session(&env.app, "local", "session-a").await.unwrap().is_empty());
        assert_eq!(claims_for_session(&env.app, "local", "test").await.unwrap().get("w1:p2"), Some(&second.id));
    }

    #[tokio::test]
    async fn nr_claim_v65_migration_backfills_the_host_session() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "migration").await;
        crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("DROP TABLE spawn_claims").execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TABLE spawn_claims (
            pane_id TEXT NOT NULL, host TEXT NOT NULL, bot_id TEXT NOT NULL REFERENCES bots(id),
            created_at TEXT NOT NULL, PRIMARY KEY(host,pane_id))").execute(&env.app.db).await.unwrap();
        sqlx::query("INSERT INTO spawn_claims VALUES ('w1:p2','local',?,'2000-01-01T00:00:00.000Z')")
            .bind(&bot.id).execute(&env.app.db).await.unwrap();
        crate::runners::am_base_tests::apply_migrations(&env.app.db).await.unwrap();
        assert_eq!(claims_for_host(&env.app,"local").await.unwrap().get("w1:p2"), Some(&bot.id), "migration must preserve the existing claim without requiring a new registration");
        assert_eq!(claims_for_session(&env.app,"local","test").await.unwrap().get("w1:p2"), Some(&bot.id));
    }

    #[tokio::test]
    async fn nr_claim_host_session_is_required_but_parent_run_is_audit_only() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "unknown").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET herdr_session=NULL WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        claim_for_run(&env.app, &bot.id, "previous-run", "w1:p2").await.unwrap();
        let row: (Option<String>, String) = sqlx::query_as("SELECT parent_run_id,herdr_session FROM spawn_claims WHERE pane_id='w1:p2'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(row, (Some("previous-run".into()), "test".into()));
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        claim(&env.app, &bot.id, "w1:p3").await.unwrap();
        assert_eq!(claims_for_host(&env.app,"local").await.unwrap().len(), 2);
        sqlx::query("UPDATE projects SET host='unconfigured-host' WHERE id=?").bind(&env.project_id).execute(&env.app.db).await.unwrap();
        assert!(claim(&env.app, &bot.id, "w1:p4").await.is_err(), "missing host session evidence must remain unknown");
    }

    #[tokio::test]
    async fn nr_claim_review_parent_sleep_and_restart_preserve_ownership() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "parent").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        claim(&env.app, &bot.id, "w1:p-child").await.unwrap();
        sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(run).execute(&env.app.db).await.unwrap();
        assert_eq!(claims_for_host(&env.app, "local").await.unwrap().get("w1:p-child"), Some(&bot.id), "sleeping parent still owns its child");
        crate::testing::fake_run(&env.app, &bot.id).await;
        assert_eq!(claims_for_host(&env.app, "local").await.unwrap().get("w1:p-child"), Some(&bot.id), "new parent run does not revoke the claim");
    }

    #[tokio::test]
    async fn nr_claim_review_a_stopped_parent_can_record_a_delayed_claim() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "parent").await;
        claim_for_run(&env.app, &bot.id, "old-run-audit-only", "w1:p-child").await.unwrap();
        assert_eq!(claims_for_host(&env.app, "local").await.unwrap().get("w1:p-child"), Some(&bot.id));
        let session: String = sqlx::query_scalar("SELECT herdr_session FROM spawn_claims WHERE pane_id='w1:p-child'").fetch_one(&env.app.db).await.unwrap();
        assert_eq!(session, env.app.herdr_session, "the host session is available without a parent run");
    }

    #[tokio::test]
    async fn nr_claim_review_v65_backfill_preserves_local_and_remote_children() {
        for host in ["local", "m4p"] {
            let env = crate::testing::env().await;
            let bot = crate::testing::claude_bot(&env.app, &env.project_id, "parent").await;
            // A different bot's active run supplies the host session even while the parent sleeps.
            let witness = crate::testing::claude_bot(&env.app, &env.project_id, "witness").await;
            sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(host).bind(&env.project_id).execute(&env.app.db).await.unwrap();
            let run = crate::testing::fake_run(&env.app, &witness.id).await;
            sqlx::query("UPDATE runs SET host=?,herdr_session='agents-manager' WHERE id=?").bind(host).bind(&run).execute(&env.app.db).await.unwrap();
            sqlx::query("DROP TABLE spawn_claims").execute(&env.app.db).await.unwrap();
            sqlx::query("CREATE TABLE spawn_claims (pane_id TEXT NOT NULL,host TEXT NOT NULL,bot_id TEXT NOT NULL REFERENCES bots(id),created_at TEXT NOT NULL,PRIMARY KEY(host,pane_id))")
                .execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO spawn_claims VALUES ('w1:p-child',?,?,'2000-01-01T00:00:00.000Z')").bind(host).bind(&bot.id).execute(&env.app.db).await.unwrap();
            sqlx::query("INSERT INTO spawn_claims VALUES ('w1:unknown','unseen-host',?,'2000-01-01T00:00:00.000Z')").bind(&bot.id).execute(&env.app.db).await.unwrap();
            sqlx::query("PRAGMA user_version=65").execute(&env.app.db).await.unwrap();
            crate::runners::am_base_tests::apply_migrations(&env.app.db).await.unwrap();
            let session: String = sqlx::query_scalar("SELECT herdr_session FROM spawn_claims WHERE pane_id='w1:p-child'").fetch_one(&env.app.db).await.unwrap();
            let unknown: String = sqlx::query_scalar("SELECT herdr_session FROM spawn_claims WHERE pane_id='w1:unknown'").fetch_one(&env.app.db).await.unwrap();
            assert_eq!(unknown, "", "a different host's run must not supply the session");
            assert_eq!(session, "agents-manager", "{host} legacy claims retain their host session");
            sqlx::query("UPDATE runs SET state='stopped' WHERE id=?").bind(run).execute(&env.app.db).await.unwrap();
            assert_eq!(claims_for_session(&env.app,host,"agents-manager").await.unwrap().get("w1:p-child"), Some(&bot.id));
            assert!(claims_for_session(&env.app,host,"other-session").await.unwrap().is_empty());
        }
    }
