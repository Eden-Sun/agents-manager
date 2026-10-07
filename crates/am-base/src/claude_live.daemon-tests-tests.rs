
    use super::*;

    const SCREEN: &str = "\
❯ /model sonnet
  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions

❯ /effort high
  ⎿  Set effort level to high (saved as your default for new sessions): Comprehensive implementation with extensive
     testing and documentation
";

    #[test]
    fn reads_the_last_model_and_effort_switch() {
        assert_eq!(parse(SCREEN), Switch { model: Some("claude-sonnet-5-5".into()), effort: Some("high".into()) });
        let later = format!("{SCREEN}\n❯ /model opus\n  ⎿  Set model to Opus 5.5 (default)\n");
        assert_eq!(parse(&later).model.as_deref(), Some("claude-opus-5-5"), "取最後一次");
    }

    #[test]
    fn ignores_the_same_words_outside_a_slash_command_output() {
        let quoted = "⏺ 我會執行 /model，畫面會顯示 Set model to Sonnet 5.5\n  Set effort level to max\n";
        assert_eq!(parse(quoted), Switch::default());
        assert_eq!(parse("❯ /model x\n  ⎿  Set model to Something Weird\n"), Switch::default(), "認不出的顯示名不猜");
    }

    #[test]
    fn ignores_tool_output_that_looks_like_model_or_effort_confirmation() {
        let tool_output = "⏺ Bash(cat source.rs)\n  ⎿ Set model to Sonnet 5.5 and saved as your default for new sessions\n\
⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to high (saved as your default for new sessions): output\n";
        assert_eq!(parse(tool_output), Switch::default());
    }

    #[test]
    fn a_real_slash_command_after_tool_output_is_still_recognised() {
        let screen = "⏺ Bash(cat tests.rs)\n  ⎿ Set model to Opus 5.5 and saved as your default\n\n❯ /model sonnet\n  ⎿  Set model to Sonnet 5.5 and saved as your default for new sessions\n\n⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to max: spoof\n";
        assert_eq!(parse(screen), Switch { model: Some("claude-sonnet-5-5".into()), effort: None });
        // 工具輸出裡長得像指令行的（縮排）不算，指令後不接 ⎿ 也不留到後面的工具輸出。
        let spoof = "⏺ Bash(cat log)\n    ❯ /model sonnet\n  ⎿ Set model to Sonnet 5.5 and saved\n❯ /model sonnet\n\n⏺ Bash(x)\n  ⎿ Set model to Opus 5.5\n";
        assert_eq!(parse(spoof), Switch::default());
    }

    #[tokio::test]
    async fn a_child_follows_the_switch_and_a_user_bot_only_after_its_baseline() {
        let env = crate::testing::env().await;
        let app = env.app.clone();

        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        env.herdr.set_screen(&format!("pane-{}", kid.id), SCREEN);
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "子 agent 的設定跟著改");
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-sonnet-5-5"));

        let user = crate::testing::claude_bot(&app, &env.project_id, "user").await;
        let run_id = crate::testing::fake_run(&app, &user.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        env.herdr.set_screen(&format!("pane-{}", user.id), SCREEN);
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model, None, "第一次看到的可能是 --resume 印回來的舊行，只當基準");
        let later = format!("{SCREEN}\n❯ /model haiku\n  ⎿  Set model to Haiku 4.5 and saved\n");
        env.herdr.set_screen(&format!("pane-{}", user.id), &later);
        observe(&app, &run, &later).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-haiku-4-5"), "之後變了才採用");
        let b = db::bot(&app.db, &user.id).await.unwrap().unwrap();
        assert_ne!(b.model.as_deref(), Some("claude-haiku-4-5"), "一般 bot 的設定不動（重啟會回到設定值，畫成 drift）");
    }

    /// 2026-10-02 cf-優化 的 nv-opus：child 的模型跟設定一樣、強度不一樣，以前只寫 runtime_effort、runtime_model 留空，
    /// 網頁畫成「CLI 預設 ⟳」。兩欄都要寫。
    #[tokio::test]
    async fn a_child_whose_model_matches_its_setting_still_records_the_runtime_model() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-sonnet-5-5', effort = NULL WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        env.herdr.set_screen(&format!("pane-{}", kid.id), SCREEN);
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")));
    }

    #[tokio::test]
    async fn a_switch_typed_before_the_first_sweep_of_a_fresh_run_is_adopted() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let user = crate::testing::claude_bot(&app, &env.project_id, "fresh").await;
        let run_id = crate::testing::fake_run(&app, &user.id).await;
        start_fresh(&run_id);
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        env.herdr.set_screen(&format!("pane-{}", user.id), SCREEN);
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "model 與 effort 都在第一輪就採用");
        let b = db::bot(&app.db, &user.id).await.unwrap().unwrap();
        assert_eq!(b.model, None, "一般 bot 的設定不動");
    }

    /// #743：run 的 UPDATE 成功、bot 的 UPDATE 失敗，下一輪要把 bot 補上（runtime 已經對了也一樣）。
    #[tokio::test]
    async fn a_child_whose_bot_update_failed_is_caught_up_on_the_next_sweep() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid743").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        // 讓 bot 的 UPDATE 失敗（run 的不受影響）。
        sqlx::query("CREATE TRIGGER fail_bot_update BEFORE UPDATE ON bots BEGIN SELECT RAISE(ABORT, 'boom'); END")
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen(&format!("pane-{}", kid.id), SCREEN);
        observe(&app, &run, SCREEN).await;
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "run 已寫入");
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-opus-5-5"), Some("xhigh")), "bot 的 UPDATE 失敗，還沒跟上");

        sqlx::query("DROP TRIGGER fail_bot_update").execute(&app.db).await.unwrap();
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-sonnet-5-5"), Some("high")), "下一輪補上 model 與 effort");
    }

    /// 2.1.286 同級 fallback：argv／設定是 Opus 5.5，server 退回上一版（statusLine 回的 id 不同）。
    fn statusline(model: serde_json::Value) -> crate::hookrecv::HookBody {
        crate::hookrecv::HookBody {
            bot_id: String::new(),
            provider: "claude".into(),
            payload: serde_json::json!({"hook_event_name": "StatusLine", "session_id": "s-750", "model": model}),
            received_at: None,
            truncated: false,
            run_id: None,
        }
    }

    #[tokio::test]
    async fn the_statusline_model_corrects_runtime_model_but_never_the_configured_one() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "fallback750").await;
        sqlx::query("UPDATE bots SET model = 'claude-opus-5-5' WHERE id = ?").bind(&bot.id).execute(&app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model = 'claude-opus-5-5' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        let runtime = || async { db::run(&app.db, &run_id).await.unwrap().unwrap().runtime_model };
        let send = |model: serde_json::Value| {
            let mut body = statusline(model);
            body.bot_id = bot.id.clone();
            let app = app.clone();
            async move { crate::hookrecv::process(&app, &body).await.unwrap() }
        };

        send(serde_json::json!({"id": "claude-opus-5", "display_name": "Opus 5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-opus-5"), "實際在跑的是 fallback 後那一版");
        let b = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        assert_eq!(b.model.as_deref(), Some("claude-opus-5-5"), "設定值不動，UI 才畫得出 drift");

        // 沒有 id（只有顯示名）、空 id、沒有 model：保留舊值，不清空、不猜。
        send(serde_json::json!({"display_name": "Opus 5.5"})).await;
        send(serde_json::json!({"id": "  "})).await;
        send(serde_json::Value::Null).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-opus-5"));

        send(serde_json::json!({"id": "claude-opus-5-5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-opus-5-5"), "下一則又回設定的那一版就收斂回去");

        // 啟動時記的是別名：同家族的完整 id 不算分歧。
        sqlx::query("UPDATE runs SET runtime_model = 'opus' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        send(serde_json::json!({"id": "claude-opus-5-5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("opus"));
        send(serde_json::json!({"id": "claude-sonnet-5-5"})).await;
        assert_eq!(runtime().await.as_deref(), Some("claude-sonnet-5-5"), "別家族一定是真的換了");
    }

    #[tokio::test]
    async fn a_stale_statusline_cannot_overwrite_the_current_run_and_a_db_error_keeps_the_old_value() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "stale750").await;
        let old = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET state = 'exited', ended_at = ?, runtime_model = 'claude-opus-5-5', native_session_id = 's-old' WHERE id = ?")
            .bind(db::now())
            .bind(&old)
            .execute(&app.db)
            .await
            .unwrap();
        let cur = crate::testing::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET runtime_model = 'claude-opus-5-5', native_session_id = 's-cur' WHERE id = ?").bind(&cur).execute(&app.db).await.unwrap();

        // 舊 session 的 statusLine（世代圍籬擋下）不能改到現在這個 run。
        let mut body = statusline(serde_json::json!({"id": "claude-haiku-4-5"}));
        body.bot_id = bot.id.clone();
        body.payload["session_id"] = serde_json::json!("s-old");
        crate::hookrecv::process(&app, &body).await.unwrap();
        let r = db::run(&app.db, &cur).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-opus-5-5"));

        // DB 寫入失敗：保留舊值（不清空）。
        sqlx::query("CREATE TRIGGER refuse_runtime BEFORE UPDATE OF runtime_model ON runs BEGIN SELECT RAISE(ABORT, 'boom'); END")
            .execute(&app.db)
            .await
            .unwrap();
        adopt_statusline_model(&app, &r, &serde_json::json!({"model": {"id": "claude-opus-5"}})).await;
        let r = db::run(&app.db, &cur).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-opus-5-5"));
    }

    #[tokio::test]
    async fn tool_output_does_not_change_a_child_runtime_or_configured_model() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'xhigh' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let tool_output = "⏺ Bash(cat source.rs)\n  ⎿ Set model to Sonnet 5.5 and saved as your default for new sessions\n\
⏺ Bash(cat tests.rs)\n  ⎿ Set effort level to high (saved as your default for new sessions): output\n";
        env.herdr.set_screen(&format!("pane-{}", kid.id), tool_output);

        observe(&app, &run, tool_output).await;

        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (Some("claude-opus-5-5"), Some("xhigh")));
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!((r.runtime_model.as_deref(), r.runtime_effort.as_deref()), (None, None));
    }

    /// 鎖外看到的確認行可能是網頁套用前的舊畫面。寫入以鎖內重讀為準，母 bot 的設定不動（drift 仍是「需重啟」）。
    #[tokio::test]
    async fn a_stale_screen_snapshot_does_not_overwrite_a_switch_already_on_the_pane() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let kid = crate::testing::claude_bot(&app, &env.project_id, "kid-stale").await;
        sqlx::query("UPDATE bots SET managed_by = 'child', model = 'claude-opus-5-5', effort = 'high' WHERE id = ?")
            .bind(&kid.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = crate::testing::fake_run(&app, &kid.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        let fresh = "❯ /model haiku\n  ⎿  Set model to Haiku 4.5 and saved\n";
        env.herdr.set_screen(&format!("pane-{}", kid.id), fresh);
        let mut rx = app.subscribe();
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &kid.id).await.unwrap().unwrap();
        assert_eq!(b.model.as_deref(), Some("claude-haiku-4-5"), "採用鎖內重讀，不用呼叫端傳來的舊確認");
        assert!(std::iter::from_fn(|| rx.try_recv().ok()).any(|f| f.kind == "bot_changed"));

        let user = crate::testing::claude_bot(&app, &env.project_id, "user-stale").await;
        sqlx::query("UPDATE bots SET model = 'claude-opus-5-5' WHERE id = ?").bind(&user.id).execute(&app.db).await.unwrap();
        let run_id = crate::testing::fake_run(&app, &user.id).await;
        start_fresh(&run_id);
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        env.herdr.set_screen(&format!("pane-{}", user.id), fresh);
        observe(&app, &run, SCREEN).await;
        let b = db::bot(&app.db, &user.id).await.unwrap().unwrap();
        assert_eq!(b.model.as_deref(), Some("claude-opus-5-5"), "母 bot 設定不被畫面蓋掉，需重啟仍比得到");
        let r = db::run(&app.db, &run_id).await.unwrap().unwrap();
        assert_eq!(r.runtime_model.as_deref(), Some("claude-haiku-4-5"));
    }

    /// Linux 的 claude 把助手／工具列畫成 `● `：一出現也要清掉「剛打的 /model」，不然後面工具輸出的 `⎿  Set model to …` 會被當成確認。
    #[test]
    fn a_dot_marker_row_clears_the_pending_slash_command_like_the_record_marker() {
        let screen = "❯ /model sonnet
● Ran a command
  ⎿  Set model to Opus 5.5
";
        assert_eq!(parse(screen), Switch::default());
    }
