#![allow(unused_imports)]

mod survey_fingerprint_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::tui_prompts::*;
    use crate::testing as tt;

    #[tokio::test]
    async fn an_ended_runs_survey_dedupe_record_is_dropped() {
        let env = tt::env().await;
        let app = env.app.clone();
        app.survey_fingerprints.lock().await.insert(
            "survey-gone".into(),
            crate::tui_prompts::test_seams::survey_fingerprint_state(3, false),
        );
        app.survey_fingerprints.lock().await.insert(
            "survey-kept".into(),
            crate::tui_prompts::test_seams::survey_fingerprint_state(4, false),
        );
        retain_survey_runs(&app, &["survey-kept".to_string()]).await;
        let m = app.survey_fingerprints.lock().await;
        assert!(!m.contains_key("survey-gone") && m.contains_key("survey-kept"));
    }
}


mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::tui_prompts::*;
    use crate::tui_prompts::test_seams::*;

    const SURVEY: &str = r#"
 ● How is Claude doing this session? (optional)
   1: Bad    2: Fine   3: Good   0: Dismiss

 > │
"#;

    /// 窄 pane 把同一段折成碎片之後，還是同一份問卷。
    const SURVEY_WRAPPED: &str = r#"
│ ● How is Claude doing   │
│ this session?           │
│ (optional)              │
│   1: Bad    2: Fine     │
│   3: Good   0: Dismiss  │
│ ╭──────────────────────╮ │
│ │ >                    │ │
│ ╰──────────────────────╯ │
│ claude | model | 42%      │
│ ⏵⏵ bypass permissions on │
"#;

    const PERMISSION: &str = r#"
 ⏺ Bash(rm -rf ./target/debug)
 ╭──────────────────────────────────────╮
 │  Do you want to proceed?             │
 │  ❯ 1. Yes                            │
 │    2. No, and tell Claude what to do │
 ╰──────────────────────────────────────╯
"#;

    async fn survey_test_env() -> (crate::testing::Env, crate::db::Run) {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "survey").await;
        let run_id = crate::testing::fake_run(&env.app, &bot.id).await;
        let run = crate::db::active_run(&env.app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(run.id, run_id);
        env.herdr.set_screen(run.pane_id.as_deref().unwrap(), screens::FEEDBACK_SURVEY);
        (env, run)
    }

    async fn assert_no_survey_key_after_race<F, Fut>(mutate: F)
    where
        F: FnOnce(Arc<App>, crate::db::Run) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let (env, run) = survey_test_env().await;
        let app = env.app.clone();
        let hook_run = run.clone();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || mutate(app, hook_run));

        assert!(!dismiss_if_survey(&env.app, &run).await, "changed authority must not count as a dismissal");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "a changed authority receives no key");
    }

    #[tokio::test]
    async fn a_replaced_dialog_receives_no_survey_zero() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let replace = env.herdr.set_screen_later();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            replace(&pane, PERMISSION);
        });

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "a permission dialog must receive no stale 0");
        assert!(!env.app.survey_fingerprints.lock().await.contains_key(&run.id), "uncertain screen must re-arm dedupe");
    }

    fn press_keys(env: &crate::testing::Env) -> Vec<serde_json::Value> {
        env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect()
    }

    /// 像真 herdr 0.9.5：`pane.read` 的 revision 恆為 0（#1910）。畫面放在 `LivePane` 的原樣畫面欄位裡。
    fn zero_revision_survey(env: &crate::testing::Env, run: &crate::db::Run) -> String {
        let pane = run.pane_id.clone().unwrap();
        env.herdr.live_pane(
            &pane,
            crate::testing::LivePane {
                cold_resume_menu: Some(screens::FEEDBACK_SURVEY.to_string()),
                zero_revision: true,
                no_spinner: true,
                ..Default::default()
            },
        );
        pane
    }

    fn set_live_screen(env: &crate::testing::Env, pane: &str, text: &str) {
        env.herdr.live.lock().unwrap().get_mut(pane).unwrap().cold_resume_menu = Some(text.to_string());
    }

    #[tokio::test]
    async fn a_revision_change_with_identical_survey_text_still_receives_zero() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let bump = env.herdr.bump_screen_revision_later();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            bump(&pane);
        });

        assert!(dismiss_if_survey(&env.app, &run).await);
        assert_eq!(press_keys(&env)[0], serde_json::json!(["0"]), "revision is no evidence; the identical text and the fence are");
    }

    #[tokio::test]
    async fn a_survey_is_dismissed_when_herdr_reports_revision_zero() {
        let (env, run) = survey_test_env().await;
        zero_revision_survey(&env, &run);

        assert!(dismiss_if_survey(&env.app, &run).await);
        assert_eq!(press_keys(&env), [serde_json::json!(["0"]), serde_json::json!(["enter"])]);
        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert_eq!(press_keys(&env).len(), 2, "the same frame is pressed once");
        assert!(
            !crate::lifecycle::claude_resume_menu::gave_up()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .first_time(&run.id, SURVEY_KEY_NOT_TAKEN, std::time::Instant::now()),
            "the unchanged survey must register the one-time give-up warning"
        );
        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert_eq!(press_keys(&env).len(), 2, "the same frame is not retried after warning");
    }

    #[tokio::test]
    async fn a_zero_revision_survey_whose_text_changes_between_reads_receives_no_key() {
        let (env, run) = survey_test_env().await;
        let pane = zero_revision_survey(&env, &run);
        let live = env.herdr.live.clone();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            live.lock().unwrap().get_mut(&pane).unwrap().cold_resume_menu = Some(PERMISSION.to_string());
        });

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(press_keys(&env).is_empty(), "a dialog that replaced the survey receives no stale 0");
        assert!(!env.app.survey_fingerprints.lock().await.contains_key(&run.id), "uncertain screen must re-arm dedupe");
    }

    #[tokio::test]
    async fn a_zero_revision_screen_that_is_not_a_survey_receives_no_key() {
        let (env, run) = survey_test_env().await;
        let pane = zero_revision_survey(&env, &run);
        set_live_screen(&env, &pane, PERMISSION);

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(press_keys(&env).is_empty());
    }

    #[tokio::test]
    async fn a_second_survey_in_the_same_run_is_dismissed_again_with_zero_revision() {
        let (env, run) = survey_test_env().await;
        let pane = zero_revision_survey(&env, &run);
        let live = env.herdr.live.clone();
        let gone = pane.clone();
        crate::lifecycle::race_point::arm("survey_after_zero", &run.id, move || async move {
            live.lock().unwrap().get_mut(&gone).unwrap().cold_resume_menu = Some(screens::IDLE_CLAUDE.to_string());
        });
        assert!(dismiss_if_survey(&env.app, &run).await);
        assert_eq!(press_keys(&env), [serde_json::json!(["0"])]);

        assert!(!dismiss_if_survey(&env.app, &run).await);
        set_live_screen(&env, &pane, screens::FEEDBACK_SURVEY);
        assert!(dismiss_if_survey(&env.app, &run).await);
        assert_eq!(press_keys(&env)[..2], [serde_json::json!(["0"]), serde_json::json!(["0"])]);
    }

    #[tokio::test]
    async fn an_unreadable_final_survey_read_receives_no_zero() {
        let (env, run) = survey_test_env().await;
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("survey_before_first_key", &run.id, move || async move {
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "an unreadable last-moment pane is uncertain");
    }

    #[tokio::test]
    async fn classifier_match_without_an_idle_composer_receives_no_zero() {
        let (env, run) = survey_test_env().await;
        env.herdr.set_screen(run.pane_id.as_deref().unwrap(), SURVEY);
        assert!(is_feedback_survey(SURVEY), "the classifier fixture is recognized");
        assert!(!composer_is_idle(&SURVEY.lines().collect::<Vec<_>>()), "the cursor shape is not an idle composer");

        assert!(!dismiss_if_survey(&env.app, &run).await);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "composer uncertainty fails closed");
    }

    #[tokio::test]
    async fn an_active_run_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET state='exited' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
            crate::testing::fake_run(&app, &run.bot_id).await;
        })
        .await;
    }

    #[tokio::test]
    async fn a_pane_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET pane_id='pane-replaced' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_herdr_session_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET herdr_session='default' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn a_native_session_replacement_receives_no_survey_zero() {
        assert_no_survey_key_after_race(|app, run| async move {
            sqlx::query("UPDATE runs SET native_session_id='native-replaced' WHERE id=?").bind(&run.id).execute(&app.db).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn one_stage_survey_dismissal_does_not_press_enter() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let replace = env.herdr.set_screen_later();
        crate::lifecycle::race_point::arm("survey_after_zero", &run.id, move || async move {
            replace(&pane, screens::IDLE_CLAUDE);
        });

        assert!(dismiss_if_survey(&env.app, &run).await, "the authorized 0 key was sent");
        let keys: Vec<serde_json::Value> = env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect();
        assert_eq!(keys, [serde_json::json!(["0"])]);
    }

    #[tokio::test]
    async fn a_changed_second_stage_receives_no_stale_enter() {
        let (env, run) = survey_test_env().await;
        let pane = run.pane_id.clone().unwrap();
        let replace = env.herdr.set_screen_later();
        crate::lifecycle::race_point::arm("survey_before_enter", &run.id, move || async move {
            replace(&pane, PERMISSION);
        });

        assert!(dismiss_if_survey(&env.app, &run).await, "the first 0 was authorized");
        let keys: Vec<serde_json::Value> = env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect();
        assert_eq!(keys, [serde_json::json!(["0"])]);
    }

    #[tokio::test]
    async fn an_unreadable_final_second_stage_read_receives_no_enter() {
        let (env, run) = survey_test_env().await;
        let fail = env.herdr.fail_later();
        crate::lifecycle::race_point::arm("survey_before_enter", &run.id, move || async move {
            fail("pane.read", crate::testing::Fault::Refuse);
        });

        assert!(dismiss_if_survey(&env.app, &run).await, "the first 0 was authorized");
        let keys: Vec<serde_json::Value> = env.herdr.calls_to("pane.send_keys").iter().map(|call| call["keys"].clone()).collect();
        assert_eq!(keys, [serde_json::json!(["0"])]);
    }

}

mod answerable_prompt_tests {
    use crate::tui_prompts::screen_has_answerable_prompt as has;

    /// 2026-11-05 使用者截圖：herdr 判 blocked，畫面只有 `Bash(…)` 工具呼叫與它的 JSON 輸出，沒有任何選項。
    const TOOL_RUNNING: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/answerable_fixtures/claude-false-blocked-tool-running.txt");

    #[test]
    fn a_running_tool_call_with_json_output_has_nothing_to_answer() {
        assert!(!has(TOOL_RUNNING));
    }

    #[test]
    fn ordinary_idle_and_working_screens_have_nothing_to_answer() {
        for (name, screen) in [
            ("claude idle", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-linux-idle.txt")),
            ("claude finished", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-linux-finished.txt")),
            ("dangerous rm 自動拒絕後", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-dangerous-rm-auto-denied.txt")),
            ("feedback survey 被引用", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-feedback-survey-quoted.txt")),
            ("codex working", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-working.txt")),
            ("codex idle", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.160-idle.txt")),
        ] {
            assert!(!has(screen), "{name}");
        }
    }

    /// 輸入列空著時，工具輸出裡的提示字樣與編號清單都不是框。
    #[test]
    fn quoted_prompt_words_above_an_empty_composer_are_not_a_prompt() {
        let screen = "⏺ 報告\n  Do you want to proceed? (y/n)\n  1. foo\n  2. bar\n  Enter to select · Esc to cancel\n\n──────────\n❯\n──────────\n  status\n";
        assert!(!has(screen));
    }

    #[test]
    fn claude_confirmation_menus_and_questionnaires_have_something_to_answer() {
        for (name, screen) in [
            ("bash 權限框", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-bash-permission.txt")),
            ("fetch 權限框", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.287-fetch-permission.txt")),
            ("Dangerous rm 確認框", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-dangerous-rm.txt")),
            ("Dangerous rm 單列", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.286-dangerous-rm-one-row.txt")),
            ("AskUserQuestion 單選", include_str!("../../../crates/am-lifecycle/src/lifecycle/answerable_fixtures/claude-2.1.290-askuserquestion-radio.txt")),
            ("AskUserQuestion 多選", include_str!("../../../crates/am-lifecycle/src/lifecycle/answerable_fixtures/claude-2.1.281-askuserquestion-multiselect.txt")),
            ("滿意度問卷", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-feedback-survey.txt")),
            ("Session paused", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-session-paused.txt")),
            ("onboarding 主題", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.278-onboarding-theme.txt")),
        ] {
            assert!(has(screen), "{name}");
        }
    }

    #[test]
    fn codex_approvals_and_dialogs_have_something_to_answer() {
        for (name, screen) in [
            ("codex approve 指令", include_str!("../../../crates/am-lifecycle/src/lifecycle/answerable_fixtures/codex-approval-command.txt")),
            ("codex y/N 問句", include_str!("../../../crates/am-lifecycle/src/lifecycle/answerable_fixtures/codex-yn-approval-narrow.txt")),
            ("codex 模型遷移", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-model-migration.txt")),
            ("codex 額度換模型", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.157-rate-limit-switch.txt")),
            ("codex 更新選單", include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-update-menu.txt")),
        ] {
            assert!(has(screen), "{name}");
        }
    }

    #[test]
    fn a_screen_that_could_not_be_read_keeps_blocked() {
        assert!(has(""));
        assert!(has("   \n\n"));
    }

    #[test]
    fn a_cursor_numbered_menu_counts_even_with_an_empty_composer_line_elsewhere() {
        assert!(has("設定\n❯ 1. 繼續\n  2. 取消\n"));
        assert!(has("Proceed?\n[y/n]\n"));
    }

    /// issue #2123: claude 2.1.326 新畫面分類測試（empty prompt hint, Queued, trust prompt, plan card, ✘/⚠/✔）。
    #[test]
    fn claude_2_1_326_screens_classification_snapshots() {
        use am_base::capture::Capture;
        use am_base::composer_parse::{box_state, BoxState};

        const EMPTY_PROMPT_HINT: &str = "\
⏺ Request failed: 500 Internal Server Error
  ⎿  Error: server overloaded, try again later

─────────────────────────────────────────────────────────────
❯ Try again or type /help for assistance
─────────────────────────────────────────────────────────────
  main | my-bot | claude-3-7-sonnet | 100% left
  ⏵⏵ bypass permissions on
";
        assert!(!has(EMPTY_PROMPT_HINT), "empty prompt hint 畫面不該有 answerable prompt");
        assert_eq!(box_state("claude", EMPTY_PROMPT_HINT), BoxState::NonEmpty, "無 ANSI 的提示文字視為有字");
        assert!(!am_base::capture::claude::PARSER.still_busy(EMPTY_PROMPT_HINT), "帶提示的輸入列不是 busy");
        assert!(!am_base::capture::claude::PARSER.awaits_input(EMPTY_PROMPT_HINT), "無 ANSI 的提示文字不算等待輸入");

        const QUEUED_PROMPT: &str = "\
⏺ Searching codebase...
  ⎿  Found 12 files

* Thinking… (4s · 1.2k tokens)

─────────────────────────────────────────────────────────────
❯ Fix the broken test
  Queued · ctrl+x ctrl+s to send now
─────────────────────────────────────────────────────────────
  main | my-bot | claude-3-7-sonnet | 98% left
  ⏵⏵ bypass permissions on
";
        assert!(am_base::capture::claude::PARSER.still_busy(QUEUED_PROMPT), "排隊中的任務畫面分類為 busy");
        assert!(!has(QUEUED_PROMPT), "Queued 提示不是 answerable confirmation prompt");

        const TRUST_PROMPT: &str = "\
Do you trust the contents of this project?
/home/ubuntu/project

Settings files that will be loaded:
  ⚠ .claude/settings.json (runs commands)
  ✔ .claude.json

❯ 1. Yes, I trust this folder
  2. No, exit

Enter to select · Esc to cancel
";
        assert!(has(TRUST_PROMPT), "信任提示（含設定檔清單與 ⚠）分類為 awaiting confirmation");

        const PLAN_CARD: &str = "\
─────────────────────────────────────────────────────────────
Approve subagent plan?
The subagent wants to edit files and run commands.

❯ Yes, and switch to default (ask each time) for this session
  No

Use arrow keys to navigate · Enter to select
";
        assert!(has(PLAN_CARD), "二選一 plan approval card 分類為 awaiting confirmation");
        assert_eq!(crate::tui_prompts::permission_prompt(PLAN_CARD).as_deref(), Some("Plan"), "plan card 權限分類為 Plan");
        assert!(crate::tui_prompts::is_plan_approval_dialog(PLAN_CARD));

        let switch_model_symbols = format!("⚠ Switch {}\n❯ 1. Yes, switch to Opus 4.8\n  2. No, go back\n", "model?");
        assert!(has(&switch_model_symbols), "帶 ⚠ 前綴的 switch model 分類為 awaiting confirmation");
        assert!(crate::tui_prompts::is_switch_model_dialog(&switch_model_symbols));
    }
}
