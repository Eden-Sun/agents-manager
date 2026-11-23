#![allow(unused_imports)]
use crate::codex_live::*;
use crate::codex_live::test_seams::*;
use crate::db;
use std::time::Duration;

    #[tokio::test]
    async fn close_picker_leaves_model_migration_rate_limit_and_update_choices_for_the_user() {
        let env = crate::testing::env().await;
        for (name, pane, screen) in [
            (
                "migration",
                "pane-codex-migration",
                include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-model-migration.txt"),
            ),
            (
                "rate-limit",
                "pane-codex-rate-limit",
                include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-rate-limit-switch.txt"),
            ),
            (
                "update-menu",
                "pane-codex-update-menu",
                include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-update-menu.txt"),
            ),
        ] {
            let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
            sqlx::query("UPDATE bots SET kind='codex' WHERE id=?").bind(&bot.id).execute(&env.app.db).await.unwrap();
            let run_id = crate::testing::fake_run(&env.app, &bot.id).await;
            sqlx::query("UPDATE runs SET pane_id=? WHERE id=?").bind(pane).bind(&run_id).execute(&env.app.db).await.unwrap();
            env.herdr.set_screen(pane, screen);
            let run = db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
            let client = env.app.herdr_for_run(&run).await.unwrap();
            assert!(!close_picker(&client, pane).await, "{name} requires the user's choice");
        }
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "Escape must not dismiss either choice");
    }

    #[tokio::test]
    async fn an_unreadable_migration_or_rate_limit_preflight_does_not_touch_a_user_choice_screen() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "unreadable-picker").await;
        sqlx::query("UPDATE bots SET kind='codex', model='gpt-6-astra', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        for (pane, screen) in [
            ("pane-codex-unreadable-migration", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-model-migration.txt")),
            ("pane-codex-unreadable-rate-limit", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-rate-limit-switch.txt")),
        ] {
            env.herdr.set_screen(pane, screen);
            env.herdr.fail_next("pane.read", crate::testing::Fault::Refuse);

            assert!(apply(&env.app.herdr, pane, &bot, Some(false), &["model"], None, None).await.is_err(), "{pane}");
            assert!(env.herdr.calls_to("pane.send_text").is_empty(), "an unreadable preflight cannot authorize /model: {pane}");
            assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "an unreadable preflight cannot authorize Enter or Escape: {pane}");
        }
    }

    #[tokio::test]
    async fn an_unreadable_ordinary_model_picker_does_not_choose_a_setting() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "unreadable-model-menu").await;
        sqlx::query("UPDATE bots SET kind='codex', model='gpt-6-astra', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let pane = "pane-codex-unreadable-model-menu";
        env.herdr.set_screen(pane, EMPTY_BOX_ANSI);
        let screens = env.herdr.screens.clone();
        let fail = env.herdr.fail_later();
        crate::race_point::arm("codex_apply_before_model_picker_read", pane, move || async move {
            screens.lock().unwrap().insert(pane.into(), MODEL_MENU.into());
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert_eq!(apply(&env.app.herdr, pane, &bot, Some(false), &["model"], None, None).await, Err("pane_read_failed"));
        assert_eq!(
            env.herdr.calls_to("pane.send_text").iter().map(|v| v["text"].as_str()).collect::<Vec<_>>(),
            vec![Some("/model")],
            "a readable initial composer may open the ordinary picker, but unreadable picker state cannot authorize a setting choice"
        );
        assert_eq!(
            env.herdr.calls_to("pane.send_keys").iter().map(|v| v["keys"].clone()).collect::<Vec<_>>(),
            vec![serde_json::json!(["Enter"])],
            "no numeric choice or cleanup key is safe until the picker can be read"
        );
    }

    #[tokio::test]
    async fn an_unreadable_final_picker_check_is_not_treated_as_closed() {
        let env = crate::testing::env().await;
        let pane = "pane-codex-final-picker-read";
        env.herdr.set_screen(pane, MODEL_MENU);
        let fail = env.herdr.fail_later();
        crate::race_point::arm("codex_picker_before_final_read", pane, move || async move {
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert!(close_picker_checked(&env.app.herdr, pane).await.is_err(), "a failed final read is not proof the picker closed");
        assert_eq!(
            env.herdr.calls_to("pane.send_keys").len(),
            4,
            "the read failure after the Escape budget cannot grant a safe-to-type result"
        );
    }

    #[tokio::test]
    async fn a_readable_empty_composer_allows_a_verified_model_and_effort_apply() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "readable-model-menu").await;
        sqlx::query("UPDATE bots SET kind='codex', model='gpt-6-astra', effort='high' WHERE id=?")
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
        let pane = "pane-codex-readable-model-menu";
        let screens = env.herdr.screens.clone();
        env.herdr.set_screen(pane, EMPTY_BOX_ANSI);
        let model_screens = screens.clone();
        crate::race_point::arm("codex_apply_before_model_picker_read", pane, move || async move {
            model_screens.lock().unwrap().insert(pane.into(), MODEL_MENU.into());
        });
        let effort_screens = screens.clone();
        crate::race_point::arm("codex_apply_before_effort_picker_read", pane, move || async move {
            effort_screens.lock().unwrap().insert(pane.into(), EFFORT_MENU.into());
        });
        crate::race_point::arm("codex_picker_before_final_read", pane, move || async move {
            screens.lock().unwrap().insert(pane.into(), COMPOSER.into());
        });

        let seen = apply(&env.app.herdr, pane, &bot, Some(false), &["model"], None, None).await.unwrap();
        assert_eq!(seen, CodexRuntime { model: "gpt-6-astra".into(), effort: Some("high".into()), fast: false });
        assert_eq!(
            env.herdr.calls_to("pane.send_text").iter().map(|v| v["text"].as_str()).collect::<Vec<_>>(),
            vec![Some("/model"), Some("1"), Some("3")],
            "a readable composer opens the picker and the verified requested rows are selected"
        );
    }

    async fn codex_bot(env: &crate::testing::Env, name: &str, model: Option<&str>, effort: Option<&str>, fast: i64) -> db::Bot {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind='codex', model=?, effort=?, fast=? WHERE id=?")
            .bind(model)
            .bind(effort)
            .bind(fast)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        db::bot(&env.app.db, &bot.id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn the_one_line_command_always_names_the_speed_and_a_concrete_effort() {
        let env = crate::testing::env().await;
        let all = ["model", "effort", "fast"];
        let fast = codex_bot(&env, "oneshot-fast", Some("gpt-6-astra"), Some("high"), 1).await;
        let cmd = |bot: &db::Bot, fields: &[&str], default: Option<&str>| oneshot_model_command(bot, fields, default);
        assert_eq!(cmd(&fast, &all, None).unwrap(), ("/model gpt-6-astra --reasoning=high --speed=priority".into(), Some("high".into())));
        let slow = codex_bot(&env, "oneshot-slow", Some("gpt-6-astra"), Some("xhigh"), 0).await;
        assert_eq!(cmd(&slow, &all, Some("low")).unwrap().0, "/model gpt-6-astra --reasoning=xhigh --speed=standard", "設了強度就不看預設");
        for (i, effort) in [None, Some(""), Some("default")].into_iter().enumerate() {
            let bot = codex_bot(&env, &format!("oneshot-no-effort-{i}"), Some("gpt-6.1-sol"), effort, 0).await;
            assert_eq!(
                cmd(&bot, &["effort"], Some("LOW")).unwrap(),
                ("/model gpt-6.1-sol --reasoning=low --speed=standard".into(), Some("low".into())),
                "{effort:?}"
            );
            assert_eq!(cmd(&bot, &["effort"], None), Err(ONESHOT_NO_DEFAULT_EFFORT), "查不到預設強度＝不送，走選單：{effort:?}");
            assert_eq!(cmd(&bot, &["effort"], Some("turbo")), Err(ONESHOT_NO_DEFAULT_EFFORT), "清單裡的值不是已知等級也不送：{effort:?}");
        }
        let no_model = codex_bot(&env, "oneshot-no-model", None, Some("max"), 1).await;
        assert_eq!(cmd(&no_model, &["effort"], None).unwrap().0, "/model --reasoning=max --speed=priority");
        assert_eq!(cmd(&fast, &["fast"], None).unwrap(), ("/model --speed=priority".into(), None));
        let unset = codex_bot(&env, "oneshot-fast-only", Some("gpt-6.1-sol"), None, 0).await;
        assert_eq!(cmd(&unset, &["fast"], None).unwrap().0, "/model --speed=standard");
        let bad = codex_bot(&env, "oneshot-bad-model", Some("gpt-6-luna --speed=flex"), None, 0).await;
        assert_eq!(cmd(&bad, &["model"], Some("low")), Err("oneshot_bad_model"));
        let bad = codex_bot(&env, "oneshot-bad-effort", Some("gpt-6-luna"), Some("turbo"), 0).await;
        assert_eq!(cmd(&bad, &["effort"], Some("low")), Err("oneshot_bad_effort"));
    }

    #[tokio::test]
    async fn the_default_effort_comes_from_the_fresh_model_list_of_the_bots_host_and_identity() {
        let env = crate::testing::env().await;
        let bot = codex_bot(&env, "oneshot-default-effort", Some("GPT-6.1-Sol"), None, 0).await;
        let list = serde_json::json!({"models": [
            {"id": "gpt-6.1-sol", "default_effort": "Low"},
            {"id": "gpt-6-luna", "default_effort": "medium"},
            {"id": "gpt-6-astra", "default_effort": null},
            {"id": "gpt-7-test", "default_effort": "bogus"},
        ]});
        assert_eq!(model_default_effort(&env.app, &bot, None).await, None, "清單還沒進快取：查不到");
        env.app.models_cache.lock().await.insert("local/codex/".into(), (std::time::Instant::now(), list.clone()));
        assert_eq!(model_default_effort(&env.app, &bot, None).await.as_deref(), Some("low"), "不分大小寫比 id");
        let unset = codex_bot(&env, "oneshot-default-effort-unset", None, None, 0).await;
        assert_eq!(model_default_effort(&env.app, &unset, Some("gpt-6-luna")).await.as_deref(), Some("medium"), "沒設模型就用 run 現在的");
        assert_eq!(model_default_effort(&env.app, &unset, None).await, None);
        for model in ["gpt-6-astra", "gpt-7-test", "gpt-9"] {
            assert_eq!(model_default_effort(&env.app, &unset, Some(model)).await, None, "{model}：沒有、或不是已知等級");
        }
        let old = std::time::Instant::now().checked_sub(crate::models::CACHE_TTL + std::time::Duration::from_secs(1)).unwrap();
        env.app.models_cache.lock().await.insert("local/codex/".into(), (old, list));
        assert_eq!(model_default_effort(&env.app, &bot, None).await, None);
    }

    /// 回應文字照 0.164.0 實測原文。
    const CHANGED: &str = "• Model changed to gpt-6-astra high fast for this session only";

    const UNAVAILABLE: &str = "■ Model 'nosuchmodel' is unavailable. Use /model to see available models.";

    const BAD_LEVEL: &str = "■ Reasoning level 'bogus' is unavailable for gpt-6-astra. Available: low, medium, high, xhigh, max, ultra.";

    const BAD_SPEED: &str = "■ Speed 'turbo' is unavailable for gpt-6-astra. Available: standard, flex, priority.";

    const BAD_ARG: &str = "■ Unexpected argument 'high'. Use '--reasoning=<level>' or '--speed=<tier>'.";

    fn with_footer(fixture: &str, footer: &str) -> String {
        let mut lines: Vec<&str> = fixture.lines().collect();
        lines.pop();
        format!("{}\n{footer}\n", lines.join("\n"))
    }

    fn idle_with_footer(footer: &str) -> String {
        with_footer(EMPTY_BOX_ANSI, footer)
    }

    /// 輸入框裡留著字（使用者的草稿、或送出的那行沒被收走）的畫面。
    fn residue_with_footer(footer: &str) -> String {
        with_footer(include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi"), footer)
    }

    /// 一行套用的 apply：送出前是輸入框空的畫面，Enter 之後畫面換成 `after`；`at_fallback` 是退回選單前一刻的畫面。
    #[derive(Default)]
    struct Run<'a> {
        name: &'a str,
        bot: (Option<&'a str>, Option<&'a str>, i64),
        fields: &'a [&'a str],
        version: Option<&'a str>,
        default_effort: Option<&'a str>,
        before_footer: &'a str,
        after: String,
        at_fallback: Option<String>,
    }

    struct Done {
        result: Result<CodexRuntime, &'static str>,
        typed: Vec<String>,
        keys: Vec<serde_json::Value>,
    }

    async fn oneshot_apply(r: Run<'static>) -> Done {
        let env = crate::testing::env().await;
        let bot = codex_bot(&env, r.name, r.bot.0, r.bot.1, r.bot.2).await;
        let pane: &'static str = Box::leak(format!("pane-codex-oneshot-{}", r.name).into_boxed_str());
        env.herdr.set_screen(pane, &idle_with_footer(r.before_footer));
        let screens = env.herdr.screens.clone();
        let after = r.after;
        let on_enter = screens.clone();
        crate::race_point::arm("codex_oneshot_after_enter", pane, move || async move {
            on_enter.lock().unwrap().insert(pane.into(), after);
        });
        if let Some(screen) = r.at_fallback {
            crate::race_point::arm("codex_oneshot_before_fallback", pane, move || async move {
                screens.lock().unwrap().insert(pane.into(), screen);
            });
        }
        let result = apply(&env.app.herdr, pane, &bot, Some(false), r.fields, r.version, r.default_effort).await;
        let typed = env.herdr.calls_to("pane.send_text").iter().filter_map(|v| v["text"].as_str().map(String::from)).collect();
        let keys = env.herdr.calls_to("pane.send_keys").iter().map(|v| v["keys"].clone()).collect();
        Done { result, typed, keys }
    }

    const BEFORE: &str = "  gpt-5.6-sol low · ~/project/agents-manager · Context 3% used · 5h 10% left";

    const V164: Option<&str> = Some("0.164.0");

    #[tokio::test]
    async fn a_supported_codex_applies_model_effort_and_speed_in_one_line_and_reads_the_status_line_back() {
        let after = format!("  {CHANGED}\n{}", idle_with_footer("  GPT-6-Astra high fast · ~/project/agents-manager · Context 3% used · 5h 10% left"));
        let d = oneshot_apply(Run {
            name: "ok", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version: V164, before_footer: BEFORE, after, ..Default::default()
        }).await;
        assert_eq!(d.result, Ok(CodexRuntime { model: "gpt-6-astra".into(), effort: Some("high".into()), fast: true }));
        assert_eq!(d.typed, vec!["/model gpt-6-astra --reasoning=high --speed=priority"], "一行就完成，沒有第二次打字、沒有選單");
        assert_eq!(d.keys, vec![serde_json::json!(["Enter"])]);
    }

    /// 第 5 點：同一個模型、沒設強度。0.164.0 實測 `/model gpt-6.1-sol --reasoning=high` 之後 `/model gpt-6.1-sol --speed=standard` 會變 `low`（重設成模型預設），
    /// 所以把清單查到的預設強度明確帶上，並且回讀要對上。
    #[tokio::test]
    async fn the_same_model_with_an_unset_effort_names_the_default_effort_and_the_readback_must_match_it() {
        let same = "  gpt-6.1-sol high · ~/project/agents-manager · Context 3% used · 5h 10% left";
        let ok = format!("  {}\n{}", CHANGED.replace("astra high fast", "sol low"), idle_with_footer("  GPT-6.1-Sol low · ~/project/agents-manager · Context 3% used · 5h 10% left"));
        let d = oneshot_apply(Run {
            name: "same-model", bot: (Some("gpt-6.1-sol"), None, 0), fields: &["effort"], version: V164, default_effort: Some("low"), before_footer: same, after: ok, ..Default::default()
        }).await;
        assert_eq!(d.result, Ok(CodexRuntime { model: "gpt-6.1-sol".into(), effort: Some("low".into()), fast: false }));
        assert_eq!(d.typed, vec!["/model gpt-6.1-sol --reasoning=low --speed=standard"]);
        let still_high = format!("  {}\n{}", CHANGED.replace("astra high fast", "sol high"), idle_with_footer(same));
        let d = oneshot_apply(Run {
            name: "same-model-high", bot: (Some("gpt-6.1-sol"), None, 0), fields: &["effort"], version: V164, default_effort: Some("low"), before_footer: same, after: still_high, ..Default::default()
        }).await;
        assert_eq!(d.result, Err("readback_effort_mismatch"));
    }

    #[tokio::test]
    async fn an_unset_effort_without_a_known_default_uses_the_menu_default_row() {
        let d = oneshot_apply(Run {
            name: "no-default", bot: (Some("gpt-6.1-sol"), None, 0), fields: &["model", "effort"], version: V164, default_effort: None, before_footer: BEFORE, after: idle_with_footer(BEFORE), ..Default::default()
        }).await;
        assert_eq!(d.typed, vec!["/model"], "一行指令沒東西可帶：直接走選單，不送帶參數的行");
        assert_eq!(d.result, Err("picker_failed"));
    }

    #[tokio::test]
    async fn a_rejected_one_line_command_fails_the_apply_and_never_retries_through_the_menus() {
        for rejection in [UNAVAILABLE, BAD_LEVEL, BAD_SPEED, BAD_ARG] {
            let after = format!("  {rejection}\n{}", idle_with_footer(BEFORE));
            let d = oneshot_apply(Run {
                name: "rejected", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, default_effort: Some("low"), before_footer: BEFORE, after, ..Default::default()
            }).await;
            assert_eq!(d.result, Err(ONESHOT_REJECTED), "{rejection}");
            assert_eq!(d.typed.len(), 1, "被拒就停：不退回 `/model` 選單 ({rejection})");
            assert_eq!(d.keys, vec![serde_json::json!(["Enter"])], "{rejection}");
        }
    }

    #[tokio::test]
    async fn a_screen_that_did_not_change_at_all_falls_back_to_the_menus_exactly_once() {
        let d = oneshot_apply(Run {
            name: "silent", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE, after: idle_with_footer(BEFORE), ..Default::default()
        }).await;
        assert_eq!(d.result, Err("picker_failed"));
        assert_eq!(d.typed, vec!["/model gpt-6-astra --reasoning=high --speed=standard".to_string(), "/model".to_string()]);
    }

    /// 第 1 點：退回選單前輸入框已經不是空的（送出的那行還留著）：再打 `/model` 會接成 `/model … /model` 再按 Enter。
    #[tokio::test]
    async fn the_fallback_never_types_into_a_composer_that_is_not_empty() {
        let d = oneshot_apply(Run {
            name: "fallback-residue", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE,
            after: idle_with_footer(BEFORE), at_fallback: Some(residue_with_footer(BEFORE)), ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_RESIDUE));
        assert_eq!(d.typed.len(), 1, "只有一行指令那一次打字，沒有選單的 `/model`");
        assert_eq!(d.keys, vec![serde_json::json!(["Enter"])], "沒有第二次 Enter");
    }

    /// 第 2 點：每個失敗出口都看一次輸入框，有東西就是 `oneshot_residue`。
    #[tokio::test]
    async fn every_failure_exit_after_sending_reports_a_composer_that_is_not_empty() {
        let hidden_footer = |composer: String| format!("  {CHANGED}\n{composer}\n  M A X\n");
        let screens: [(&str, String, &str); 4] = [
            ("hidden", hidden_footer(with_footer(include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi"), "")), ONESHOT_RESIDUE),
            ("mismatch", format!("  {CHANGED}\n{}", residue_with_footer("  GPT-6-Astra low · ~/project/agents-manager · Context 3% used · 5h 10% left")), ONESHOT_RESIDUE),
            ("no-answer", residue_with_footer(BEFORE), ONESHOT_RESIDUE),
            ("hidden-empty", hidden_footer(with_footer(EMPTY_BOX_ANSI, "")), ONESHOT_UNVERIFIED),
        ];
        for (name, after, want) in screens {
            let d = oneshot_apply(Run {
                name, bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model", "effort"], version: V164, before_footer: BEFORE, after, ..Default::default()
            }).await;
            assert_eq!(d.result, Err(want), "{name}");
            assert_eq!(d.typed.len(), 1, "{name}: 不再打字");
        }
        let empty_mismatch = format!("  {CHANGED}\n{}", idle_with_footer("  GPT-6-Astra low · ~/project/agents-manager · Context 3% used · 5h 10% left"));
        let d = oneshot_apply(Run {
            name: "mismatch-empty", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model", "effort"], version: V164, before_footer: BEFORE, after: empty_mismatch, ..Default::default()
        }).await;
        assert_eq!(d.result, Err("readback_effort_mismatch"), "輸入框是空的就照回讀的原因回");
    }

    /// 第 3 點：畫面上只要多出任何新的非空行，就不是「不認得」，不能退回選單。
    #[tokio::test]
    async fn any_new_line_on_the_screen_after_sending_is_never_taken_for_an_unknown_syntax() {
        let with_note = format!("{}  (something new on screen)\n", idle_with_footer(BEFORE));
        let d = oneshot_apply(Run {
            name: "new-line", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE, after: with_note, ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_UNVERIFIED));
        assert_eq!(d.typed.len(), 1, "不退回選單");
        assert!(has_new_nonblank_line("a\nb\n", "a\nb\nb\n"), "同樣的字多出一行也算");
        assert!(!has_new_nonblank_line("a\n\n  b  \n", "b\na\n\n"), "只是空白行、順序不同不算");
    }

    /// 第 4 點：送出前就已經是目標，回應慢（逾時還沒出來）也不能退回選單重送。
    #[tokio::test]
    async fn a_slow_answer_when_the_target_is_already_reached_is_never_resent_through_the_menus() {
        let at_target = "  gpt-6-astra high fast · ~/project/agents-manager · Context 3% used · 5h 10% left";
        let d = oneshot_apply(Run {
            name: "slow", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version: V164, before_footer: at_target,
            after: idle_with_footer(at_target), ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_UNVERIFIED));
        assert_eq!(d.typed.len(), 1, "不退回選單");
        let d = oneshot_apply(Run {
            name: "slow-control", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version: V164, before_footer: BEFORE,
            after: idle_with_footer(BEFORE), ..Default::default()
        }).await;
        assert_eq!(d.typed.len(), 2);
    }

    #[tokio::test]
    async fn a_line_that_turned_into_a_prompt_is_never_followed_by_more_typing() {
        let after = format!("› /model gpt-6-astra --reasoning=high --speed=standard\n• Working (1s • esc to interrupt)\n{}", idle_with_footer(BEFORE));
        let d = oneshot_apply(Run {
            name: "prompt", bot: (Some("gpt-6-astra"), Some("high"), 0), fields: &["model"], version: V164, before_footer: BEFORE, after, ..Default::default()
        }).await;
        assert_eq!(d.result, Err(ONESHOT_BECAME_PROMPT));
        assert_eq!(d.typed.len(), 1);
    }

    #[tokio::test]
    async fn an_older_or_unseen_codex_keeps_the_menu_path_and_never_types_arguments() {
        for version in [None, Some("0.163.2"), Some("0.162.1")] {
            let d = oneshot_apply(Run {
                name: "old", bot: (Some("gpt-6-astra"), Some("high"), 1), fields: &["model", "effort", "fast"], version, default_effort: Some("low"), before_footer: BEFORE,
                after: idle_with_footer(BEFORE), ..Default::default()
            }).await;
            assert_eq!(d.result, Err("picker_failed"), "{version:?}");
            assert_eq!(d.typed, vec!["/model"], "舊版 `/model` 帶參數會被當 prompt 送出：{version:?}");
        }
    }

    async fn run_with_runtime(env: &crate::testing::Env, fast: i64) -> String {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "payload").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model='gpt-5.6-luna', runtime_effort='max', runtime_fast=? WHERE id=?")
            .bind(fast)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        run
    }

    async fn fast_of(env: &crate::testing::Env, run: &str) -> Option<i64> {
        db::run(&env.app.db, run).await.unwrap().unwrap().runtime_fast
    }

    /// 2026-09-22 使用者：標題列寫 fast，codex 狀態列其實沒有 fast。啟動時記的 1 之後沒人改。
    #[tokio::test]
    async fn a_status_line_without_fast_corrects_a_stale_runtime_fast() {
        let env = crate::testing::env().await;
        let run = run_with_runtime(&env, 1).await;
        let screen = "› Ask Codex\n\n  gpt-5.6-luna max · ~/project/sample-agents · Context 43% used · 5h 12% left\n";
        assert!(correct_runtime_from_screen(&env.app, &run, screen).await);
        assert_eq!(fast_of(&env, &run).await, Some(0));
    }

    #[tokio::test]
    async fn a_status_line_with_fast_corrects_the_other_way() {
        let env = crate::testing::env().await;
        let run = run_with_runtime(&env, 0).await;
        let screen = "  gpt-5.6-luna max fast · /tmp · Context 43% used · 5h 12% left\n";
        assert!(correct_runtime_from_screen(&env.app, &run, screen).await);
        assert_eq!(fast_of(&env, &run).await, Some(1));
    }

    #[tokio::test]
    async fn fast_switches_follow_only_children_and_unchanged_runtime_keeps_pending_settings() {
        for managed in ["child", "user"] {
            let env = crate::testing::env().await;
            let (bot, run) = child_with_effort(&env, managed, Some("high"), Some("high")).await;
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, i64::from(managed != "child"));
            assert_eq!(fast_of(&env, &run).await, Some(0));

            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            let low = ASTRA_HIGH.replace("astra high", "astra low");
            assert!(correct_runtime_from_screen(&env.app, &run, &low).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1);
            assert!(!correct_runtime_from_screen(&env.app, &run, &low).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1);

            sqlx::query("UPDATE bots SET fast=0 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            let on = low.replace("astra low", "astra low fast");
            assert!(correct_runtime_from_screen(&env.app, &run, &on).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, i64::from(managed == "child"));
            assert_eq!(fast_of(&env, &run).await, Some(1));
        }
    }

    #[tokio::test]
    async fn fast_follow_failure_is_not_replayed_and_unknown_is_not_a_switch() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET runtime_fast=NULL WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "初次觀察未知不是已知的切換");
        sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_fast_follow BEFORE UPDATE OF fast ON bots BEGIN SELECT RAISE(FAIL, 'test fast write failure'); END").execute(&env.app.db).await.unwrap();
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(fast_of(&env, &run).await, Some(1));
        sqlx::query("DROP TRIGGER reject_fast_follow").execute(&env.app.db).await.unwrap();
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "失敗的 edge 不再回寫設定，只補 runtime");
        assert_eq!(fast_of(&env, &run).await, Some(0));
    }

    /// 讓 fast edge 先嘗試一次並失敗（trigger 擋住 bots.fast 寫入），之後同一個 edge 再出現就是被去重。
    async fn deduped_fast_edge(env: &crate::testing::Env) -> (String, String) {
        let (bot, run) = child_with_effort(env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_fast_follow BEFORE UPDATE OF fast ON bots BEGIN SELECT RAISE(FAIL, 'test fast write failure'); END").execute(&env.app.db).await.unwrap();
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        sqlx::query("DROP TRIGGER reject_fast_follow").execute(&env.app.db).await.unwrap();
        (bot, run)
    }

    async fn bot_model_fast(env: &crate::testing::Env, bot: &str) -> (Option<String>, i64) {
        let b = db::bot(&env.app.db, bot).await.unwrap().unwrap();
        (b.model, b.fast)
    }

    /// #1906：fast edge 被去重時，同一輪畫面上的 model／effort 切換仍要寫進 bots，否則 runtime 已更新、下一輪 same 成立，永遠補不回來。
    #[tokio::test]
    async fn a_deduped_fast_edge_still_follows_a_model_and_effort_change() {
        let env = crate::testing::env().await;
        let (bot, run) = deduped_fast_edge(&env).await;
        let switched = ASTRA_HIGH.replace("gpt-6-astra high", "gpt-6-luna low");
        assert!(correct_runtime_from_screen(&env.app, &run, &switched).await);
        let (model, fast) = bot_model_fast(&env, &bot).await;
        assert_eq!(model.as_deref(), Some("gpt-6-luna"), "model 不能因 fast edge 被去重而丟掉");
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
        assert_eq!(fast, 1, "被去重的 fast edge 不重播 fast");
        assert_eq!(fast_of(&env, &run).await, Some(0));
        assert!(!correct_runtime_from_screen(&env.app, &run, &switched).await);
    }

    /// 沒有 fast edge（fast 沒變）時 model／effort 照常跟隨，且不碰 bots.fast（UI 尚未套用的 fast 意圖不能被蓋掉）。
    #[tokio::test]
    async fn without_a_fast_edge_model_and_effort_follow_and_fast_is_untouched() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        let switched = ASTRA_HIGH.replace("gpt-6-astra high", "gpt-6-luna low");
        assert!(correct_runtime_from_screen(&env.app, &run, &switched).await);
        let (model, fast) = bot_model_fast(&env, &bot).await;
        assert_eq!(model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
        assert_eq!(fast, 1, "沒有 fast edge 就不寫 fast");
        assert!(crate::codex_live::test_seams::fast_follow_edges().lock().unwrap().map.get(&run).is_none(), "沒有 edge 就不登記");
    }

    /// 第一次嘗試成功的 fast edge：model 與 fast 在同一個交易一起提交。
    #[tokio::test]
    async fn a_first_fast_edge_attempt_writes_model_and_fast_in_one_transaction() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=0 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        let switched = ASTRA_HIGH.replace("gpt-6-astra high", "gpt-6-luna high fast");
        assert!(correct_runtime_from_screen(&env.app, &run, &switched).await);
        assert_eq!(bot_model_fast(&env, &bot).await, (Some("gpt-6-luna".to_string()), 1));
        assert_eq!(fast_of(&env, &run).await, Some(1));

        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        sqlx::query("UPDATE bots SET fast=0 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
        sqlx::query("CREATE TRIGGER reject_fast_follow BEFORE UPDATE OF fast ON bots BEGIN SELECT RAISE(FAIL, 'test fast write failure'); END").execute(&env.app.db).await.unwrap();
        assert!(!correct_runtime_from_screen(&env.app, &run, &switched).await);
        assert_eq!(bot_model_fast(&env, &bot).await, (Some("gpt-6-astra".to_string()), 0));
        assert_eq!(fast_of(&env, &run).await, Some(0));
    }

    /// 本機的一張小表（上限 `limit`），測試不碰全域的那張。
    fn small_table(limit: usize) -> SharedEdges {
        Box::leak(Box::new(std::sync::Mutex::new(FastFollowEdges { limit, ..Default::default() })))
    }

    async fn active_run(env: &crate::testing::Env, name: &str) -> String {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        crate::testing::fake_run(&env.app, &bot.id).await
    }

    fn put(edges: SharedEdges, run: &str) {
        assert!(crate::codex_live::test_seams::lock_edges(edges).attempt(run, true, false).follow);
    }

    /// 清理一次：已結束／不存在的 run 的 edge 移掉，仍在跑的留著；超過一批（500）的表也要全部巡到。
    #[tokio::test]
    async fn one_sweep_drops_ended_runs_in_every_batch_and_keeps_active_ones() {
        let env = crate::testing::env().await;
        let edges = small_table(10_000);
        let (a, b) = (active_run(&env, "a").await, active_run(&env, "b").await);
        let ended = active_run(&env, "ended").await;
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&ended).execute(&env.app.db).await.unwrap();
        for run in [&a, &b, &ended] { put(edges, run); }
        let phantoms: Vec<String> = (0..3 * crate::codex_live::test_seams::SWEEP_BATCH + 7).map(|i| format!("gone-{i}")).collect();
        for run in &phantoms { put(edges, run); }

        let removed = crate::codex_live::test_seams::sweep_finished_fast_edges(edges, &env.app.db).await;
        assert_eq!(removed, phantoms.len() + 1);
        let left: std::collections::BTreeSet<String> = crate::codex_live::test_seams::lock_edges(edges).map.keys().cloned().collect();
        assert_eq!(left, [a, b].into_iter().collect(), "只剩仍在跑的 run");
    }

    /// #1898：表滿時先清掉已結束的 run 再試，新的 run 進得來；清完還是滿才擋下。
    #[tokio::test]
    async fn a_full_table_makes_room_by_sweeping_ended_runs_but_never_evicts_active_ones() {
        let env = crate::testing::env().await;
        let edges = small_table(3);
        let (a, b) = (active_run(&env, "a").await, active_run(&env, "b").await);
        put(edges, &a);
        put(edges, &b);
        put(edges, "gone-1");
        assert!(crate::codex_live::test_seams::lock_edges(edges).sweeper, "第一個 edge 起了清理 task 的旗標");

        let c = active_run(&env, "c").await;
        let got = crate::codex_live::test_seams::attempt_fast_follow_in(edges, &env.app.db, &c, true, false).await;
        assert!(got.is_some(), "表滿但有已結束的 run：清完就進得來");
        let keys: std::collections::BTreeSet<String> = crate::codex_live::test_seams::lock_edges(edges).map.keys().cloned().collect();
        assert_eq!(keys, [a.clone(), b.clone(), c.clone()].into_iter().collect());

        let d = active_run(&env, "d").await;
        assert!(crate::codex_live::test_seams::attempt_fast_follow_in(edges, &env.app.db, &d, true, false).await.is_none());
        assert_eq!(crate::codex_live::test_seams::lock_edges(edges).map.len(), 3);
        assert!(crate::codex_live::test_seams::lock_edges(edges).last_full_warn.is_some(), "擋下時記了 WARN 的時間");
        let e = active_run(&env, "e").await;
        let before = crate::codex_live::test_seams::lock_edges(edges).last_full_warn;
        assert!(crate::codex_live::test_seams::attempt_fast_follow_in(edges, &env.app.db, &e, true, false).await.is_none());
        assert_eq!(crate::codex_live::test_seams::lock_edges(edges).last_full_warn, before, "一小時內不再 WARN");
    }

    #[tokio::test]
    async fn a_tui_fast_switch_after_ui_live_apply_is_a_new_edge_with_or_without_a_matching_poll() {
        for matching_poll in [false, true] {
            let env = crate::testing::env().await;
            let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0);
            assert_eq!(fast_of(&env, &run).await, Some(0));

            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            if matching_poll {
                let on = ASTRA_HIGH.replace("astra high", "astra high fast");
                assert!(!correct_runtime_from_screen(&env.app, &run, &on).await, "matching observation emits no change");
                assert!(crate::codex_live::test_seams::fast_follow_edges().lock().unwrap().map[&run].attempted);
            }
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(fast_of(&env, &run).await, Some(0));
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0, "the second TUI off must follow after UI live apply");
            assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        }
    }

    #[tokio::test]
    async fn a_failed_or_cancelled_runtime_write_never_publishes_or_replays_child_settings() {
        for cancel in [false, true] {
            let env = crate::testing::env().await;
            let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            sqlx::query("UPDATE runs SET runtime_fast=1 WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            let mut events = env.app.subscribe();
            if cancel {
                let entered = std::sync::Arc::new(tokio::sync::Notify::new());
                let ready = entered.clone();
                crate::race_point::arm("codex_follow_before_runtime_write", &run, move || async move {
                    ready.notify_one();
                    std::future::pending::<()>().await;
                });
                let app = env.app.clone();
                let run_id = run.clone();
                let task = tokio::spawn(async move { correct_runtime_from_screen(&app, &run_id, ASTRA_HIGH).await });
                entered.notified().await;
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
            } else {
                sqlx::query("CREATE TRIGGER reject_runtime_follow BEFORE UPDATE OF runtime_fast ON runs BEGIN SELECT RAISE(FAIL, 'test runtime write failure'); END").execute(&env.app.db).await.unwrap();
                assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
                sqlx::query("DROP TRIGGER reject_runtime_follow").execute(&env.app.db).await.unwrap();
            }
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "child UPDATE must roll back with runtime");
            assert_eq!(fast_of(&env, &run).await, Some(1));
            assert!(events.try_recv().is_err(), "no event before atomic commit");
            sqlx::query("UPDATE bots SET fast=1 WHERE id=?").bind(&bot).execute(&env.app.db).await.unwrap();
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 1, "preserve the newer UI intent");
            assert_eq!(fast_of(&env, &run).await, Some(0));
            assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            let on = ASTRA_HIGH.replace("astra high", "astra high fast");
            assert!(correct_runtime_from_screen(&env.app, &run, &on).await);
            assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
            assert_eq!(db::bot(&env.app.db, &bot).await.unwrap().unwrap().fast, 0);
            sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run).execute(&env.app.db).await.unwrap();
            assert!(crate::codex_live::test_seams::sweep_finished_fast_edges(crate::codex_live::test_seams::fast_follow_edges(), &env.app.db).await >= 1);
            assert!(!crate::codex_live::test_seams::fast_follow_edges().lock().unwrap().map.contains_key(&run));
        }
    }

    /// 子 agent（`managed_by=child`）與 run 的強度設定；`prev` 是 run 上一輪記的 `runtime_effort`（`None`＝未知）。
    async fn child_with_effort(env: &crate::testing::Env, managed_by: &str, effort: Option<&str>, prev: Option<&str>) -> (String, String) {
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "payload").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE bots SET managed_by=?, model='gpt-6-astra', effort=? WHERE id=?")
            .bind(managed_by)
            .bind(effort)
            .bind(&bot.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET runtime_model='gpt-6-astra', runtime_effort=?, runtime_fast=0 WHERE id=?")
            .bind(prev)
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();
        (bot.id, run)
    }

    async fn bot_effort(env: &crate::testing::Env, bot: &str) -> Option<String> {
        sqlx::query_scalar("SELECT effort FROM bots WHERE id=?").bind(bot).fetch_one(&env.app.db).await.unwrap()
    }

    async fn run_effort(env: &crate::testing::Env, run: &str) -> Option<String> {
        db::run(&env.app.db, run).await.unwrap().unwrap().runtime_effort
    }

    const ASTRA_DEFAULT: &str = "› Ask Codex to do anything\n\n  GPT-6-Astra default · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";

    const ASTRA_HIGH: &str = "› Ask Codex to do anything\n\n  gpt-6-astra high · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";

    /// 未知→明確 default：runtime 記成 `default`（不是 NULL），但子 agent 設定保留 high，web 會照常警示「實際 CLI 預設、設定 High」。
    #[tokio::test]
    async fn unknown_to_default_records_the_cli_default_and_keeps_the_child_effort() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), None).await;
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"), "未知不等於已套用 default");
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
    }

    /// 已知具體值 high → 明確 default：子 agent 設定清成 NULL（CLI 預設），runtime 記 `default`。
    #[tokio::test]
    async fn a_known_high_switched_to_the_cli_default_clears_the_child_effort() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(bot_effort(&env, &bot).await, None);
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
    }

    /// default → default：已經是預設，不清也不寫設定（不會因為重複讀到 default 就把設定抹掉）。
    #[tokio::test]
    async fn a_default_that_stays_default_does_not_touch_the_child_effort() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("default")).await;
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
    }

    /// 一般 bot（`managed_by` 不是 child）：不跟隨，設定與 runtime 各自照規則走。
    #[tokio::test]
    async fn a_user_bot_keeps_its_effort_when_the_tui_goes_to_default() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "user", Some("high"), Some("high")).await;
        assert!(correct_runtime_from_screen(&env.app, &run, ASTRA_DEFAULT).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("default"));
    }

    /// 讀不到強度的畫面（沒有 footer、或冷啟動兩段 footer）：runtime 與子 agent 設定都不清。
    #[tokio::test]
    async fn a_screen_without_a_readable_footer_clears_nothing() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        for screen in ["› Ask Codex to do anything\n", "  GPT-6-Astra default · ~/project/agents-manager\n"] {
            assert!(!correct_runtime_from_screen(&env.app, &run, screen).await, "{screen:?}");
        }
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"));
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("high"));
    }

    /// 網頁另設 low、還沒套用：畫面的 runtime 沒變（仍是 high），設定不能被蓋回去。
    #[tokio::test]
    async fn a_pending_web_change_is_kept_while_the_runtime_is_unchanged() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("low"), Some("high")).await;
        assert!(!correct_runtime_from_screen(&env.app, &run, ASTRA_HIGH).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
    }

    /// 真的差異照列：畫面換成具體值，子 agent 設定跟著改（既有行為不變）。
    #[tokio::test]
    async fn a_real_effort_switch_still_follows_into_the_child_settings() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        let low = "› Ask Codex to do anything\n\n  gpt-6-astra low · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";
        assert!(correct_runtime_from_screen(&env.app, &run, low).await);
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("low"));
        assert_eq!(run_effort(&env, &run).await.as_deref(), Some("low"));
    }

    /// 有效的 footer 但沒有強度字、也沒有 default（未知）：已知的 high 保留，model／fast 仍照畫面校正。
    #[tokio::test]
    async fn a_valid_footer_without_an_effort_word_keeps_the_known_effort_and_still_corrects_model_and_fast() {
        let env = crate::testing::env().await;
        let (bot, run) = child_with_effort(&env, "child", Some("high"), Some("high")).await;
        let footer = "› Ask Codex to do anything\n\n  gpt-6-luna fast · ~/project/agents-manager · Context 3% used\n  ? for shortcuts\n";
        assert!(correct_runtime_from_screen(&env.app, &run, footer).await);
        let row = db::run(&env.app.db, &run).await.unwrap().unwrap();
        assert_eq!(row.runtime_model.as_deref(), Some("gpt-6-luna"), "model 照畫面校正");
        assert_eq!(row.runtime_fast, Some(1), "fast 照畫面校正");
        assert_eq!(row.runtime_effort.as_deref(), Some("high"), "未知的強度不清成 NULL");
        assert_eq!(bot_effort(&env, &bot).await.as_deref(), Some("high"), "未知的強度不跟隨到子 agent 設定");
        assert!(!correct_runtime_from_screen(&env.app, &run, footer).await);
    }

    /// 收編時 runs.runtime_* 全為 NULL，狀態列帶主題段能正確填入 runtime_model、runtime_effort 與 runtime_fast。
    #[tokio::test]
    async fn a_status_line_with_topic_corrects_null_runtime_and_enables_fast() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "payload").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model=NULL, runtime_effort=NULL, runtime_fast=NULL WHERE id=?")
            .bind(&run)
            .execute(&env.app.db)
            .await
            .unwrap();

        let screen = "› Ask Codex to do anything\n\n  GPT-6-Luna max fast · ~/project/agents-manager · 修正主力 cache 冷 context 規則\n  ? for shortcuts\n";
        assert!(correct_runtime_from_screen(&env.app, &run, screen).await);

        let row = db::run(&env.app.db, &run).await.unwrap().unwrap();
        assert_eq!(row.runtime_model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(row.runtime_effort.as_deref(), Some("max"));
        assert_eq!(row.runtime_fast, Some(1));
    }

    /// 讀不到狀態列（選單開著、畫面被清）不能當成 fast=false。
    #[tokio::test]
    async fn an_unreadable_screen_leaves_the_runtime_alone() {
        let env = crate::testing::env().await;
        let run = run_with_runtime(&env, 1).await;
        for screen in ["", "› Ask Codex to do anything\n", MODEL_MENU] {
            assert!(!correct_runtime_from_screen(&env.app, &run, screen).await, "{screen:?}");
        }
        assert_eq!(fast_of(&env, &run).await, Some(1));
    }

    const MODEL_MENU: &str = "\
  Select Model and Effort
  Access legacy models by running codex -m <model_name> or in your config.toml

  1. gpt-6-astra (default)   Our most capable model for complex, demanding work.
  2. gpt-5.6-sol             Reliable agentic workhorse for everyday tasks.
  3. gpt-5.6-terra           Balanced agentic coding model for everyday work.
› 4. gpt-5.6-luna (current)  Fast and affordable agentic coding model.
  5. gpt-5.5                 Proven previous-generation model for coding and general work.

  Press enter to confirm or esc to go back
";

    const EFFORT_MENU: &str = "\
  Select Reasoning Level for gpt-5.6-sol

› 1. Low (default)    Fast responses with lighter reasoning
  2. Medium           Balances speed and reasoning depth for everyday tasks
  3. High             Greater reasoning depth for complex problems
  4. Extra high       Extra high reasoning depth for complex problems
  5. More reasoning…  Max and Ultra consume usage limits faster

  Press enter to confirm or esc to go back
";

    #[tokio::test]
    async fn missing_or_ambiguous_picker_rows_do_not_authorize_number_entry() {
        let env = crate::testing::env().await;
        const SCREEN: &str = "\
  Select Model and Effort

  1. gpt-5.5     First duplicate
  2. gpt-5.5     Second duplicate

  Press enter to confirm or esc to go back
";
        for (pane, needle) in [
            ("pane-codex-ambiguous-picker", "gpt-5.5"),
            ("pane-codex-missing-picker", "gpt-6-astra"),
        ] {
            env.herdr.set_screen(pane, SCREEN);
            assert_eq!(press_number(&env.app.herdr, pane, 1, needle).await, Ok(false), "{needle}");
        }
        assert!(env.herdr.calls_to("pane.send_text").is_empty(), "an ambiguous row must not receive a numeric choice");
        assert!(
            env.herdr.calls_to("pane.send_keys").iter().all(|call| call["keys"] == serde_json::json!(["Escape"])),
            "the picker may only be escaped, never assigned an ambiguous row number"
        );
    }

    /// 帶樣式的真空輸入框（灰色佔位字）：閒著套用前會先看輸入框有沒有草稿，純文字的 [`COMPOSER`] 分不出佔位字。
    const EMPTY_BOX_ANSI: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-idle.ansi");

    /// Real composer, codex 0.154.0 (2026-09-10) — nothing in the way.
    const COMPOSER: &str = "\
─ Worked for 1m 17s ────────────────────────────────────

• Model changed to gpt-6-astra high

› Ask Codex to do anything

  gpt-6-astra high · ~/project/agents-manager · Context 44% used · 5h 10% left
";

    fn bot(model: Option<&str>, effort: Option<&str>, fast: bool) -> crate::db::Bot {
        crate::db::Bot { role: None, manager_use: 1, track: None, runtime_override_json: None,
            id: "b".into(),
            project_id: "p".into(),
            name: "b".into(),
            kind: "codex".into(),
            model: model.map(Into::into),
            effort: effort.map(Into::into),
            fast: i64::from(fast),
            persona: None,
            args_json: "[]".into(),
            autostart: 0,
            inject_hooks: 1,
            auto_approve: 1,
            identity: None,
            env_json: "{}".into(),
            managed_by: "user".into(),
            cwd: None,
            herdr_session: None,
            parent_bot_id: None,
            is_primary: 0,
            primary_position: 0,
            hook_token: "t".into(),
            deleted_at: None,
            created_at: crate::db::now(),
        }
    }

    /// #1671 真實快照（mac_builder、gpt-6.1-sol、213×55）：沒有 `? for shortcuts` 行，warning 接在模型列右側。
    fn fixture_1671() -> String {
        let v: serde_json::Value = serde_json::from_str(include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-1671-model-row-with-warning.json")).unwrap();
        v["text"].as_str().unwrap().to_string()
    }

    const MODEL_ROW_1671: &str = "GPT-6.1-Sol high · ~/project/sample-agents/projects/rt · Context 17% used · weekly 100% left";

    #[tokio::test]
    async fn the_footer_is_written_to_the_run_only_when_it_changes_and_never_cleared_by_an_unreadable_screen() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "footer").await;
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        let read = |env: &crate::testing::Env, run: &str| {
            let db = env.app.db.clone();
            let run = run.to_string();
            async move {
                let r = db::run(&db, &run).await.unwrap().unwrap();
                (r.status_line, r.status_json)
            }
        };
        assert!(record_footer_from_screen(&env.app, &run, &fixture_1671()).await);
        let (line, json) = read(&env, &run).await;
        assert_eq!(line.as_deref(), Some(MODEL_ROW_1671));
        let json: serde_json::Value = serde_json::from_str(&json.unwrap()).unwrap();
        assert_eq!(json["context_window"]["used_percentage"], 17.0);

        assert!(!record_footer_from_screen(&env.app, &run, &fixture_1671()).await, "同一張畫面不重寫");
        assert!(!record_footer_from_screen(&env.app, &run, "• Compacting context (1s • esc to interrupt)\n").await, "讀不到不清掉舊值");
        assert_eq!(read(&env, &run).await.0.as_deref(), Some(MODEL_ROW_1671));

        let next = fixture_1671().replace("Context 17% used", "Context 18% used");
        assert!(record_footer_from_screen(&env.app, &run, &next).await);
        assert!(read(&env, &run).await.0.unwrap().contains("Context 18% used"));
    }

    /// #1924：run 啟動後從開機畫面記版本，不等巡邏。
    async fn codex_boot_run(name: &str) -> (crate::testing::Env, String, String) {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, name).await;
        sqlx::query("UPDATE bots SET kind = 'codex' WHERE id = ?").bind(&bot.id).execute(&env.app.db).await.unwrap();
        let run = crate::testing::fake_run(&env.app, &bot.id).await;
        (env, bot.id, run)
    }

    const BOOT_164: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.164-startup.txt");

    #[tokio::test]
    async fn the_boot_banner_is_recorded_even_when_it_only_shows_on_a_later_read() {
        let (env, bot_id, run) = codex_boot_run("boot-late").await;
        let reads = std::cell::Cell::new(0);
        let read = || {
            reads.set(reads.get() + 1);
            let n = reads.get();
            async move { (n >= 2).then(|| BOOT_164.to_string()) }
        };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, read, Duration::from_millis(1)).await;
        assert_eq!(reads.get(), 2, "第一次讀還沒畫出來，第二次讀到就停");
        assert_eq!(crate::codex_update::running_version_of(&run).as_deref(), Some("0.164.0"));
        let stored: Option<String> = sqlx::query_scalar("SELECT cli_version FROM runs WHERE id = ?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(stored.as_deref(), Some("0.164.0"), "開機畫面的版本也寫進 DB，daemon 重啟後靠它");
        sqlx::query("UPDATE runs SET cli_version = '0.100.0' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, || async { Some(BOOT_164.to_string()) }, Duration::from_millis(1)).await;
        let stored: Option<String> = sqlx::query_scalar("SELECT cli_version FROM runs WHERE id = ?").bind(&run).fetch_one(&env.app.db).await.unwrap();
        assert_eq!(stored.as_deref(), Some("0.164.0"), "畫面讀到的覆寫 DB 裡的舊值");
    }

    #[tokio::test]
    async fn an_unreadable_boot_screen_leaves_the_version_unknown() {
        let (env, bot_id, run) = codex_boot_run("boot-blank").await;
        let reads = std::cell::Cell::new(0);
        let no_banner = || {
            reads.set(reads.get() + 1);
            async { Some("› Ask Codex to do anything\n".to_string()) }
        };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, no_banner, Duration::from_millis(1)).await;
        assert_eq!(reads.get(), 3, "第一次讀加兩次重試，共三次讀取，全部讀不到就停");
        assert_eq!(crate::codex_update::running_version_of(&run), None, "解析不到維持未知，照舊退回選單");

        let pane_failed = || async { None::<String> };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, pane_failed, Duration::from_millis(1)).await;
        assert_eq!(crate::codex_update::running_version_of(&run), None, "pane 讀失敗也維持未知");
    }

    #[tokio::test]
    async fn only_codex_reads_the_boot_screen() {
        let (env, bot_id, run) = codex_boot_run("boot-claude").await;
        let reads = std::cell::Cell::new(0);
        let read = || {
            reads.set(reads.get() + 1);
            async { Some(BOOT_164.to_string()) }
        };
        remember_boot_version(&env.app.db, None, "claude", &bot_id, &run, read, Duration::from_millis(1)).await;
        assert_eq!(reads.get(), 0, "非 codex 一次都不讀畫面");
        assert_eq!(crate::codex_update::running_version_of(&run), None);
    }

    #[tokio::test]
    async fn a_boot_banner_is_not_recorded_once_the_run_is_no_longer_the_current_one() {
        let (env, bot_id, run) = codex_boot_run("boot-stale").await;
        sqlx::query("UPDATE runs SET state = 'exited' WHERE id = ?").bind(&run).execute(&env.app.db).await.unwrap();
        let read = || async { Some(BOOT_164.to_string()) };
        remember_boot_version(&env.app.db, None, "codex", &bot_id, &run, read, Duration::from_millis(1)).await;
        assert_eq!(crate::codex_update::running_version_of(&run), None, "已經不是當前的 run 不記");
    }
