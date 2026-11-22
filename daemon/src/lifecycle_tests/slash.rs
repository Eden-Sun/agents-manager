#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod live_slash_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::slash::test_seams::*;
    use crate::lifecycle::*;
    use crate::state::App;
    use crate::testing as tt;
    /// #872：計畫綁 run A、鎖內 active 已是 B（或 A 不再 idle）→ `compact` 回 409 `superseded_run`，假 herdr 一個字、一個鍵都沒收到。
    #[tokio::test]
    async fn compact_with_a_stale_expected_run_types_nothing() {
        use crate::lifecycle::LcError;
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "compact-stale").await;
        let run_a = tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ? WHERE id = ?").bind(crate::db::now()).bind(&run_a).execute(&env.app.db).await.unwrap();
        let run_b = tt::fake_run(&env.app, &bot.id).await;

        match crate::lifecycle::slash::compact(&env.app, &bot.id, Some(&run_a)).await.err().expect("superseded") {
            LcError::Conflict(v) => {
                assert_eq!(v["reason"], "superseded_run", "{v}");
                assert_eq!(v["run_id"], run_b.as_str());
            }
            other => panic!("{other:?}"),
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty() && env.herdr.calls_to("pane.send_keys").is_empty(), "一個字、一個鍵都沒打");

        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&run_b).execute(&env.app.db).await.unwrap();
        match crate::lifecycle::slash::compact(&env.app, &bot.id, Some(&run_b)).await.err().expect("not idle") {
            LcError::Conflict(v) => assert_eq!(v["reason"], "superseded_run", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty() && env.herdr.calls_to("pane.send_keys").is_empty());
    }
    #[tokio::test]
    async fn synthetic_codex_compact_records_the_sent_command_before_fallback() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "compact-proof").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        env.herdr.set_screen(&pane, "› \n");
        crate::lifecycle::slash::compact(&app, &bot.id, Some(&run)).await.unwrap();
        if let Some(poller) = app.progress_pollers.lock().await.remove(&run) {
            poller.abort();
        }
        let turn = crate::db::in_flight_turn(&app.db, &run).await.unwrap().unwrap();
        assert_eq!(turn.prompt_text.as_deref(), Some("/compact"));
        assert_eq!(turn.delivery, "ok");
        env.herdr.set_screen(&pane, "• An older answer still on screen\n› \n");
        assert!(crate::lifecycle::poller::test_seams::try_fallback(&app, &run, Some(&turn.id)).await.unwrap());
        let messages: Vec<(String,String)> = sqlx::query_as("SELECT role,content FROM messages WHERE turn_id=?")
            .bind(&turn.id).fetch_all(&app.db).await.unwrap();
        assert_eq!(messages, vec![("system".into(), "Codex 已壓縮對話。".into())]);
    }
    #[tokio::test]
    async fn claude_api_compact_verifies_idle_and_active_run() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "api-compact-proof").await;
        sqlx::query("UPDATE bots SET kind=? WHERE id=?").bind(am_config::CLAUDE_API_KIND).bind(&bot.id).execute(&app.db).await.unwrap();
        assert!(crate::lifecycle::slash::compact(&app, &bot.id, None).await.is_err());
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status = 'working' WHERE id = ?").bind(&run).execute(&app.db).await.unwrap();
        assert!(crate::lifecycle::slash::compact(&app, &bot.id, None).await.is_err());
    }
    #[tokio::test]
    async fn agy_compact_refuses_non_empty_composer_and_sends_when_empty() {
        use crate::lifecycle::LcError;
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "agy-compact-draft").await;
        sqlx::query("UPDATE bots SET kind='agy' WHERE id=?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let box_screen = |row: &str| format!("Antigravity CLI 1.5.14\n────────────────────────────────────────\n{row}\n────────────────────────────────────────\n? for shortcuts\n");
        env.herdr.set_screen(&pane, &box_screen("> draft in progress"));
        match crate::lifecycle::slash::compact(&app, &bot.id, Some(&run)).await.err().expect("draft blocks compact") {
            LcError::Conflict(v) => assert_eq!(v["reason"], "composer_busy", "{v}"),
            other => panic!("{other:?}"),
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty());

        env.herdr.set_screen(&pane, &box_screen(">"));
        crate::lifecycle::slash::compact(&app, &bot.id, Some(&run)).await.unwrap();
        assert_eq!(
            env.herdr.calls_to("pane.send_text").last().and_then(|v| v["text"].as_str()),
            Some("/compact")
        );
    }
    #[tokio::test]
    async fn agy_live_model_and_effort_verify_footer_guard_account_list_and_protect_orchestrator() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "agy-live-switch").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        sqlx::query("UPDATE bots SET kind='agy', model='gemini-3.8-flash-high' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET pane_id=?, runtime_model='gemini-3.8-flash-medium' WHERE id=?")
            .bind(&pane)
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();

        let screen = |above: &str, footer: &str| {
            format!(
                "Antigravity CLI 1.5.14\n{above}────────────────────────────────────────\n>\n────────────────────────────────────────\n{footer}\n"
            )
        };

        let listed = serde_json::json!({
            "kind": "agy",
            "host": "local",
            "source": "agy-cli",
            "models": [
                {"id": "gemini-3.8-flash-medium", "is_default": true},
                {"id": "gemini-3.8-flash-high", "is_default": false},
                {"id": "gemini-3.8-flash-low", "is_default": false},
                {"id": "gemini-pro-orchestrator-g1-pro", "is_default": false}
            ]
        });
        app.models_cache
            .lock()
            .await
            .insert("local/agy/".into(), (std::time::Instant::now(), listed));

        sqlx::query("UPDATE bots SET model='gemini-4-neon-medium' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(&pane, &screen("", "  Gemini 3.8 Flash · medium"));
        assert_eq!(
            crate::lifecycle::slash::apply_live_setting(&app, &bot.id, &["model"]).await.as_deref(),
            Some("agy_model_not_in_account_list")
        );
        assert!(env.herdr.calls_to("pane.send_text").is_empty());

        sqlx::query("UPDATE bots SET model='gemini-3.8-flash-high' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(
            &pane,
            &screen("> /model\n  ⎿  Model set to Gemini 3.8 Flash (High)\n", "  Gemini 3.8 Flash · high"),
        );
        assert_eq!(
            crate::lifecycle::slash::apply_live_setting(&app, &bot.id, &["model"]).await,
            None
        );
        assert_eq!(
            env.herdr.calls_to("pane.send_text").last().and_then(|v| v["text"].as_str()),
            Some("/model gemini-3.8-flash-high")
        );
        let rt_model: Option<String> = sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(rt_model.as_deref(), Some("gemini-3.8-flash-high"));

        sqlx::query("UPDATE bots SET effort='low' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(
            &pane,
            &screen("> /effort\n  ⎿  Effort set to low\n", "  Gemini 3.8 Flash · low"),
        );
        assert_eq!(
            crate::lifecycle::slash::apply_live_setting(&app, &bot.id, &["effort"]).await,
            None
        );
        assert_eq!(
            env.herdr.calls_to("pane.send_text").last().and_then(|v| v["text"].as_str()),
            Some("/effort low")
        );
        let (rt_model, rt_effort): (Option<String>, Option<String>) =
            sqlx::query_as("SELECT runtime_model, runtime_effort FROM runs WHERE id=?")
                .bind(&run_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(rt_model.as_deref(), Some("gemini-3.8-flash-low"));
        assert_eq!(rt_effort.as_deref(), Some("low"));

        sqlx::query("UPDATE bots SET model='gemini-pro-orchestrator-g1-pro' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(
            &pane,
            &screen("> /model\n  ⎿  Model set to Orchestrator (Argon)\n", "  auto · Orchestrator · Gemini 4 Neon · xhigh"),
        );
        assert_eq!(
            crate::lifecycle::slash::apply_live_setting(&app, &bot.id, &["model"]).await,
            None
        );
        let rt_model: Option<String> = sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(rt_model.as_deref(), Some("gemini-pro-orchestrator-g1-pro"));

        app.models_cache.lock().await.clear();
        sqlx::query("UPDATE bots SET model='gemini-4-neon-xhigh' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(
            &pane,
            &screen("", "  auto · Orchestrator · Gemini 4 Neon · xhigh"),
        );
        assert_eq!(
            crate::lifecycle::slash::apply_live_setting(&app, &bot.id, &["model"]).await.as_deref(),
            Some("agy_model_readback_mismatch")
        );
        let rt_model: Option<String> = sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
            .bind(&run_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(rt_model.as_deref(), Some("gemini-pro-orchestrator-g1-pro"));
    }
    const SWITCH_MODEL: &str = "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch to Claude Opus 5.5\n  2. No, go back\n";
    async fn model_apply_fixture(
        env: &tt::Env,
        name: &str,
        field: &str,
    ) -> (String, String, String) {
        let bot = tt::claude_bot(&env.app, &env.project_id, name).await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = format!("pane-{name}");
        let (setting_column, runtime_column, setting_value, old_value) = match field {
            "model" => (
                "model",
                "runtime_model",
                "claude-opus-5-5",
                "claude-sonnet-4-5",
            ),
            "effort" => ("effort", "runtime_effort", "max", "low"),
            _ => unreachable!(),
        };
        sqlx::query(&format!("UPDATE bots SET {setting_column}=? WHERE id=?"))
            .bind(setting_value)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query(&format!(
            "UPDATE runs SET pane_id=?, {runtime_column}=? WHERE id=?"
        ))
        .bind(&pane)
        .bind(old_value)
        .bind(&run_id)
        .execute(&env.app.db)
        .await
        .unwrap();
        (bot.id, run_id, pane)
    }
    #[tokio::test]
    async fn unreadable_confirmation_reads_never_write_the_requested_runtime_value() {
        let env = tt::env().await;
        for (name, field, runtime_column, desired, command) in [
            (
                "model-read-error",
                "model",
                "runtime_model",
                "claude-opus-5-5",
                "/model claude-opus-5-5",
            ),
            (
                "effort-read-error",
                "effort",
                "runtime_effort",
                "max",
                "/effort max",
            ),
        ] {
            let (bot_id, run_id, pane) = model_apply_fixture(&env, name, field).await;
            env.herdr.set_screen(&pane, "Claude Code\n❯\n");
            let screens = env.herdr.screens.clone();
            let fail = env.herdr.fail_later();
            let pane_for_hook = pane.clone();
            crate::lifecycle::race_point::arm(
                "slash_after_enter_before_read",
                &pane,
                move || async move {
                    screens
                        .lock()
                        .unwrap()
                        .insert(pane_for_hook, SWITCH_MODEL.into());
                    fail("pane.read", tt::Fault::Refuse);
                },
            );

            let reason = crate::lifecycle::slash::apply_live_setting(&env.app, &bot_id, &[field]).await;
            assert!(
                reason.is_some(),
                "a failed dialog read cannot be reported as a live apply"
            );
            assert!(
                reason
                    .as_deref()
                    .is_some_and(|why| why.starts_with("slash_send_failed")),
                "the failed authority read must be reported directly: {reason:?}"
            );
            let runtime: Option<String> =
                sqlx::query_scalar(&format!("SELECT {runtime_column} FROM runs WHERE id=?"))
                    .bind(&run_id)
                    .fetch_one(&env.app.db)
                    .await
                    .unwrap();
            assert_eq!(
                runtime.as_deref(),
                Some(if field == "model" {
                    "claude-sonnet-4-5"
                } else {
                    "low"
                })
            );
            assert_eq!(
                env.herdr
                    .calls_to("pane.send_text")
                    .last()
                    .and_then(|v| v["text"].as_str()),
                Some(command)
            );
            let keys = env.herdr.calls_to("pane.send_keys");
            assert_eq!(
                keys.last().map(|v| v["keys"].clone()),
                Some(serde_json::json!(["Enter"]))
            );
            assert!(!keys.iter().any(|v| v["keys"] == serde_json::json!(["1"])
                || v["keys"] == serde_json::json!(["Escape"])));
            assert!(desired != runtime.as_deref().unwrap_or_default());
        }
    }
    #[tokio::test]
    async fn unreadable_reread_before_answer_does_not_press_yes() {
        let env = tt::env().await;
        let pane = "pane-before-answer";
        env.herdr.set_screen(pane, SWITCH_MODEL);
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("slash_confirm_before_answer", pane, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        assert!(
            crate::lifecycle::slash::test_seams::send_slash_line(&env.app.herdr, pane, "/model claude-opus-5-5")
                .await
                .is_err()
        );
        assert_eq!(
            env.herdr
                .calls_to("pane.send_keys")
                .iter()
                .map(|v| v["keys"].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!(["Enter"])]
        );
    }
    #[tokio::test]
    async fn unreadable_read_after_answer_is_unknown_and_is_not_cleaned_up_with_escape() {
        let env = tt::env().await;
        let pane = "pane-after-answer";
        env.herdr.set_screen(pane, SWITCH_MODEL);
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("slash_after_answer_before_read", pane, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        assert!(
            crate::lifecycle::slash::test_seams::send_slash_line(&env.app.herdr, pane, "/model claude-opus-5-5")
                .await
                .is_err()
        );
        assert_eq!(
            env.herdr
                .calls_to("pane.send_keys")
                .iter()
                .map(|v| v["keys"].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!(["Enter"]), serde_json::json!(["1"]),]
        );
    }
    #[tokio::test]
    async fn an_unsettled_composer_does_not_write_runtime_model() {
        let env = tt::env().await;
        let (bot_id, run_id, pane) = model_apply_fixture(&env, "unsettled", "model").await;
        env.herdr
            .set_screen(&pane, "Claude Code\nworking on a task\n");

        let reason = crate::lifecycle::slash::apply_live_setting(&env.app, &bot_id, &["model"]).await;
        assert!(
            reason.is_some(),
            "settle timeout must fall back to restart/unknown"
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-sonnet-4-5"));
    }
    #[tokio::test]
    async fn an_unreadable_settle_read_does_not_write_runtime_model() {
        let env = tt::env().await;
        let (bot_id, run_id, pane) = model_apply_fixture(&env, "settle-read-error", "model").await;
        env.herdr.set_screen(&pane, "Claude Code\n❯\n");
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("slash_before_settle_read", &pane, move || async move {
            fail("pane.read", tt::Fault::Refuse);
        });

        let reason = crate::lifecycle::slash::apply_live_setting(&env.app, &bot_id, &["model"]).await;
        assert!(
            reason.is_some(),
            "a failed settle read cannot be counted as a stable composer"
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-sonnet-4-5"));
    }
    #[tokio::test]
    async fn an_unreadable_grok_startup_effort_screen_does_not_send_or_mark_runtime() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "grok-startup-read-error").await;
        sqlx::query("UPDATE bots SET kind='grok', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = "pane-grok-startup-read-error";
        sqlx::query("UPDATE runs SET pane_id=?, runtime_effort='low' WHERE id=?")
            .bind(pane)
            .bind(&run_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        env.herdr.set_screen(pane, "__READ_ERROR__");
        let bot = crate::db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();

        assert!(
            crate::lifecycle::slash::apply_grok_startup_effort(&env.app, &bot, &run_id, pane, &env.app.herdr)
                .await
                .is_ok()
        );
        assert!(env.herdr.calls_to("pane.send_text").is_empty());
        assert!(env.herdr.calls_to("pane.send_keys").is_empty());
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_effort FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("low"));
    }
    #[tokio::test]
    async fn a_readable_closed_confirmation_and_settled_composer_can_be_applied() {
        let env = tt::env().await;
        let (bot_id, run_id, pane) = model_apply_fixture(&env, "confirmed", "model").await;
        env.herdr.set_screen(&pane, SWITCH_MODEL);
        let screens = env.herdr.screens.clone();
        let pane_for_hook = pane.clone();
        crate::lifecycle::race_point::arm(
            "slash_after_answer_before_read",
            &pane,
            move || async move {
                screens
                    .lock()
                    .unwrap()
                    .insert(pane_for_hook, "Claude Code\n❯\n".into());
            },
        );

        assert_eq!(
            crate::lifecycle::slash::apply_live_setting(&env.app, &bot_id, &["model"]).await,
            None
        );
        let runtime: Option<String> =
            sqlx::query_scalar("SELECT runtime_model FROM runs WHERE id=?")
                .bind(run_id)
                .fetch_one(&env.app.db)
                .await
                .unwrap();
        assert_eq!(runtime.as_deref(), Some("claude-opus-5-5"));
    }
    #[tokio::test]
    async fn a_closed_model_switch_confirmation_does_not_get_a_stale_one() {
        let env = tt::env().await;
        let pane = "pane-switch-race";
        env.herdr.set_screen(pane, "Switch model?\nYour next response will be slower\n❯ 1. Yes, switch\n  2. No, go back\n");
        let screens = env.herdr.screens.clone();
        crate::lifecycle::race_point::arm("slash_confirm_before_answer", pane, move || async move {
            screens.lock().unwrap().insert(pane.into(), "Claude Code\n❯\n".into());
        });

        crate::lifecycle::slash::test_seams::send_slash_line(&env.app.herdr, pane, "/login").await.unwrap();
        let keys = env.herdr.calls_to("pane.send_keys");
        assert_eq!(keys.len(), 1, "only the initial Enter is sent");
        assert_eq!(keys[0]["keys"], serde_json::json!(["Enter"]));
    }
}

mod send_text_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::slash::test_seams::*;
    use crate::lifecycle::*;
    use crate::state::App;
    use crate::lifecycle::slash::*;
    use crate::testing as tt;
    /// #648：併送直接打進 pane，必須先記下 pane_typed，之後的 prompt 才不會走 agent.prompt。
    #[tokio::test]
    async fn typing_text_into_the_pane_marks_it_so_later_prompts_are_typed() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "text-mark").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        let pane = format!("pane-{}", bot.id);
        let transcript = tt::trusted_transcript(&env.app, &bot.id, &env.dir, "text-mark.jsonl").await;
        std::fs::write(&transcript, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id='sess-1', transcript_path=? WHERE id=?")
            .bind(transcript.to_str().unwrap())
            .bind(&run_id)
            .execute(&env.app.db)
            .await
            .unwrap();
        env.herdr.live_pane(&pane, tt::LivePane::default());
        env.herdr.set_agent("agent", &pane, true);

        send_text(&env.app, &bot.id, "併送一句", true, None).await.unwrap();
        assert!(crate::db::pane_typed(&env.app.db, &run_id).await.unwrap());

        let run = crate::db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        let bot = crate::db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let client = crate::lifecycle::client_for_run(&env.app, &run).await.unwrap();
        let planned = crate::lifecycle::delivery::plan_delivery(&env.app, &client, &run, &bot, "下一則", false, false).await.unwrap();
        assert!(matches!(planned, Ok(Plan::Type { .. })), "併送之後不能再走 agent.prompt：{planned:?}");
    }
    /// 記號寫不進去就不打（跟 /login 同一條）。
    #[tokio::test]
    async fn text_is_not_typed_when_pane_typed_cannot_be_recorded() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "text-unwritable").await;
        tt::fake_run(&env.app, &bot.id).await;
        sqlx::query("CREATE TRIGGER no_pane_typed BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&env.app.db)
            .await
            .unwrap();

        let err = send_text(&env.app, &bot.id, "不要打", true, None).await.unwrap_err();
        assert!(matches!(err, LcError::Upstream(_)), "{err:?}");
        assert!(env.herdr.calls_to("pane.send_text").is_empty(), "寫不進去就不打");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty());
    }
    async fn in_flight_turn(app: &Arc<App>, bot_id: &str, run_id: &str) -> String {
        let conv = crate::db::conversation_id(&app.db, bot_id).await.unwrap();
        let turn_id = crate::db::ulid();
        sqlx::query(
            "INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at)
             VALUES (?,?,?,'web','in_flight','ok','跑一次測試',?)",
        )
        .bind(&turn_id)
        .bind(&conv)
        .bind(run_id)
        .bind(crate::db::now())
        .execute(&app.db)
        .await
        .unwrap();
        turn_id
    }
    async fn user_rows(app: &Arc<App>, bot_id: &str) -> Vec<(Option<String>, String, Option<String>)> {
        let conv = crate::db::conversation_id(&app.db, bot_id).await.unwrap();
        sqlx::query_as("SELECT turn_id, content, sent_via FROM messages WHERE conversation_id=? AND role='user' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }
    /// 使用者 2026-09-28：補充也是發出的訊息，要在對話窗右邊。打字成功後記成進行中回合的使用者訊息並推 `message_added`。
    #[tokio::test]
    async fn a_recorded_supplement_is_a_user_message_on_the_in_flight_turn() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "supp-rec").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        env.herdr.live_pane(&format!("pane-{}", bot.id), tt::LivePane::default());
        let turn_id = in_flight_turn(&env.app, &bot.id, &run_id).await;
        let mut rx = env.app.subscribe();

        let m = send_text_recorded(&env.app, &bot.id, "順便看一下 log", true, None, true).await.unwrap().expect("有進行中的回合就記");
        assert_eq!((m.role.as_str(), m.source.as_str(), m.sent_via.as_deref()), ("user", "web", Some("supplement")));
        assert_eq!(user_rows(&env.app, &bot.id).await, vec![(Some(turn_id.clone()), "順便看一下 log".to_string(), Some("supplement".to_string()))]);
        assert_eq!(env.herdr.calls_to("pane.send_text").len(), 1, "照樣打進 pane");
        let mut pushed = false;
        while let Ok(ev) = rx.try_recv() {
            pushed |= ev.kind == "message_added" && ev.data["message"]["id"] == m.id && ev.data["bot_id"] == bot.id;
        }
        assert!(pushed, "推 message_added，網頁才看得到泡泡");

        assert!(send_text_recorded(&env.app, &bot.id, "1", true, None, false).await.unwrap().is_none());
        assert_eq!(user_rows(&env.app, &bot.id).await.len(), 1);
    }
    /// 沒有進行中的回合：這一句 CLI 會當成新的一輪，hook 開外部回合時自己記；這裡記了就會多一則。
    #[tokio::test]
    async fn a_supplement_without_an_in_flight_turn_is_not_recorded() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "supp-idle").await;
        tt::fake_run(&env.app, &bot.id).await;
        env.herdr.live_pane(&format!("pane-{}", bot.id), tt::LivePane::default());

        assert!(send_text_recorded(&env.app, &bot.id, "閒著", true, None, true).await.unwrap().is_none());
        assert!(user_rows(&env.app, &bot.id).await.is_empty());
    }
    /// 打字失敗（這裡是 pane_typed 寫不進去、一個字都沒打）：不留訊息，錯照回。
    #[tokio::test]
    async fn a_supplement_that_was_not_typed_is_not_recorded() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "supp-fail").await;
        let run_id = tt::fake_run(&env.app, &bot.id).await;
        in_flight_turn(&env.app, &bot.id, &run_id).await;
        sqlx::query("CREATE TRIGGER no_pane_typed BEFORE UPDATE OF pane_typed ON runs BEGIN SELECT RAISE(ABORT, 'disk hiccup'); END")
            .execute(&env.app.db)
            .await
            .unwrap();

        assert!(send_text_recorded(&env.app, &bot.id, "沒打進去", true, None, true).await.is_err());
        assert!(user_rows(&env.app, &bot.id).await.is_empty(), "沒打進去就不能有泡泡");
    }
}
